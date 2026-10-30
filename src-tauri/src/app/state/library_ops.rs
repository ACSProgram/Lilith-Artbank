//! 图库查询与读写包装：图片、文件夹、标签、集合、移动与回收站。

use std::{path::PathBuf, sync::Arc};

use crate::{
    app::error::{AppError, AppResult},
    concurrency::resources::{PathScope, ResourceGuard},
    library::{
        BatchReporter,
        capability::{self, FeatureSummary, ImageCapabilities},
        collections::{self, CollectionInfo, CombineMode},
        content,
        db::LibraryDb,
        folders as folder_service,
        metadata::{
            self, DeleteReport, ImageDetails, ImageState, MoveReport, SelectionTags, TagRef,
        },
        paths::{join_folder, normalize_folder, normalize_folder_name, parent_path},
        query::{self, FolderChange, FolderInfo, ImagePage, LibraryQuery, TagInfo},
        stats::{self, StatsRequest, StatsSnapshot},
        thumbs::ThumbPack,
        viewer::PoseData,
    },
};

use super::AppState;

impl AppState {
    pub fn query_images(&self, query: LibraryQuery) -> AppResult<ImagePage> {
        let (database, root) = self.query_source()?;
        query::query_page(&database, &root, query)
    }

    pub fn query_image_ids(&self, query: LibraryQuery) -> AppResult<Vec<i64>> {
        let (database, _) = self.query_source()?;
        query::query_ids(&database, query)
    }

    pub fn library_stats(&self, request: StatsRequest) -> AppResult<StatsSnapshot> {
        let (database, _) = self.query_source()?;
        stats::library_stats(&database, request)
    }

    pub fn folders(&self) -> AppResult<Vec<FolderInfo>> {
        let (database, _) = self.query_source()?;
        query::list_folders(&database)
    }

    pub fn create_folder(&self, path: &str) -> AppResult<FolderChange> {
        let normalized = normalize_folder(path)?;
        // 父级一起锁：新建会更新同级顺序，两个新文件夹并发创建时不能读到同一个末尾序号。
        let parent = parent_path(&normalized);
        let _guard = self.begin_folder_change(
            "新建文件夹",
            vec![PathScope::write(parent), PathScope::write(&normalized)],
        )?;
        let (database, root) = self.query_source()?;
        self.mutate(|| folder_service::create(&database, &root, path))
            .map(folder_change)
    }

    pub fn rename_folder(&self, folder_id: i64, name: &str) -> AppResult<FolderChange> {
        let name = normalize_folder_name(name)?;
        let (database, root) = self.query_source()?;
        for _ in 0..2 {
            let current = folder_service::info(&database, folder_id)?;
            if current.path.is_empty() {
                return Err(AppError::message("不能重命名图库根目录"));
            }
            let new_path = join_folder(parent_path(&current.path), &name);
            let _guard = self.begin_folder_change(
                "重命名文件夹",
                vec![PathScope::write(&current.path), PathScope::write(&new_path)],
            )?;
            let latest = folder_service::info(&database, folder_id)?;
            if latest.path.eq_ignore_ascii_case(&current.path) {
                return self
                    .mutate(|| folder_service::rename(&database, &root, folder_id, &name))
                    .map(folder_change);
            }
        }
        Err(AppError::message("文件夹已变化，请重试"))
    }

    pub fn move_folder(&self, folder_id: i64, parent_id: i64) -> AppResult<FolderChange> {
        let (database, root) = self.query_source()?;
        for _ in 0..2 {
            let current = folder_service::info(&database, folder_id)?;
            if current.path.is_empty() {
                return Err(AppError::message("不能移动图库根目录"));
            }
            let parent = folder_service::info(&database, parent_id)?;
            let leaf = current
                .path
                .rsplit_once('/')
                .map_or(current.path.as_str(), |(_, leaf)| leaf);
            let new_path = join_folder(&parent.path, leaf);
            let _guard = self.begin_folder_change(
                "移动文件夹",
                vec![
                    PathScope::write(&current.path),
                    PathScope::write(&new_path),
                    // 移动会把目标同级序号追加到末尾，同一父级下的并发移动要串行。
                    PathScope::write(&parent.path),
                ],
            )?;
            let latest = folder_service::info(&database, folder_id)?;
            if latest.path.eq_ignore_ascii_case(&current.path) {
                return self
                    .mutate(|| folder_service::move_to(&database, &root, folder_id, parent_id))
                    .map(folder_change);
            }
        }
        Err(AppError::message("文件夹已变化，请重试"))
    }

