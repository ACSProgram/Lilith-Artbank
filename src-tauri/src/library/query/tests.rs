use rusqlite::{Connection, params, types::Value};
use tempfile::tempdir;

use super::folders::FOLDER_COUNTS_SQL;
use super::sql::{PreparedQuery, SearchMode, folder_scope_paths, grouped_sql, sum_sql};
use super::stats::pin_stats_indexes;
use super::tags::{MAX_TAG_DEAD_IMAGES, list_tags_page_in, list_tags_page_legacy};
use super::*;
use crate::library::{db::LibraryDb, folders, import::ensure_folder, service, temp};

struct Fixture {
    _temp: tempfile::TempDir,
    session: service::LibrarySession,
    root: std::path::PathBuf,
    folder_people: i64,
    folder_scenery: i64,
    tag_sketch: i64,
    tag_reference: i64,
}

fn fixture() -> Fixture {
    let temp = tempdir().unwrap();
    let root = temp.path().join("library");
    let (session, _) = service::create(&root, "查询测试", 2).unwrap();
    let (folder_people, folder_scenery, tag_sketch, tag_reference) = session
        .db
        .with_writer(|connection| {
            let people = ensure_folder(connection, "人物", 1)?;
            let scenery = ensure_folder(connection, "风景", 1)?;
            connection.execute("INSERT INTO tags(name) VALUES ('速写')", [])?;
            let sketch = connection.last_insert_rowid();
            connection.execute("INSERT INTO tags(name) VALUES ('参考')", [])?;
            let reference = connection.last_insert_rowid();

            insert_image(
                connection,
                "a",
                "jpg",
                3,
                people,
                1_000,
                300,
                640,
                480,
                None,
                &[sketch, reference],
            );
            insert_image(
                connection,
                "b",
                "png",
                5,
                people,
                2_000,
                200,
                800,
                600,
                None,
                &[sketch],
            );
            insert_image(
                connection,
                "c",
                "jpg",
                1,
                scenery,
                3_000,
                100,
                400,
                400,
                None,
                &[],
            );
            insert_image(
                connection,
                "d",
                "webp",
                4,
                scenery,
                4_000,
                400,
                1200,
                800,
                None,
                &[reference],
            );
            insert_image(
                connection,
                "e",
                "png",
                3,
                people,
                5_000,
                500,
                900,
                900,
                Some(1),
                &[sketch, reference],
            );
            connection.execute(
                "UPDATE images SET description = '人物速写' WHERE id = 1",
                [],
            )?;
            connection.execute(
                "UPDATE images SET description = '人物参考' WHERE id = 2",
                [],
            )?;
            connection.execute(
                "UPDATE images SET description = '风景照片' WHERE id = 3",
                [],
            )?;
            connection.execute(
                "UPDATE images SET description = '城市夜景' WHERE id = 4",
                [],
            )?;
            Ok((people, scenery, sketch, reference))
        })
        .unwrap();

    Fixture {
        _temp: temp,
        session,
        root,
        folder_people,
        folder_scenery,
        tag_sketch,
        tag_reference,
    }
}

#[allow(clippy::too_many_arguments)]
fn insert_image(
    connection: &Connection,
    seed: &str,
    extension: &str,
    score: i64,
    folder_id: i64,
    size: i64,
    imported_at: i64,
    width: i64,
    height: i64,
    deleted_at: Option<i64>,
    tags: &[i64],
) {
    let hash = format!("{seed:0>64}");
    connection
        .execute(
            "INSERT INTO images(
                content_hash, file_ext, folder_id, file_size, width, height, format,
                has_alpha, phash, colors4, colors8, score, description, imported_at,
                modified_at, deleted_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?2, 0,
                zeroblob(32), zeroblob(16), zeroblob(32), ?7, '', ?8, ?8, ?9)",
            params![
                hash,
                extension,
                folder_id,
                size,
                width,
                height,
                score,
                imported_at,
                deleted_at
            ],
        )
        .unwrap();
    let image_id = connection.last_insert_rowid();
    for tag_id in tags {
        connection
            .execute(
                "INSERT INTO image_tags(image_id, tag_id) VALUES (?1, ?2)",
                params![image_id, tag_id],
            )
            .unwrap();
    }
}

fn base_query() -> LibraryQuery {
    LibraryQuery {
        limit: Some(2),
        ..Default::default()
    }
}

fn ids(page: &ImagePage) -> Vec<i64> {
    page.items.iter().map(|item| item.id).collect()
}

fn all_ids(query: LibraryQuery, fixture: &Fixture) -> Vec<i64> {
    let mut cursor = None;
    let mut result = Vec::new();
    loop {
        let page = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                cursor: cursor.clone(),
                limit: Some(2),
                ..query.clone()
            },
        )
        .unwrap();
        result.extend(ids(&page));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    result
}

#[test]
fn pages_with_keyset_without_duplicates() {
    let fixture = fixture();
    let query = base_query();
    let first = query_page(&fixture.session.db, &fixture.root, query.clone()).unwrap();
    assert_eq!(ids(&first), vec![4, 1]);
    assert_eq!(
        first.next_cursor,
        Some(QueryCursor {
            sort_values: vec!["300".into()],
            id: 1
        })
    );

    let collected = all_ids(query.clone(), &fixture);
    assert_eq!(collected, vec![4, 1, 2, 3]);

    let page = query_page(&fixture.session.db, &fixture.root, query).unwrap();
    let tag_names = &page.items[0].tags;
    assert_eq!(tag_names, &vec!["参考".to_owned()]);
    assert!(page.items[0].file_path.ends_with("0000d.webp"));
}

#[test]
fn filters_by_folder_extension_score_and_tags() {
    let fixture = fixture();

    let folder = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            filters: LibraryFilters {
                folder_ids: vec![fixture.folder_people],
                ..Default::default()
            },
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(ids(&folder), vec![1, 2]);

    let extension = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            filters: LibraryFilters {
                extensions: vec!["PNG".into()],
                ..Default::default()
            },
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(ids(&extension), vec![2]);

    let scores = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            filters: LibraryFilters {
                scores: vec![1, 5],
                ..Default::default()
            },
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(ids(&scores), vec![2, 3]);

    let any = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            filters: LibraryFilters {
                tag_ids: vec![fixture.tag_reference],
                tag_mode: TagMode::Any,
                ..Default::default()
            },
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(ids(&any), vec![4, 1]);

    let all = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            filters: LibraryFilters {
                tag_ids: vec![fixture.tag_sketch, fixture.tag_reference],
                tag_mode: TagMode::All,
                ..Default::default()
            },
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(ids(&all), vec![1]);
}

#[test]
fn filters_by_tag_presence_and_exclusions() {
    let fixture = fixture();

    let has = all_ids(
        LibraryQuery {
            filters: LibraryFilters {
                tag_presence: PresenceFilter::Has,
                ..Default::default()
            },
            ..Default::default()
        },
        &fixture,
    );
    assert_eq!(has, vec![4, 1, 2]);

    let missing = all_ids(
        LibraryQuery {
            filters: LibraryFilters {
                tag_presence: PresenceFilter::Missing,
                ..Default::default()
            },
            ..Default::default()
        },
        &fixture,
    );
    assert_eq!(missing, vec![3]);

    // 排除标签：剩下“没有速写”的图片。
    let excluded = all_ids(
        LibraryQuery {
            filters: LibraryFilters {
                exclude_tag_ids: vec![fixture.tag_sketch],
                ..Default::default()
            },
            ..Default::default()
        },
        &fixture,
    );
    assert_eq!(excluded, vec![4, 3]);

    // 包含与排除叠加：有速写、但没有参考 → 只剩 2。
    let combined = all_ids(
        LibraryQuery {
            filters: LibraryFilters {
                tag_ids: vec![fixture.tag_sketch],
                exclude_tag_ids: vec![fixture.tag_reference],
                ..Default::default()
            },
            ..Default::default()
        },
        &fixture,
    );
    assert_eq!(combined, vec![2]);

    // “没有任何标签”与“包含某些标签”互斥。
    let conflict = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            filters: LibraryFilters {
                tag_presence: PresenceFilter::Missing,
                tag_ids: vec![fixture.tag_sketch],
                ..Default::default()
            },
            ..base_query()
        },
    );
    assert!(conflict.is_err());
}

