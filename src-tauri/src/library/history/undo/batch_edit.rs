use std::{collections::HashMap, path::Path};

use rusqlite::Connection;

use crate::{
    app::{
        error::{AppError, AppResult},
        tasks::TaskHandle,
    },
    library::{db::LibraryDb, thumbs::ThumbPack},
};

use super::super::{
    BatchEditRecord, ChunkFormat, EditRedoState, EditSnapshot,
    chunks::{decode_json, load_chunks},
};
use super::edit::{load_redo_states, restore_new, restore_old};

/// 读取批量编辑的元数据与逐张快照；早期记录只有裸 `EditSnapshot`，按可撤销处理。
fn load_records(
    connection: &Connection,
    operation_id: i64,
) -> AppResult<(bool, Vec<EditSnapshot>)> {
    let mut reversible = true;
    let mut snapshots = Vec::new();
    for chunk in load_chunks(connection, operation_id)? {
        if chunk.format != ChunkFormat::Json {
            continue;
        }
        match serde_json::from_slice::<BatchEditRecord>(&chunk.data) {
            Ok(BatchEditRecord::Meta { reversible: value }) => reversible = value,
            Ok(BatchEditRecord::Snapshot { snapshot }) => snapshots.push(*snapshot),
            // 兼容早期只写裸 `EditSnapshot` 的记录。
            Err(_) => snapshots.push(decode_json(&chunk.data)?),
        }
    }
    Ok((reversible, snapshots))
}

/// 撤销整批：按编辑顺序的逆序逐张恢复；中途取消保留已恢复部分，记录停在 `undoing` 供续做。
pub(crate) fn undo_batch_edit(
    database: &LibraryDb,
    root: &Path,
    thumbs: &ThumbPack,
    task: &TaskHandle,
    operation_id: i64,
) -> AppResult<bool> {
    let (reversible, snapshots) =
        database.with_reader(|connection| load_records(connection, operation_id))?;
    if !reversible || snapshots.is_empty() {
        return Err(AppError::message(
            "本次批量编辑没有保留撤销副本；原图已在编辑后替换",
        ));
    }
    task.set_total(snapshots.len());
    for snapshot in snapshots.iter().rev() {
        if task.should_cancel() {
            return Ok(false);
        }
        restore_old(database, root, thumbs, operation_id, snapshot)?;
        task.inc_completed();
    }
    Ok(true)
}

/// 重做整批：按编辑顺序逐张恢复。
pub(crate) fn redo_batch_edit(
    database: &LibraryDb,
    root: &Path,
    thumbs: &ThumbPack,
    task: &TaskHandle,
    operation_id: i64,
) -> AppResult<bool> {
    let (_, snapshots) =
        database.with_reader(|connection| load_records(connection, operation_id))?;
    if snapshots.is_empty() {
        return Err(AppError::message("这条批量编辑记录缺少内容快照"));
    }
    let states: HashMap<i64, EditRedoState> = load_redo_states(database, operation_id)?;
    task.set_total(snapshots.len());
    for snapshot in &snapshots {
        let state = states
            .get(&snapshot.image_id)
            .ok_or_else(|| AppError::message("这条记录缺少重做数据"))?;
        if task.should_cancel() {
            return Ok(false);
        }
        restore_new(database, root, thumbs, operation_id, snapshot, state)?;
        task.inc_completed();
    }
    Ok(true)
}
