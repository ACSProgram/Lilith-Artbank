use serde::{Deserialize, Serialize};

use crate::{
    app::error::AppResult,
    library::{
        db::LibraryDb,
        query::{self, FilterCounts, FolderStats, LibraryQuery},
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum StatsProfile {
    Facets,
    Summary,
    FolderDetail,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsRequest {
    pub profile: StatsProfile,
    pub query: LibraryQuery,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryStats {
    pub total: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "profile", content = "data", rename_all = "camelCase")]
pub enum StatsSnapshot {
    Facets(FilterCounts),
    Summary(SummaryStats),
    FolderDetail(FolderStats),
}

/// 只读统计的唯一命令入口；SQL 主体仍留在 `query`，按 profile 分发。
pub fn library_stats(database: &LibraryDb, request: StatsRequest) -> AppResult<StatsSnapshot> {
    match request.profile {
        StatsProfile::Facets => {
            query::filter_counts(database, request.query).map(StatsSnapshot::Facets)
        }
        StatsProfile::Summary => query::query_total(database, request.query)
            .map(|total| StatsSnapshot::Summary(SummaryStats { total })),
        StatsProfile::FolderDetail => {
            query::folder_stats(database, request.query).map(StatsSnapshot::FolderDetail)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::*;
    use crate::library::{import::ensure_folder, query::LibraryView, service};

    fn library() -> (tempfile::TempDir, Arc<LibraryDb>, i64) {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "统计测试", 2).unwrap();
        let folder_id = session
            .db
            .with_writer(|connection| ensure_folder(connection, "人物", 1))
            .unwrap();
        (temp, session.db, folder_id)
    }

    fn insert_image(connection: &Connection, seed: &str, folder_id: i64) {
        connection
            .execute(
                "INSERT INTO images(
                    content_hash, file_ext, folder_id, file_size, width, height, format,
                    has_alpha, phash, colors4, colors8, score, description, imported_at, modified_at
                 ) VALUES (?1, 'jpg', ?2, 100, 4, 3, 'jpeg', 0, zeroblob(32), zeroblob(16),
                           zeroblob(32), 3, '', 1, 1)",
                rusqlite::params![format!("{:0>64}", seed), folder_id],
            )
            .unwrap();
    }

    #[test]
    fn snapshot_and_profile_use_camel_case_tags() {
        let summary =
            serde_json::to_value(StatsSnapshot::Summary(SummaryStats { total: 3 })).unwrap();
        assert_eq!(summary["profile"], "summary");
        assert_eq!(summary["data"]["total"], 3);

        let request: StatsRequest = serde_json::from_value(serde_json::json!({
            "profile": "folderDetail",
            "query": {}
        }))
        .unwrap();
        assert_eq!(request.profile, StatsProfile::FolderDetail);
    }

    #[test]
    fn dispatches_profiles() {
        let (_temp, db, folder_id) = library();
        db.with_writer(|connection| {
            insert_image(connection, "a", folder_id);
            Ok(())
        })
        .unwrap();

        let facets = library_stats(
            &db,
            StatsRequest {
                profile: StatsProfile::Facets,
                query: LibraryQuery::default(),
            },
        )
        .unwrap();
        assert!(matches!(facets, StatsSnapshot::Facets(_)));

        let summary = library_stats(
            &db,
            StatsRequest {
                profile: StatsProfile::Summary,
                query: LibraryQuery::default(),
            },
        )
        .unwrap();
        match summary {
            StatsSnapshot::Summary(stats) => assert_eq!(stats.total, 1),
            _ => panic!("expected summary"),
        }

        let detail = library_stats(
            &db,
            StatsRequest {
                profile: StatsProfile::FolderDetail,
                query: LibraryQuery {
                    filters: crate::library::query::LibraryFilters {
                        folder_ids: vec![folder_id],
                        ..Default::default()
                    },
                    ..Default::default()
                },
            },
        )
        .unwrap();
        match detail {
            StatsSnapshot::FolderDetail(stats) => assert_eq!(stats.image_count, 1),
            _ => panic!("expected folder detail"),
        }
    }

    #[test]
    fn rejects_folder_detail_without_scope_or_in_trash() {
        let (_temp, db, _folder_id) = library();
        let empty = library_stats(
            &db,
            StatsRequest {
                profile: StatsProfile::FolderDetail,
                query: LibraryQuery::default(),
            },
        );
        assert!(empty.is_err());

        let trash = library_stats(
            &db,
            StatsRequest {
                profile: StatsProfile::FolderDetail,
                query: LibraryQuery {
                    view: LibraryView::Trash,
                    filters: crate::library::query::LibraryFilters {
                        folder_ids: vec![1],
                        ..Default::default()
                    },
                    ..Default::default()
                },
            },
        );
        assert!(trash.is_err());
    }
}