#[test]
fn lists_tags_with_offset_and_total() {
    let fixture = fixture();
    let total = count_tags(&fixture.session.db, None).unwrap();
    assert_eq!(total, 2);

    let first = list_tags_page(&fixture.session.db, None, 1, 0).unwrap();
    assert_eq!(first.len(), 1);
    let second = list_tags_page(&fixture.session.db, None, 1, 1).unwrap();
    assert_eq!(second.len(), 1);
    assert_ne!(first[0].id, second[0].id);
    assert!(
        list_tags_page(&fixture.session.db, None, 1, 2)
            .unwrap()
            .is_empty()
    );

    // 搜索路径与全量路径口径一致，也按 offset 切片。
    let searched = count_tags(&fixture.session.db, Some("速")).unwrap();
    assert_eq!(searched, 1);
    let page = list_tags_page(&fixture.session.db, Some("速"), 10, 0).unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].id, fixture.tag_sketch);
}

#[test]
fn sorts_by_score_and_size_and_views_trash() {
    let fixture = fixture();
    let score = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            sort: vec![SortSpec {
                key: SortKey::Score,
                descending: true,
            }],
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(ids(&score), vec![2, 4]);

    let size_ascending = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            sort: vec![SortSpec {
                key: SortKey::Size,
                descending: false,
            }],
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(ids(&size_ascending), vec![1, 2]);

    let trash = all_ids(
        LibraryQuery {
            view: LibraryView::Trash,
            ..base_query()
        },
        &fixture,
    );
    assert_eq!(trash, vec![5]);
}

#[test]
fn folder_filter_includes_descendants_but_not_similar_prefixes() {
    let fixture = fixture();
    let (nested_id, lookalike_id) = fixture
        .session
        .db
        .with_writer(|connection| {
            let nested = ensure_folder(connection, "人物/速写", 1)?;
            let lookalike = ensure_folder(connection, "人物速写", 1)?;
            insert_image(
                connection,
                "f",
                "jpg",
                2,
                nested,
                600,
                6_000,
                600,
                600,
                None,
                &[],
            );
            insert_image(
                connection,
                "g",
                "jpg",
                2,
                lookalike,
                700,
                7_000,
                600,
                600,
                None,
                &[],
            );
            Ok((nested, lookalike))
        })
        .unwrap();

    let by_parent = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            filters: LibraryFilters {
                folder_ids: vec![fixture.folder_people],
                ..Default::default()
            },
            limit: Some(10),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&by_parent), vec![6, 1, 2]);

    let by_nested = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            filters: LibraryFilters {
                folder_ids: vec![nested_id],
                ..Default::default()
            },
            limit: Some(10),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&by_nested), vec![6]);

    let by_lookalike = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            filters: LibraryFilters {
                folder_ids: vec![lookalike_id],
                ..Default::default()
            },
            limit: Some(10),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&by_lookalike), vec![7]);

    let folders = list_folders(&fixture.session.db).unwrap();
    let people = folders
        .iter()
        .find(|item| item.id == fixture.folder_people)
        .unwrap();
    assert_eq!(people.direct_image_count, 2);
    assert_eq!(people.total_image_count, 3);
    let root = folders.iter().find(|item| item.path.is_empty()).unwrap();
    assert_eq!(root.total_image_count, 6);
}

#[test]
fn folder_direct_filter_excludes_descendants_and_handles_root() {
    let fixture = fixture();
    let (_nested_id, _lookalike_id) = fixture
        .session
        .db
        .with_writer(|connection| {
            let nested = ensure_folder(connection, "人物/速写", 1)?;
            let lookalike = ensure_folder(connection, "人物速写", 1)?;
            insert_image(
                connection,
                "f",
                "jpg",
                2,
                nested,
                600,
                6_000,
                600,
                600,
                None,
                &[],
            );
            insert_image(
                connection,
                "g",
                "jpg",
                2,
                lookalike,
                700,
                7_000,
                600,
                600,
                None,
                &[],
            );
            Ok((nested, lookalike))
        })
        .unwrap();

    let direct = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            filters: LibraryFilters {
                folder_ids: vec![fixture.folder_people],
                folder_direct: true,
                ..Default::default()
            },
            limit: Some(10),
            ..base_query()
        },
    )
    .unwrap();
    let mut direct_ids = ids(&direct);
    direct_ids.sort_unstable();
    assert_eq!(direct_ids, vec![1, 2]);

    let counts = filter_counts(
        &fixture.session.db,
        LibraryQuery {
            filters: LibraryFilters {
                folder_ids: vec![fixture.folder_people],
                folder_direct: true,
                ..Default::default()
            },
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(counts.total, 2);

    let root_id = list_folders(&fixture.session.db)
        .unwrap()
        .into_iter()
        .find(|folder| folder.path.is_empty())
        .unwrap()
        .id;
    let root_direct = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            filters: LibraryFilters {
                folder_ids: vec![root_id],
                folder_direct: true,
                ..Default::default()
            },
            limit: Some(10),
            ..base_query()
        },
    )
    .unwrap();
    assert!(root_direct.items.is_empty());
}

