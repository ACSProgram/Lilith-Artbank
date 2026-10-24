use std::cmp::Ordering;

use rusqlite::OptionalExtension;

use crate::{
    app::error::{AppError, AppResult},
    imageops::{
        colors::{self, LabColor},
        phash,
    },
    library::db::LibraryDb,
};

pub const DEFAULT_PHASH_THRESHOLD: u32 = 6;
pub const MAX_PHASH_THRESHOLD: u32 = 256;
/// 8 段 × 32 位分段索引在阈值 ≤7 时由鸽笼原理保证至少一段完全相同。
pub const MAX_INDEXED_PHASH_THRESHOLD: u32 = 7;
pub const DEFAULT_LIMIT: usize = 2_000;
pub const MAX_LIMIT: usize = 10_000;

const PHASH_BANDS: usize = 8;
/// 单次全库分组的候选检查上限；超过即标记截断，避免退化数据把扫描拖成 O(n²)。
const MAX_CANDIDATE_CHECKS: usize = 20_000_000;
/// 单个分段候选组的最大规模；更大的组不展开两两比较，只标记截断。
pub(crate) const MAX_GROUP_SIZE: usize = 4_096;
/// 单次分组返回的最大组数；embedding 复核复用同一上限。
pub(crate) const MAX_GROUPS: usize = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorPalette {
    Colors4,
    Colors8,
}

pub struct TargetVisuals {
    pub phash: [u8; 32],
    pub colors4: [u8; colors::COLORS4_BYTES],
    pub colors8: [u8; colors::COLORS8_BYTES],
}

pub struct PhashGroups {
    pub groups: Vec<Vec<i64>>,
    pub truncated: bool,
}

#[derive(Clone, Copy)]
struct VisualRow {
    id: i64,
    phash: [u64; 4],
    colors4: [LabColor; 4],
    colors8: [LabColor; 8],
}

/// 常驻的紧凑视觉索引：pHash 与两套 OKLab 主色。
///
/// 由调用方（AiRuntime）按图库代次缓存；`is_current` 用 `count(*) + max(id)` 识别
/// 导入等新增行。编辑/移入回收站等操作由调用方显式失效。
pub struct VisualIndex {
    rows: Vec<VisualRow>,
    stamp: (i64, i64),
}

