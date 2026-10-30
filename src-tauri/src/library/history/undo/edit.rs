use std::{fs, path::Path};

use rusqlite::{TransactionBehavior, params};

use crate::{
    app::error::{AppError, AppResult},
    imageops::thumbnail,
    import::decode,
    library::{
        content, db::LibraryDb, import as repository, metadata, paths::LibraryPaths,
        service::now_ms, thumbs::ThumbPack, viewer,
    },
};

use super::super::{
    ChunkFormat, EditRedoState, EditSnapshot, RedoRecord,
    chunks::{decode_json, load_chunks},
    replay::{append_redo, load_redo_records},
    undo_dir,
};

/// 编辑撤销：用暂存旧原图恢复内容列与缩略图；当前内容已经不是本次编辑结果时拒绝猜测。
/// 新原图不删除，移进暂存目录留给重做。
pub(crate) fn undo_edit(
    database: &LibraryDb,
    root: &Path,
    thumbs: &ThumbPack,
    operation_id: i64,
) -> AppResult<()> {
    let snapshot: EditSnapshot = database.with_reader(|connection| {
        let mut snapshot = None;
        for chunk in load_chunks(connection, operation_id)? {
            if chunk.format == ChunkFormat::Json {
                snapshot = Some(decode_json(&chunk.data)?);
            }
        }
        snapshot.ok_or_else(|| AppError::message("编辑记录缺少内容快照"))
    })?;

    let paths = LibraryPaths::new(root);
    let staged_dir = undo_dir(root, operation_id);
    let old_staged = staged_dir.join(format!("{}.{}", snapshot.content_hash, snapshot.file_ext));
    let new_staged = staged_dir.join(format!(
        "redo-{}.{}",
        snapshot.new_hash, snapshot.new_file_ext
    ));
    let old_content = metadata::content_path(
        &paths,
        &snapshot.folder_path,
        &snapshot.content_hash,
        &snapshot.file_ext,
    )?;
    let new_content = metadata::content_path(
        &paths,
        &snapshot.folder_path,
        &snapshot.new_hash,
        &snapshot.new_file_ext,
    )?;

    let current = content::load(database, snapshot.image_id)?;
    if current.content_hash == snapshot.content_hash {
        // 上次中断已经恢复过内容；尽量把新文件收进暂存，供重做。
        if !new_staged.is_file() && new_content.is_file() {
            if let Err(error) = fs::rename(&new_content, &new_staged) {
                log::warn!(
                    "撤销编辑暂存新原图失败 image={} path={}: {error}",
                    snapshot.image_id,
                    new_content.display()
                );
            }
        } else if new_staged.is_file() && new_content.is_file() {
            let _ = fs::remove_file(&new_content);
        }
        let _ = fs::remove_file(viewer::preview_path(
            &paths,
            snapshot.image_id,
            &snapshot.new_hash,
        ));
        return Ok(());
    }
    if current.content_hash != snapshot.new_hash {
        return Err(AppError::message("图片内容已经变化，无法撤销这次编辑"));
    }

    if !stage_file(&old_content, &old_staged)? {
        return Err(AppError::message("编辑前的原图暂存已丢失，无法撤销"));
    }
    let decoded = decode::decode(&old_staged)?;
    let thumb = thumbnail::encode(&decoded.image)?;
    let location = thumbs.append(&thumb.bytes)?;

    let linked = if old_content.is_file() {
        false
    } else {
        if let Some(parent) = old_content.parent() {
            fs::create_dir_all(parent)?;
        }
        if fs::hard_link(&old_staged, &old_content).is_err() {
            fs::copy(&old_staged, &old_content)?;
        }
        true
    };

    let mut staged_new = false;
    if !new_staged.is_file() && new_content.is_file() {
        if let Some(parent) = new_staged.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Err(error) = fs::rename(&new_content, &new_staged) {
            if linked {
                let _ = fs::remove_file(&old_content);
            }
            return Err(error.into());
        }
        staged_new = true;
    }

    let now = now_ms();
    let commit = database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        append_redo(
            &transaction,
            operation_id,
            &RedoRecord::EditState {
                state: EditRedoState {
                    image_id: current.id,
                    folder_path: current.folder_path.clone(),
                    content_hash: current.content_hash.clone(),
                    file_ext: current.file_ext.clone(),
                    file_size: current.file_size,
                    width: current.width,
                    height: current.height,
                    format: current.format.clone(),
                    has_alpha: current.has_alpha,
                    phash: current.phash.clone(),
                    colors4: current.colors4.clone(),
                    colors8: current.colors8.clone(),
                    modified_at: current.modified_at,
                },
            },
        )?;
        let thumb_offset = i64::try_from(location.offset)
            .map_err(|_| AppError::message("缩略图偏移超出数据库范围"))?;
        let changed = transaction.execute(
            "UPDATE images SET
                content_hash = ?1, file_ext = ?2, file_size = ?3, width = ?4, height = ?5,
                format = ?6, has_alpha = ?7, phash = ?8, colors4 = ?9, colors8 = ?10,
                modified_at = ?11, thumb_segment = ?12, thumb_offset = ?13,
                thumb_length = ?14, thumb_format = 'webp', thumb_width = ?15,
                thumb_height = ?16, thumb_version = thumb_version + 1
             WHERE id = ?17",
            params![
                snapshot.content_hash,
                snapshot.file_ext,
                snapshot.file_size,
                snapshot.width,
                snapshot.height,
                snapshot.format,
                snapshot.has_alpha,
                snapshot.phash,
                snapshot.colors4,
                snapshot.colors8,
                snapshot.modified_at,
                location.segment,
                thumb_offset,
                location.length,
                thumb.width,
                thumb.height,
                snapshot.image_id,
            ],
        )?;
        if changed == 0 {
            return Err(AppError::message("图片不存在"));
        }
        transaction.execute(
            "DELETE FROM feature_blobs WHERE image_id = ?1",
            [snapshot.image_id],
        )?;
        repository::ensure_thumb_segment(&transaction, location.segment, now)?;
        if let Ok(size) = thumbs.segment_size_bytes(location.segment) {
            repository::update_thumb_segment_size(&transaction, location.segment, size)?;
        }
        transaction.commit()?;
        Ok(())
    });
    if let Err(error) = commit {
        if staged_new && !new_content.is_file() {
            let _ = fs::rename(&new_staged, &new_content);
        }
        if linked {
            let _ = fs::remove_file(&old_content);
        }
        return Err(error);
    }

    // 行已改回旧哈希；当前内容里的新原图若还在（暂存已有副本），直接删掉。
    if new_content.exists()
        && new_staged.is_file()
        && let Err(error) = metadata::remove_content_file(root, &new_content)
    {
        log::warn!(
            "撤销编辑后清理新原图失败 image={} path={}: {error}",
            snapshot.image_id,
            new_content.display()
        );
    }
    let _ = fs::remove_file(viewer::preview_path(
        &paths,
        snapshot.image_id,
        &snapshot.new_hash,
    ));
    Ok(())
}