#[test]
fn reports_filter_counts_and_lists_folders_and_tags() {
    let fixture = fixture();
    let counts = filter_counts(
        &fixture.session.db,
        LibraryQuery {
            filters: LibraryFilters {
                folder_ids: vec![fixture.folder_people],
                ..Default::default()
            },
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(counts.total, 2);
    assert_eq!(
        counts
            .extensions
            .iter()
            .find(|item| item.value == "png")
            .map(|item| item.count),
        Some(1)
    );
    assert_eq!(
        counts
            .scores
            .iter()
            .find(|item| item.value == "3")
            .map(|item| item.count),
        Some(1)
    );
    let folders = list_folders(&fixture.session.db).unwrap();
    assert_eq!(folders.len(), 3);
    assert_eq!(folders[1].path, "人物");
    assert_eq!(folders[1].direct_image_count, 2);
    assert_eq!(folders[1].total_image_count, 2);

    let tags = list_tags(&fixture.session.db, None).unwrap();
    assert_eq!(tags.len(), 2);
    assert_eq!(tags[0].name, "参考");
    assert_eq!(tags[0].image_count, 2);
}

#[test]
fn keyset_handles_duplicate_sort_keys() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("library");
    let (session, _) = service::create(&root, "游标测试", 1).unwrap();
    session
        .db
        .with_writer(|connection| {
            let folder = ensure_folder(connection, "", 1)?;
            for seed in ["1", "2", "3"] {
                insert_image(
                    connection,
                    seed,
                    "jpg",
                    3,
                    folder,
                    100,
                    100,
                    10,
                    10,
                    None,
                    &[],
                );
            }
            Ok(())
        })
        .unwrap();

    let mut cursor = None;
    let mut collected = Vec::new();
    loop {
        let page = query_page(
            &session.db,
            &root,
            LibraryQuery {
                sort: vec![SortSpec {
                    key: SortKey::Score,
                    descending: true,
                }],
                cursor,
                limit: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        collected.extend(ids(&page));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(collected, vec![3, 2, 1]);
}

#[test]
fn returns_all_matching_ids() {
    let fixture = fixture();
    let ids = query_ids(
        &fixture.session.db,
        LibraryQuery {
            filters: LibraryFilters {
                extensions: vec!["jpg".into()],
                ..Default::default()
            },
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(ids, vec![1, 3]);
}

#[test]
fn searches_description_tags_and_folders() {
    let fixture = fixture();

    let short = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            search: "人物".into(),
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(ids(&short), vec![1, 2]);

    let trigram = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            search: "人物速".into(),
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(ids(&trigram), vec![1]);

    let by_tag = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            search: "速写".into(),
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(ids(&by_tag), vec![1, 2]);

    let by_folder = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            search: "风景".into(),
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(ids(&by_folder), vec![4, 3]);
}

#[test]
fn library_scope_search_includes_trash() {
    let fixture = fixture();
    let in_view = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            search: "速写".into(),
            search_scope: SearchScope::View,
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(ids(&in_view), vec![1, 2]);

    let whole_library = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            search: "速写".into(),
            search_scope: SearchScope::Library,
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(ids(&whole_library), vec![5, 1]);
    assert!(whole_library.items[0].deleted_at.is_some());
}

#[test]
fn filters_by_time_size_and_presence() {
    let fixture = fixture();

    let time = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            filters: LibraryFilters {
                imported_from: Some(100),
                imported_to: Some(300),
                ..Default::default()
            },
            limit: Some(10),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&time), vec![1, 2, 3]);

    let size = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            filters: LibraryFilters {
                size_min: Some(2_000),
                size_max: Some(3_000),
                ..Default::default()
            },
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(ids(&size), vec![2, 3]);

    let with_description = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            filters: LibraryFilters {
                description: PresenceFilter::Has,
                ..Default::default()
            },
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(ids(&with_description), vec![4, 1]);

    let missing_embedding = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            filters: LibraryFilters {
                embedding: PresenceFilter::Missing,
                pose: PresenceFilter::Missing,
                ..Default::default()
            },
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(ids(&missing_embedding), vec![4, 1]);
}

#[test]
fn views_collections_in_manual_order() {
    let fixture = fixture();
    let collection_id = fixture
        .session
        .db
        .with_writer(|connection| {
            connection.execute(
                "INSERT INTO collections(name, sort_order, created_at, updated_at)
                 VALUES ('参考集', 0, 1, 1)",
                [],
            )?;
            let collection_id = connection.last_insert_rowid();
            for (position, image_id) in [3_i64, 1, 4].into_iter().enumerate() {
                connection.execute(
                    "INSERT INTO collection_items(collection_id, image_id, position)
                     VALUES (?1, ?2, ?3)",
                    params![collection_id, image_id, position as i64],
                )?;
            }
            Ok(collection_id)
        })
        .unwrap();

    let page = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            view: LibraryView::Collection,
            collection_id: Some(collection_id),
            sort: vec![SortSpec {
                key: SortKey::CollectionPosition,
                descending: false,
            }],
            limit: Some(10),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&page), vec![3, 1, 4]);

    let counts = filter_counts(
        &fixture.session.db,
        LibraryQuery {
            view: LibraryView::Collection,
            collection_id: Some(collection_id),
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(counts.total, 3);
    assert_eq!(
        counts
            .scores
            .iter()
            .find(|item| item.value == "1")
            .map(|item| item.count),
        Some(1)
    );
}

#[test]
fn search_and_collection_counts_exclude_own_facet() {
    let fixture = fixture();

    let searched = filter_counts(
        &fixture.session.db,
        LibraryQuery {
            search: "参考".into(),
            filters: LibraryFilters {
                folder_ids: vec![fixture.folder_people],
                ..Default::default()
            },
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(searched.total, 2);
    assert_eq!(
        searched
            .tags
            .iter()
            .find(|item| item.tag_id == fixture.tag_sketch)
            .map(|item| item.count),
        Some(2)
    );
    // 标签分面只排除标签筛选，文件夹筛选仍然生效：仅图片 1 在人物文件夹且带“参考”。
    assert_eq!(
        searched
            .tags
            .iter()
            .find(|item| item.tag_id == fixture.tag_reference)
            .map(|item| item.count),
        Some(1)
    );

    let library = filter_counts(
        &fixture.session.db,
        LibraryQuery {
            search: "速写".into(),
            search_scope: SearchScope::Library,
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(library.total, 3);

    let collection_id = fixture
        .session
        .db
        .with_writer(|connection| {
            connection.execute(
                "INSERT INTO collections(name, sort_order, created_at, updated_at)
                 VALUES ('参考集', 0, 1, 1)",
                [],
            )?;
            let collection_id = connection.last_insert_rowid();
            for (position, image_id) in [3_i64, 1, 4].into_iter().enumerate() {
                connection.execute(
                    "INSERT INTO collection_items(collection_id, image_id, position)
                     VALUES (?1, ?2, ?3)",
                    params![collection_id, image_id, position as i64],
                )?;
            }
            Ok(collection_id)
        })
        .unwrap();

    let collection = filter_counts(
        &fixture.session.db,
        LibraryQuery {
            view: LibraryView::Collection,
            collection_id: Some(collection_id),
            filters: LibraryFilters {
                tag_ids: vec![fixture.tag_reference],
                ..Default::default()
            },
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(collection.total, 2);
    // 标签分面排除标签筛选：集合内 1/3/4 的标签关系都要计入。
    assert_eq!(
        collection
            .tags
            .iter()
            .find(|item| item.tag_id == fixture.tag_reference)
            .map(|item| item.count),
        Some(2)
    );
    assert_eq!(
        collection
            .tags
            .iter()
            .find(|item| item.tag_id == fixture.tag_sketch)
            .map(|item| item.count),
        Some(1)
    );
}

#[test]
fn rejects_invalid_query_parts() {
    let fixture = fixture();
    let result = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            view: LibraryView::Collection,
            ..base_query()
        },
    );
    assert!(result.is_err());

    let result = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            collection_id: Some(1),
            ..base_query()
        },
    );
    assert!(result.is_err());

    let result = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            sort: vec![SortSpec {
                key: SortKey::CollectionPosition,
                descending: false,
            }],
            ..base_query()
        },
    );
    assert!(result.is_err());

    let result = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            filters: LibraryFilters {
                imported_from: Some(10),
                imported_to: Some(5),
                ..Default::default()
            },
            ..base_query()
        },
    );
    assert!(result.is_err());
}

#[test]
fn multi_level_sort_pages_with_duplicates_across_six_levels() {
    let fixture = fixture();
    let (folder_a, folder_b) = fixture
        .session
        .db
        .with_writer(|connection| {
            Ok((
                ensure_folder(connection, "排序测试", 1)?,
                ensure_folder(connection, "排序测试二", 1)?,
            ))
        })
        .unwrap();
    let rows = [
        ("m1", 2, 100, 5, 10, 10, "排序测试"),
        ("m2", 2, 100, 5, 10, 10, "排序测试二"),
        ("m3", 2, 100, 5, 20, 5, "排序测试"),
        ("m4", 3, 50, 6, 10, 10, "排序测试二"),
        ("m5", 1, 200, 7, 10, 10, "排序测试"),
        ("m6", 2, 100, 4, 10, 10, "排序测试二"),
        ("m7", 3, 50, 6, 10, 10, "排序测试"),
        ("m8", 2, 100, 5, 5, 20, "排序测试二"),
        ("m9", 0, 100, 5, 10, 10, "排序测试"),
        ("m10", 5, 1, 1, 1, 1, "排序测试二"),
        ("m11", 2, 100, 5, 10, 10, "排序测试"),
        ("m12", 4, 100, 3, 16, 16, "排序测试二"),
    ];
    let mut expected: Vec<(i64, i64, i64, i64, i64, String)> = Vec::new();
    fixture
        .session
        .db
        .with_writer(|connection| {
            for (seed, score, size, imported, width, height, path) in rows {
                let folder = if path == "排序测试" {
                    folder_a
                } else {
                    folder_b
                };
                insert_image(
                    connection,
                    seed,
                    "jpg",
                    score,
                    folder,
                    size,
                    imported,
                    width,
                    height,
                    None,
                    &[],
                );
                let id = connection.last_insert_rowid();
                expected.push((id, score, size, imported, width * height, path.to_owned()));
            }
            Ok(())
        })
        .unwrap();

    // 六级：评分↓、大小↑、导入↑、像素↑、修改时间↓、文件夹↓。
    let sort = vec![
        SortSpec {
            key: SortKey::Score,
            descending: true,
        },
        SortSpec {
            key: SortKey::Size,
            descending: false,
        },
        SortSpec {
            key: SortKey::ImportedAt,
            descending: false,
        },
        SortSpec {
            key: SortKey::Pixels,
            descending: false,
        },
        SortSpec {
            key: SortKey::ModifiedAt,
            descending: true,
        },
        SortSpec {
            key: SortKey::Folder,
            descending: true,
        },
    ];
    expected.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then(left.2.cmp(&right.2))
            .then(left.3.cmp(&right.3))
            .then(left.4.cmp(&right.4))
            .then(right.3.cmp(&left.3))
            .then(right.5.cmp(&left.5))
            .then(right.0.cmp(&left.0))
    });
    let expected: Vec<i64> = expected.into_iter().map(|row| row.0).collect();

    let mut page_token = None;
    let mut collected = Vec::new();
    loop {
        let page = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                filters: LibraryFilters {
                    folder_ids: vec![folder_a, folder_b],
                    ..Default::default()
                },
                sort: sort.clone(),
                page_token: page_token.clone(),
                limit: Some(3),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(page.next_cursor.is_none());
        collected.extend(ids(&page));
        match page.next_page_token {
            Some(next) => page_token = Some(next),
            None => break,
        }
    }
    assert_eq!(collected.len(), rows.len());
    assert_eq!(collected, expected);
}

