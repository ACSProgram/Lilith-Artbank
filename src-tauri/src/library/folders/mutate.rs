use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::{
    app::error::{AppError, AppResult},
    library::{
        db::LibraryDb,
        fs_journal::{self, FileJournal},
        history::{
            self, FolderPathChange, FolderRef, FolderReorder, FolderSortChange, HistoryKind,
            NewOperation,
        },
        import::{ensure_folder_path, next_sort_index},
        paths::{
            LibraryPaths, folder_within, join_folder, normalize_folder, normalize_folder_name,
            parent_path, same_folder, subtree_like_pattern,
        },
        query::{self, FolderInfo},
        service::now_ms,
    },
};

use super::tree::{ancestor_folder_ids, folder_rows_in, subtree_folder_ids};
use super::undo::update_subtree_paths;
use super::{
    FolderSortKey, ensure_inside_root, ensure_normal_folder, ensure_target_free, folder_id,
    folder_path,
};

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
            draft.journal().create_dir_all(&target)?;
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
            fs_journal::rename_path(&old_dir, &new_dir, case_only)?;
            draft.journal().renamed(&old_dir, &new_dir);
            ensure_inside_root(root, &new_dir)?;

            update_subtree_paths(transaction, &old_path, &new_path, folder_id_value)?;
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
                draft.journal().create_dir_all(parent)?;
                ensure_inside_root(root, parent)?;
            }
            if new_dir.exists() {
                return Err(AppError::message("目标文件夹目录已存在"));
            }
            fs_journal::rename_path(&old_dir, &new_dir, false)?;
            draft.journal().renamed(&old_dir, &new_dir);
            ensure_inside_root(root, &new_dir)?;

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
    let mut affected_ids: Vec<i64> = Vec::new();
    history::write_with_id(
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
                draft.journal().create_dir_all(&parent_dir)?;
                ensure_inside_root(root, &parent_dir)?;
            }
            for (_, old_path, _, new_path, old_dir, new_dir) in &targets {
                if old_path == new_path {
                    continue;
                }
                fs_journal::rename_path(old_dir, new_dir, same_folder(old_path, new_path))?;
                draft.journal().renamed(old_dir, new_dir);
            }

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
        },
    )?;
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

pub(super) fn info_in(connection: &Connection, folder_id_value: i64) -> AppResult<FolderInfo> {
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
pub fn deletion_blocker(db: &LibraryDb, folder_id_value: i64) -> AppResult<Option<String>> {
    db.with_reader(|connection| deletion_blocker_in(connection, folder_id_value))
}

fn deletion_blocker_in(connection: &Connection, folder_id_value: i64) -> AppResult<Option<String>> {
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

pub(super) fn remove_folder_if_empty(
    transaction: &Transaction<'_>,
    paths: &LibraryPaths,
    root: &Path,
    id: i64,
    path: &str,
    strict: bool,
    journal: &mut FileJournal,
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