impl VisualIndex {
    pub fn load(database: &LibraryDb) -> AppResult<Self> {
        let stamp = visual_stamp(database)?;
        let rows = database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, phash, colors4, colors8 FROM images
                 WHERE deleted_at IS NULL
                   AND length(phash) = 32 AND length(colors4) = 16 AND length(colors8) = 32",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id, phash, colors4, colors8) = row?;
                let mut hash = [0_u8; 32];
                hash.copy_from_slice(&phash);
                out.push(VisualRow {
                    id,
                    phash: phash::words(&hash),
                    colors4: colors::prepare_palette::<4>(&colors4),
                    colors8: colors::prepare_palette::<8>(&colors8),
                });
            }
            Ok(out)
        })?;
        Ok(Self { rows, stamp })
    }

    pub fn is_current(&self, database: &LibraryDb) -> AppResult<bool> {
        Ok(visual_stamp(database)? == self.stamp)
    }

    pub fn similar_phash(
        &self,
        target: [u64; 4],
        threshold: u32,
        limit: usize,
        exclude: i64,
    ) -> Vec<(i64, u32)> {
        let mut scored = Vec::new();
        for row in &self.rows {
            if row.id == exclude {
                continue;
            }
            let distance = phash::distance_words(&target, &row.phash);
            if distance <= threshold {
                scored.push((row.id, distance));
            }
        }
        scored.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));
        scored.truncate(limit.clamp(1, MAX_LIMIT));
        scored
    }

    pub fn similar_color(
        &self,
        target: &[LabColor],
        palette: ColorPalette,
        limit: usize,
        exclude: i64,
    ) -> Vec<(i64, f32)> {
        let mut scored = Vec::new();
        for row in &self.rows {
            if row.id == exclude {
                continue;
            }
            let candidate = match palette {
                ColorPalette::Colors4 => &row.colors4[..],
                ColorPalette::Colors8 => &row.colors8[..],
            };
            if candidate.len() != target.len() {
                continue;
            }
            scored.push((row.id, colors::prepared_distance(target, candidate)));
        }
        scored.sort_by(|left, right| {
            left.1
                .partial_cmp(&right.1)
                .unwrap_or(Ordering::Equal)
                .then_with(|| left.0.cmp(&right.0))
        });
        scored.truncate(limit.clamp(1, MAX_LIMIT));
        scored
    }

    /// 全库 pHash 相似对分组；索引已常驻，不再读库。
    pub fn phash_groups(&self, threshold: u32) -> AppResult<PhashGroups> {
        if threshold > MAX_INDEXED_PHASH_THRESHOLD {
            return Err(AppError::message(
                "全库 pHash 分组只支持 0-7；更大阈值请使用目标图相似",
            ));
        }
        if self.rows.len() < 2 {
            return Ok(PhashGroups {
                groups: Vec::new(),
                truncated: false,
            });
        }
        let words: Vec<[u64; 4]> = self.rows.iter().map(|row| row.phash).collect();
        let ids: Vec<i64> = self.rows.iter().map(|row| row.id).collect();
        let mut union = UnionFind::new(words.len());

        // 完全相同的哈希先并组，避免纯色图等大组在分段候选里重复枚举。
        let mut exact: Vec<(u64, u64, u64, u64, usize)> = words
            .iter()
            .enumerate()
            .map(|(index, words)| (words[0], words[1], words[2], words[3], index))
            .collect();
        exact.sort_unstable();
        for pair in exact.windows(2) {
            if pair[0].0 == pair[1].0
                && pair[0].1 == pair[1].1
                && pair[0].2 == pair[1].2
                && pair[0].3 == pair[1].3
            {
                union.union(pair[0].4, pair[1].4);
            }
        }

        let mut bands = Vec::with_capacity(words.len() * PHASH_BANDS);
        for (index, words) in words.iter().enumerate() {
            for band in 0..PHASH_BANDS {
                let chunk = (words[band / 2] >> ((band % 2) * 32)) as u32;
                bands.push((band as u8, chunk, index as u32));
            }
        }
        bands.sort_unstable();

        let mut checks = 0_usize;
        let mut truncated = false;
        let mut start = 0;
        while start < bands.len() {
            let mut end = start + 1;
            while end < bands.len()
                && bands[end].0 == bands[start].0
                && bands[end].1 == bands[start].1
            {
                end += 1;
            }
            let run = &bands[start..end];
            if run.len() >= 2 {
                if run.len() > MAX_GROUP_SIZE {
                    truncated = true;
                } else {
                    'pairs: for left in 0..run.len() - 1 {
                        for right in left + 1..run.len() {
                            let a = run[left].2 as usize;
                            let b = run[right].2 as usize;
                            checks += 1;
                            if checks > MAX_CANDIDATE_CHECKS {
                                truncated = true;
                                break 'pairs;
                            }
                            if phash::distance_words(&words[a], &words[b]) <= threshold {
                                union.union(a, b);
                            }
                        }
                    }
                }
            }
            start = end;
        }

        let mut grouped: std::collections::HashMap<usize, Vec<i64>> =
            std::collections::HashMap::new();
        for (index, id) in ids.iter().enumerate() {
            grouped.entry(union.find(index)).or_default().push(*id);
        }
        let mut groups: Vec<Vec<i64>> = grouped
            .into_values()
            .filter(|ids| ids.len() >= 2)
            .map(|mut ids| {
                ids.sort_unstable();
                ids
            })
            .collect();
        groups.sort_by(|left, right| {
            right
                .len()
                .cmp(&left.len())
                .then_with(|| left[0].cmp(&right[0]))
        });
        if groups.len() > MAX_GROUPS {
            groups.truncate(MAX_GROUPS);
            truncated = true;
        }
        Ok(PhashGroups { groups, truncated })
    }
}

