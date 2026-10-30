//! 一次读取的文件夹树：祖先、子树与整树计数都从这里算。
//!
//! 文件夹路径在数据库里按 `COLLATE NOCASE` 唯一，旧数据还可能出现同一子树
//! 两种大小写前缀；路径比较一律走 [`folder_within`] / 小写键，不能再用裸
//! `starts_with`。查询层与文件夹写路径共用这份实现，避免各自维护一套口径。

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;

use crate::app::error::AppResult;

use super::paths::{folder_within, parent_path};

#[derive(Debug, Clone, Default)]
pub(crate) struct FolderTree {
    ids: Vec<i64>,
    paths: Vec<String>,
    by_id: HashMap<i64, usize>,
    by_key: HashMap<String, usize>,
}

impl FolderTree {
    pub(crate) fn new(rows: impl IntoIterator<Item = (i64, String)>) -> Self {
        let mut tree = Self::default();
        for (id, path) in rows {
            let index = tree.ids.len();
            tree.ids.push(id);
            tree.by_id.insert(id, index);
            tree.by_key.insert(path.to_ascii_lowercase(), index);
            tree.paths.push(path);
        }
        tree
    }

    /// 正常树：回收站与待释放行对文件夹树、统计与 MCP 都不可见。
    pub(crate) fn normal(connection: &Connection) -> AppResult<Self> {
        let mut statement = connection.prepare(
            "SELECT id, path FROM folders
             WHERE kind = 'normal' AND trashed_at IS NULL AND purge_operation_id IS NULL
             ORDER BY path",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Self::new(rows))
    }

    pub(crate) fn id_at(&self, path: &str) -> Option<i64> {
        self.by_key
            .get(&path.to_ascii_lowercase())
            .map(|index| self.ids[*index])
    }

    /// 按加载顺序（正常树为路径序）逐行返回 id 与路径。
    pub(crate) fn entries(&self) -> impl Iterator<Item = (i64, &str)> {
        self.ids
            .iter()
            .copied()
            .zip(self.paths.iter().map(String::as_str))
    }

    /// 从指定路径（含）到根的现有行；顺序由深到浅。
    pub(crate) fn ancestors_of_path(&self, path: &str) -> Vec<i64> {
        let mut ids = Vec::new();
        let mut current = Some(path);
        while let Some(prefix) = current {
            if let Some(&index) = self.by_key.get(&prefix.to_ascii_lowercase()) {
                ids.push(self.ids[index]);
            }
            if prefix.is_empty() {
                break;
            }
            current = Some(parent_path(prefix));
        }
        ids
    }

    /// 指定路径自身与其下全部现有行；空路径表示根，只返回根行本身。
    pub(crate) fn ids_under_path(&self, path: &str) -> Vec<i64> {
        if path.is_empty() {
            return self.id_at(path).into_iter().collect();
        }
        self.paths
            .iter()
            .enumerate()
            .filter(|(_, candidate)| folder_within(candidate, path))
            .map(|(index, _)| self.ids[index])
            .collect()
    }

    /// 把直属图片数累加到自身与全部祖先；只返回计数非零的文件夹，空文件夹由调用方补 0。
    pub(crate) fn totals(&self, direct: impl IntoIterator<Item = (i64, u64)>) -> HashMap<i64, u64> {
        let mut totals: HashMap<i64, u64> = HashMap::new();
        for (folder_id, count) in direct {
            let Some(&index) = self.by_id.get(&folder_id) else {
                continue;
            };
            let path = self.paths[index].to_ascii_lowercase();
            let mut current = Some(path.as_str());
            while let Some(prefix) = current {
                if let Some(&ancestor) = self.by_key.get(prefix) {
                    *totals.entry(self.ids[ancestor]).or_default() += count;
                }
                if prefix.is_empty() {
                    break;
                }
                current = Some(parent_path(prefix));
            }
        }
        totals
    }

    /// 选中文件夹之下的子文件夹数；空选择表示全库，根行本身不计入。
    pub(crate) fn descendant_count(&self, selected: &[i64]) -> u64 {
        if selected.is_empty() {
            return self.paths.iter().filter(|path| !path.is_empty()).count() as u64;
        }
        let selected_indexes: HashSet<usize> = selected
            .iter()
            .filter_map(|id| self.by_id.get(id).copied())
            .collect();
        if selected_indexes.is_empty() {
            return 0;
        }
        let selected_keys: HashSet<String> = selected_indexes
            .iter()
            .map(|index| self.paths[*index].to_ascii_lowercase())
            .collect();
        self.paths
            .iter()
            .enumerate()
            .filter(|(index, path)| {
                if selected_indexes.contains(index) {
                    return false;
                }
                let mut current = Some(path.as_str());
                while let Some(prefix) = current {
                    if selected_keys.contains(&prefix.to_ascii_lowercase()) {
                        return true;
                    }
                    if prefix.is_empty() {
                        break;
                    }
                    current = Some(parent_path(prefix));
                }
                false
            })
            .count() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> FolderTree {
        FolderTree::new([
            (1, "".to_owned()),
            (2, "Art".to_owned()),
            (3, "Art/Sketch".to_owned()),
            (4, "ART/Ink".to_owned()),
            (5, "Photo".to_owned()),
        ])
    }

    #[test]
    fn ancestors_follow_case_insensitive_paths() {
        let tree = tree();
        assert_eq!(tree.ancestors_of_path("art/ink"), vec![4, 2, 1]);
        assert_eq!(tree.ancestors_of_path("Art/Sketch"), vec![3, 2, 1]);
        assert_eq!(tree.ancestors_of_path("Missing/Deep"), vec![1]);
    }

    #[test]
    fn subtree_ids_include_self_and_ignore_case() {
        let tree = tree();
        let mut ids = tree.ids_under_path("art");
        ids.sort_unstable();
        assert_eq!(ids, vec![2, 3, 4]);
        assert_eq!(tree.ids_under_path(""), vec![1]);
        assert!(tree.ids_under_path("Phot").is_empty());
    }

    #[test]
    fn totals_accumulate_to_every_ancestor() {
        let tree = tree();
        let totals = tree.totals([(3, 2), (4, 5), (5, 1), (99, 7)]);
        assert_eq!(totals.get(&3), Some(&2));
        assert_eq!(totals.get(&4), Some(&5));
        assert_eq!(totals.get(&2), Some(&7));
        assert_eq!(totals.get(&5), Some(&1));
        assert_eq!(totals.get(&1), Some(&8));
    }

    #[test]
    fn descendant_count_excludes_selected_rows() {
        let tree = tree();
        assert_eq!(tree.descendant_count(&[]), 4);
        assert_eq!(tree.descendant_count(&[2]), 2);
        assert_eq!(tree.descendant_count(&[2, 3]), 1);
        assert_eq!(tree.descendant_count(&[3]), 0);
        assert_eq!(tree.descendant_count(&[99]), 0);
    }
}
