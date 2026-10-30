//! 批量任务开始前的只读预览：范围 N 张、将处理 M 张。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{
    app::{
        error::{AppError, AppResult},
        tasks::{BatchTaskKind, TaskScope, TaskSkipBucket},
    },
    library::{
        db::LibraryDb,
        edit_batch::{self, BatchEditOptions},
        query::{self, PendingFilter},
    },
};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchTaskPreviewRequest {
    pub kind: BatchTaskKind,
    pub scope: TaskScope,
    #[serde(default)]
    pub overwrite: bool,
    /// 仅 embedding 需要；由前端从 AI health 带入。
    #[serde(default)]
    pub capability: Option<PreviewCapability>,
    /// 只有批量编辑需要：预检按 recipe 计算跳过原因、体积与统一画布可用性。
    #[serde(default)]
    pub edit: Option<BatchEditOptions>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewCapability {
    pub model: String,
    pub dim: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchTaskPreview {
    pub total: u64,
    /// 缺少模型维度或离线时为空，界面显示“—”，不猜一个数字。
    pub will_process: Option<u64>,
    /// 批量编辑：按原因细分的跳过数。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_breakdown: Option<Vec<TaskSkipBucket>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_output_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uniform_dimensions: Option<bool>,
}

pub fn preview(
    database: &LibraryDb,
    root: &Path,
    request: &BatchTaskPreviewRequest,
) -> AppResult<BatchTaskPreview> {
    if request.kind == BatchTaskKind::Edit {
        let (options, ids) = edit_request(request)?;
        let estimate = edit_batch::preview(database, root, &ids, &options)?;
        return Ok(BatchTaskPreview {
            total: estimate.total,
            will_process: Some(estimate.will_process),
            skip_breakdown: Some(
                estimate
                    .skip_breakdown
                    .iter()
                    .map(|(key, count)| TaskSkipBucket {
                        key: key.clone(),
                        count: *count,
                    })
                    .collect(),
            ),
            estimated_output_bytes: Some(estimate.estimated_output_bytes),
            available_bytes: estimate.available_bytes,
            uniform_dimensions: Some(estimate.uniform_dimensions),
        });
    }
    let filter = filter_for(request)?;
    // 缺模型维度时只需要范围总数：用 `All` 扫一遍拿 `total`，`will_process` 不返回。
    let query_filter = filter.clone().unwrap_or(PendingFilter::All);
    let stats = match request.scope.to_library_query()? {
        Some(query) => query::pending_stats(database, query, &query_filter)?,
        None => match &request.scope {
            TaskScope::Selected { image_ids } => {
                query::selected_pending_stats(database, image_ids, &query_filter)?
            }
            _ => return Err(AppError::message("选择集范围解析失败")),
        },
    };
    Ok(BatchTaskPreview {
        total: stats.total,
        will_process: filter.is_some().then_some(stats.will_process),
        skip_breakdown: None,
        estimated_output_bytes: None,
        available_bytes: None,
        uniform_dimensions: None,
    })
}

fn edit_request(request: &BatchTaskPreviewRequest) -> AppResult<(BatchEditOptions, Vec<i64>)> {
    let options = request
        .edit
        .clone()
        .ok_or_else(|| AppError::message("请提供批量编辑参数"))?;
    let TaskScope::Selected { image_ids } = &request.scope else {
        return Err(AppError::message("批量编辑只支持选中的图片"));
    };
    // 与 `start_batch_task` 用同一份规范化，预检的张数才和任务实际处理的一致。
    Ok((options, query::normalize_scope_ids(image_ids)?))
}

fn filter_for(request: &BatchTaskPreviewRequest) -> AppResult<Option<PendingFilter>> {
    Ok(match request.kind {
        BatchTaskKind::Thumbnail => Some(if request.overwrite {
            PendingFilter::All
        } else {
            PendingFilter::Thumbnail
        }),
        BatchTaskKind::AiEmbedding => {
            if request.overwrite {
                Some(PendingFilter::All)
            } else {
                match &request.capability {
                    Some(capability)
                        if !capability.model.trim().is_empty() && capability.dim > 0 =>
                    {
                        Some(PendingFilter::Embedding {
                            model: capability.model.clone(),
                            dim: capability.dim,
                        })
                    }
                    _ => None,
                }
            }
        }
        BatchTaskKind::AiPose => Some(if request.overwrite {
            PendingFilter::All
        } else {
            PendingFilter::Pose
        }),
        BatchTaskKind::AiDescription => Some(PendingFilter::Description {
            overwrite: request.overwrite,
        }),
        // 检查与导出都要处理范围内全部图片，没有「待处理」这个子集。
        BatchTaskKind::CheckFast | BatchTaskKind::CheckFull | BatchTaskKind::Export => {
            Some(PendingFilter::All)
        }
        BatchTaskKind::SimilarityGroups
        | BatchTaskKind::OptimizeEstimate
        | BatchTaskKind::Optimize
        | BatchTaskKind::Edit => {
            return Err(AppError::message("该任务类型不提供待处理数量"));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_kinds_are_rejected() {
        let request = BatchTaskPreviewRequest {
            kind: BatchTaskKind::Optimize,
            scope: TaskScope::Library,
            overwrite: false,
            capability: None,
            edit: None,
        };
        let error = filter_for(&request).unwrap_err();
        assert!(error.to_string().contains("不提供待处理数量"));
    }

    #[test]
    fn embedding_without_dimension_has_no_count() {
        let request = BatchTaskPreviewRequest {
            kind: BatchTaskKind::AiEmbedding,
            scope: TaskScope::Library,
            overwrite: false,
            capability: None,
            edit: None,
        };
        assert!(filter_for(&request).unwrap().is_none());

        let request = BatchTaskPreviewRequest {
            capability: Some(PreviewCapability {
                model: " ".into(),
                dim: 4,
            }),
            ..request
        };
        assert!(filter_for(&request).unwrap().is_none());
    }
}
