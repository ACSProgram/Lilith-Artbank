use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use rusqlite::{
    Connection, OptionalExtension, Transaction, params, params_from_iter, types::Value,
};
use serde::{Deserialize, Serialize};

use crate::{
    app::error::{AppError, AppResult},
    library::{
        db::LibraryDb,
        history::{
            self, FolderDeleted, FolderPathChange, FolderRef, FolderReorder, FolderRow,
            FolderSortChange, HistoryKind, NewOperation,
        },
        import::{ensure_folder_path, next_sort_index},
        paths::{
            LibraryPaths, folder_within, join_folder, normalize_folder, normalize_folder_name,
            parent_path, same_folder, subtree_like_pattern,
        },
        placeholders,
        query::{self, FolderChange, FolderInfo},
        service::now_ms,
    },
};

fn folder_id(connection: &Connection, path: &str) -> AppResult<Option<i64>> {
    connection
        .query_row(
            "SELECT id FROM folders WHERE path = ?1 COLLATE NOCASE",
            [path],
            |row| row.get(0),
        )
        .optional()
        .map_err(AppError::from)
}

fn folder_path(connection: &Connection, id: i64) -> AppResult<String> {
    if id <= 0 {
        return Err(AppError::message("文件夹 id 无效"));
    }
    connection
        .query_row("SELECT path FROM folders WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()?
        .ok_or_else(|| AppError::message("文件夹不存在"))
}

/// 临时文件夹由生命周期管理，禁止重命名、移动、删除或参与同级排序。
/// 回收站文件夹与待释放文件夹也不参与正常树的写入。
fn ensure_normal_folder(connection: &Connection, id: i64) -> AppResult<()> {
    let state: Option<(String, Option<i64>, Option<i64>)> = connection
        .query_row(
            "SELECT kind, trashed_at, purge_operation_id FROM folders WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((kind, trashed_at, purge_operation_id)) = state else {
        return Ok(());
    };
    if kind == "temp" {
        return Err(AppError::message("临时文件夹不能重命名、移动、删除或排序"));
    }
    if purge_operation_id.is_some() {
        return Err(AppError::message("文件夹正在释放，不能再修改"));
    }
    if trashed_at.is_some() {
        return Err(AppError::message("文件夹在回收站中，请先恢复或永久删除"));
    }
    Ok(())
}

/// 只有回收站里的普通文件夹能恢复或永久删除。
fn ensure_trashed_folder(connection: &Connection, id: i64) -> AppResult<String> {
    let state: Option<(String, Option<i64>, Option<i64>, String)> = connection
        .query_row(
            "SELECT kind, trashed_at, purge_operation_id, path FROM folders WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((kind, trashed_at, purge_operation_id, path)) = state else {
        return Err(AppError::message("文件夹不存在"));
    };
    if kind == "temp" {
        return Err(AppError::message("临时文件夹不能恢复或永久删除"));
    }
    if purge_operation_id.is_some() {
        return Err(AppError::message("文件夹正在释放，请稍后再试"));
    }
    if trashed_at.is_none() {
        return Err(AppError::message("文件夹不在回收站中"));
    }
    Ok(path)
}

/// 目标路径已被占用时给出可操作的原因：回收站与待释放优先于普通重名。
fn ensure_target_free(
    connection: &Connection,
    path: &str,
    self_id: i64,
    conflict: &str,
) -> AppResult<()> {
    let state: Option<(i64, Option<i64>, Option<i64>)> = connection
        .query_row(
            "SELECT id, trashed_at, purge_operation_id FROM folders
             WHERE path = ?1 COLLATE NOCASE",
            [path],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((id, trashed_at, purge_operation_id)) = state else {
        return Ok(());
    };
    if id == self_id {
        return Ok(());
    }
    if purge_operation_id.is_some() {
        return Err(AppError::message("目标文件夹正在释放，请稍后再试"));
    }
    if trashed_at.is_some() {
        return Err(AppError::message(
            "目标路径的文件夹在回收站中，请先恢复或永久删除",
        ));
    }
    Err(AppError::message(conflict))
}

fn ensure_inside_root(root: &Path, directory: &Path) -> AppResult<()> {
    let canonical_root = root.canonicalize()?;
    let canonical_directory = directory.canonicalize()?;
    if !canonical_directory.starts_with(&canonical_root) {
        return Err(AppError::message("文件夹位于图库目录之外"));
    }
    Ok(())
}

pub fn create(db: &LibraryDb, root: &Path, path: &str) -> AppResult<Vec<FolderInfo>> {
    create_with_id(db, root, path).map(|(folders, _)| folders)
}

pub fn create_with_id(
    db: &LibraryDb,
    root: &Path,
    path: &str,
) -> AppResult<(Vec<FolderInfo>, Option<i64>)> {
    let path = normalize_folder(path)?;
    if path.is_empty() {
        return Err(AppError::message("根目录已经存在"));
    }
    let paths = LibraryPaths::new(root);
    let mut affected_ids: Vec<i64> = Vec::new();
    let ((), operation_id) = history::write_with_id(
        db,
        NewOperation::new(HistoryKind::FolderCreate, format!("新建文件夹「{path}」")),
        |transaction, draft| {
            let existing: Option<(Option<i64>, Option<i64>)> = transaction
                .query_row(
                    "SELECT trashed_at, purge_operation_id FROM folders
                     WHERE path = ?1 COLLATE NOCASE",
                    [&path],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((trashed_at, purge_operation_id)) = existing {
                if purge_operation_id.is_some() {
                    return Err(AppError::message("文件夹正在释放，请稍后再试"));
                }
                if trashed_at.is_some() {
                    return Err(AppError::message(
                        "该路径的文件夹在回收站中，请先恢复或永久删除",
                    ));
                }
                return Err(AppError::message("文件夹已存在"));
            }
            // 先补数据库行拿到规范路径，再按规范路径落物理目录：祖先在库里是
            // `Art`、请求是 `art/sub` 时，新目录必须建在 `Art/sub`。
            let mut created = Vec::new();
            let (_, canonical) = ensure_folder_path(transaction, &path, now_ms(), &mut created)?;
            if created.is_empty() {
                return Err(AppError::message("文件夹已存在"));
            }
            let target = paths.folder_dir(&canonical);
            std::fs::create_dir_all(&target)?;
            ensure_inside_root(root, &target)?;
            affected_ids = created.iter().map(|(id, _)| *id).collect();
            let rows: Vec<FolderRef> = created
                .into_iter()
                .map(|(id, path)| FolderRef { id, path })
                .collect();
            draft.json(&rows)?;
            draft.affected(rows.len() as u64);
            Ok(())
        },
    )?;
    Ok((query::folder_infos(db, &affected_ids)?, operation_id))
}

pub fn rename(
    db: &LibraryDb,
    root: &Path,
    folder_id_value: i64,
    name: &str,
) -> AppResult<Vec<FolderInfo>> {
    rename_with_id(db, root, folder_id_value, name).map(|(folders, _)| folders)
}

pub fn rename_with_id(
    db: &LibraryDb,
    root: &Path,
    folder_id_value: i64,
    name: &str,
) -> AppResult<(Vec<FolderInfo>, Option<i64>)> {
    let name = normalize_folder_name(name)?;
    let paths = LibraryPaths::new(root);
    let mut affected_ids: Vec<i64> = Vec::new();
    let ((), operation_id) = history::write_with_id(
        db,
        NewOperation::new(
            HistoryKind::FolderRename,
            format!("重命名文件夹为「{name}」"),
        ),
        |transaction, draft| {
            ensure_normal_folder(transaction, folder_id_value)?;
            let old_path = folder_path(transaction, folder_id_value)?;
            if old_path.is_empty() {
                return Err(AppError::message("不能重命名图库根目录"));
            }
            let parent = parent_path(&old_path);
            let new_path = join_folder(parent, &name);
            if old_path == new_path {
                return Ok(());
            }
            ensure_target_free(transaction, &new_path, folder_id_value, "目标文件夹已存在")?;

            let old_dir = paths.folder_dir(&old_path);
            let new_dir = paths.folder_dir(&new_path);
            if !old_dir.is_dir() {
                return Err(AppError::message(format!(
                    "文件夹目录不存在：{}",
                    old_dir.display()
                )));
            }
            let case_only = same_folder(&new_path, &old_path);
            if !case_only && new_dir.exists() {
                return Err(AppError::message("目标文件夹目录已存在"));
            }
            rename_directory(&old_dir, &new_dir, case_only)?;
            ensure_inside_root(root, &new_dir)?;

            if let Err(error) =
                update_subtree_paths(transaction, &old_path, &new_path, folder_id_value)
            {
                if let Err(rollback) = rename_directory(&new_dir, &old_dir, true) {
                    log::error!(
                        "回滚文件夹重命名失败：{rollback}；目录位于 {}",
                        new_dir.display()
                    );
                }
                return Err(error);
            }
            let sort_index: i64 = transaction.query_row(
                "SELECT sort_index FROM folders WHERE id = ?1",
                [folder_id_value],
                |row| row.get(0),
            )?;
            draft.json(&FolderPathChange {
                id: folder_id_value,
                old_path,
                new_path: new_path.clone(),
                old_sort_index: sort_index,
                new_sort_index: sort_index,
            })?;
            affected_ids = subtree_folder_ids(transaction, &new_path)?;
            draft.affected(1);
            Ok(())
        },
    )?;
    Ok((query::folder_infos(db, &affected_ids)?, operation_id))
}

pub fn move_to(
    db: &LibraryDb,
    root: &Path,
    folder_id_value: i64,
    parent_id: i64,
) -> AppResult<Vec<FolderInfo>> {
    move_to_with_id(db, root, folder_id_value, parent_id).map(|(folders, _)| folders)
}

pub fn move_to_with_id(
    db: &LibraryDb,
    root: &Path,
    folder_id_value: i64,
    parent_id: i64,
) -> AppResult<(Vec<FolderInfo>, Option<i64>)> {
    let paths = LibraryPaths::new(root);
    let label_path = db.with_reader(|connection| folder_path(connection, folder_id_value))?;
    let mut affected_ids: Vec<i64> = Vec::new();
    let ((), operation_id) = history::write_with_id(
        db,
        NewOperation::new(
            HistoryKind::FolderMove,
            format!("移动文件夹「{label_path}」"),
        ),
        |transaction, draft| {
            ensure_normal_folder(transaction, folder_id_value)?;
            ensure_normal_folder(transaction, parent_id)?;
            let old_path = folder_path(transaction, folder_id_value)?;
            if old_path.is_empty() {
                return Err(AppError::message("不能移动图库根目录"));
            }
            let parent_path = folder_path(transaction, parent_id)?;
            if folder_within(&parent_path, &old_path) {
                return Err(AppError::message("不能把文件夹移动到自身或子文件夹"));
            }
            let leaf = old_path
                .rsplit_once('/')
                .map_or(old_path.as_str(), |(_, leaf)| leaf);
            let new_path = join_folder(&parent_path, leaf);
            if new_path == old_path {
                return Ok(());
            }
            ensure_target_free(
                transaction,
                &new_path,
                folder_id_value,
                "目标位置已经有同名文件夹",
            )?;

            let old_dir = paths.folder_dir(&old_path);
            let new_dir = paths.folder_dir(&new_path);
            if !old_dir.is_dir() {
                return Err(AppError::message(format!(
                    "文件夹目录不存在：{}",
                    old_dir.display()
                )));
            }
            if let Some(parent) = new_dir.parent() {
                std::fs::create_dir_all(parent)?;
                ensure_inside_root(root, parent)?;
            }
            if new_dir.exists() {
                return Err(AppError::message("目标文件夹目录已存在"));
            }
            std::fs::rename(&old_dir, &new_dir)?;
            ensure_inside_root(root, &new_dir)?;

            let result = (|| -> AppResult<(i64, i64)> {
                let old_sort_index: i64 = transaction.query_row(
                    "SELECT sort_index FROM folders WHERE id = ?1",
                    [folder_id_value],
                    |row| row.get(0),
                )?;
                let new_sort_index = next_sort_index(transaction, &new_path)?;
                update_subtree_paths(transaction, &old_path, &new_path, folder_id_value)?;
                transaction.execute(
                    "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
                    params![new_sort_index, folder_id_value],
                )?;
                Ok((old_sort_index, new_sort_index))
            })();
            let (old_sort_index, new_sort_index) = match result {
                Ok(values) => values,
                Err(error) => {
                    if let Err(rollback) = std::fs::rename(&new_dir, &old_dir) {
                        log::error!(
                            "回滚文件夹移动失败：{rollback}；目录位于 {}",
                            new_dir.display()
                        );
                    }
                    return Err(error);
                }
            };
            draft.json(&FolderPathChange {
                id: folder_id_value,
                old_path: old_path.clone(),
                new_path: new_path.clone(),
                old_sort_index,
                new_sort_index,
            })?;
            affected_ids = subtree_folder_ids(transaction, &new_path)?;
            let old_parent = old_path.rsplit_once('/').map_or("", |(parent, _)| parent);
            let new_parent = new_path.rsplit_once('/').map_or("", |(parent, _)| parent);
            affected_ids.extend(ancestor_folder_ids(transaction, old_parent)?);
            affected_ids.extend(ancestor_folder_ids(transaction, new_parent)?);
            draft.affected(1);
            Ok(())
        },
    )?;
    Ok((query::folder_infos(db, &affected_ids)?, operation_id))
}

/// 把同一父级下的若干文件夹移动到指定位置；只调整 `sort_index`，不动目录。
pub fn reorder(
    db: &LibraryDb,
    folder_ids: &[i64],
    parent_id: i64,
    sort_index: i64,
) -> AppResult<Vec<FolderInfo>> {
    if folder_ids.is_empty() {
        return Err(AppError::message("没有要调整顺序的文件夹"));
    }
    let mut moved = HashSet::new();
    for id in folder_ids {
        if *id <= 0 || !moved.insert(*id) {
            return Err(AppError::message("文件夹 id 无效或重复"));
        }
    }
    let mut affected_ids: Vec<i64> = Vec::new();
    history::write(
        db,
        NewOperation::new(HistoryKind::FolderReorder, "调整文件夹顺序"),
        |transaction, draft| {
            ensure_normal_folder(transaction, parent_id)?;
            for id in folder_ids {
                ensure_normal_folder(transaction, *id)?;
            }
            let parent = folder_path(transaction, parent_id)?;
            // 同级文件夹一次读出来：手动排序只在拖放时发生，几千行路径的读取代价可以接受。
            let mut statement = transaction.prepare(
                "SELECT id, path, sort_index FROM folders
                 WHERE kind = 'normal' AND trashed_at IS NULL AND purge_operation_id IS NULL
                 ORDER BY sort_index, id",
            )?;
            let folders = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);

            let siblings: Vec<(i64, String, i64)> = folders
                .into_iter()
                .filter(|(_, path, _)| !path.is_empty() && same_folder(parent_path(path), &parent))
                .collect();
            if let Some(foreign) = moved
                .iter()
                .find(|id| !siblings.iter().any(|(existing, _, _)| existing == *id))
            {
                return Err(AppError::message(format!(
                    "文件夹 {foreign} 不在目标位置，只能调整同一级文件夹的顺序"
                )));
            }

            let old_positions: Vec<(i64, i64)> = siblings
                .iter()
                .map(|(id, _, index)| (*id, *index))
                .collect();
            let mut order: Vec<i64> = siblings
                .iter()
                .map(|(id, _, _)| *id)
                .filter(|id| !moved.contains(id))
                .collect();
            let insert_at = sort_index.clamp(0, order.len() as i64) as usize;
            order.splice(insert_at..insert_at, folder_ids.iter().copied());

            let changed = order
                .iter()
                .enumerate()
                .filter(|(position, id)| {
                    old_positions
                        .iter()
                        .find(|(existing, _)| existing == *id)
                        .is_some_and(|(_, index)| *index != *position as i64)
                })
                .count();
            if changed == 0 {
                return Ok(());
            }
            for (position, id) in order.iter().enumerate() {
                transaction.execute(
                    "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
                    params![position as i64, id],
                )?;
            }
            draft.json(&FolderReorder {
                rows: old_positions,
            })?;
            affected_ids = siblings.iter().map(|(id, _, _)| *id).collect();
            draft.affected(changed as u64);
            Ok(())
        },
    )?;
    query::folder_infos(db, &affected_ids)
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FolderSortKey {
    Name,
    Total,
}

impl FolderSortKey {
    fn label(self) -> &'static str {
        match self {
            Self::Name => "名称",
            Self::Total => "图片数",
        }
    }
}

fn folder_rows_in(connection: &Connection) -> AppResult<Vec<(i64, String, i64)>> {
    let mut statement = connection.prepare(
        "SELECT id, path, sort_index FROM folders
         WHERE kind = 'normal' AND trashed_at IS NULL AND purge_operation_id IS NULL
         ORDER BY path",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// 子树内全部文件夹 id（含自身）；空路径表示根，不用于重命名/移动。
fn subtree_folder_ids(connection: &Connection, path: &str) -> AppResult<Vec<i64>> {
    let mut statement = connection.prepare(
        "SELECT id FROM folders
         WHERE path = ?1 COLLATE NOCASE OR path LIKE ?2 ESCAPE '\\'",
    )?;
    let ids = statement
        .query_map(params![path, subtree_like_pattern(path)], |row| {
            row.get::<_, i64>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids)
}

/// 从根到指定路径（含）的祖先文件夹 id；用于移动后更新两侧祖先的子树计数。
fn ancestor_folder_ids(connection: &Connection, path: &str) -> AppResult<Vec<i64>> {
    let mut ids = Vec::new();
    let mut current = Some(path);
    while let Some(prefix) = current {
        if let Some(id) = folder_id(connection, prefix)? {
            ids.push(id);
        }
        if prefix.is_empty() {
            break;
        }
        current = Some(parent_path(prefix));
    }
    Ok(ids)
}

fn rollback_renames(done: &[(PathBuf, PathBuf)]) {
    for (old, new) in done.iter().rev() {
        if let Err(error) = std::fs::rename(new, old) {
            log::error!("回滚文件夹移动失败：{error}；目录位于 {}", new.display());
        }
    }
}

/// 撤销/重做里已经落地的文件系统动作；任一步失败时逆序撤回。
///
/// 一次 `place_many` 会写多条 `FolderPathChange`，撤销/重做在同一个事务里逐条处理。
/// 后一项失败时数据库整体回滚，前面已经改过的目录必须一起改回去，否则行指向的目录
/// 在磁盘上已经不存在。
#[derive(Default)]
pub(crate) struct FolderFsJournal {
    steps: Vec<FolderFsStep>,
}

enum FolderFsStep {
    /// 目录已从 `from` 搬到 `to`。
    Renamed { from: PathBuf, to: PathBuf },
    /// 空目录已删除。
    Removed(PathBuf),
    /// 目录已新建。
    Created(PathBuf),
}

impl FolderFsJournal {
    fn renamed(&mut self, from: &Path, to: &Path) {
        self.steps.push(FolderFsStep::Renamed {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        });
    }

    fn removed(&mut self, path: &Path) {
        self.steps.push(FolderFsStep::Removed(path.to_path_buf()));
    }

    fn created(&mut self, path: &Path) {
        self.steps.push(FolderFsStep::Created(path.to_path_buf()));
    }

    /// 逆序撤回全部动作；回滚失败只记日志，不掩盖原始错误。
    pub(crate) fn rollback(&mut self) {
        for step in self.steps.drain(..).rev() {
            match step {
                FolderFsStep::Renamed { from, to } => {
                    let case_only = to
                        .to_string_lossy()
                        .eq_ignore_ascii_case(&from.to_string_lossy());
                    if let Err(error) = rename_directory(&to, &from, case_only) {
                        log::error!("回滚文件夹搬移失败：{error}；目录位于 {}", to.display());
                    }
                }
                FolderFsStep::Removed(path) => {
                    if let Err(error) = std::fs::create_dir_all(&path) {
                        log::error!("回滚文件夹删除失败：{error}；目录位于 {}", path.display());
                    }
                }
                FolderFsStep::Created(path) => {
                    if let Err(error) = std::fs::remove_dir(&path) {
                        log::warn!("回滚文件夹新建失败：{error}；目录位于 {}", path.display());
                    }
                }
            }
        }
    }
}

/// 一次把多个文件夹移动/插入到目标父级；只写一条操作记录。
pub fn place_many(
    db: &LibraryDb,
    root: &Path,
    folder_ids: &[i64],
    parent_id: i64,
    sort_index: Option<i64>,
) -> AppResult<Vec<FolderInfo>> {
    if folder_ids.is_empty() {
        return Err(AppError::message("没有要移动的文件夹"));
    }
    let mut seen = HashSet::new();
    for id in folder_ids {
        if *id <= 0 || !seen.insert(*id) {
            return Err(AppError::message("文件夹 id 无效或重复"));
        }
    }
    let label_parent = db.with_reader(|connection| folder_path(connection, parent_id))?;
    let label_target = if label_parent.is_empty() {
        "根目录".to_owned()
    } else {
        format!("「{label_parent}」")
    };
    let paths = LibraryPaths::new(root);
    let mut done: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut affected_ids: Vec<i64> = Vec::new();
    let result = history::write_with_id(
        db,
        NewOperation::new(
            HistoryKind::FolderMove,
            format!("移动 {} 个文件夹到{label_target}", folder_ids.len()),
        ),
        |transaction, draft| {
            ensure_normal_folder(transaction, parent_id)?;
            let target_parent_path = folder_path(transaction, parent_id)?;
            let rows = folder_rows_in(transaction)?;
            let by_id: HashMap<i64, (String, i64)> = rows
                .iter()
                .map(|(id, path, sort_index)| (*id, (path.clone(), *sort_index)))
                .collect();

            let mut selected = Vec::new();
            for id in folder_ids {
                let Some((path, sort_index)) = by_id.get(id) else {
                    return Err(AppError::message("文件夹不存在"));
                };
                if path.is_empty() {
                    return Err(AppError::message("不能移动图库根目录"));
                }
                selected.push((*id, path.clone(), *sort_index));
            }
            selected.sort_by_key(|(_, path, _)| path.matches('/').count());
            let mut kept: Vec<(i64, String, i64)> = Vec::new();
            for item in selected {
                if kept.iter().any(|(_, path, _)| folder_within(&item.1, path)) {
                    continue;
                }
                kept.push(item);
            }
            if kept
                .iter()
                .any(|(_, path, _)| folder_within(&target_parent_path, path))
            {
                return Err(AppError::message("不能把文件夹移动到自身或子文件夹"));
            }

            let kept_ids: HashSet<i64> = kept.iter().map(|(id, _, _)| *id).collect();
            let mut leaves = HashSet::new();
            let mut targets = Vec::with_capacity(kept.len());
            for (id, old_path, old_sort_index) in &kept {
                let leaf = old_path
                    .rsplit_once('/')
                    .map_or(old_path.as_str(), |(_, leaf)| leaf);
                if !leaves.insert(leaf.to_lowercase()) {
                    return Err(AppError::message("目标位置有同名文件夹"));
                }
                let new_path = if target_parent_path.is_empty() {
                    leaf.to_owned()
                } else {
                    format!("{target_parent_path}/{leaf}")
                };
                if rows.iter().any(|(existing_id, path, _)| {
                    !kept_ids.contains(existing_id) && same_folder(path, &new_path)
                }) {
                    return Err(AppError::message("目标位置已经有同名文件夹"));
                }
                let old_dir = paths.folder_dir(old_path);
                let new_dir = paths.folder_dir(&new_path);
                if !old_dir.is_dir() {
                    return Err(AppError::message(format!(
                        "文件夹目录不存在：{}",
                        old_dir.display()
                    )));
                }
                if new_path != *old_path && new_dir.exists() {
                    return Err(AppError::message("目标文件夹目录已存在"));
                }
                targets.push((
                    *id,
                    old_path.clone(),
                    *old_sort_index,
                    new_path,
                    old_dir,
                    new_dir,
                ));
            }

            let mut siblings: Vec<(i64, i64)> = rows
                .iter()
                .filter(|(id, path, _)| {
                    !path.is_empty()
                        && !kept_ids.contains(id)
                        && same_folder(parent_path(path), &target_parent_path)
                })
                .map(|(id, _, sort_index)| (*id, *sort_index))
                .collect();
            siblings.sort_by(|left, right| left.1.cmp(&right.1).then(left.0.cmp(&right.0)));
            let at = sort_index
                .unwrap_or(siblings.len() as i64)
                .clamp(0, siblings.len() as i64) as usize;
            let mut order: Vec<i64> = siblings.iter().map(|(id, _)| *id).collect();
            order.splice(at..at, kept.iter().map(|(id, _, _)| *id));
            let positions: HashMap<i64, i64> = order
                .iter()
                .enumerate()
                .map(|(position, id)| (*id, position as i64))
                .collect();
            let old_positions: HashMap<i64, i64> = rows
                .iter()
                .map(|(id, _, sort_index)| (*id, *sort_index))
                .collect();
            let path_changed = targets
                .iter()
                .any(|(_, old_path, _, new_path, _, _)| old_path != new_path);
            let index_changed = order.iter().enumerate().any(|(position, id)| {
                old_positions.get(id).copied().unwrap_or(position as i64) != position as i64
            });
            if !path_changed && !index_changed {
                return Ok(());
            }

            if !target_parent_path.is_empty() {
                let parent_dir = paths.folder_dir(&target_parent_path);
                std::fs::create_dir_all(&parent_dir)?;
                ensure_inside_root(root, &parent_dir)?;
            }
            for (_, old_path, _, new_path, old_dir, new_dir) in &targets {
                if old_path == new_path {
                    continue;
                }
                if let Err(error) = std::fs::rename(old_dir, new_dir) {
                    rollback_renames(&done);
                    done.clear();
                    return Err(error.into());
                }
                done.push((old_dir.clone(), new_dir.clone()));
            }

            let write_result = (|| -> AppResult<()> {
                for (id, old_path, _, new_path, _, _) in &targets {
                    if old_path != new_path {
                        ensure_inside_root(root, &paths.folder_dir(new_path))?;
                        update_subtree_paths(transaction, old_path, new_path, *id)?;
                    }
                }
                for (id, position) in &positions {
                    transaction.execute(
                        "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
                        params![position, id],
                    )?;
                }
                for (id, old_path, old_sort_index, new_path, _, _) in &targets {
                    if old_path == new_path
                        && old_sort_index == positions.get(id).unwrap_or(old_sort_index)
                    {
                        continue;
                    }
                    draft.json(&FolderPathChange {
                        id: *id,
                        old_path: old_path.clone(),
                        new_path: new_path.clone(),
                        old_sort_index: *old_sort_index,
                        new_sort_index: *positions.get(id).unwrap_or(old_sort_index),
                    })?;
                }
                let sort_rows: Vec<(i64, i64, i64)> = siblings
                    .iter()
                    .filter_map(|(id, old_index)| {
                        let new_index = *positions.get(id)?;
                        (*old_index != new_index).then_some((*id, *old_index, new_index))
                    })
                    .collect();
                draft.sort_changes(&FolderSortChange { rows: sort_rows })?;
                affected_ids.extend(positions.keys().copied());
                for (_, old_path, _, new_path, _, _) in &targets {
                    if old_path == new_path {
                        continue;
                    }
                    affected_ids.extend(subtree_folder_ids(transaction, new_path)?);
                    affected_ids.extend(ancestor_folder_ids(transaction, parent_path(old_path))?);
                }
                affected_ids.extend(ancestor_folder_ids(transaction, &target_parent_path)?);
                draft.affected(kept.len() as u64);
                Ok(())
            })();
            if let Err(error) = write_result {
                rollback_renames(&done);
                done.clear();
                return Err(error);
            }
            Ok(())
        },
    );
    if result.is_err() {
        rollback_renames(&done);
    }
    result?;
    query::folder_infos(db, &affected_ids)
}

/// 把前端算好的同级顺序写回 `sort_index`；一次操作一条记录。
pub fn apply_order(
    db: &LibraryDb,
    folder_id: i64,
    key: FolderSortKey,
    descending: bool,
    recursive: bool,
    orders: &[(i64, Vec<i64>)],
) -> AppResult<Vec<FolderInfo>> {
    if orders.len() > 20_000 {
        return Err(AppError::message("排序范围过大"));
    }
    let total: usize = orders.iter().map(|(_, ids)| ids.len()).sum();
    if total > 100_000 {
        return Err(AppError::message("排序范围过大"));
    }
    let label_path = db.with_reader(|connection| folder_path(connection, folder_id))?;
    let scope = if label_path.is_empty() {
        "顶层文件夹".to_owned()
    } else {
        format!("「{label_path}」的子文件夹")
    };
    let suffix = if recursive {
        "（含所有后代）"
    } else {
        ""
    };
    let direction = if descending { "降序" } else { "升序" };
    let mut affected_ids: Vec<i64> = Vec::new();
    history::write_with_id(
        db,
        NewOperation::new(
            HistoryKind::FolderReorder,
            format!("按{}排序{scope}{suffix}·{direction}", key.label()),
        ),
        |transaction, draft| {
            ensure_normal_folder(transaction, folder_id)?;
            let root_path = folder_path(transaction, folder_id)?;
            if orders.is_empty() {
                return Ok(());
            }
            // 一次建好“父路径 -> 直接子文件夹”，避免每个父级都扫一遍全表。
            let mut children_by_parent: HashMap<String, Vec<(i64, i64)>> = HashMap::new();
            for (id, path, sort_index) in folder_rows_in(transaction)? {
                if path.is_empty() {
                    continue;
                }
                children_by_parent
                    .entry(parent_path(&path).to_ascii_lowercase())
                    .or_default()
                    .push((id, sort_index));
            }

            let mut seen_parents = HashSet::new();
            let mut changes: Vec<(i64, i64, i64)> = Vec::new();
            for (parent_id, ids) in orders {
                if !seen_parents.insert(*parent_id) {
                    return Err(AppError::message("排序父级重复"));
                }
                ensure_normal_folder(transaction, *parent_id)?;
                let sort_parent_path = folder_path(transaction, *parent_id)?;
                if !folder_within(&sort_parent_path, &root_path) {
                    return Err(AppError::message("排序范围包含其它文件夹"));
                }
                if !recursive && *parent_id != folder_id {
                    return Err(AppError::message("非递归排序只能调整直接子文件夹"));
                }
                let children = children_by_parent
                    .get(&sort_parent_path.to_ascii_lowercase())
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let old_by_id: HashMap<i64, i64> = children.iter().copied().collect();
                let requested: HashSet<i64> = ids.iter().copied().collect();
                // 必须与当前直接子文件夹完全一致，否则前端顺序已经过期。
                if requested.len() != ids.len()
                    || requested.len() != old_by_id.len()
                    || !requested.iter().all(|id| old_by_id.contains_key(id))
                {
                    return Err(AppError::message("文件夹已变化，请重新排序"));
                }
                for (position, id) in ids.iter().enumerate() {
                    let Some(old_index) = old_by_id.get(id) else {
                        continue;
                    };
                    if *old_index != position as i64 {
                        changes.push((*id, *old_index, position as i64));
                    }
                }
            }
            if changes.is_empty() {
                return Ok(());
            }
            for (id, _, new_index) in &changes {
                transaction.execute(
                    "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
                    params![new_index, id],
                )?;
            }
            draft.json(&FolderReorder {
                rows: changes.iter().map(|(id, old, _)| (*id, *old)).collect(),
            })?;
            affected_ids = changes.iter().map(|(id, _, _)| *id).collect();
            draft.affected(changes.len() as u64);
            Ok(())
        },
    )?;
    query::folder_infos(db, &affected_ids)
}

/// 整棵子树移入回收站：保留原路径与文件，只写 `trashed_at` / `deleted_at`。
pub fn trash(db: &LibraryDb, folder_id_value: i64) -> AppResult<FolderChange> {
    trash_with_id(db, folder_id_value).map(|(change, _)| change)
}

pub fn trash_with_id(
    db: &LibraryDb,
    folder_id_value: i64,
) -> AppResult<(FolderChange, Option<i64>)> {
    let label_path = db.with_reader(|connection| folder_path(connection, folder_id_value))?;
    let before = db.with_reader(|connection| info_in(connection, folder_id_value))?;
    let mut removed_ids = Vec::new();
    let mut image_count = 0_u64;
    let ((), operation_id) = history::write_with_id(
        db,
        NewOperation::new(
            HistoryKind::FolderTrash,
            format!(
                "把文件夹「{label_path}」移入回收站（{} 张图片）",
                before.total_image_count
            ),
        ),
        |transaction, draft| {
            ensure_normal_folder(transaction, folder_id_value)?;
            let path: String = transaction
                .query_row(
                    "SELECT path FROM folders WHERE id = ?1",
                    [folder_id_value],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| AppError::message("文件夹不存在"))?;
            if path.is_empty() {
                return Err(AppError::message("不能删除图库根目录"));
            }
            let pattern = subtree_like_pattern(&path);
            let stamp = next_trash_stamp(transaction, &path)?;
            let rows: Vec<FolderRow> = {
                let mut statement = transaction.prepare(
                    "SELECT id, path, created_at, sort_index FROM folders
                     WHERE (path = ?1 OR path LIKE ?2 ESCAPE '\\')
                       AND trashed_at IS NULL AND purge_operation_id IS NULL
                     ORDER BY path",
                )?;
                statement
                    .query_map(params![&path, &pattern], |row| {
                        Ok(FolderRow {
                            id: row.get(0)?,
                            path: row.get(1)?,
                            created_at: row.get(2)?,
                            sort_index: row.get(3)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            if rows.is_empty() {
                return Err(AppError::message("文件夹不在正常树中"));
            }
            transaction.execute(
                "UPDATE folders SET trashed_at = ?1
                 WHERE (path = ?2 OR path LIKE ?3 ESCAPE '\\')
                   AND trashed_at IS NULL AND purge_operation_id IS NULL",
                params![stamp, &path, &pattern],
            )?;
            let targets: Vec<(i64, i64)> = {
                let mut statement = transaction.prepare(
                    "SELECT images.id, images.modified_at FROM images
                     JOIN folders ON folders.id = images.folder_id
                     WHERE (folders.path = ?1 OR folders.path LIKE ?2 ESCAPE '\\')
                       AND images.deleted_at IS NULL AND images.purge_operation_id IS NULL",
                )?;
                statement
                    .query_map(params![&path, &pattern], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            for chunk in targets.chunks(4_000) {
                let marks = placeholders(chunk.len());
                transaction.execute(
                    &format!(
                        "UPDATE images SET deleted_at = ?1, modified_at = ?1
                         WHERE id IN ({marks})
                           AND deleted_at IS NULL AND purge_operation_id IS NULL"
                    ),
                    params_from_iter(
                        std::iter::once(Value::Integer(stamp))
                            .chain(chunk.iter().map(|(id, _)| Value::Integer(*id))),
                    ),
                )?;
            }
            let triples: Vec<(i64, i64, i64)> = targets
                .iter()
                .map(|(id, modified)| (*id, 0, *modified))
                .collect();
            draft.json(&rows)?;
            draft.triples(&triples);
            draft.affected(rows.len() as u64);
            removed_ids = rows.iter().map(|row| row.id).collect();
            image_count = targets.len() as u64;
            Ok(())
        },
    )?;
    let ancestors =
        db.with_reader(|connection| ancestor_folder_ids(connection, parent_path(&label_path)))?;
    Ok((
        FolderChange {
            folders: query::folder_infos(db, &ancestors)?,
            removed_ids,
            affected_images: image_count,
        },
        operation_id,
    ))
}

/// 恢复回收站文件夹：恢复这次删除的整个单元（同一时间戳的最高一级文件夹及其子树）。
///
/// 删除前就已经单独在回收站里的图片不跟着恢复；文件夹恢复后它们归到根行“直属图片”。
/// 上级文件夹若来自更早的一次删除，拒绝恢复，避免把两个删除单元拆开。
pub fn restore(db: &LibraryDb, folder_id_value: i64) -> AppResult<FolderChange> {
    restore_with_id(db, folder_id_value).map(|(change, _)| change)
}

pub fn restore_with_id(
    db: &LibraryDb,
    folder_id_value: i64,
) -> AppResult<(FolderChange, Option<i64>)> {
    let label_path = db.with_reader(|connection| folder_path(connection, folder_id_value))?;
    let mut restored_images = 0_u64;
    let mut restored_path = label_path.clone();
    let mut restored_ids = Vec::new();
    let ((), operation_id) = history::write_with_id(
        db,
        NewOperation::new(
            HistoryKind::FolderRestore,
            format!("恢复文件夹「{label_path}」"),
        ),
        |transaction, draft| {
            let requested = ensure_trashed_folder(transaction, folder_id_value)?;
            let stamp: i64 = transaction.query_row(
                "SELECT trashed_at FROM folders WHERE id = ?1",
                [folder_id_value],
                |row| row.get(0),
            )?;
            // 同一次删除可能包含上级：选中子树里的文件夹时，恢复整个删除单元。
            let mut path = requested;
            let mut current = parent_path(&path);
            while !current.is_empty() {
                let Some(id) = folder_id(transaction, current)? else {
                    break;
                };
                let (ancestor_path, ancestor_trashed, ancestor_purge): (
                    String,
                    Option<i64>,
                    Option<i64>,
                ) = transaction.query_row(
                    "SELECT path, trashed_at, purge_operation_id FROM folders WHERE id = ?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
                if ancestor_purge.is_some() {
                    return Err(AppError::message("上级文件夹正在释放，请稍后再试"));
                }
                match ancestor_trashed {
                    Some(ancestor_stamp) if ancestor_stamp == stamp => {
                        path = ancestor_path;
                        current = parent_path(&path);
                    }
                    Some(_) => {
                        return Err(AppError::message(format!(
                            "上级文件夹「{ancestor_path}」仍在回收站中，请先恢复它"
                        )));
                    }
                    None => break,
                }
            }
            let pattern = subtree_like_pattern(&path);
            let rows: Vec<FolderRow> = {
                let mut statement = transaction.prepare(
                    "SELECT id, path, created_at, sort_index FROM folders
                     WHERE (path = ?1 OR path LIKE ?2 ESCAPE '\\')
                       AND trashed_at = ?3 AND purge_operation_id IS NULL
                     ORDER BY path",
                )?;
                statement
                    .query_map(params![&path, &pattern, stamp], |row| {
                        Ok(FolderRow {
                            id: row.get(0)?,
                            path: row.get(1)?,
                            created_at: row.get(2)?,
                            sort_index: row.get(3)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            if rows.is_empty() {
                return Err(AppError::message("文件夹不在回收站中"));
            }
            restored_ids = rows.iter().map(|row| row.id).collect();
            for chunk in rows.chunks(4_000) {
                let marks = placeholders(chunk.len());
                let ids: Vec<i64> = chunk.iter().map(|row| row.id).collect();
                transaction.execute(
                    &format!("UPDATE folders SET trashed_at = NULL WHERE id IN ({marks})"),
                    params_from_iter(ids.iter()),
                )?;
            }
            let targets: Vec<(i64, i64, i64)> = {
                let mut statement = transaction.prepare(
                    "SELECT images.id, images.deleted_at, images.modified_at FROM images
                     JOIN folders ON folders.id = images.folder_id
                     WHERE (folders.path = ?1 OR folders.path LIKE ?2 ESCAPE '\\')
                       AND images.deleted_at = ?3 AND images.purge_operation_id IS NULL",
                )?;
                statement
                    .query_map(params![&path, &pattern, stamp], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            let now = now_ms();
            for chunk in targets.chunks(4_000) {
                let marks = placeholders(chunk.len());
                transaction.execute(
                    &format!(
                        "UPDATE images SET deleted_at = NULL, modified_at = ?1
                         WHERE id IN ({marks}) AND purge_operation_id IS NULL"
                    ),
                    params_from_iter(
                        std::iter::once(Value::Integer(now))
                            .chain(chunk.iter().map(|(id, _, _)| Value::Integer(*id))),
                    ),
                )?;
            }
            draft.json(&rows)?;
            draft.triples(&targets);
            draft.affected(rows.len() as u64);
            restored_images = targets.len() as u64;
            restored_path = path;
            Ok(())
        },
    )?;
    let mut ids = restored_ids;
    ids.extend(
        db.with_reader(|connection| ancestor_folder_ids(connection, parent_path(&restored_path)))?,
    );
    ids.sort_unstable();
    ids.dedup();
    Ok((
        FolderChange {
            folders: query::folder_infos(db, &ids)?,
            removed_ids: Vec::new(),
            affected_images: restored_images,
        },
        operation_id,
    ))
}

/// 永久删除回收站文件夹：只写待释放标记，释放任务删完图片后再清理文件夹行与空目录。
pub fn delete_permanently(db: &LibraryDb, folder_id_value: i64) -> AppResult<FolderChange> {
    delete_permanently_with_id(db, folder_id_value).map(|(change, _)| change)
}

pub fn delete_permanently_with_id(
    db: &LibraryDb,
    folder_id_value: i64,
) -> AppResult<(FolderChange, Option<i64>)> {
    let label_path = db.with_reader(|connection| folder_path(connection, folder_id_value))?;
    let mut removed_ids = Vec::new();
    let mut image_count = 0_u64;
    let ((), operation_id) = history::write_with_id(
        db,
        NewOperation::new(
            HistoryKind::FolderPurge,
            format!("永久删除文件夹「{label_path}」"),
        ),
        |transaction, draft| {
            let path = ensure_trashed_folder(transaction, folder_id_value)?;
            let pattern = subtree_like_pattern(&path);
            let ids: Vec<i64> = {
                let mut statement = transaction.prepare(
                    "SELECT id FROM folders
                     WHERE (path = ?1 OR path LIKE ?2 ESCAPE '\\')
                       AND trashed_at IS NOT NULL AND purge_operation_id IS NULL",
                )?;
                statement
                    .query_map(params![&path, &pattern], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            if ids.is_empty() {
                return Err(AppError::message("文件夹已经在释放中"));
            }
            transaction.execute(
                "UPDATE folders SET purge_operation_id = ?1
                 WHERE (path = ?2 OR path LIKE ?3 ESCAPE '\\')
                   AND purge_operation_id IS NULL",
                params![draft.id(), &path, &pattern],
            )?;
            image_count = transaction.execute(
                "UPDATE images SET purge_operation_id = ?1
                 WHERE folder_id IN (
                     SELECT purge_folders.id FROM folders purge_folders
                     WHERE purge_folders.path = ?2 OR purge_folders.path LIKE ?3 ESCAPE '\\'
                 )
                   AND deleted_at IS NOT NULL AND purge_operation_id IS NULL",
                params![draft.id(), &path, &pattern],
            )? as u64;
            draft.affected(ids.len() as u64);
            removed_ids = ids;
            Ok(())
        },
    )?;
    Ok((
        FolderChange {
            folders: Vec::new(),
            removed_ids,
            affected_images: image_count,
        },
        operation_id,
    ))
}

/// 文件夹删除事件的时间戳：必须大于子树里已有的回收站时间，恢复时才能按等值判断“这次一起进来的图片”。
fn next_trash_stamp(connection: &Connection, path: &str) -> AppResult<i64> {
    let latest: Option<i64> = connection.query_row(
        "SELECT MAX(stamp) FROM (
             SELECT MAX(images.deleted_at) AS stamp FROM images
              JOIN folders ON folders.id = images.folder_id
              WHERE folders.path = ?1 OR folders.path LIKE ?2 ESCAPE '\\'
             UNION ALL
             SELECT MAX(folders.trashed_at) FROM folders
              WHERE folders.path = ?1 OR folders.path LIKE ?2 ESCAPE '\\'
         )",
        params![path, subtree_like_pattern(path)],
        |row| row.get(0),
    )?;
    Ok(now_ms().max(latest.unwrap_or(0).saturating_add(1)))
}

/// 按相对路径找文件夹 id；大小写不敏感，与建目录时的解析一致。
pub fn find_id(db: &LibraryDb, path: &str) -> AppResult<Option<i64>> {
    let path = normalize_folder(path)?;
    db.with_reader(|connection| folder_id(connection, &path))
}

/// 单个文件夹的位置与图片数；只查这一棵子树，不做全库扫描。
pub fn info(db: &LibraryDb, folder_id_value: i64) -> AppResult<FolderInfo> {
    db.with_reader(|connection| info_in(connection, folder_id_value))
}

/// 子树里的子文件夹数（不含自身）；供 MCP dryRun 说明影响范围。
pub fn subtree_folder_count(db: &LibraryDb, folder_id_value: i64) -> AppResult<u64> {
    db.with_reader(|connection| {
        let path = folder_path(connection, folder_id_value)?;
        if path.is_empty() {
            let count: i64 = connection.query_row(
                "SELECT count(*) FROM folders
                 WHERE kind = 'normal' AND trashed_at IS NULL AND purge_operation_id IS NULL
                   AND path <> ''",
                [],
                |row| row.get(0),
            )?;
            return Ok(count as u64);
        }
        let count: i64 = connection.query_row(
            "SELECT count(*) FROM folders
             WHERE kind = 'normal' AND trashed_at IS NULL AND purge_operation_id IS NULL
               AND path LIKE ?1 ESCAPE '\\'",
            [subtree_like_pattern(&path)],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    })
}

fn info_in(connection: &Connection, folder_id_value: i64) -> AppResult<FolderInfo> {
    let (path, sort_index): (String, i64) = connection
        .query_row(
            "SELECT path, sort_index FROM folders WHERE id = ?1",
            [folder_id_value],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| AppError::message("文件夹不存在"))?;
    let direct_image_count: i64 = connection.query_row(
        "SELECT count(*) FROM images
         WHERE folder_id = ?1 AND deleted_at IS NULL AND purge_operation_id IS NULL",
        [folder_id_value],
        |row| row.get(0),
    )?;
    let total_image_count: i64 = if path.is_empty() {
        direct_image_count
    } else {
        connection.query_row(
            "SELECT count(*) FROM images JOIN folders ON folders.id = images.folder_id
             WHERE folders.path LIKE ?1 ESCAPE '\\'
               AND images.deleted_at IS NULL AND images.purge_operation_id IS NULL",
            [subtree_like_pattern(&path)],
            |row| row.get(0),
        )?
    };
    Ok(FolderInfo {
        id: folder_id_value,
        path,
        sort_index,
        direct_image_count: direct_image_count.max(0) as u64,
        total_image_count: total_image_count.max(0) as u64,
    })
}

/// 移入回收站的前置条件：存在、普通文件夹、非根、当前在正常树中。
pub fn deletion_blocker(
    db: &LibraryDb,
    root: &Path,
    folder_id_value: i64,
) -> AppResult<Option<String>> {
    db.with_reader(|connection| deletion_blocker_in(connection, root, folder_id_value))
}

fn deletion_blocker_in(
    connection: &Connection,
    _root: &Path,
    folder_id_value: i64,
) -> AppResult<Option<String>> {
    let state: Option<(String, Option<i64>, Option<i64>, String)> = connection
        .query_row(
            "SELECT kind, trashed_at, purge_operation_id, path FROM folders WHERE id = ?1",
            [folder_id_value],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((kind, trashed_at, purge_operation_id, path)) = state else {
        return Ok(Some("文件夹不存在".to_owned()));
    };
    if kind == "temp" {
        return Ok(Some("临时文件夹不能删除".to_owned()));
    }
    if purge_operation_id.is_some() {
        return Ok(Some("文件夹正在释放，不能再删除".to_owned()));
    }
    if trashed_at.is_some() {
        return Ok(Some("文件夹已经在回收站中".to_owned()));
    }
    if path.is_empty() {
        return Ok(Some("不能删除图库根目录".to_owned()));
    }
    Ok(None)
}

/// 撤销“新建文件夹”：记录里的行仍然存在且为空时删目录与行；已经被移动或已有内容则失败。
pub(crate) fn undo_create(
    transaction: &Transaction<'_>,
    root: &Path,
    created: &[FolderRef],
    journal: &mut FolderFsJournal,
) -> AppResult<()> {
    let paths = LibraryPaths::new(root);
    let mut rows = created.to_vec();
    rows.sort_by_key(|row| std::cmp::Reverse(row.path.matches('/').count()));
    for item in rows {
        let current: Option<String> = transaction
            .query_row("SELECT path FROM folders WHERE id = ?1", [item.id], |row| {
                row.get(0)
            })
            .optional()?;
        let Some(current) = current else {
            continue;
        };
        if !same_folder(&current, &item.path) {
            return Err(AppError::message("文件夹已被移动，不能撤销新建"));
        }
        remove_folder_if_empty(
            transaction,
            &paths,
            root,
            item.id,
            &item.path,
            true,
            journal,
        )?;
    }
    Ok(())
}

/// 导入撤销只清理本次新建且仍为空的文件夹；已经有内容的就保留，不让整条记录失败。
pub(crate) fn cleanup_created(
    transaction: &Transaction<'_>,
    root: &Path,
    created: &[FolderRef],
    journal: &mut FolderFsJournal,
) -> AppResult<()> {
    let paths = LibraryPaths::new(root);
    let mut rows = created.to_vec();
    rows.sort_by_key(|row| std::cmp::Reverse(row.path.matches('/').count()));
    for item in rows {
        let current: Option<String> = transaction
            .query_row("SELECT path FROM folders WHERE id = ?1", [item.id], |row| {
                row.get(0)
            })
            .optional()?;
        let Some(current) = current else {
            continue;
        };
        if !same_folder(&current, &item.path) {
            continue;
        }
        remove_folder_if_empty(
            transaction,
            &paths,
            root,
            item.id,
            &item.path,
            false,
            journal,
        )?;
    }
    Ok(())
}

fn remove_folder_if_empty(
    transaction: &Transaction<'_>,
    paths: &LibraryPaths,
    root: &Path,
    id: i64,
    path: &str,
    strict: bool,
    journal: &mut FolderFsJournal,
) -> AppResult<()> {
    let children: i64 = transaction.query_row(
        "SELECT count(*) FROM folders WHERE path LIKE ?1 ESCAPE '\\'",
        [subtree_like_pattern(path)],
        |row| row.get(0),
    )?;
    let images: i64 = transaction.query_row(
        "SELECT count(*) FROM images WHERE folder_id = ?1",
        [id],
        |row| row.get(0),
    )?;
    if children > 0 || images > 0 {
        if strict {
            return Err(AppError::message("文件夹里已经有内容，不能撤销新建"));
        }
        return Ok(());
    }
    let directory = paths.folder_dir(path);
    if directory.exists() {
        ensure_inside_root(root, &directory)?;
        if std::fs::read_dir(&directory)?.next().is_some() {
            if strict {
                return Err(AppError::message("文件夹目录不是空的，不能撤销新建"));
            }
            return Ok(());
        }
        std::fs::remove_dir(&directory)?;
        journal.removed(&directory);
    }
    transaction.execute("DELETE FROM folders WHERE id = ?1", [id])?;
    Ok(())
}

/// 撤销重命名/移动：当前路径是旧路径视为已完成；是目标路径才反向搬回。
pub(crate) fn undo_path_change(
    transaction: &Transaction<'_>,
    root: &Path,
    change: &FolderPathChange,
    journal: &mut FolderFsJournal,
) -> AppResult<()> {
    let current = folder_path(transaction, change.id)?;
    if same_folder(&current, &change.old_path) {
        transaction.execute(
            "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
            params![change.old_sort_index, change.id],
        )?;
        return Ok(());
    }
    if !same_folder(&current, &change.new_path) {
        return Err(AppError::message("文件夹当前路径与记录不符，无法撤销"));
    }

    let paths = LibraryPaths::new(root);
    let old_dir = paths.folder_dir(&change.old_path);
    let new_dir = paths.folder_dir(&change.new_path);
    let case_only = same_folder(&change.old_path, &change.new_path);
    // 上次中断可能已经把目录搬回，只剩数据库未更新；这时只补 SQL。
    let physical_done = !new_dir.is_dir() && old_dir.is_dir();
    if !physical_done {
        if !new_dir.is_dir() {
            return Err(AppError::message("被移动的文件夹目录不存在，无法撤销"));
        }
        if !case_only && old_dir.exists() {
            return Err(AppError::message("原位置已经有同名文件夹，无法撤销"));
        }
        rename_directory(&new_dir, &old_dir, case_only)?;
        journal.renamed(&new_dir, &old_dir);
        ensure_inside_root(root, &old_dir)?;
    }

    update_subtree_paths(transaction, &change.new_path, &change.old_path, change.id).and_then(
        |_| {
            transaction.execute(
                "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
                params![change.old_sort_index, change.id],
            )?;
            Ok(())
        },
    )
}

/// 撤销“文件夹移入回收站”：文件夹行回到正常树，图片恢复旧状态与修改时间。
pub(crate) fn undo_trash(
    transaction: &Transaction<'_>,
    rows: &[FolderRow],
    triples: &[(i64, i64, i64)],
) -> AppResult<()> {
    restore_folder_rows(transaction, rows)?;
    for (image_id, deleted_at, modified_at) in triples {
        transaction.execute(
            "UPDATE images SET deleted_at = ?1, modified_at = ?2
             WHERE id = ?3 AND purge_operation_id IS NULL",
            params![
                if *deleted_at == 0 {
                    None
                } else {
                    Some(*deleted_at)
                },
                modified_at,
                image_id
            ],
        )?;
    }
    Ok(())
}

/// 重做“文件夹移入回收站”：文件仍在原路径，只重新写隐藏时间。
pub(crate) fn redo_trash(
    transaction: &Transaction<'_>,
    rows: &[FolderRow],
    triples: &[(i64, i64, i64)],
) -> AppResult<()> {
    let stamp = now_ms();
    trash_folder_rows(transaction, rows, stamp)?;
    for (image_id, _, _) in triples {
        transaction.execute(
            "UPDATE images SET deleted_at = ?1, modified_at = ?1
             WHERE id = ?2 AND deleted_at IS NULL AND purge_operation_id IS NULL",
            params![stamp, image_id],
        )?;
    }
    Ok(())
}

/// 撤销“恢复文件夹”：重新移入回收站，图片回到恢复前的状态。
pub(crate) fn undo_restore(
    transaction: &Transaction<'_>,
    rows: &[FolderRow],
    triples: &[(i64, i64, i64)],
) -> AppResult<()> {
    let stamp = triples
        .iter()
        .map(|(_, deleted_at, _)| *deleted_at)
        .max()
        .filter(|value| *value > 0)
        .unwrap_or_else(now_ms);
    trash_folder_rows(transaction, rows, stamp)?;
    for (image_id, deleted_at, modified_at) in triples {
        transaction.execute(
            "UPDATE images SET deleted_at = ?1, modified_at = ?2
             WHERE id = ?3 AND purge_operation_id IS NULL",
            params![
                if *deleted_at == 0 {
                    None
                } else {
                    Some(*deleted_at)
                },
                modified_at,
                image_id
            ],
        )?;
    }
    Ok(())
}

/// 重做“恢复文件夹”：文件夹行与图片重新回到正常树。
pub(crate) fn redo_restore(
    transaction: &Transaction<'_>,
    rows: &[FolderRow],
    triples: &[(i64, i64, i64)],
) -> AppResult<()> {
    let now = now_ms();
    restore_folder_rows(transaction, rows)?;
    for (image_id, _, _) in triples {
        transaction.execute(
            "UPDATE images SET deleted_at = NULL, modified_at = ?1
             WHERE id = ?2 AND purge_operation_id IS NULL",
            params![now, image_id],
        )?;
    }
    Ok(())
}

fn restore_folder_rows(transaction: &Transaction<'_>, rows: &[FolderRow]) -> AppResult<()> {
    for chunk in rows.chunks(4_000) {
        let marks = placeholders(chunk.len());
        transaction.execute(
            &format!("UPDATE folders SET trashed_at = NULL WHERE id IN ({marks})"),
            params_from_iter(chunk.iter().map(|row| row.id)),
        )?;
    }
    Ok(())
}

fn trash_folder_rows(
    transaction: &Transaction<'_>,
    rows: &[FolderRow],
    stamp: i64,
) -> AppResult<()> {
    for chunk in rows.chunks(4_000) {
        let marks = placeholders(chunk.len());
        transaction.execute(
            &format!(
                "UPDATE folders SET trashed_at = ?1
                 WHERE id IN ({marks}) AND trashed_at IS NULL"
            ),
            params_from_iter(
                std::iter::once(Value::Integer(stamp))
                    .chain(chunk.iter().map(|row| Value::Integer(row.id))),
            ),
        )?;
    }
    Ok(())
}

/// 撤销“删除空文件夹”：用原 id 重建数据库行与物理目录；行已存在视为已完成。
pub(crate) fn undo_delete(
    transaction: &Transaction<'_>,
    root: &Path,
    deleted: &FolderDeleted,
    journal: &mut FolderFsJournal,
) -> AppResult<()> {
    let existing: Option<String> = transaction
        .query_row(
            "SELECT path FROM folders WHERE id = ?1",
            [deleted.id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        if same_folder(&existing, &deleted.path) {
            return Ok(());
        }
        // folders.id 是普通 INTEGER PRIMARY KEY，删除最大 id 后新建可能复用同一个 id。
        return Err(AppError::message(
            "原文件夹 id 已被其他文件夹占用，无法恢复",
        ));
    }
    if let Some(existing) = folder_id(transaction, &deleted.path)? {
        return Err(AppError::message(format!(
            "路径已被文件夹 {existing} 占用，无法恢复"
        )));
    }
    let directory = LibraryPaths::new(root).folder_dir(&deleted.path);
    if directory.exists() && !directory.is_dir() {
        return Err(AppError::message("原文件夹位置被文件占用，无法恢复"));
    }
    let created = !directory.exists();
    std::fs::create_dir_all(&directory)?;
    if created {
        journal.created(&directory);
    }
    ensure_inside_root(root, &directory)?;
    transaction.execute(
        "INSERT INTO folders(id, path, created_at, sort_index) VALUES (?1, ?2, ?3, ?4)",
        params![
            deleted.id,
            deleted.path,
            deleted.created_at,
            deleted.sort_index
        ],
    )?;
    Ok(())
}

/// 撤销同级排序：把每个文件夹恢复成记录里的 `sort_index`。
pub(crate) fn undo_reorder(
    transaction: &Transaction<'_>,
    reorder: &FolderReorder,
) -> AppResult<()> {
    for (id, sort_index) in &reorder.rows {
        let changed = transaction.execute(
            "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
            params![sort_index, id],
        )?;
        if changed == 0 {
            return Err(AppError::message("排序记录中的文件夹不存在"));
        }
    }
    Ok(())
}

/// 撤销批量移动时目标父级兄弟的排序位变化。
pub(crate) fn undo_sort_change(
    transaction: &Transaction<'_>,
    change: &FolderSortChange,
) -> AppResult<()> {
    for (id, old_index, _) in &change.rows {
        let changed = transaction.execute(
            "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
            params![old_index, id],
        )?;
        if changed == 0 {
            return Err(AppError::message("排序记录中的文件夹不存在"));
        }
    }
    Ok(())
}

/// 重做批量移动时目标父级兄弟的排序位变化。
pub(crate) fn redo_sort_change(
    transaction: &Transaction<'_>,
    change: &FolderSortChange,
) -> AppResult<()> {
    for (id, _, new_index) in &change.rows {
        let changed = transaction.execute(
            "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
            params![new_index, id],
        )?;
        if changed == 0 {
            return Err(AppError::message("排序记录中的文件夹不存在"));
        }
    }
    Ok(())
}

/// 重做“新建文件夹”：按原 id、路径与排序位补回行和目录；已存在则跳过。
pub(crate) fn redo_create(
    transaction: &Transaction<'_>,
    root: &Path,
    rows: &[FolderRow],
    journal: &mut FolderFsJournal,
) -> AppResult<()> {
    let paths = LibraryPaths::new(root);
    let mut rows = rows.to_vec();
    rows.sort_by_key(|row| row.path.matches('/').count());
    for item in rows {
        let existing: Option<String> = transaction
            .query_row("SELECT path FROM folders WHERE id = ?1", [item.id], |row| {
                row.get(0)
            })
            .optional()?;
        if let Some(existing) = existing {
            if !same_folder(&existing, &item.path) {
                return Err(AppError::message(format!(
                    "重做时文件夹 id {} 已被其他文件夹占用",
                    item.id
                )));
            }
            let directory = paths.folder_dir(&item.path);
            if !directory.is_dir() {
                std::fs::create_dir_all(&directory)?;
                journal.created(&directory);
                ensure_inside_root(root, &directory)?;
            }
            continue;
        }
        if let Some(existing) = folder_id(transaction, &item.path)? {
            return Err(AppError::message(format!(
                "重做时路径「{}」已被文件夹 {existing} 占用",
                item.path
            )));
        }
        let directory = paths.folder_dir(&item.path);
        std::fs::create_dir_all(&directory)?;
        journal.created(&directory);
        ensure_inside_root(root, &directory)?;
        transaction.execute(
            "INSERT INTO folders(id, path, created_at, sort_index) VALUES (?1, ?2, ?3, ?4)",
            params![item.id, item.path, item.created_at, item.sort_index],
        )?;
    }
    Ok(())
}

/// 重做“重命名/移动文件夹”：与撤销对称，当前已在目标路径时只补排序位。
pub(crate) fn redo_path_change(
    transaction: &Transaction<'_>,
    root: &Path,
    change: &FolderPathChange,
    journal: &mut FolderFsJournal,
) -> AppResult<()> {
    let current = folder_path(transaction, change.id)?;
    if same_folder(&current, &change.new_path) {
        transaction.execute(
            "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
            params![change.new_sort_index, change.id],
        )?;
        return Ok(());
    }
    if !same_folder(&current, &change.old_path) {
        return Err(AppError::message("文件夹当前路径与记录不符，无法重做"));
    }

    let paths = LibraryPaths::new(root);
    let old_dir = paths.folder_dir(&change.old_path);
    let new_dir = paths.folder_dir(&change.new_path);
    let case_only = same_folder(&change.old_path, &change.new_path);
    let physical_done = !old_dir.is_dir() && new_dir.is_dir();
    if !physical_done {
        if !old_dir.is_dir() {
            return Err(AppError::message("文件夹目录不存在，无法重做"));
        }
        if !case_only && new_dir.exists() {
            return Err(AppError::message("目标位置已经有同名文件夹，无法重做"));
        }
        rename_directory(&old_dir, &new_dir, case_only)?;
        journal.renamed(&old_dir, &new_dir);
        ensure_inside_root(root, &new_dir)?;
    }

    update_subtree_paths(transaction, &change.old_path, &change.new_path, change.id).and_then(
        |_| {
            transaction.execute(
                "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
                params![change.new_sort_index, change.id],
            )?;
            Ok(())
        },
    )
}

/// 重做“删除空文件夹”：仍为空时删目录与行；已经不存在视为完成。
pub(crate) fn redo_delete(
    transaction: &Transaction<'_>,
    root: &Path,
    deleted: &FolderDeleted,
    journal: &mut FolderFsJournal,
) -> AppResult<()> {
    let existing: Option<String> = transaction
        .query_row(
            "SELECT path FROM folders WHERE id = ?1",
            [deleted.id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(existing) = existing else {
        return Ok(());
    };
    if !same_folder(&existing, &deleted.path) {
        return Err(AppError::message(
            "文件夹 id 已被其他文件夹占用，无法重做删除",
        ));
    }
    let pattern = subtree_like_pattern(&deleted.path);
    let children: i64 = transaction.query_row(
        "SELECT count(*) FROM folders WHERE path LIKE ?1 ESCAPE '\\'",
        [pattern],
        |row| row.get(0),
    )?;
    let images: i64 = transaction.query_row(
        "SELECT count(*) FROM images WHERE folder_id = ?1",
        [deleted.id],
        |row| row.get(0),
    )?;
    if children > 0 || images > 0 {
        return Err(AppError::message("文件夹里已经有内容，不能重做删除"));
    }
    let directory = LibraryPaths::new(root).folder_dir(&deleted.path);
    if directory.exists() {
        ensure_inside_root(root, &directory)?;
        if std::fs::read_dir(&directory)?.next().is_some() {
            return Err(AppError::message("文件夹目录不是空的，不能重做删除"));
        }
        std::fs::remove_dir(&directory)?;
        journal.removed(&directory);
    }
    transaction.execute("DELETE FROM folders WHERE id = ?1", [deleted.id])?;
    Ok(())
}

/// 重做同级排序：恢复撤销时捕获的目标排序位。
pub(crate) fn redo_reorder(transaction: &Transaction<'_>, rows: &[(i64, i64)]) -> AppResult<()> {
    for (id, sort_index) in rows {
        let changed = transaction.execute(
            "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
            params![sort_index, id],
        )?;
        if changed == 0 {
            return Err(AppError::message("排序记录中的文件夹不存在"));
        }
    }
    Ok(())
}

fn rename_directory(old: &Path, new: &Path, case_only: bool) -> AppResult<()> {
    if !case_only {
        return std::fs::rename(old, new).map_err(AppError::from);
    }
    // Windows 不允许直接把 Foo 改成 foo（目标被视为已存在），用临时名中转。
    let parent = old.parent().unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(".artbank-rename-{}", uuid::Uuid::new_v4()));
    std::fs::rename(old, &temporary)?;
    match std::fs::rename(&temporary, new) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::rename(&temporary, old);
            Err(error.into())
        }
    }
}

fn update_subtree_paths(
    connection: &Connection,
    old_path: &str,
    new_path: &str,
    folder_id_value: i64,
) -> AppResult<()> {
    let pattern = subtree_like_pattern(old_path);
    connection.execute(
        "UPDATE folders
         SET path = ?1 || substr(path, length(?2) + 1)
         WHERE path LIKE ?3 ESCAPE '\\' AND id <> ?4",
        params![new_path, old_path, pattern, folder_id_value],
    )?;
    connection.execute(
        "UPDATE folders SET path = ?1 WHERE id = ?2",
        params![new_path, folder_id_value],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, sync::Arc};

    use tempfile::tempdir;

    use super::*;
    use crate::library::{history, service};

    fn fixture() -> (tempfile::TempDir, Arc<LibraryDb>, PathBuf) {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "文件夹测试", 1).unwrap();
        let database = session.db.clone();
        (temp, database, root)
    }

    #[test]
    fn creates_nested_folders_with_ancestors() {
        let (_temp, db, root) = fixture();
        let folders = create(&db, &root, "人物/速写/手部").unwrap();
        let paths: Vec<&str> = folders.iter().map(|item| item.path.as_str()).collect();
        assert!(paths.contains(&"人物"));
        assert!(paths.contains(&"人物/速写"));
        assert!(paths.contains(&"人物/速写/手部"));
        assert!(root.join("人物").join("速写").join("手部").is_dir());
        assert!(create(&db, &root, "人物").is_err());
        assert!(create(&db, &root, "CON").is_err());

        let entries = history::list(&db).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, HistoryKind::FolderCreate);
        assert_eq!(entries[0].affected, 3);
    }

    #[test]
    fn renames_and_moves_subtrees() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "人物/速写").unwrap();
        let people = db
            .with_reader(|connection| folder_id(connection, "人物"))
            .unwrap()
            .unwrap();
        let sketch = db
            .with_reader(|connection| folder_id(connection, "人物/速写"))
            .unwrap()
            .unwrap();
        let scenery = create(&db, &root, "风景").unwrap();
        let scenery_id = scenery
            .iter()
            .find(|item| item.path == "风景")
            .map(|item| item.id)
            .unwrap();

        rename(&db, &root, sketch, "草稿").unwrap();
        assert!(root.join("人物").join("草稿").is_dir());
        assert!(!root.join("人物").join("速写").exists());
        let sketch = db
            .with_reader(|connection| folder_id(connection, "人物/草稿"))
            .unwrap()
            .unwrap();

        move_to(&db, &root, sketch, scenery_id).unwrap();
        assert!(root.join("风景").join("草稿").is_dir());
        let moved = db
            .with_reader(|connection| folder_id(connection, "风景/草稿"))
            .unwrap();
        assert_eq!(moved, Some(sketch));

        assert!(move_to(&db, &root, people, people).is_err());
        assert!(move_to(&db, &root, people, scenery_id).is_ok());
    }

    #[test]
    fn renames_case_only_and_rejects_conflicts() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "Sketches").unwrap();
        let id = db
            .with_reader(|connection| folder_id(connection, "Sketches"))
            .unwrap()
            .unwrap();
        rename(&db, &root, id, "sketches").unwrap();
        let names: Vec<String> = fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|name| name == "sketches"));
        assert!(!names.iter().any(|name| name == "Sketches"));
        assert!(create(&db, &root, "SKETCHES").is_err());
    }

    #[test]
    fn reorders_siblings_and_rejects_foreign_folders() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "一").unwrap();
        create(&db, &root, "二").unwrap();
        create(&db, &root, "三").unwrap();
        create(&db, &root, "组/子").unwrap();
        let id = |path: &str| {
            db.with_reader(|connection| folder_id(connection, path))
                .unwrap()
                .unwrap()
        };
        let root_id = id("");
        let (one, two, three) = (id("一"), id("二"), id("三"));
        let group_child = id("组/子");

        let order = |db: &LibraryDb| {
            db.with_reader(|connection| {
                let mut statement = connection.prepare(
                    "SELECT path FROM folders
                     WHERE kind = 'normal' AND instr(path, '/') = 0 AND path <> ''
                     ORDER BY sort_index, id",
                )?;
                let paths = statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(paths)
            })
            .unwrap()
        };

        reorder(&db, &[three], root_id, 0).unwrap();
        assert_eq!(order(&db), vec!["三", "一", "二", "组"]);

        reorder(&db, &[one], root_id, 10).unwrap();
        reorder(&db, &[two], root_id, 0).unwrap();
        assert_eq!(order(&db), vec!["二", "三", "组", "一"]);

        assert!(reorder(&db, &[group_child], root_id, 0).is_err());
        assert!(reorder(&db, &[one, two], three, 0).is_err());
    }

    #[test]
    fn trashes_whole_subtree_and_restores_it() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "父/子").unwrap();
        let parent = id_of(&db, "父");
        let child = id_of(&db, "父/子");

        let change = trash(&db, parent).unwrap();
        assert_eq!(change.removed_ids.len(), 2);
        assert!(root.join("父").is_dir());
        assert!(root.join("父").join("子").is_dir());
        assert_eq!(id_of(&db, "父"), parent, "行保留，只写 trashed_at");
        assert_eq!(id_of(&db, "父/子"), child);
        assert!(
            !query::list_folders(&db)
                .unwrap()
                .iter()
                .any(|folder| folder.path == "父" || folder.path == "父/子"),
            "正常树看不到回收站文件夹"
        );
        let trash_folders = query::list_trash_folders(&db).unwrap();
        assert_eq!(trash_folders.folders.len(), 2);
        assert!(
            create(&db, &root, "父/新").is_err(),
            "回收站路径不能新建同名或子文件夹"
        );

        restore(&db, child).unwrap();
        assert!(
            query::list_folders(&db)
                .unwrap()
                .iter()
                .any(|folder| folder.path == "父/子"),
            "恢复子树里的文件夹会带回整个删除单元"
        );
        assert!(query::list_trash_folders(&db).unwrap().folders.is_empty());

        trash(&db, parent).unwrap();
        let change = delete_permanently(&db, parent).unwrap();
        assert_eq!(change.removed_ids.len(), 2);
        assert!(query::list_trash_folders(&db).unwrap().folders.is_empty());
        let purged: i64 = db
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT count(*) FROM folders
                     WHERE path IN ('父', '父/子') AND purge_operation_id IS NOT NULL",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(purged, 2, "永久删除只写待释放标记，行等释放任务清理");
    }

    #[test]
    fn restore_keeps_separate_deletion_units_apart() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "父/子").unwrap();
        let parent = id_of(&db, "父");
        let child = id_of(&db, "父/子");

        trash(&db, child).unwrap();
        trash(&db, parent).unwrap();
        assert!(
            restore(&db, child).is_err(),
            "上级文件夹来自另一次删除时不能单独恢复"
        );

        restore(&db, parent).unwrap();
        assert!(
            query::list_trash_folders(&db)
                .unwrap()
                .folders
                .iter()
                .any(|folder| folder.path == "父/子"),
            "更早单独删除的子文件夹不随上级恢复"
        );
        restore(&db, child).unwrap();
        assert!(query::list_trash_folders(&db).unwrap().folders.is_empty());
    }

    #[test]
    fn undo_restores_created_renamed_moved_deleted_and_reordered_folders() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "人物/速写").unwrap();
        let people = db
            .with_reader(|connection| folder_id(connection, "人物"))
            .unwrap()
            .unwrap();
        let sketch = db
            .with_reader(|connection| folder_id(connection, "人物/速写"))
            .unwrap()
            .unwrap();
        rename(&db, &root, sketch, "草稿").unwrap();
        let scenery = create(&db, &root, "风景").unwrap();
        let scenery_id = scenery
            .iter()
            .find(|item| item.path == "风景")
            .map(|item| item.id)
            .unwrap();
        move_to(&db, &root, sketch, scenery_id).unwrap();
        let root_id = db
            .with_reader(|connection| folder_id(connection, ""))
            .unwrap()
            .unwrap();
        reorder(&db, &[scenery_id, people], root_id, 0).unwrap();
        trash(&db, sketch).unwrap();

        let entries = history::list(&db).unwrap();
        let target = entries.last().unwrap().id;
        let thumbs = crate::library::thumbs::ThumbPack::open(
            crate::library::paths::LibraryPaths::new(&root).thumbs,
        )
        .unwrap();
        let task = crate::app::tasks::TaskHandle::new(
            "folder-undo",
            crate::app::tasks::TaskKind::Undo,
            "测试",
        );
        history::run_undo(&db, &root, &thumbs, target, task).unwrap();

        let entries = history::list(&db).unwrap();
        assert_eq!(entries.len(), 6);
        assert!(
            entries
                .iter()
                .all(|entry| entry.status == crate::library::history::OperationStatus::Undone)
        );
        assert!(!root.join("人物").exists());
        assert!(!root.join("风景").exists());
        assert!(
            db.with_reader(|connection| folder_id(connection, "人物/速写"))
                .unwrap()
                .is_none()
        );

        let target = entries[0].id;
        let task = crate::app::tasks::TaskHandle::new(
            "folder-redo",
            crate::app::tasks::TaskKind::Redo,
            "测试",
        );
        history::run_redo(&db, &root, &thumbs, target, task).unwrap();
        let entries = history::list(&db).unwrap();
        assert!(
            entries
                .iter()
                .all(|entry| entry.status == crate::library::history::OperationStatus::Applied)
        );
        assert!(root.join("人物").is_dir());
        assert!(root.join("风景").is_dir());
        assert!(
            root.join("风景").join("草稿").is_dir(),
            "移入回收站不搬文件，重做后目录仍在原路径"
        );
        assert!(!root.join("人物").join("速写").exists());
        assert!(!root.join("人物").join("草稿").exists());
        let order: Vec<String> = db
            .with_reader(|connection| {
                let mut statement = connection.prepare(
                    "SELECT path FROM folders
                     WHERE kind = 'normal' AND instr(path, '/') = 0 AND path <> ''
                     ORDER BY sort_index, id",
                )?;
                let paths = statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(paths)
            })
            .unwrap();
        assert_eq!(order, vec!["风景".to_owned(), "人物".to_owned()]);
    }

    fn id_of(db: &LibraryDb, path: &str) -> i64 {
        db.with_reader(|connection| folder_id(connection, path))
            .unwrap()
            .unwrap()
    }

    fn children(db: &LibraryDb, parent: &str) -> Vec<String> {
        db.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT path FROM folders WHERE kind = 'normal' AND path <> '' ORDER BY sort_index, id",
            )?;
            let paths = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(paths)
        })
        .unwrap()
        .into_iter()
        .filter(|path| parent_path(path) == parent)
        .collect()
    }

    fn undo_last(db: &LibraryDb, root: &Path) -> i64 {
        let target = history::list(db).unwrap().first().unwrap().id;
        let thumbs = crate::library::thumbs::ThumbPack::open(
            crate::library::paths::LibraryPaths::new(root).thumbs,
        )
        .unwrap();
        let task = crate::app::tasks::TaskHandle::new(
            "folder-test-undo",
            crate::app::tasks::TaskKind::Undo,
            "测试",
        );
        history::run_undo(db, root, &thumbs, target, task).unwrap();
        target
    }

    fn redo(db: &LibraryDb, root: &Path, target: i64) {
        let thumbs = crate::library::thumbs::ThumbPack::open(
            crate::library::paths::LibraryPaths::new(root).thumbs,
        )
        .unwrap();
        let task = crate::app::tasks::TaskHandle::new(
            "folder-test-redo",
            crate::app::tasks::TaskKind::Redo,
            "测试",
        );
        history::run_redo(db, root, &thumbs, target, task).unwrap();
    }

    #[test]
    fn places_multiple_folders_in_one_operation_and_undoes() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "甲").unwrap();
        create(&db, &root, "乙").unwrap();
        create(&db, &root, "目标").unwrap();
        let (甲, 乙, 目标) = (id_of(&db, "甲"), id_of(&db, "乙"), id_of(&db, "目标"));

        place_many(&db, &root, &[甲, 乙], 目标, None).unwrap();
        assert!(root.join("目标").join("甲").is_dir());
        assert!(root.join("目标").join("乙").is_dir());
        assert!(!root.join("甲").exists());
        assert_eq!(children(&db, "目标"), vec!["目标/甲", "目标/乙"]);
        let entries = history::list(&db).unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].kind, HistoryKind::FolderMove);
        assert!(entries[0].label.contains("目标"));

        let target = undo_last(&db, &root);
        assert!(root.join("甲").is_dir());
        assert!(root.join("乙").is_dir());
        assert!(!root.join("目标").join("甲").exists());
        assert_eq!(children(&db, ""), vec!["甲", "乙", "目标"]);

        redo(&db, &root, target);
        assert!(root.join("目标").join("甲").is_dir());
        assert_eq!(children(&db, "目标"), vec!["目标/甲", "目标/乙"]);
    }

    #[test]
    fn places_multiple_folders_at_a_position_and_restores_sibling_order() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "目标/一").unwrap();
        create(&db, &root, "目标/二").unwrap();
        create(&db, &root, "甲").unwrap();
        create(&db, &root, "乙").unwrap();
        let (甲, 乙, 目标) = (id_of(&db, "甲"), id_of(&db, "乙"), id_of(&db, "目标"));

        place_many(&db, &root, &[甲, 乙], 目标, Some(1)).unwrap();
        assert_eq!(
            children(&db, "目标"),
            vec!["目标/一", "目标/甲", "目标/乙", "目标/二"]
        );

        let target = undo_last(&db, &root);
        assert_eq!(children(&db, "目标"), vec!["目标/一", "目标/二"]);
        assert_eq!(children(&db, ""), vec!["目标", "甲", "乙"]);

        redo(&db, &root, target);
        assert_eq!(
            children(&db, "目标"),
            vec!["目标/一", "目标/甲", "目标/乙", "目标/二"]
        );
    }

    #[test]
    fn place_many_keeps_only_outermost_selected_folders() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "父/子").unwrap();
        create(&db, &root, "目标").unwrap();
        let 父 = id_of(&db, "父");
        let 子 = id_of(&db, "父/子");
        let 目标 = id_of(&db, "目标");
        place_many(&db, &root, &[父, 子], 目标, None).unwrap();
        assert!(root.join("目标").join("父").join("子").is_dir());
        assert_eq!(children(&db, "目标"), vec!["目标/父"]);
    }

    #[test]
    fn applies_folder_order_in_one_record_and_undoes() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "丙").unwrap();
        create(&db, &root, "甲").unwrap();
        create(&db, &root, "乙").unwrap();
        let root_id = id_of(&db, "");
        let (甲, 乙, 丙) = (id_of(&db, "甲"), id_of(&db, "乙"), id_of(&db, "丙"));
        let before = children(&db, "");

        apply_order(
            &db,
            root_id,
            FolderSortKey::Name,
            false,
            false,
            &[(root_id, vec![甲, 乙, 丙])],
        )
        .unwrap();
        assert_eq!(children(&db, ""), vec!["甲", "乙", "丙"]);
        let entries = history::list(&db).unwrap();
        assert_eq!(entries[0].kind, HistoryKind::FolderReorder);
        assert!(entries[0].label.contains("按名称"));
        assert_eq!(entries[0].affected, 3);

        let target = undo_last(&db, &root);
        assert_eq!(children(&db, ""), before);
        redo(&db, &root, target);
        assert_eq!(children(&db, ""), vec!["甲", "乙", "丙"]);
    }

    #[test]
    fn apply_order_rejects_stale_or_foreign_children() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "甲").unwrap();
        create(&db, &root, "乙").unwrap();
        create(&db, &root, "组/子").unwrap();
        let root_id = id_of(&db, "");
        let (甲, 乙) = (id_of(&db, "甲"), id_of(&db, "乙"));
        let 子 = id_of(&db, "组/子");

        assert!(
            apply_order(
                &db,
                root_id,
                FolderSortKey::Total,
                true,
                false,
                &[(root_id, vec![甲])],
            )
            .is_err()
        );
        assert!(
            apply_order(
                &db,
                root_id,
                FolderSortKey::Name,
                false,
                false,
                &[(root_id, vec![甲, 乙, 子])],
            )
            .is_err()
        );
        let 组 = id_of(&db, "组");
        assert!(
            apply_order(
                &db,
                root_id,
                FolderSortKey::Name,
                false,
                false,
                &[(组, vec![子])],
            )
            .is_err()
        );
    }

    #[test]
    fn move_and_delete_labels_include_paths() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "父/子").unwrap();
        create(&db, &root, "目标").unwrap();
        let 子 = id_of(&db, "父/子");
        let 目标 = id_of(&db, "目标");
        move_to(&db, &root, 子, 目标).unwrap();
        let entries = history::list(&db).unwrap();
        assert!(entries[0].label.contains("父/子"));
        trash(&db, id_of(&db, "目标/子")).unwrap();
        let entries = history::list(&db).unwrap();
        assert_eq!(entries[0].kind, HistoryKind::FolderTrash);
        assert!(entries[0].label.contains("目标/子"));
    }

    #[test]
    fn undo_multi_folder_move_restores_every_directory_on_failure() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "甲").unwrap();
        create(&db, &root, "乙").unwrap();
        create(&db, &root, "目标").unwrap();
        let destination = id_of(&db, "目标");
        place_many(
            &db,
            &root,
            &[id_of(&db, "甲"), id_of(&db, "乙")],
            destination,
            None,
        )
        .unwrap();
        assert!(root.join("目标").join("甲").is_dir());
        assert!(root.join("目标").join("乙").is_dir());

        // 第二项在磁盘上消失：撤销会在处理它时报错，第一项已经搬回原位置。
        fs::remove_dir_all(root.join("目标").join("乙")).unwrap();

        let target = history::list(&db).unwrap().first().unwrap().id;
        let thumbs =
            crate::library::thumbs::ThumbPack::open(LibraryPaths::new(&root).thumbs).unwrap();
        let task = crate::app::tasks::TaskHandle::new(
            "folder-test-undo",
            crate::app::tasks::TaskKind::Undo,
            "测试",
        );
        assert!(history::run_undo(&db, &root, &thumbs, target, task).is_err());

        // 整次当作没发生：数据库仍是移动后的路径，磁盘也必须回到移动后的位置。
        let moved = id_of(&db, "目标/甲");
        assert_eq!(
            db.with_reader(|connection| folder_path(connection, moved))
                .unwrap(),
            "目标/甲"
        );
        assert!(root.join("目标").join("甲").is_dir());
        assert!(!root.join("甲").exists());
    }

    #[test]
    fn move_guard_is_case_insensitive_for_legacy_paths() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "Art").unwrap();
        let art = id_of(&db, "Art");
        // 旧数据可能把子路径写成另一种大小写；这里直接造出这种行。
        let child = db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO folders(path, created_at, sort_index) VALUES ('art/sub', 1, 0)",
                    [],
                )?;
                Ok(connection.last_insert_rowid())
            })
            .unwrap();
        fs::create_dir_all(root.join("Art").join("sub")).unwrap();

        let error = place_many(&db, &root, &[art], child, None).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("不能把文件夹移动到自身或子文件夹"),
            "{error}"
        );
        assert!(move_to(&db, &root, art, child).is_err());
    }
}
