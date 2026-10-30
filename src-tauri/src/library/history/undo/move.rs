use std::{
    fs,
    path::{Path, PathBuf},
};

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::{
    app::error::{AppError, AppResult},
    library::{db::LibraryDb, folders, fs_journal::FileJournal, metadata, paths::LibraryPaths},
};

use super::super::{
    ChunkFormat, FolderRef, MoveState, RedoRecord,
    chunks::{decode_json, decode_moves, load_chunks},
    replay::{append_redo, load_redo_records},
};
use super::folders::folder_rows;

/// 图片移动：按当前 folder 幂等判断，文件先移回、再更新行；中途失败会回滚已完成的文件移动。
pub(crate) fn undo_move(database: &LibraryDb, root: &Path, operation_id: i64) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let paths = LibraryPaths::new(root);
        let mut renames: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut created_folders: Vec<FolderRef> = Vec::new();
        let mut redo_moves = Vec::new();
        let mut journal = FileJournal::default();
        let result = (|| -> AppResult<()> {
            for chunk in load_chunks(&transaction, operation_id)? {
                if chunk.format == ChunkFormat::Json {
                    created_folders.extend(decode_json::<Vec<FolderRef>>(&chunk.data)?);
                    continue;
                }
                if chunk.format != ChunkFormat::Moves {
                    continue;
                }
                for (image_id, from_folder, to_folder, old_modified_at) in
                    decode_moves(&chunk.data)?
                {
                    let current: Option<(i64, String, String, String, i64)> = transaction
                        .query_row(
                            "SELECT images.folder_id, images.content_hash, images.file_ext,
                                    folders.path, images.modified_at
                             FROM images JOIN folders ON folders.id = images.folder_id
                             WHERE images.id = ?1",
                            [image_id],
                            |row| {
                                Ok((
                                    row.get(0)?,
                                    row.get(1)?,
                                    row.get(2)?,
                                    row.get(3)?,
                                    row.get(4)?,
                                ))
                            },
                        )
                        .optional()?;
                    let Some((folder_id, content_hash, file_ext, to_path, current_modified_at)) =
                        current
                    else {
                        return Err(AppError::message(format!("图片 {image_id} 已不存在")));
                    };
                    if folder_id == from_folder {
                        continue;
                    }
                    if folder_id != to_folder {
                        return Err(AppError::message(format!(
                            "图片 {image_id} 已被移动到其他文件夹"
                        )));
                    }
                    redo_moves.push(MoveState {
                        image_id,
                        from_folder_id: from_folder,
                        to_folder_id: to_folder,
                        new_modified_at: current_modified_at,
                    });
                    let from_path: String = transaction.query_row(
                        "SELECT path FROM folders WHERE id = ?1",
                        [from_folder],
                        |row| row.get(0),
                    )?;
                    let source =
                        metadata::content_path(&paths, &to_path, &content_hash, &file_ext)?;
                    let destination =
                        metadata::content_path(&paths, &from_path, &content_hash, &file_ext)?;
                    if destination.is_file() {
                        if source.is_file() {
                            // 源文件已在原位置（例如上次中断），删掉目标位置的副本即可。
                            let _ = fs::remove_file(&source);
                        }
                    } else {
                        if let Some(parent) = destination.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        fs::rename(&source, &destination)?;
                        renames.push((destination, source));
                    }
                    let changed = transaction.execute(
                        "UPDATE images SET folder_id = ?1, modified_at = ?2 WHERE id = ?3",
                        params![from_folder, old_modified_at, image_id],
                    )?;
                    if changed == 0 {
                        return Err(AppError::message(format!("图片 {image_id} 已不存在")));
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            for (new_path, old_path) in renames.iter().rev() {
                if new_path.is_file() && !old_path.exists() {
                    let _ = fs::rename(new_path, old_path);
                }
            }
            journal.rollback();
            return Err(error);
        }
        if !redo_moves.is_empty() {
            append_redo(
                &transaction,
                operation_id,
                &RedoRecord::MoveStates { items: redo_moves },
            )?;
        }
        let folder_rows = folder_rows(&transaction, &created_folders)?;
        if !folder_rows.is_empty() {
            append_redo(
                &transaction,
                operation_id,
                &RedoRecord::FolderRows { items: folder_rows },
            )?;
        }
        if let Err(error) =
            folders::cleanup_created(&transaction, root, &created_folders, &mut journal)
        {
            journal.rollback();
            return Err(error);
        }
        if let Err(error) = transaction.commit() {
            journal.rollback();
            return Err(error.into());
        }
        Ok(())
    })
}

pub(crate) fn redo_move(database: &LibraryDb, root: &Path, operation_id: i64) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let paths = LibraryPaths::new(root);
        let mut moves = Vec::new();
        let mut folders = Vec::new();
        for record in load_redo_records(&transaction, operation_id)? {
            match record {
                RedoRecord::MoveStates { items } => moves.extend(items),
                RedoRecord::FolderRows { items } => folders.extend(items),
                _ => {}
            }
        }
        // 只新建了目标文件夹、没有任何图片移动成功的记录也要能重做。
        if moves.is_empty() && folders.is_empty() {
            return Err(AppError::message("这条记录缺少重做数据"));
        }
        let mut journal = FileJournal::default();
        let mut renames: Vec<(PathBuf, PathBuf)> = Vec::new();
        let result = (|| -> AppResult<()> {
            // 与撤销对称：任一条失败都逆序撤回已经落地的目录动作。
            folders::redo_create(&transaction, root, &folders, &mut journal)?;
            for item in &moves {
                let current: Option<(i64, String, String, String)> = transaction
                    .query_row(
                        "SELECT images.folder_id, images.content_hash, images.file_ext,
                                folders.path
                         FROM images JOIN folders ON folders.id = images.folder_id
                         WHERE images.id = ?1",
                        [item.image_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .optional()?;
                let Some((folder_id, content_hash, file_ext, from_path)) = current else {
                    return Err(AppError::message(format!(
                        "图片 {} 已不存在",
                        item.image_id
                    )));
                };
                if folder_id == item.to_folder_id {
                    continue;
                }
                if folder_id != item.from_folder_id {
                    return Err(AppError::message(format!(
                        "图片 {} 已被移动到其他文件夹",
                        item.image_id
                    )));
                }
                let to_path: String = transaction.query_row(
                    "SELECT path FROM folders WHERE id = ?1",
                    [item.to_folder_id],
                    |row| row.get(0),
                )?;
                let source = metadata::content_path(&paths, &from_path, &content_hash, &file_ext)?;
                let destination =
                    metadata::content_path(&paths, &to_path, &content_hash, &file_ext)?;
                if destination.is_file() {
                    if source.is_file() {
                        let _ = fs::remove_file(&source);
                    }
                } else {
                    if let Some(parent) = destination.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::rename(&source, &destination)?;
                    renames.push((destination, source));
                }
                let changed = transaction.execute(
                    "UPDATE images SET folder_id = ?1, modified_at = ?2 WHERE id = ?3",
                    params![item.to_folder_id, item.new_modified_at, item.image_id],
                )?;
                if changed == 0 {
                    return Err(AppError::message(format!(
                        "图片 {} 已不存在",
                        item.image_id
                    )));
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            for (new_path, old_path) in renames.iter().rev() {
                if new_path.is_file() && !old_path.exists() {
                    let _ = fs::rename(new_path, old_path);
                }
            }
            journal.rollback();
            return Err(error);
        }
        if let Err(error) = transaction.commit() {
            journal.rollback();
            return Err(error.into());
        }
        Ok(())
    })
}
