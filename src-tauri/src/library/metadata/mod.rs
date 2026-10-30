use crate::{
    app::error::{AppError, AppResult},
    library::ids,
};

mod fields;
mod r#move;
mod purge;
mod read;
mod tags;

pub use fields::{
    set_ai_description, set_ai_description_live, set_scores, set_scores_with_id,
    update_description, update_description_with_id, update_score,
};
pub use r#move::{
    DeleteReport, MoveReport, delete_images, empty_trash, move_images, move_images_with_id,
    set_trashed, set_trashed_with_id,
};
pub use purge::purge_by_operation;
pub(crate) use purge::{
    ContentRemover, FolderDirState, content_path, folder_dir_state, remove_content_file,
};
pub use read::{
    ImageDetails, ImageState, SelectionTags, TagRef, image_details, image_states, selection_tags,
};
pub use tags::{
    MergeReport, add_tags, add_tags_with_id, delete_tag, delete_tag_with_id, delete_zero_tags,
    merge_preview, merge_tags_with_id, remove_tags, remove_tags_with_id, rename_tag,
    rename_tag_with_id, set_image_tags, tag_image_count, tag_name,
};

const SQL_CHUNK: usize = 400;
/// 单次批量上限。涉及文件的操作（移动、永久删除）不再单独设更低的门槛：
/// 它们的 SQL 与文件操作本来就按 400 张分块执行。
const MAX_BATCH_IDS: usize = 1_000_000;

fn validate_ids(values: &[i64], max: usize) -> AppResult<Vec<i64>> {
    let unique = ids::unique_positive(values, "图片")?;
    if unique.is_empty() {
        return Err(AppError::message("没有选择图片"));
    }
    ids::ensure_max(unique.len(), max, format!("一次最多操作 {max} 张图片"))?;
    Ok(unique)
}

#[cfg(test)]
mod tests;