    pub fn place_folders(
        &self,
        folder_ids: &[i64],
        parent_id: i64,
        sort_index: Option<i64>,
    ) -> AppResult<FolderChange> {
        let (database, root) = self.query_source()?;
        let parent = folder_service::info(&database, parent_id)?;
        let mut paths = vec![PathScope::write(&parent.path)];
        for id in folder_ids {
            let current = folder_service::info(&database, *id)?;
            let leaf = current
                .path
                .rsplit_once('/')
                .map_or(current.path.as_str(), |(_, leaf)| leaf);
            paths.push(PathScope::write(&current.path));
            paths.push(PathScope::write(join_folder(&parent.path, leaf)));
        }
        let _guard = self.begin_folder_change("整理文件夹", paths)?;
        self.mutate(|| {
            folder_service::place_many(&database, &root, folder_ids, parent_id, sort_index)
        })
        .map(folder_change)
    }

    pub fn apply_folder_order(
        &self,
        folder_id: i64,
        key: folder_service::FolderSortKey,
        descending: bool,
        recursive: bool,
        orders: &[(i64, Vec<i64>)],
    ) -> AppResult<FolderChange> {
        let (database, _) = self.query_source()?;
        let current = folder_service::info(&database, folder_id)?;
        let _guard =
            self.begin_folder_change("文件夹排序", vec![PathScope::write(&current.path)])?;
        self.mutate(|| {
            folder_service::apply_order(&database, folder_id, key, descending, recursive, orders)
        })
        .map(folder_change)
    }

    pub fn reorder_folders(
        &self,
        folder_ids: &[i64],
        parent_id: i64,
        sort_index: i64,
    ) -> AppResult<FolderChange> {
        let (database, _) = self.query_source()?;
        let parent = folder_service::info(&database, parent_id)?;
        let _guard =
            self.begin_folder_change("文件夹排序", vec![PathScope::write(&parent.path)])?;
        self.mutate(|| folder_service::reorder(&database, folder_ids, parent_id, sort_index))
            .map(folder_change)
    }

    pub fn delete_folder(&self, folder_id: i64) -> AppResult<FolderChange> {
        let (database, _) = self.query_source()?;
        let current = folder_service::info(&database, folder_id)?;
        let _guard =
            self.begin_folder_change("移入回收站", vec![PathScope::write(&current.path)])?;
        self.mutate(|| folder_service::trash(&database, folder_id))
    }

    pub fn restore_folder(&self, folder_id: i64) -> AppResult<FolderChange> {
        let (database, _) = self.query_source()?;
        let current = folder_service::info(&database, folder_id)?;
        let _guard =
            self.begin_folder_change("恢复文件夹", vec![PathScope::write(&current.path)])?;
        self.mutate(|| folder_service::restore(&database, folder_id))
    }

    pub fn delete_folder_permanently(&self, folder_id: i64) -> AppResult<FolderChange> {
        let (database, _) = self.query_source()?;
        let current = folder_service::info(&database, folder_id)?;
        let _guard =
            self.begin_folder_change("永久删除文件夹", vec![PathScope::write(&current.path)])?;
        self.mutate(|| folder_service::delete_permanently(&database, folder_id))
    }

    pub fn trash_folders(&self) -> AppResult<query::TrashFolders> {
        let (database, _) = self.query_source()?;
        query::list_trash_folders(&database)
    }

    pub fn tags(&self, limit: Option<u32>, include_empty: bool) -> AppResult<Vec<TagInfo>> {
        let (database, _) = self.query_source()?;
        query::list_tags_with_empty(&database, limit, include_empty)
    }

    pub fn search_tags(
        &self,
        query_text: &str,
        limit: Option<u32>,
        include_empty: bool,
    ) -> AppResult<Vec<TagInfo>> {
        let (database, _) = self.query_source()?;
        query::search_tags_with_empty(&database, query_text, limit, include_empty)
    }

    pub fn image_detail(&self, image_id: i64) -> AppResult<ImageDetails> {
        let (database, root) = self.query_source()?;
        metadata::image_details(&database, &root, image_id)
    }