pub(crate) fn redo_edit(
    database: &LibraryDb,
    root: &Path,
    thumbs: &ThumbPack,
    operation_id: i64,
) -> AppResult<()> {
    let snapshot: EditSnapshot = database.with_reader(|connection| {
        let mut snapshot = None;
        for chunk in load_chunks(connection, operation_id)? {
            if chunk.format == ChunkFormat::Json {
                snapshot = Some(decode_json(&chunk.data)?);
            }
        }
        snapshot.ok_or_else(|| AppError::message("编辑记录缺少内容快照"))
    })?;
    let state = database.with_reader(|connection| {
        load_redo_records(connection, operation_id)?
            .into_iter()
            .find_map(|record| match record {
                RedoRecord::EditState { state } => Some(state),
                _ => None,
            })
            .ok_or_else(|| AppError::message("这条记录缺少重做数据"))
    })?;

    let paths = LibraryPaths::new(root);
    let staged_dir = undo_dir(root, operation_id);
    let old_staged = staged_dir.join(format!("{}.{}", snapshot.content_hash, snapshot.file_ext));
    let new_staged = staged_dir.join(format!(
        "redo-{}.{}",
        snapshot.new_hash, snapshot.new_file_ext
    ));
    let old_content = metadata::content_path(
        &paths,
        &snapshot.folder_path,
        &snapshot.content_hash,
        &snapshot.file_ext,
    )?;
    let new_content = metadata::content_path(
        &paths,
        &state.folder_path,
        &state.content_hash,
        &state.file_ext,
    )?;

    let current = content::load(database, snapshot.image_id)?;
    if current.content_hash == state.content_hash {
        // 上次已经重做完；补回撤销所需的旧图暂存，并清掉提交后没来得及清理的副本。
        if stage_file(&old_content, &old_staged)? {
            if new_staged.is_file() && new_content.is_file() {
                let _ = fs::remove_file(&new_staged);
            }
            if old_content != new_content && old_content.exists() {
                let _ = metadata::remove_content_file(root, &old_content);
            }
        } else {
            log::warn!(
                "重做编辑后旧原图暂存缺失 image={} path={}",
                snapshot.image_id,
                old_staged.display()
            );
        }
        let _ = fs::remove_file(viewer::preview_path(
            &paths,
            state.image_id,
            &snapshot.content_hash,
        ));
        return Ok(());
    }
    if current.content_hash != snapshot.content_hash {
        return Err(AppError::message("图片内容已经变化，无法重做这次编辑"));
    }

    if !stage_file(&old_content, &old_staged)? {
        log::warn!(
            "重做编辑前暂存旧原图失败 image={} path={}",
            snapshot.image_id,
            old_content.display()
        );
    }
    let mut moved = false;
    if !new_content.is_file() {
        if !new_staged.is_file() {
            return Err(AppError::message("编辑后的原图暂存已丢失，无法重做"));
        }
        if let Some(parent) = new_content.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&new_staged, &new_content)?;
        moved = true;
    }
    let decoded = decode::decode(&new_content)?;
    let thumb = thumbnail::encode(&decoded.image)?;
    let location = thumbs.append(&thumb.bytes)?;
    let now = now_ms();

    let commit = database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let thumb_offset = i64::try_from(location.offset)
            .map_err(|_| AppError::message("缩略图偏移超出数据库范围"))?;
        let changed = transaction.execute(
            "UPDATE images SET
                content_hash = ?1, file_ext = ?2, file_size = ?3, width = ?4, height = ?5,
                format = ?6, has_alpha = ?7, phash = ?8, colors4 = ?9, colors8 = ?10,
                modified_at = ?11, thumb_segment = ?12, thumb_offset = ?13,
                thumb_length = ?14, thumb_format = 'webp', thumb_width = ?15,
                thumb_height = ?16, thumb_version = thumb_version + 1
             WHERE id = ?17",
            params![
                state.content_hash,
                state.file_ext,
                state.file_size,
                state.width,
                state.height,
                state.format,
                state.has_alpha,
                state.phash,
                state.colors4,
                state.colors8,
                state.modified_at,
                location.segment,
                thumb_offset,
                location.length,
                thumb.width,
                thumb.height,
                state.image_id,
            ],
        )?;
        if changed == 0 {
            return Err(AppError::message("图片不存在"));
        }
        transaction.execute(
            "DELETE FROM feature_blobs WHERE image_id = ?1",
            [state.image_id],
        )?;
        repository::ensure_thumb_segment(&transaction, location.segment, now)?;
        if let Ok(size) = thumbs.segment_size_bytes(location.segment) {
            repository::update_thumb_segment_size(&transaction, location.segment, size)?;
        }
        transaction.commit()?;
        Ok(())
    });
    if let Err(error) = commit {
        if moved && !new_staged.exists() {
            let _ = fs::rename(&new_content, &new_staged);
        }
        return Err(error);
    }

    if new_staged.is_file() {
        let _ = fs::remove_file(&new_staged);
    }
    if old_content != new_content
        && old_content.exists()
        && let Err(error) = metadata::remove_content_file(root, &old_content)
    {
        log::warn!(
            "重做编辑后清理旧原图失败 image={} path={}: {error}",
            state.image_id,
            old_content.display()
        );
    }
    let _ = fs::remove_file(viewer::preview_path(
        &paths,
        state.image_id,
        &snapshot.content_hash,
    ));
    Ok(())
}

/// 把原图放进暂存目录；已有暂存直接返回。目录不存在时创建。
fn stage_file(source: &Path, staged: &Path) -> AppResult<bool> {
    if staged.is_file() {
        return Ok(true);
    }
    if !source.is_file() {
        return Ok(false);
    }
    if let Some(parent) = staged.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::hard_link(source, staged).is_err() {
        fs::copy(source, staged)?;
    }
    Ok(true)
}