#[test]
fn folder_sort_uses_id_cursor_and_orders_by_path() {
    let fixture = fixture();
    let query = LibraryQuery {
        sort: vec![SortSpec {
            key: SortKey::Folder,
            descending: false,
        }],
        limit: Some(2),
        ..Default::default()
    };
    let mut page_token = None;
    let mut paths = Vec::new();
    loop {
        let page = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                sort: query.sort.clone(),
                page_token: page_token.clone(),
                limit: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(page.next_cursor.is_none());
        paths.extend(page.items.iter().map(|item| item.folder_path.clone()));
        match page.next_page_token {
            Some(next) => page_token = Some(next),
            None => break,
        }
    }
    // 第 5 张在回收站，活动视图只返回 4 张。
    assert_eq!(paths.len(), 4);
    assert_eq!(paths.first().map(String::as_str), Some("人物"));
    assert!(paths.windows(2).all(|pair| pair[0] <= pair[1]));
}

#[test]
fn anchor_page_starts_at_the_requested_image() {
    let fixture = fixture();
    let page = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            filters: LibraryFilters {
                folder_ids: vec![fixture.folder_people],
                ..Default::default()
            },
            sort: vec![SortSpec {
                key: SortKey::Score,
                descending: false,
            }],
            anchor_id: Some(1),
            limit: Some(10),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(page.items.first().map(|item| item.id), Some(1));
    assert!(page.items.iter().any(|item| item.id == 2));
}

#[test]
fn rejects_invalid_sort_specs() {
    let fixture = fixture();
    let spec = |key: SortKey| SortSpec {
        key,
        descending: true,
    };
    let page = |query: LibraryQuery| query_page(&fixture.session.db, &fixture.root, query);

    // 超过指标数量的级数。
    let too_many = vec![
        spec(SortKey::ImportedAt),
        spec(SortKey::ModifiedAt),
        spec(SortKey::Score),
        spec(SortKey::Size),
        spec(SortKey::Pixels),
        spec(SortKey::Folder),
        spec(SortKey::CollectionPosition),
        spec(SortKey::Score),
    ];
    let error = page(LibraryQuery {
        sort: too_many,
        ..base_query()
    })
    .unwrap_err();
    assert!(error.to_string().contains("排序最多"));

    // 同一指标重复。
    let error = page(LibraryQuery {
        sort: vec![spec(SortKey::Score), spec(SortKey::Score)],
        ..base_query()
    })
    .unwrap_err();
    assert!(error.to_string().contains("不能重复"));

    // 键集游标的排序值数量与级数不一致。
    let error = page(LibraryQuery {
        sort: vec![spec(SortKey::Score), spec(SortKey::Size)],
        cursor: Some(QueryCursor {
            sort_values: vec!["3".into()],
            id: 1,
        }),
        ..base_query()
    })
    .unwrap_err();
    assert!(error.to_string().contains("游标排序值"));

    // 锚点与分页 token 的格式。
    assert!(
        page(LibraryQuery {
            anchor_id: Some(0),
            ..base_query()
        })
        .is_err()
    );
    assert!(
        page(LibraryQuery {
            page_token: Some("missing-offset".into()),
            ..base_query()
        })
        .is_err()
    );
}

#[test]
fn purge_marked_rows_are_hidden_everywhere() {
    let fixture = fixture();
    fixture
        .session
        .db
        .with_writer(|connection| {
            connection.execute(
                "UPDATE images SET description = '已释放', purge_operation_id = 99 WHERE id = 5",
                [],
            )?;
            Ok(())
        })
        .unwrap();

    // 普通视图不额外加 `purge_operation_id` 条件，靠 `deleted_at IS NULL` 挡住待释放行。
    let all = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            view: LibraryView::All,
            limit: Some(10),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(all.items.iter().all(|row| row.id != 5));

    let trash = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            view: LibraryView::Trash,
            limit: Some(10),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(trash.items.is_empty());

    // 全库搜索包含回收站，但不应重新显示已从回收站移除的待释放行。
    let search = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            search: "已释放".into(),
            search_scope: SearchScope::Library,
            limit: Some(10),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(search.items.is_empty());

    let rows = rows_by_ids(&fixture.session.db, &fixture.root, &[5, 1]).unwrap();
    assert_eq!(rows.iter().map(|row| row.id).collect::<Vec<_>>(), vec![1]);

    let (_, trash_count, _) = fixture.session.db.counts().unwrap();
    assert_eq!(trash_count, 0);
}