fn visual_stamp(database: &LibraryDb) -> AppResult<(i64, i64)> {
    database.with_reader(|connection| {
        Ok(connection.query_row(
            "SELECT count(*), COALESCE(max(id), 0) FROM images WHERE deleted_at IS NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    })
}

/// 确认图片存在且不在回收站；pose 相似度不读哈希/主色，只需要可见性。
pub fn ensure_visible(database: &LibraryDb, image_id: i64) -> AppResult<()> {
    let found = database.with_reader(|connection| {
        Ok(connection
            .query_row(
                "SELECT 1 FROM images WHERE id = ?1 AND deleted_at IS NULL",
                [image_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    })?;
    if found {
        Ok(())
    } else {
        Err(AppError::message("图片不存在或已在回收站"))
    }
}

/// 读取目标图的 pHash 与两套主色；回收站中的图片不参与相似度。
pub fn target_visuals(database: &LibraryDb, image_id: i64) -> AppResult<TargetVisuals> {
    let visuals = database.with_reader(|connection| {
        let row = connection
            .query_row(
                "SELECT phash, colors4, colors8 FROM images
                 WHERE id = ?1 AND deleted_at IS NULL",
                [image_id],
                |row| {
                    let phash: Vec<u8> = row.get(0)?;
                    let colors4: Vec<u8> = row.get(1)?;
                    let colors8: Vec<u8> = row.get(2)?;
                    Ok((phash, colors4, colors8))
                },
            )
            .optional()?;
        Ok(row)
    })?;
    let Some((phash, colors4, colors8)) = visuals else {
        return Err(AppError::message("图片不存在或已在回收站"));
    };
    if phash.len() != 32
        || colors4.len() != colors::COLORS4_BYTES
        || colors8.len() != colors::COLORS8_BYTES
    {
        return Err(AppError::message("图片的哈希或主色数据不完整"));
    }
    let mut target = TargetVisuals {
        phash: [0; 32],
        colors4: [0; colors::COLORS4_BYTES],
        colors8: [0; colors::COLORS8_BYTES],
    };
    target.phash.copy_from_slice(&phash);
    target.colors4.copy_from_slice(&colors4);
    target.colors8.copy_from_slice(&colors8);
    Ok(target)
}

/// 目标图 pHash 相似：Hamming 距离升序，距离相同按 id 升序；结果排除目标图本身。
#[cfg(test)]
pub fn similar_by_phash(
    database: &LibraryDb,
    target_id: i64,
    threshold: u32,
    limit: usize,
) -> AppResult<Vec<(i64, u32)>> {
    if threshold > MAX_PHASH_THRESHOLD {
        return Err(AppError::message("pHash 阈值超出 0-256"));
    }
    let target = target_visuals(database, target_id)?;
    let index = VisualIndex::load(database)?;
    Ok(index.similar_phash(phash::words(&target.phash), threshold, limit, target_id))
}

/// 目标图主色相似：对称加权 OKLab 距离升序；结果排除目标图本身。
#[cfg(test)]
pub fn similar_by_color(
    database: &LibraryDb,
    target_id: i64,
    palette: ColorPalette,
    limit: usize,
) -> AppResult<Vec<(i64, f32)>> {
    let target = target_visuals(database, target_id)?;
    let index = VisualIndex::load(database)?;
    match palette {
        ColorPalette::Colors4 => {
            let prepared = colors::prepare_palette::<4>(&target.colors4);
            Ok(index.similar_color(&prepared, palette, limit, target_id))
        }
        ColorPalette::Colors8 => {
            let prepared = colors::prepare_palette::<8>(&target.colors8);
            Ok(index.similar_color(&prepared, palette, limit, target_id))
        }
    }
}

/// 全库 pHash 相似对分组；分段索引见 `VisualIndex::phash_groups`。
#[cfg(test)]
pub fn phash_groups(database: &LibraryDb, threshold: u32) -> AppResult<PhashGroups> {
    VisualIndex::load(database)?.phash_groups(threshold)
}

/// 并查集；pHash 分组与 embedding 复核共用同一实现。
pub(crate) struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    pub(crate) fn new(len: usize) -> Self {
        Self {
            parent: (0..len).collect(),
            rank: vec![0; len],
        }
    }

    pub(crate) fn find(&mut self, index: usize) -> usize {
        let mut root = index;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut cursor = index;
        while self.parent[cursor] != cursor {
            let next = self.parent[cursor];
            self.parent[cursor] = root;
            cursor = next;
        }
        root
    }

    pub(crate) fn union(&mut self, left: usize, right: usize) {
        let left = self.find(left);
        let right = self.find(right);
        if left == right {
            return;
        }
        match self.rank[left].cmp(&self.rank[right]) {
            Ordering::Less => self.parent[left] = right,
            Ordering::Greater => self.parent[right] = left,
            Ordering::Equal => {
                self.parent[right] = left;
                self.rank[left] += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::params;

    use super::*;
    use crate::library::service;

    fn fixture() -> (tempfile::TempDir, service::LibrarySession) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "相似度测试", 1).unwrap();
        (temp, session)
    }

    fn insert(
        session: &service::LibrarySession,
        id: i64,
        phash: [u8; 32],
        colors4: [u8; 16],
        colors8: [u8; 32],
    ) {
        session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO images(
                        id, content_hash, file_ext, folder_id, file_size, width, height, format,
                        has_alpha, phash, colors4, colors8, score, description, imported_at,
                        modified_at
                     ) VALUES (?1, ?2, 'png', 1, 10, 4, 4, 'png', 0, ?3, ?4, ?5, 0, '', 1, 1)",
                    params![
                        id,
                        format!("{id:064x}"),
                        phash.as_slice(),
                        colors4.as_slice(),
                        colors8.as_slice()
                    ],
                )?;
                Ok(())
            })
            .unwrap();
    }

    /// 单一颜色的 4/8 色记录：首项吃掉余量，保证权重和为 255。
    fn solid_palette(size: usize, rgb: [u8; 3]) -> Vec<u8> {
        let mut palette = vec![0_u8; size * 4];
        let weight = 255 / size;
        for index in 0..size {
            palette[index * 4] = rgb[0];
            palette[index * 4 + 1] = rgb[1];
            palette[index * 4 + 2] = rgb[2];
            palette[index * 4 + 3] = if index == 0 {
                255 - weight as u8 * (size as u8 - 1)
            } else {
                weight as u8
            };
        }
        palette
    }

    fn phash_with_bits(bits: &[usize]) -> [u8; 32] {
        let mut hash = [0_u8; 32];
        for bit in bits {
            hash[bit / 8] |= 1 << (7 - (bit % 8));
        }
        hash
    }

    #[test]
    fn phash_target_orders_by_distance_and_excludes_target() {
        let (_temp, session) = fixture();
        let base = phash_with_bits(&[0, 1, 2, 3, 4, 5, 6, 7]);
        let near = phash_with_bits(&[0, 1, 2, 3, 4, 5, 6]);
        let far = phash_with_bits(&[100, 101, 102, 103, 104, 105, 106, 107]);
        insert(&session, 1, base, [0; 16], [0; 32]);
        insert(&session, 2, near, [0; 16], [0; 32]);
        insert(&session, 3, far, [0; 16], [0; 32]);

        let results = similar_by_phash(&session.db, 1, 6, 10).unwrap();
        assert_eq!(results, vec![(2, 1)]);
        let results = similar_by_phash(&session.db, 1, 256, 10).unwrap();
        assert_eq!(results[0], (2, 1));
        assert_eq!(results[1].0, 3);
        assert!(results.iter().all(|(id, _)| *id != 1));
    }

    #[test]
    fn color_target_orders_by_weighted_distance() {
        let (_temp, session) = fixture();
        let hash = [0_u8; 32];
        let red = solid_palette(4, [210, 30, 30]);
        let near_red = solid_palette(4, [200, 40, 40]);
        let blue = solid_palette(4, [30, 40, 210]);
        insert(&session, 1, hash, red.clone().try_into().unwrap(), [0; 32]);
        insert(&session, 2, hash, near_red.try_into().unwrap(), [0; 32]);
        insert(&session, 3, hash, blue.try_into().unwrap(), [0; 32]);

        let results = similar_by_color(&session.db, 1, ColorPalette::Colors4, 10).unwrap();
        assert_eq!(results[0].0, 2);
        assert_eq!(results[1].0, 3);
        assert!(results[0].1 < results[1].1);
    }

    #[test]
    fn phash_groups_finds_pairs_across_band_boundaries() {
        let (_temp, session) = fixture();
        // 7 个段各改 1 位，留第 8 段完全相同；总距离 7，索引必须能找到这一对。
        let mut near = [0_u8; 32];
        for band in 0..7 {
            near[band * 4] = 1 << (band % 8);
        }
        let far = [0xff_u8; 32];
        insert(&session, 1, [0; 32], [0; 16], [0; 32]);
        insert(&session, 2, near, [0; 16], [0; 32]);
        insert(&session, 3, far, [0; 16], [0; 32]);

        let groups = phash_groups(&session.db, 7).unwrap();
        assert!(!groups.truncated);
        assert_eq!(groups.groups, vec![vec![1, 2]]);
        assert!(phash_groups(&session.db, 8).is_err());
    }

    #[test]
    fn deleted_images_are_excluded() {
        let (_temp, session) = fixture();
        insert(&session, 1, [0; 32], [0; 16], [0; 32]);
        insert(&session, 2, [0; 32], [0; 16], [0; 32]);
        session
            .db
            .with_writer(|connection| {
                connection.execute("UPDATE images SET deleted_at = 1 WHERE id = 2", [])?;
                Ok(())
            })
            .unwrap();
        assert!(similar_by_phash(&session.db, 1, 6, 10).unwrap().is_empty());
        assert!(phash_groups(&session.db, 6).unwrap().groups.is_empty());
    }
}