    pub fn image_states(&self, image_ids: &[i64]) -> AppResult<Vec<ImageState>> {
        let (database, _) = self.query_source()?;
        metadata::image_states(&database, image_ids)
    }

    pub fn selection_tags(&self, image_ids: &[i64]) -> AppResult<SelectionTags> {
        let (database, _) = self.query_source()?;
        metadata::selection_tags(&database, image_ids)
    }

    pub fn update_image_score(&self, image_id: i64, score: i64) -> AppResult<()> {
        let _guard = self.begin_metadata_write("修改评分")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::update_score(&database, image_id, score))
    }

    pub fn set_images_score(&self, image_ids: &[i64], score: i64) -> AppResult<u64> {
        let _guard = self.begin_metadata_write("修改评分")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::set_scores(&database, image_ids, score))
    }

    pub fn update_image_description(&self, image_id: i64, description: &str) -> AppResult<()> {
        let _guard = self.begin_metadata_write("修改描述")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::update_description(&database, image_id, description))
    }

    pub fn set_image_tags(&self, image_id: i64, tag_names: &[String]) -> AppResult<Vec<TagRef>> {
        let _guard = self.begin_metadata_write("修改标签")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::set_image_tags(&database, image_id, tag_names))
    }

    pub fn add_tags(&self, image_ids: &[i64], tag_names: &[String]) -> AppResult<u64> {
        let _guard = self.begin_metadata_write("修改标签")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::add_tags(&database, image_ids, tag_names))
    }

    pub fn remove_tags(&self, image_ids: &[i64], tag_names: &[String]) -> AppResult<u64> {
        let _guard = self.begin_metadata_write("修改标签")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::remove_tags(&database, image_ids, tag_names))
    }

    pub fn rename_tag(&self, tag_id: i64, name: &str) -> AppResult<()> {
        let _guard = self.begin_metadata_write("重命名标签")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::rename_tag(&database, tag_id, name))
    }

    pub fn delete_tag(&self, tag_id: i64) -> AppResult<u64> {
        let _guard = self.begin_metadata_write("删除标签")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::delete_tag(&database, tag_id))
    }

    pub fn merge_tags(&self, source_id: i64, target_id: i64) -> AppResult<metadata::MergeReport> {
        let _guard = self.begin_metadata_write("合并标签")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| {
            metadata::merge_tags_with_id(&database, source_id, target_id).map(|(r, _)| r)
        })
    }

    pub fn move_images(
        &self,
        image_ids: &[i64],
        folder_path: &str,
        reporter: &dyn BatchReporter,
    ) -> AppResult<MoveReport> {
        let (database, root) = self.query_source()?;
        self.mutate(|| metadata::move_images(&database, &root, image_ids, folder_path, reporter))
    }

    pub fn trash_images(&self, image_ids: &[i64]) -> AppResult<u64> {
        let _guard = self.begin_metadata_write("移入回收站")?;
        let (database, _) = self.query_source()?;
        let changed = self.mutate(|| metadata::set_trashed(&database, image_ids, true))?;
        self.ai.invalidate_similarity();
        Ok(changed)
    }

    pub fn restore_images(&self, image_ids: &[i64]) -> AppResult<u64> {
        let _guard = self.begin_metadata_write("恢复图片")?;
        let (database, _) = self.query_source()?;
        let changed = self.mutate(|| metadata::set_trashed(&database, image_ids, false))?;
        self.ai.invalidate_similarity();
        Ok(changed)
    }

    pub fn delete_images(
        &self,
        image_ids: &[i64],
        reporter: &dyn BatchReporter,
    ) -> AppResult<DeleteReport> {
        let (database, _) = self.query_source()?;
        let report = self.mutate(|| metadata::delete_images(&database, image_ids, reporter))?;
        self.ai.invalidate_similarity();
        Ok(report)
    }

    pub fn empty_trash(&self, reporter: &dyn BatchReporter) -> AppResult<DeleteReport> {
        let (database, _) = self.query_source()?;
        let report = self.mutate(|| metadata::empty_trash(&database, reporter))?;
        self.ai.invalidate_similarity();
        Ok(report)
    }

    pub fn collections(&self) -> AppResult<Vec<CollectionInfo>> {
        let (database, _) = self.query_source()?;
        collections::list(&database)
    }

    pub fn create_collection(&self, name: &str) -> AppResult<CollectionInfo> {
        let _guard = self.begin_metadata_write("新建集合")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::create(&database, name))
    }

    pub fn rename_collection(&self, collection_id: i64, name: &str) -> AppResult<()> {
        let _guard = self.begin_metadata_write("重命名集合")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::rename(&database, collection_id, name))
    }

    pub fn delete_collection(&self, collection_id: i64) -> AppResult<u64> {
        let _guard = self.begin_metadata_write("删除集合")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::delete(&database, collection_id))
    }

    pub fn reorder_collections(&self, collection_ids: &[i64]) -> AppResult<()> {
        let _guard = self.begin_metadata_write("整理集合")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::reorder(&database, collection_ids))
    }

    pub fn add_images_to_collection(
        &self,
        collection_id: i64,
        image_ids: &[i64],
    ) -> AppResult<u64> {
        let _guard = self.begin_metadata_write("修改集合成员")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::add_images(&database, collection_id, image_ids))
    }

    pub fn remove_images_from_collection(
        &self,
        collection_id: i64,
        image_ids: &[i64],
    ) -> AppResult<u64> {
        let _guard = self.begin_metadata_write("修改集合成员")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::remove_images(&database, collection_id, image_ids))
    }

    pub fn combine_collections(
        &self,
        mode: CombineMode,
        collection_ids: &[i64],
        name: &str,
    ) -> AppResult<CollectionInfo> {
        let _guard = self.begin_metadata_write("合并集合")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::combine(&database, mode, collection_ids, name))
    }

    pub fn create_collections_from_groups(
        &self,
        groups: &[Vec<i64>],
        base_name: &str,
    ) -> AppResult<Vec<CollectionInfo>> {
        let _guard = self.begin_metadata_write("新建集合")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::create_groups(&database, groups, base_name))
    }

    pub fn image_pose(&self, image_id: i64) -> AppResult<Option<PoseData>> {
        self.ai.read_pose(image_id)
    }

    pub(crate) fn begin_edit(&self, image_id: i64) -> AppResult<ResourceGuard> {
        let (database, _) = self.query_source()?;
        for _ in 0..3 {
            let row = content::load(&database, image_id)?;
            let guard =
                self.begin_content_write("保存编辑", vec![PathScope::write(&row.folder_path)])?;
            let latest = content::load(&database, image_id)?;
            if latest.folder_path.eq_ignore_ascii_case(&row.folder_path)
                && latest.content_hash == row.content_hash
            {
                self.ai.invalidate_similarity();
                query::clear_page_cache();
                return Ok(guard);
            }
        }
        Err(AppError::message("图片位置正在变化，请稍后重试"))
    }

    pub(crate) fn edit_handles(&self) -> AppResult<(Arc<LibraryDb>, Arc<ThumbPack>, PathBuf)> {
        self.library_handles()
            .ok_or_else(|| AppError::message("尚未打开图库"))
    }

    pub(crate) fn copy_source(&self, image_id: i64) -> AppResult<(PathBuf, ResourceGuard)> {
        let (database, root) = self.query_source()?;
        let holder = Self::short_holder("复制图片", "正在读取图片");
        let (row, guard) =
            content::try_lock_original(&database, &self.resources(), image_id, &holder)?;
        let path = row.path(&crate::library::paths::LibraryPaths::new(&root))?;
        Ok((path, guard))
    }

    pub fn image_capabilities(&self, image_ids: &[i64]) -> AppResult<Vec<ImageCapabilities>> {
        let (database, _) = self.query_source()?;
        capability::image_capabilities(&database, image_ids)
    }

    pub fn feature_summary(&self) -> AppResult<FeatureSummary> {
        let (database, _) = self.query_source()?;
        capability::feature_summary(&database)
    }

    /// 相似分组合并前的保留项挑选：只读，返回与 `groups` 等长的保留 id。
    pub fn similarity_group_keeps(
        &self,
        groups: &[Vec<i64>],
        sort: &[query::SortSpec],
    ) -> AppResult<Vec<Option<i64>>> {
        let (database, _) = self.query_source()?;
        query::group_keeps(&database, groups, sort)
    }
}

/// 文件夹写命令只回传受影响的行，前端按 id 就地合并。
fn folder_change(folders: Vec<FolderInfo>) -> FolderChange {
    FolderChange {
        folders,
        removed_ids: Vec::new(),
        affected_images: 0,
    }
}