#[test]
fn temp_images_are_hidden_from_normal_views_and_searches() {
    let fixture = fixture();
    let (temp_folder, temp_image) = fixture
        .session
        .db
        .with_writer(|connection| {
            let folder = temp::folder(connection)?.unwrap().id;
            connection.execute("INSERT INTO tags(name) VALUES ('临时标签')", [])?;
            let tag = connection.last_insert_rowid();
            insert_image(
                connection,
                "f",
                "jpg",
                0,
                folder,
                600,
                6_000,
                700,
                700,
                None,
                &[tag],
            );
            let image: i64 =
                connection.query_row("SELECT max(id) FROM images", [], |row| row.get(0))?;
            Ok((folder, image))
        })
        .unwrap();
    let registered = fixture
        .session
        .db
        .with_reader(temp::folder)
        .unwrap()
        .unwrap();
    assert_eq!(temp_folder, registered.id);

    let all = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            view: LibraryView::All,
            limit: Some(10),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&all).len(), 4);
    assert!(all.items.iter().all(|row| row.id != temp_image));

    let temp_page = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            view: LibraryView::Temp,
            limit: Some(10),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&temp_page), vec![temp_image]);

    let search = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            search: "临时标签".into(),
            search_scope: SearchScope::Library,
            limit: Some(10),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(search.items.is_empty());

    // 临时视图里的“视图内搜索”能看到临时图片，但同一视图切到全库搜索时不算临时图片。
    let in_temp = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            view: LibraryView::Temp,
            search: "临时标签".into(),
            search_scope: SearchScope::View,
            limit: Some(10),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&in_temp), vec![temp_image]);
    let library_wide = query_page(
        &fixture.session.db,
        &fixture.root,
        LibraryQuery {
            view: LibraryView::Temp,
            search: "临时标签".into(),
            search_scope: SearchScope::Library,
            limit: Some(10),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(library_wide.items.is_empty());

    let tags = list_tags(&fixture.session.db, None).unwrap();
    assert!(!tags.iter().any(|tag| tag.name == "临时标签"));

    let (_, _, temp_count) = fixture.session.db.counts().unwrap();
    assert_eq!(temp_count, 1);
}

/// 标签管理器要看到 0 张标签；默认查询仍然过滤掉它们。
#[test]
fn include_empty_lists_zero_count_tags() {
    let fixture = fixture();
    fixture
        .session
        .db
        .with_writer(|connection| {
            connection.execute("INSERT INTO tags(name) VALUES ('空标签')", [])?;
            Ok(())
        })
        .unwrap();

    let visible = list_tags(&fixture.session.db, None).unwrap();
    assert!(
        !visible.iter().any(|tag| tag.name == "空标签"),
        "默认口径不返回 0 张标签"
    );

    let all = list_tags_with_empty(&fixture.session.db, None, true).unwrap();
    let empty = all.iter().find(|tag| tag.name == "空标签").unwrap();
    assert_eq!(empty.image_count, 0);
    // 有图片的标签仍在，且顺序按图片数降序。
    assert!(all.len() > visible.len());
    assert!(all[0].image_count >= all[all.len() - 1].image_count);

    let searched = search_tags_with_empty(&fixture.session.db, "空", Some(50), true).unwrap();
    assert_eq!(searched.len(), 1);
    assert_eq!(searched[0].name, "空标签");
    assert!(
        search_tags(&fixture.session.db, "空", Some(50))
            .unwrap()
            .is_empty()
    );
}

/// 标签分页换形状后必须与旧 SQL 逐行一致：回收站、临时区、两者都有、死亡关系超阈值。
#[test]
fn list_tags_page_matches_legacy_shape() {
    let fixture = fixture();
    let compare = |database: &LibraryDb| {
        database
            .with_reader(|connection| {
                let fast = list_tags_page_in(connection, None, 100, 0, MAX_TAG_DEAD_IMAGES)?;
                let legacy = list_tags_page_legacy(connection, None, 100, 0)?;
                assert_eq!(fast.len(), legacy.len());
                for (left, right) in fast.iter().zip(legacy.iter()) {
                    assert_eq!(
                        (left.id, left.name.as_str(), left.image_count),
                        (right.id, right.name.as_str(), right.image_count)
                    );
                }
                Ok(())
            })
            .unwrap()
    };

    // fixture 里 id=5 已在回收站，且它带着两个标签关系。
    compare(&fixture.session.db);

    // 再加一张临时图片与它的标签：临时图片不算标签计数。
    fixture
        .session
        .db
        .with_writer(|connection| {
            let folder = temp::folder(connection)?.unwrap().id;
            connection.execute("INSERT INTO tags(name) VALUES ('临时标签')", [])?;
            let tag = connection.last_insert_rowid();
            insert_image(
                connection,
                "f",
                "jpg",
                0,
                folder,
                600,
                6_000,
                700,
                700,
                None,
                &[tag],
            );
            Ok(())
        })
        .unwrap();
    compare(&fixture.session.db);

    // 死亡关系数超过阈值时回退旧形状，结果必须一致。
    fixture
        .session
        .db
        .with_reader(|connection| {
            let fast = list_tags_page_in(connection, None, 100, 0, MAX_TAG_DEAD_IMAGES)?;
            let fallback = list_tags_page_in(connection, None, 100, 0, 0)?;
            assert_eq!(
                fast.iter()
                    .map(|tag| (tag.id, tag.image_count))
                    .collect::<Vec<_>>(),
                fallback
                    .iter()
                    .map(|tag| (tag.id, tag.image_count))
                    .collect::<Vec<_>>()
            );
            Ok(())
        })
        .unwrap();
}

/// 名称搜索与分页也必须与旧形状一致。
#[test]
fn list_tags_page_search_and_offset_match_legacy_shape() {
    let fixture = fixture();
    fixture
        .session
        .db
        .with_reader(|connection| {
            for (search, limit, offset) in [(None, 1, 0), (None, 1, 1), (Some("速写"), 100, 0)] {
                let fast =
                    list_tags_page_in(connection, search, limit, offset, MAX_TAG_DEAD_IMAGES)?;
                let legacy = list_tags_page_legacy(connection, search, limit, offset)?;
                assert_eq!(
                    fast.iter()
                        .map(|tag| (tag.id, tag.image_count))
                        .collect::<Vec<_>>(),
                    legacy
                        .iter()
                        .map(|tag| (tag.id, tag.image_count))
                        .collect::<Vec<_>>(),
                    "search={search:?} limit={limit} offset={offset}"
                );
            }
            Ok(())
        })
        .unwrap();
}

/// 文件夹写命令只回传受影响的 id，计数口径必须与整树一致。
#[test]
fn folder_infos_returns_only_the_requested_ids() {
    let fixture = fixture();
    let all = list_folders(&fixture.session.db).unwrap();
    let subset = folder_infos(&fixture.session.db, &[fixture.folder_people, -1]).unwrap();
    assert_eq!(subset.len(), 1);
    let expected = all
        .iter()
        .find(|folder| folder.id == fixture.folder_people)
        .unwrap();
    assert_eq!(subset[0].path, expected.path);
    assert_eq!(subset[0].direct_image_count, expected.direct_image_count);
    assert_eq!(subset[0].total_image_count, expected.total_image_count);
    assert!(folder_infos(&fixture.session.db, &[]).unwrap().is_empty());
}

/// 文件夹计数聚合不能退回“按像素索引逐行回表”。
#[test]
fn folder_counts_plan_avoids_per_row_indexes() {
    let fixture = fixture();
    let plan = fixture
        .session
        .db
        .with_reader(|connection| {
            let mut statement =
                connection.prepare(&format!("EXPLAIN QUERY PLAN {FOLDER_COUNTS_SQL}"))?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(3))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows.join("\n"))
        })
        .unwrap();
    assert!(
        !plan.contains("idx_images_active"),
        "文件夹计数仍在逐行回表：{plan}"
    );
}

fn folder_query(folder_ids: Vec<i64>) -> LibraryQuery {
    LibraryQuery {
        filters: LibraryFilters {
            folder_ids,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn explain(connection: &Connection, sql: &str, params: &[Value]) -> String {
    let mut statement = connection
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap();
    let rows = statement
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            row.get::<_, String>(3)
        })
        .unwrap();
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("\n")
}

#[test]
fn folder_stats_match_counts_bytes_and_distributions() {
    let fixture = fixture();
    let query = folder_query(vec![fixture.folder_people]);
    let stats = folder_stats(&fixture.session.db, query.clone()).unwrap();
    let counts = filter_counts(&fixture.session.db, query).unwrap();

    assert_eq!(stats.image_count, counts.total);
    assert_eq!(stats.image_count, 2, "回收站里的 e 不计入");
    assert_eq!(stats.total_bytes, 3_000);
    assert_eq!(stats.subfolder_count, 0);
    assert_eq!(
        stats.extensions,
        vec![
            ValueCount {
                value: "jpg".into(),
                count: 1
            },
            ValueCount {
                value: "png".into(),
                count: 1
            }
        ]
    );
    let scores: Vec<(String, u64)> = stats
        .scores
        .into_iter()
        .map(|item| (item.value, item.count))
        .collect();
    assert_eq!(scores, vec![("5".into(), 1), ("3".into(), 1)]);
    let tags: Vec<(String, u64)> = stats
        .tags
        .into_iter()
        .map(|tag| (tag.name, tag.count))
        .collect();
    assert_eq!(tags, vec![("速写".into(), 2), ("参考".into(), 1)]);
}

#[test]
fn folder_stats_handle_children_direct_mode_and_union() {
    let fixture = fixture();
    let child = fixture
        .session
        .db
        .with_writer(|connection| {
            let child = ensure_folder(connection, "人物/速写", 1)?;
            insert_image(
                connection,
                "f",
                "jpg",
                2,
                child,
                700,
                600,
                800,
                800,
                None,
                &[],
            );
            Ok(child)
        })
        .unwrap();

    let subtree = folder_stats(
        &fixture.session.db,
        folder_query(vec![fixture.folder_people]),
    )
    .unwrap();
    assert_eq!(subtree.image_count, 3);
    assert_eq!(subtree.total_bytes, 3_700);
    assert_eq!(subtree.subfolder_count, 1);

    let mut direct = folder_query(vec![fixture.folder_people]);
    direct.filters.folder_direct = true;
    let direct = folder_stats(&fixture.session.db, direct).unwrap();
    assert_eq!(direct.image_count, 2);
    assert_eq!(direct.total_bytes, 3_000);
    assert_eq!(direct.subfolder_count, 1);

    let union = folder_stats(
        &fixture.session.db,
        folder_query(vec![fixture.folder_people, child]),
    )
    .unwrap();
    assert_eq!(union.image_count, 3, "嵌套选择不能重复计数");
    assert_eq!(union.subfolder_count, 0, "选中的子文件夹不算子文件夹");
}

#[test]
fn folder_stats_library_search_excludes_trash() {
    let fixture = fixture();
    fixture
        .session
        .db
        .with_writer(|connection| {
            connection.execute(
                "UPDATE images SET description = '人物速写' WHERE id = 5",
                [],
            )?;
            Ok(())
        })
        .unwrap();

    let query = LibraryQuery {
        search: "人物速写".into(),
        search_scope: SearchScope::Library,
        filters: LibraryFilters {
            folder_ids: vec![fixture.folder_people],
            ..Default::default()
        },
        ..Default::default()
    };
    let stats = folder_stats(&fixture.session.db, query.clone()).unwrap();
    let counts = filter_counts(&fixture.session.db, query).unwrap();
    assert_eq!(counts.total, 2, "列表命中回收站里的 e");
    assert_eq!(stats.image_count, 1, "统计始终排除回收站");
}

#[test]
fn folder_stats_limit_tags_to_eight() {
    let fixture = fixture();
    fixture
        .session
        .db
        .with_writer(|connection| {
            for name in ["甲", "乙", "丙", "丁", "戊", "己", "庚", "辛"] {
                connection.execute("INSERT INTO tags(name) VALUES (?1)", [name])?;
                let tag_id = connection.last_insert_rowid();
                connection.execute(
                    "INSERT INTO image_tags(image_id, tag_id) VALUES (1, ?1)",
                    [tag_id],
                )?;
            }
            Ok(())
        })
        .unwrap();

    let stats = folder_stats(
        &fixture.session.db,
        folder_query(vec![fixture.folder_people]),
    )
    .unwrap();
    assert_eq!(stats.tags.len(), 8);
    assert_eq!(stats.tags[0].name, "速写");
    assert_eq!(stats.tags[0].count, 2);
}

#[test]
fn folder_stats_use_covering_indexes() {
    let fixture = fixture();
    let query = folder_query(vec![fixture.folder_people]);
    let (sum_plan, score_plan) = fixture
        .session
        .db
        .with_reader(|connection| {
            let folder_paths = folder_scope_paths(connection, &query.filters.folder_ids)?;
            let prepared = PreparedQuery::new(&query, None, SearchMode::Count, &folder_paths);
            assert!(pin_stats_indexes(&query));
            let (sum_sql, sum_params) = sum_sql(&prepared, Some("idx_images_visible_bytes"));
            let (score_sql, score_params) = grouped_sql(
                &prepared,
                Some("idx_images_visible_facets"),
                "images.score, count(*)",
                "GROUP BY images.score",
            );
            Ok((
                explain(connection, &sum_sql, &sum_params),
                explain(connection, &score_sql, &score_params),
            ))
        })
        .unwrap();
    assert!(
        sum_plan.contains("idx_images_visible_bytes"),
        "总字节没有走覆盖索引：{sum_plan}"
    );
    assert!(
        score_plan.contains("idx_images_visible_facets"),
        "评分分组没有走分面索引：{score_plan}"
    );
}

#[test]
fn pin_stats_indexes_policy() {
    let mut query = folder_query(vec![1]);
    assert!(pin_stats_indexes(&query));

    query.search = "人物".into();
    assert!(!pin_stats_indexes(&query));

    query.search.clear();
    query.view = LibraryView::Trash;
    assert!(!pin_stats_indexes(&query));

    query.view = LibraryView::All;
    query.filters.size_min = Some(1);
    assert!(!pin_stats_indexes(&query));

    query.filters.size_min = None;
    query.filters.tag_ids = vec![1];
    assert!(pin_stats_indexes(&query), "标签条件仍可由分面索引覆盖");

    query.filters.description = PresenceFilter::Has;
    assert!(!pin_stats_indexes(&query));
}

fn set_thumbnail(fixture: &Fixture, id: i64, segment: i64, length: i64) {
    fixture
        .session
        .db
        .with_writer(|connection| {
            connection.execute(
                "UPDATE images SET thumb_segment = ?1, thumb_length = ?2 WHERE id = ?3",
                params![segment, length, id],
            )?;
            Ok(())
        })
        .unwrap();
}

fn insert_feature(connection: &Connection, image_id: i64, kind: &str, model: &str, dim: u32) {
    connection
        .execute(
            "INSERT INTO feature_blobs(
                image_id, kind, model, dim, pack_name, pack_offset, pack_length, updated_at
             ) VALUES (?1, ?2, ?3, ?4, 'test.pack', 0, 0, 0)",
            params![image_id, kind, model, dim],
        )
        .unwrap();
}

#[test]
fn pending_stats_follow_scope_and_thumbnail_pointer() {
    let fixture = fixture();
    let database = &fixture.session.db;

    // 全库：5 行里 1 行在回收站；回收站视图的“将处理”必须是 0。
    let stats = pending_stats(database, LibraryQuery::default(), &PendingFilter::All).unwrap();
    assert_eq!(
        stats,
        PendingStats {
            total: 4,
            will_process: 4
        }
    );
    let trash = pending_stats(
        database,
        LibraryQuery {
            view: LibraryView::Trash,
            ..Default::default()
        },
        &PendingFilter::All,
    )
    .unwrap();
    assert_eq!(
        trash,
        PendingStats {
            total: 1,
            will_process: 0
        }
    );

    let stats =
        pending_stats(database, LibraryQuery::default(), &PendingFilter::Thumbnail).unwrap();
    assert_eq!(stats.will_process, 4);

    // 指针判定与 `thumbs::load_pointer` 一致：segment 与 length 都必须有值。
    set_thumbnail(&fixture, 1, 1, 100);
    set_thumbnail(&fixture, 2, 1, 0);
    set_thumbnail(&fixture, 3, 0, 50);
    let stats =
        pending_stats(database, LibraryQuery::default(), &PendingFilter::Thumbnail).unwrap();
    assert_eq!(stats.total, 4);
    assert_eq!(stats.will_process, 3, "只有 id=1 同时有 segment 与 length");

    let direct = LibraryQuery {
        filters: LibraryFilters {
            folder_ids: vec![fixture.folder_people],
            folder_direct: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let stats = pending_stats(database, direct, &PendingFilter::All).unwrap();
    assert_eq!(
        stats,
        PendingStats {
            total: 2,
            will_process: 2
        }
    );

    let subtree = LibraryQuery {
        filters: LibraryFilters {
            folder_ids: vec![fixture.folder_scenery],
            ..Default::default()
        },
        ..Default::default()
    };
    let stats = pending_stats(database, subtree, &PendingFilter::All).unwrap();
    assert_eq!(stats.total, 2);
}

#[test]
fn pending_embedding_binds_model_and_dim_in_order() {
    let fixture = fixture();
    fixture
        .session
        .db
        .with_writer(|connection| {
            insert_feature(connection, 1, "embedding", "alpha", 4);
            insert_feature(connection, 2, "embedding", "alpha", 8);
            insert_feature(connection, 3, "embedding", "beta", 4);
            Ok(())
        })
        .unwrap();
    let database = &fixture.session.db;

    let alpha = pending_stats(
        database,
        LibraryQuery::default(),
        &PendingFilter::Embedding {
            model: "alpha".into(),
            dim: 4,
        },
    )
    .unwrap();
    assert_eq!(
        alpha,
        PendingStats {
            total: 4,
            will_process: 3
        }
    );
    let alpha8 = pending_stats(
        database,
        LibraryQuery::default(),
        &PendingFilter::Embedding {
            model: "alpha".into(),
            dim: 8,
        },
    )
    .unwrap();
    assert_eq!(alpha8.will_process, 3);
    let beta = pending_stats(
        database,
        LibraryQuery::default(),
        &PendingFilter::Embedding {
            model: "beta".into(),
            dim: 4,
        },
    )
    .unwrap();
    assert_eq!(beta.will_process, 3);

    fixture
        .session
        .db
        .with_writer(|connection| {
            insert_feature(connection, 2, "pose", "any-model", 33);
            Ok(())
        })
        .unwrap();
    let pose = pending_stats(database, LibraryQuery::default(), &PendingFilter::Pose).unwrap();
    assert_eq!(pose.will_process, 3, "姿势不按模型判断");
}

#[test]
fn pending_description_respects_trim_and_source() {
    let fixture = fixture();
    fixture
        .session
        .db
        .with_writer(|connection| {
            connection.execute(
                "UPDATE images SET description = '手写描述', description_source = 'user'
                 WHERE id = 1",
                [],
            )?;
            connection.execute(
                "UPDATE images SET description = '导入描述', description_source = 'import'
                 WHERE id = 2",
                [],
            )?;
            connection.execute(
                "UPDATE images SET description = '', description_source = 'ai' WHERE id = 3",
                [],
            )?;
            connection.execute(
                "UPDATE images SET description = '   ', description_source = 'user'
                 WHERE id = 4",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let database = &fixture.session.db;

    let fill = pending_stats(
        database,
        LibraryQuery::default(),
        &PendingFilter::Description { overwrite: false },
    )
    .unwrap();
    assert_eq!(fill.will_process, 2, "只补空描述");
    let redo = pending_stats(
        database,
        LibraryQuery::default(),
        &PendingFilter::Description { overwrite: true },
    )
    .unwrap();
    assert_eq!(redo.will_process, 2, "重做时跳过手写描述");
}

#[test]
fn selected_pending_excludes_deleted_and_keeps_input_order() {
    let fixture = fixture();
    let database = &fixture.session.db;
    // id=5 在回收站；999 不存在；3 重复。
    let ids = [3, 1, 3, 999, 5];
    let stats = selected_pending_stats(database, &ids, &PendingFilter::All).unwrap();
    assert_eq!(
        stats,
        PendingStats {
            total: 3,
            will_process: 2
        }
    );
    let pending = selected_pending_ids(database, &ids, &PendingFilter::All).unwrap();
    assert_eq!(pending, vec![3, 1]);

    assert_eq!(
        selected_pending_stats(database, &[], &PendingFilter::All).unwrap(),
        PendingStats {
            total: 0,
            will_process: 0
        }
    );
    assert!(selected_pending_ids(database, &[0], &PendingFilter::All).is_err());
}

#[test]
fn selected_pending_embedding_binds_json_before_model_and_dim() {
    let fixture = fixture();
    fixture
        .session
        .db
        .with_writer(|connection| {
            insert_feature(connection, 1, "embedding", "alpha", 4);
            Ok(())
        })
        .unwrap();
    let stats = selected_pending_stats(
        &fixture.session.db,
        &[1, 2],
        &PendingFilter::Embedding {
            model: "alpha".into(),
            dim: 4,
        },
    )
    .unwrap();
    assert_eq!(
        stats,
        PendingStats {
            total: 2,
            will_process: 1
        }
    );
    let pending = selected_pending_ids(
        &fixture.session.db,
        &[2, 1],
        &PendingFilter::Embedding {
            model: "alpha".into(),
            dim: 4,
        },
    )
    .unwrap();
    assert_eq!(pending, vec![2]);
}

#[test]
fn selected_and_scope_pending_agree() {
    let fixture = fixture();
    set_thumbnail(&fixture, 1, 1, 100);
    let query = LibraryQuery {
        filters: LibraryFilters {
            folder_ids: vec![fixture.folder_people],
            ..Default::default()
        },
        ..Default::default()
    };
    let database = &fixture.session.db;
    let ids = query_ids(database, query.clone()).unwrap();
    let stats = pending_stats(database, query, &PendingFilter::Thumbnail).unwrap();
    let selected = selected_pending_ids(database, &ids, &PendingFilter::Thumbnail).unwrap();
    assert_eq!(stats.total as usize, ids.len());
    assert_eq!(stats.will_process as usize, selected.len());
}

fn insert_collection(fixture: &Fixture, image_ids: &[i64]) -> i64 {
    fixture
        .session
        .db
        .with_writer(|connection| {
            connection.execute(
                "INSERT INTO collections(name, created_at, updated_at) VALUES ('测试', 0, 0)",
                [],
            )?;
            let id = connection.last_insert_rowid();
            for (position, image_id) in image_ids.iter().enumerate() {
                connection.execute(
                    "INSERT INTO collection_items(collection_id, image_id, position)
                     VALUES (?1, ?2, ?3)",
                    params![id, image_id, position as i64],
                )?;
            }
            Ok(id)
        })
        .unwrap()
}

#[test]
fn pending_stats_support_collection_scope() {
    let fixture = fixture();
    let collection_id = insert_collection(&fixture, &[4, 2]);
    let stats = pending_stats(
        &fixture.session.db,
        LibraryQuery {
            view: LibraryView::Collection,
            collection_id: Some(collection_id),
            ..Default::default()
        },
        &PendingFilter::All,
    )
    .unwrap();
    assert_eq!(
        stats,
        PendingStats {
            total: 2,
            will_process: 2
        }
    );
}

#[test]
fn pending_stats_bind_pending_join_and_filter_params_in_order() {
    let fixture = fixture();
    let collection_id = insert_collection(&fixture, &[1, 2]);
    fixture
        .session
        .db
        .with_writer(|connection| {
            insert_feature(connection, 1, "embedding", "alpha", 4);
            Ok(())
        })
        .unwrap();
    // 集合里 id=1（评分 3、已有 embedding）与 id=2（评分 5）；评分筛选只要 3。
    // 三组参数按 SQL 文本顺序拼接：pending 的模型与维度、集合 id、评分筛选。
    let stats = pending_stats(
        &fixture.session.db,
        LibraryQuery {
            view: LibraryView::Collection,
            collection_id: Some(collection_id),
            filters: LibraryFilters {
                scores: vec![3],
                ..Default::default()
            },
            ..Default::default()
        },
        &PendingFilter::Embedding {
            model: "alpha".into(),
            dim: 4,
        },
    )
    .unwrap();
    assert_eq!(
        stats,
        PendingStats {
            total: 1,
            will_process: 0
        }
    );
}

#[test]
fn group_keeps_picks_first_row_by_sort() {
    let fixture = fixture();
    let database = &fixture.session.db;
    // 夹具：id=1 评分 3、导入 300；id=2 评分 5、导入 200；id=3 评分 1、导入 100。
    let groups = vec![vec![1, 2], vec![3, 1]];

    let keeps = group_keeps(database, &groups, &DEFAULT_KEEP_SORT).unwrap();
    assert_eq!(keeps, vec![Some(2), Some(1)]);

    // 换成导入时间升序：第二组保留最早的 id=3。
    let sort = [SortSpec {
        key: SortKey::ImportedAt,
        descending: false,
    }];
    let keeps = group_keeps(database, &groups, &sort).unwrap();
    assert_eq!(keeps, vec![Some(2), Some(3)]);

    // 回收站里的图片不参与挑选。
    database
        .with_writer(|connection| {
            connection.execute("UPDATE images SET deleted_at = 1 WHERE id = 2", [])?;
            Ok(())
        })
        .unwrap();
    let keeps = group_keeps(database, &groups, &DEFAULT_KEEP_SORT).unwrap();
    assert_eq!(keeps, vec![Some(1), Some(1)]);

    // 跨集合才有意义的指标不能用来决定保留哪张。
    let bad = [SortSpec {
        key: SortKey::CollectionPosition,
        descending: true,
    }];
    assert!(group_keeps(database, &groups, &bad).is_err());

    // 空规则回落到默认值，而不是报错。
    let keeps = group_keeps(database, &groups, &[]).unwrap();
    assert_eq!(keeps, vec![Some(1), Some(1)]);
}

#[test]
fn trash_view_separates_folder_trash_from_loose_images() {
    let fixture = fixture();
    // fixture 里 id=5 已单独在回收站；整目录移入后 3、4 也进回收站。
    folders::trash(&fixture.session.db, fixture.folder_scenery).unwrap();

    let mut all = all_ids(
        LibraryQuery {
            view: LibraryView::Trash,
            ..base_query()
        },
        &fixture,
    );
    all.sort_unstable();
    assert_eq!(all, vec![3, 4, 5]);

    let mut loose = all_ids(
        LibraryQuery {
            view: LibraryView::Trash,
            filters: LibraryFilters {
                trash_loose: true,
                ..Default::default()
            },
            ..base_query()
        },
        &fixture,
    );
    loose.sort_unstable();
    assert_eq!(loose, vec![5], "直属图片只含原文件夹仍在图库中的回收站图片");

    let mut folder = all_ids(
        LibraryQuery {
            view: LibraryView::Trash,
            filters: LibraryFilters {
                folder_ids: vec![fixture.folder_scenery],
                ..Default::default()
            },
            ..base_query()
        },
        &fixture,
    );
    folder.sort_unstable();
    assert_eq!(folder, vec![3, 4]);

    let trash_folders = list_trash_folders(&fixture.session.db).unwrap();
    assert_eq!(trash_folders.folders.len(), 1);
    assert_eq!(trash_folders.folders[0].path, "风景");
    assert_eq!(trash_folders.folders[0].total_image_count, 2);
    assert_eq!(trash_folders.direct_image_count, 1);
    assert_eq!(trash_folders.total_image_count, 3);
    assert!(
        list_folders(&fixture.session.db)
            .unwrap()
            .iter()
            .all(|folder| folder.path != "风景"),
        "正常文件夹列表不含回收站文件夹"
    );
}

#[test]
fn subtree_counts_survive_mismatched_path_case() {
    let fixture = fixture();
    // 旧数据可能把子路径写成另一种大小写：祖先计数不能因此整段丢失。
    let child = fixture
        .session
        .db
        .with_writer(|connection| {
            connection.execute(
                "INSERT INTO folders(path, created_at, sort_index) VALUES ('人物/速写', 1, 0)",
                [],
            )?;
            connection.execute(
                "INSERT INTO folders(path, created_at, sort_index) VALUES ('人物/速写/手部', 1, 0)",
                [],
            )?;
            Ok(connection.last_insert_rowid())
        })
        .unwrap();
    fixture
        .session
        .db
        .with_writer(|connection| {
            insert_image(
                connection,
                "f",
                "jpg",
                4,
                child,
                4_000,
                400,
                640,
                480,
                None,
                &[],
            );
            Ok(())
        })
        .unwrap();

    let folders = list_folders(&fixture.session.db).unwrap();
    let total = |path: &str| {
        folders
            .iter()
            .find(|folder| folder.path == path)
            .map(|folder| folder.total_image_count)
            .unwrap_or(0)
    };
    assert_eq!(total("人物/速写/手部"), 1);
    assert_eq!(total("人物/速写"), 1, "大小写不同也要算进祖先");
    assert_eq!(total("人物"), 3, "原有 2 张加新增 1 张");

    let stats = folder_stats(
        &fixture.session.db,
        LibraryQuery {
            filters: LibraryFilters {
                folder_ids: vec![fixture.folder_people],
                ..Default::default()
            },
            ..base_query()
        },
    )
    .unwrap();
    assert_eq!(stats.subfolder_count, 2, "子文件夹数按祖先关系算");
}
