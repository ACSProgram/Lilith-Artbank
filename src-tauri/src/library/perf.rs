use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    time::{Duration, Instant},
};

use rusqlite::{Connection, params};

use crate::{
    app::error::AppResult,
    library::{
        metadata,
        query::{self, LibraryFilters, LibraryQuery, QueryCursor, SortKey, SortSpec},
        service,
        stats::{self, StatsProfile, StatsRequest},
    },
};

const IMAGE_COUNT: i64 = 200_000;
const TAG_COUNT: i64 = 200;
const TAGS_PER_IMAGE: i64 = 3;
const PAGE_SIZE: i64 = 150;
/// 每个顶层文件夹下 10×50 个叶子，补足 1 万个嵌套文件夹。
const TREE_FOLDERS: i64 = 10_000;
const TOTAL_FOLDERS: i64 = 1 + 20 + 200 + TREE_FOLDERS;

fn percentile(samples: &[Duration], percentile: f64) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let index = ((sorted.len() as f64 - 1.0) * percentile).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn ms(value: Duration) -> f64 {
    value.as_secs_f64() * 1000.0
}

fn sample_query<T>(
    connection: &Connection,
    sql: &str,
    iterations: usize,
    mut run: impl FnMut(&mut rusqlite::Statement<'_>) -> rusqlite::Result<T>,
) -> AppResult<Vec<Duration>> {
    let mut statement = connection.prepare_cached(sql)?;
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let _ = run(&mut statement)?;
        samples.push(start.elapsed());
    }
    Ok(samples)
}

fn seed(connection: &mut Connection) -> AppResult<Duration> {
    let start = Instant::now();
    crate::library::fixture::seed(connection, &seed_spec(), Path::new(""))?;
    Ok(start.elapsed())
}

/// 20 万行基准的种子形状：2 万/1 万文件夹树、200 个标签、每图 3 个标签。
fn seed_spec() -> crate::library::fixture::SeedSpec {
    let mut folders = Vec::with_capacity(20 + 200 + TREE_FOLDERS as usize);
    let mut leaf_folders = Vec::with_capacity(TREE_FOLDERS as usize);
    for top in 0..20 {
        let top_path = format!("folder{top}");
        folders.push(top_path.clone());
        for middle in 0..10 {
            let middle_path = format!("{top_path}/set{middle}");
            folders.push(middle_path.clone());
            for leaf in 0..50 {
                let leaf_path = format!("{middle_path}/leaf{leaf}");
                folders.push(leaf_path.clone());
                leaf_folders.push(leaf_path);
            }
        }
    }
    crate::library::fixture::SeedSpec {
        folders,
        image_folders: leaf_folders,
        tags: (0..TAG_COUNT).map(|index| format!("标签{index}")).collect(),
        images: IMAGE_COUNT as usize,
        tags_per_image: TAGS_PER_IMAGE as usize,
        extensions: vec!["jpg".into()],
        write_files: false,
    }
}

#[test]
#[ignore = "高成本基准（非日常）：20 万行规模；用 cargo test -- --ignored --nocapture perf_metadata_200k 运行"]
fn perf_metadata_200k() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().join("perf-library");
    let (session, _manifest) = service::create(&root, "性能测试", 4).expect("create library");
    let database = session.db;

    let seed_start = Instant::now();
    let seed_elapsed = database.with_writer(seed).expect("seed database");
    let wall = seed_start.elapsed();
    println!(
        "[perf] seed {IMAGE_COUNT} images + {} tag links: seed={:.2}s wall={:.2}s",
        IMAGE_COUNT * TAGS_PER_IMAGE,
        seed_elapsed.as_secs_f64(),
        wall.as_secs_f64()
    );

    let (open_elapsed, counts) = database
        .with_reader(|connection| {
            let start = Instant::now();
            let active: i64 = connection.query_row(
                "SELECT count(*) FROM images WHERE deleted_at IS NULL",
                [],
                |row| row.get(0),
            )?;
            let trash: i64 = connection.query_row(
                "SELECT count(*) FROM images WHERE deleted_at IS NOT NULL",
                [],
                |row| row.get(0),
            )?;
            Ok((start.elapsed(), (active, trash)))
        })
        .expect("counts");
    println!(
        "[perf] count active={} trash={} in {:.2}ms",
        counts.0,
        counts.1,
        ms(open_elapsed)
    );

    let target_id: i64 = database
        .with_reader(|connection| {
            Ok(
                connection.query_row("SELECT id FROM images ORDER BY id LIMIT 1", [], |row| {
                    row.get(0)
                })?,
            )
        })
        .expect("target id");
    let target = crate::library::similarity::target_visuals(&database, target_id).expect("target");
    let build_started = Instant::now();
    let index = crate::library::similarity::VisualIndex::load(&database).expect("visual index");
    println!(
        "[perf] visual index build ({IMAGE_COUNT} rows): {:.2}ms",
        ms(build_started.elapsed())
    );
    let stamp_started = Instant::now();
    assert!(index.is_current(&database).expect("stamp check"));
    println!(
        "[perf] visual index stamp check: {:.2}ms",
        ms(stamp_started.elapsed())
    );
    let started = Instant::now();
    let similar = index.similar_phash(
        crate::imageops::phash::words(&target.phash),
        6,
        2_000,
        target_id,
    );
    println!(
        "[perf] pHash target scan (warm index, {IMAGE_COUNT} rows): {:.2}ms, results={}",
        ms(started.elapsed()),
        similar.len()
    );
    let started = Instant::now();
    let groups = index.phash_groups(6, None, None).expect("phash groups");
    println!(
        "[perf] pHash group scan (warm index, {IMAGE_COUNT} rows): {:.2}ms, groups={}, truncated={}",
        ms(started.elapsed()),
        groups.groups.len(),
        groups.truncated
    );
    let target_colors = crate::imageops::colors::prepare_palette::<8>(&target.colors8);
    let started = Instant::now();
    let colors = index.similar_color(
        &target_colors,
        crate::library::similarity::ColorPalette::Colors8,
        2_000,
        target_id,
    );
    println!(
        "[perf] color target scan (warm index, {IMAGE_COUNT} rows): {:.2}ms, results={}",
        ms(started.elapsed()),
        colors.len()
    );

    database
        .with_reader(|connection| {
            let first_page_sql = "SELECT id, content_hash, folder_id, file_size, width, height, score, description, imported_at
                 FROM images WHERE deleted_at IS NULL
                 ORDER BY score DESC, id DESC LIMIT ?1";
            let first_page = sample_query(connection, first_page_sql, 60, |statement| {
                let rows = statement.query_map([PAGE_SIZE], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, i64>(8)?,
                    ))
                })?;
                let mut count = 0;
                for row in rows {
                    let _ = row?;
                    count += 1;
                }
                Ok(count)
            })?;
            println!(
                "[perf] page 150 by score: p50={:.2}ms p95={:.2}ms",
                ms(percentile(&first_page, 0.5)),
                ms(percentile(&first_page, 0.95))
            );

            let deep_sql = "SELECT id, content_hash, folder_id, file_size, score
                 FROM images WHERE deleted_at IS NULL AND (score, id) < (?1, ?2)
                 ORDER BY score DESC, id DESC LIMIT ?3";
            let deep = sample_query(connection, deep_sql, 40, |statement| {
                let rows = statement.query_map(params![2_i64, 150_000_i64, PAGE_SIZE], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                })?;
                let mut count = 0;
                for row in rows {
                    let _ = row?;
                    count += 1;
                }
                Ok(count)
            })?;
            println!(
                "[perf] deep keyset page 150 near id=150k: p50={:.2}ms p95={:.2}ms",
                ms(percentile(&deep, 0.5)),
                ms(percentile(&deep, 0.95))
            );

            let folder_sql = "SELECT id, content_hash, file_size, score
                 FROM images WHERE deleted_at IS NULL AND folder_id = ?1
                 ORDER BY imported_at DESC, id DESC LIMIT ?2";
            let folder = sample_query(connection, folder_sql, 40, |statement| {
                let rows = statement.query_map(params![7_i64, PAGE_SIZE], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                })?;
                let mut count = 0;
                for row in rows {
                    let _ = row?;
                    count += 1;
                }
                Ok(count)
            })?;
            println!(
                "[perf] page 150 by folder: p50={:.2}ms p95={:.2}ms",
                ms(percentile(&folder, 0.5)),
                ms(percentile(&folder, 0.95))
            );

            let tag_sql = "SELECT images.id, images.content_hash, images.file_size
                 FROM images
                 JOIN image_tags ON image_tags.image_id = images.id
                 WHERE images.deleted_at IS NULL AND image_tags.tag_id = ?1
                 ORDER BY images.imported_at DESC, images.id DESC LIMIT ?2";
            let tag = sample_query(connection, tag_sql, 40, |statement| {
                let rows = statement.query_map(params![17_i64, PAGE_SIZE], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })?;
                let mut count = 0;
                for row in rows {
                    let _ = row?;
                    count += 1;
                }
                Ok(count)
            })?;
            println!(
                "[perf] page 150 by tag: p50={:.2}ms p95={:.2}ms",
                ms(percentile(&tag, 0.5)),
                ms(percentile(&tag, 0.95))
            );

            let fts_sql = "SELECT rowid FROM image_search WHERE image_search MATCH ?1 LIMIT ?2";
            let fts = sample_query(connection, fts_sql, 30, |statement| {
                let rows = statement.query_map(params!["第199990张", PAGE_SIZE], |row| {
                    row.get::<_, i64>(0)
                })?;
                let mut count = 0;
                for row in rows {
                    let _ = row?;
                    count += 1;
                }
                Ok(count)
            })?;
            println!(
                "[perf] fts trigram query: p50={:.2}ms p95={:.2}ms",
                ms(percentile(&fts, 0.5)),
                ms(percentile(&fts, 0.95))
            );

            let counts_start = Instant::now();
            let folder_counts: i64 = connection.query_row(
                "SELECT count(*) FROM (SELECT folder_id FROM images WHERE deleted_at IS NULL GROUP BY folder_id)",
                [],
                |row| row.get(0),
            )?;
            println!(
                "[perf] folder filter counts ({folder_counts} groups): {:.2}ms",
                ms(counts_start.elapsed())
            );
            Ok(())
        })
        .expect("queries");

    let reopen_start = Instant::now();
    let reopened = service::open(&root, 4).expect("reopen");
    let reopen_elapsed = reopen_start.elapsed();
    // 批次 6 起开库不再同步算计数，`LibraryInfo` 里先给 0，计数由前端单独拉。
    let counts_start = Instant::now();
    let (active, trash, temp) = reopened.db.counts().expect("counts");
    println!(
        "[perf] reopen library: {:.2}ms；counts() 单独 {:.2}ms (active={active} trash={trash} temp={temp})",
        ms(reopen_elapsed),
        ms(counts_start.elapsed())
    );

    let database = &reopened.db;
    let collection_id = database
        .with_writer(|connection| {
            connection.execute(
                "INSERT INTO collections(name, sort_order, created_at, updated_at)
                 VALUES ('性能集合', 0, 1, 1)",
                [],
            )?;
            let collection_id = connection.last_insert_rowid();
            connection.execute(
                "INSERT INTO collection_items(collection_id, image_id, position)
                 SELECT ?1, id, id FROM images",
                [collection_id],
            )?;
            Ok(collection_id)
        })
        .expect("seed collection");

    let page_query = LibraryQuery {
        limit: Some(PAGE_SIZE as u32),
        ..Default::default()
    };
    let (p50, p95) = time_query(database, &root, page_query.clone(), 40);
    println!(
        "[perf] query engine first page 150 (imported desc): p50={:.2}ms p95={:.2}ms",
        ms(p50),
        ms(p95)
    );

    let deep_query = LibraryQuery {
        sort: vec![SortSpec {
            key: SortKey::Score,
            descending: true,
        }],
        cursor: Some(QueryCursor {
            sort_values: vec!["2".into()],
            id: 150_000,
        }),
        limit: Some(PAGE_SIZE as u32),
        ..Default::default()
    };
    let (p50, p95) = time_query(database, &root, deep_query, 40);
    println!(
        "[perf] query engine deep keyset 150 (score/id cursor): p50={:.2}ms p95={:.2}ms",
        ms(p50),
        ms(p95)
    );

    // 选中一个顶层文件夹（500 个叶子、约 1 万张图），让子树条件真正展开一批文件夹。
    let top_folder_id = folder_id(database, "folder0");
    let folder_query = LibraryQuery {
        filters: LibraryFilters {
            folder_ids: vec![top_folder_id],
            ..Default::default()
        },
        limit: Some(PAGE_SIZE as u32),
        ..Default::default()
    };
    let subtree_page =
        query::query_page(database, &root, folder_query.clone()).expect("subtree page");
    assert_eq!(
        subtree_page.items.len() as i64,
        PAGE_SIZE,
        "子树条件应能填满一页"
    );
    let (p50, p95) = time_query(database, &root, folder_query.clone(), 40);
    println!(
        "[perf] query engine subtree folder 150: p50={:.2}ms p95={:.2}ms",
        ms(p50),
        ms(p95)
    );

    let leaf_folder_id = folder_id(database, "folder0/set0/leaf0");
    let direct_query = LibraryQuery {
        filters: LibraryFilters {
            folder_ids: vec![leaf_folder_id],
            folder_direct: true,
            ..Default::default()
        },
        limit: Some(PAGE_SIZE as u32),
        ..Default::default()
    };
    let direct_page =
        query::query_page(database, &root, direct_query.clone()).expect("direct page");
    assert!(!direct_page.items.is_empty(), "直属条件应命中叶子文件夹");
    let (p50, p95) = time_query(database, &root, direct_query.clone(), 40);
    println!(
        "[perf] query engine direct folder 150: p50={:.2}ms p95={:.2}ms",
        ms(p50),
        ms(p95)
    );
    let direct_count_start = Instant::now();
    let direct_count = query::query_total(database, direct_query).expect("direct folder total");
    println!(
        "[perf] query engine direct folder total ({direct_count} images): {:.2}ms",
        ms(direct_count_start.elapsed())
    );

    let mut folder_list_samples = Vec::with_capacity(20);
    for _ in 0..20 {
        let start = Instant::now();
        let folders = query::list_folders(database).expect("list folders");
        assert_eq!(folders.len() as i64, TOTAL_FOLDERS);
        folder_list_samples.push(start.elapsed());
    }
    println!(
        "[perf] query engine get_folders ({TOTAL_FOLDERS} folders): p50={:.2}ms p95={:.2}ms",
        ms(percentile(&folder_list_samples, 0.5)),
        ms(percentile(&folder_list_samples, 0.95))
    );

    let tag_query = LibraryQuery {
        filters: LibraryFilters {
            tag_ids: vec![17],
            ..Default::default()
        },
        limit: Some(PAGE_SIZE as u32),
        ..Default::default()
    };
    let (p50, p95) = time_query(database, &root, tag_query.clone(), 40);
    println!(
        "[perf] query engine tag 150: p50={:.2}ms p95={:.2}ms",
        ms(p50),
        ms(p95)
    );

    let mut counts_samples = Vec::with_capacity(10);
    for _ in 0..10 {
        let start = Instant::now();
        let counts = query::filter_counts(database, folder_query.clone()).expect("counts");
        counts_samples.push(start.elapsed());
        assert!(counts.total > 0);
    }
    println!(
        "[perf] query engine filter counts (subtree folder): p50={:.2}ms p95={:.2}ms",
        ms(percentile(&counts_samples, 0.5)),
        ms(percentile(&counts_samples, 0.95))
    );

    let index_build = database
        .with_writer(|connection| {
            connection.execute("DROP INDEX idx_images_visible_bytes", [])?;
            let start = Instant::now();
            connection.execute(
                "CREATE INDEX idx_images_visible_bytes
                 ON images(deleted_at, folder_id, file_size)",
                [],
            )?;
            Ok(start.elapsed())
        })
        .expect("rebuild visible bytes index");
    println!(
        "[perf] v8->v9 visible bytes index build on {IMAGE_COUNT} rows: {:.2}ms",
        ms(index_build)
    );

    for (label, profile) in [
        ("summary", StatsProfile::Summary),
        ("facets", StatsProfile::Facets),
        ("folderDetail", StatsProfile::FolderDetail),
    ] {
        let mut samples = Vec::with_capacity(10);
        for _ in 0..10 {
            let start = Instant::now();
            stats::library_stats(
                database,
                StatsRequest {
                    profile,
                    query: folder_query.clone(),
                },
            )
            .expect("stats profile");
            samples.push(start.elapsed());
        }
        println!(
            "[perf] stats {label} (subtree folder): p50={:.2}ms p95={:.2}ms",
            ms(percentile(&samples, 0.5)),
            ms(percentile(&samples, 0.95))
        );
    }

    // ---------- 批次 6：可见性聚合、标签分页与无筛选分面 ----------

    let mut count_samples = Vec::with_capacity(10);
    for _ in 0..10 {
        let start = Instant::now();
        let (active, trash, temp) = database.counts().expect("counts");
        count_samples.push(start.elapsed());
        assert_eq!(active, IMAGE_COUNT as u64);
        assert_eq!((trash, temp), (0, 0));
    }
    println!(
        "[perf] counts() ({IMAGE_COUNT} rows, 3 tiers): p50={:.2}ms p95={:.2}ms",
        ms(percentile(&count_samples, 0.5)),
        ms(percentile(&count_samples, 0.95))
    );

    let mut tag_page_samples = Vec::with_capacity(20);
    for _ in 0..20 {
        let start = Instant::now();
        let tags = query::list_tags_page(database, None, 100, 0).expect("tags page");
        tag_page_samples.push(start.elapsed());
        assert_eq!(tags.len(), 100, "200 个标签里应有足够多可填满一页");
    }
    println!(
        "[perf] list_tags_page 100 (image_tags 覆盖聚合): p50={:.2}ms p95={:.2}ms",
        ms(percentile(&tag_page_samples, 0.5)),
        ms(percentile(&tag_page_samples, 0.95))
    );

    let mut legacy_tag_samples = Vec::with_capacity(5);
    for _ in 0..5 {
        let start = Instant::now();
        let tags = query::list_tags_page_legacy_bench(database, 100).expect("legacy tags page");
        legacy_tag_samples.push(start.elapsed());
        assert_eq!(tags.len(), 100);
    }
    println!(
        "[perf] list_tags_page 100 (旧形状，逐张回表): p50={:.2}ms p95={:.2}ms",
        ms(percentile(&legacy_tag_samples, 0.5)),
        ms(percentile(&legacy_tag_samples, 0.95))
    );

    let no_filter_query = LibraryQuery {
        limit: Some(PAGE_SIZE as u32),
        ..Default::default()
    };
    let mut no_filter_samples = Vec::with_capacity(5);
    for _ in 0..5 {
        let start = Instant::now();
        let counts = query::filter_counts(database, no_filter_query.clone()).expect("counts");
        no_filter_samples.push(start.elapsed());
        assert_eq!(counts.total, IMAGE_COUNT as u64);
    }
    println!(
        "[perf] filter counts (no filters, 5 branches): p50={:.2}ms p95={:.2}ms",
        ms(percentile(&no_filter_samples, 0.5)),
        ms(percentile(&no_filter_samples, 0.95))
    );

    let start = Instant::now();
    let ids = query::query_ids(database, tag_query).expect("ids");
    println!(
        "[perf] query engine select-all ids ({}): {:.2}ms",
        ids.len(),
        ms(start.elapsed())
    );

    let start = Instant::now();
    let all_ids = query::query_ids(database, page_query).expect("all ids");
    println!(
        "[perf] query engine select-all ids ({}) without filters: {:.2}ms",
        all_ids.len(),
        ms(start.elapsed())
    );

    let selections = [
        ("2", all_ids.iter().copied().take(2).collect::<Vec<_>>()),
        ("1k", all_ids.iter().copied().take(1_000).collect()),
        ("20k", all_ids.iter().copied().take(20_000).collect()),
        ("200k", all_ids.clone()),
    ];
    for (label, selection) in selections {
        let iterations = if selection.len() > 20_000 { 3 } else { 10 };
        let mut samples = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let start = Instant::now();
            let result = metadata::selection_tags(database, &selection).expect("selection tags");
            assert_eq!(result.total, selection.len() as u64);
            samples.push(start.elapsed());
        }
        println!(
            "[perf] selection tags ({label}): p50={:.2}ms p95={:.2}ms",
            ms(percentile(&samples, 0.5)),
            ms(percentile(&samples, 0.95))
        );
    }

    let search_query = LibraryQuery {
        search: "人物速".into(),
        limit: Some(PAGE_SIZE as u32),
        ..Default::default()
    };
    let (p50, p95) = time_query(database, &root, search_query.clone(), 30);
    println!(
        "[perf] query engine search 150 (FTS trigram): p50={:.2}ms p95={:.2}ms",
        ms(p50),
        ms(p95)
    );

    let short_search_query = LibraryQuery {
        search: "编号".into(),
        limit: Some(PAGE_SIZE as u32),
        ..Default::default()
    };
    let (p50, p95) = time_query(database, &root, short_search_query.clone(), 30);
    println!(
        "[perf] query engine search 150 (2-char LIKE): p50={:.2}ms p95={:.2}ms",
        ms(p50),
        ms(p95)
    );

    let tag_search_query = LibraryQuery {
        search: "标签17".into(),
        limit: Some(PAGE_SIZE as u32),
        ..Default::default()
    };
    let (p50, p95) = time_query(database, &root, tag_search_query.clone(), 20);
    println!(
        "[perf] query engine search 150 via tag name: p50={:.2}ms p95={:.2}ms",
        ms(p50),
        ms(p95)
    );

    let folder_search_query = LibraryQuery {
        search: "folder7".into(),
        limit: Some(PAGE_SIZE as u32),
        ..Default::default()
    };
    let (p50, p95) = time_query(database, &root, folder_search_query.clone(), 20);
    println!(
        "[perf] query engine search 150 via folder path: p50={:.2}ms p95={:.2}ms",
        ms(p50),
        ms(p95)
    );

    let collection_query = LibraryQuery {
        view: crate::library::query::LibraryView::Collection,
        collection_id: Some(collection_id),
        sort: vec![SortSpec {
            key: SortKey::CollectionPosition,
            descending: false,
        }],
        limit: Some(PAGE_SIZE as u32),
        ..Default::default()
    };
    let (p50, p95) = time_query(database, &root, collection_query.clone(), 30);
    println!(
        "[perf] query engine collection 150 (manual order): p50={:.2}ms p95={:.2}ms",
        ms(p50),
        ms(p95)
    );

    for (label, query) in [
        ("search FTS", search_query),
        ("search 2-char", short_search_query),
        ("search tag", tag_search_query),
        ("search folder", folder_search_query),
        ("collection", collection_query),
    ] {
        let mut samples = Vec::with_capacity(10);
        for _ in 0..10 {
            let start = Instant::now();
            let counts = query::filter_counts(database, query.clone()).expect("counts");
            samples.push(start.elapsed());
            assert!(counts.total > 0);
        }
        println!(
            "[perf] filter counts ({label}): p50={:.2}ms p95={:.2}ms",
            ms(percentile(&samples, 0.5)),
            ms(percentile(&samples, 0.95))
        );
    }

    // ---------- P4：7 个排序指标与 id 列表游标 ----------

    // 多级排序先建 token 再测后续页：每次取第一页都会挤掉一份缓存列表，旧 token 会失效。
    // 「集合内顺序」只在集合视图可用，这里用前 5 级代表多级排序，集合顺序另有专项。
    let sort_levels = vec![
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
            descending: true,
        },
        SortSpec {
            key: SortKey::Pixels,
            descending: true,
        },
        SortSpec {
            key: SortKey::ModifiedAt,
            descending: false,
        },
        SortSpec {
            key: SortKey::Folder,
            descending: false,
        },
    ];

    let first_query = LibraryQuery {
        sort: sort_levels.clone(),
        limit: Some(PAGE_SIZE as u32),
        ..Default::default()
    };
    let first_page =
        query::query_page(database, &root, first_query.clone()).expect("multi sort page");
    assert_eq!(first_page.items.len() as i64, PAGE_SIZE);
    let list_token = first_page
        .next_page_token
        .expect("多级排序应返回 id 列表游标");
    let token_prefix = list_token
        .rsplit_once(':')
        .map(|(prefix, _)| prefix.to_owned())
        .expect("token 形如 uuid:offset");

    let paged_query = LibraryQuery {
        sort: sort_levels.clone(),
        page_token: Some(list_token),
        limit: Some(PAGE_SIZE as u32),
        ..Default::default()
    };
    let mut samples = Vec::with_capacity(30);
    for _ in 0..30 {
        let start = Instant::now();
        let page = query::query_page(database, &root, paged_query.clone()).expect("paged");
        assert_eq!(page.items.len() as i64, PAGE_SIZE);
        samples.push(start.elapsed());
    }
    println!(
        "[perf] sort 6 levels page 1 (token slice): p50={:.2}ms p95={:.2}ms",
        ms(percentile(&samples, 0.5)),
        ms(percentile(&samples, 0.95))
    );

    let deep_paged = LibraryQuery {
        sort: sort_levels.clone(),
        page_token: Some(format!("{token_prefix}:{}", IMAGE_COUNT - PAGE_SIZE)),
        limit: Some(PAGE_SIZE as u32),
        ..Default::default()
    };
    let mut samples = Vec::with_capacity(30);
    for _ in 0..30 {
        let start = Instant::now();
        let page = query::query_page(database, &root, deep_paged.clone()).expect("deep paged");
        assert_eq!(page.items.len() as i64, PAGE_SIZE);
        samples.push(start.elapsed());
    }
    println!(
        "[perf] sort 6 levels page at offset {} (token slice): p50={:.2}ms p95={:.2}ms",
        IMAGE_COUNT - PAGE_SIZE,
        ms(percentile(&samples, 0.5)),
        ms(percentile(&samples, 0.95))
    );

    let (p50, p95) = time_query(database, &root, first_query.clone(), 8);
    println!(
        "[perf] sort 6 levels first page (build id list, {IMAGE_COUNT} rows): p50={:.2}ms p95={:.2}ms",
        ms(p50),
        ms(p95)
    );

    for (label, key) in [
        ("importedAt", SortKey::ImportedAt),
        ("modifiedAt", SortKey::ModifiedAt),
        ("score", SortKey::Score),
        ("size", SortKey::Size),
        ("pixels", SortKey::Pixels),
        ("folder", SortKey::Folder),
    ] {
        let query = LibraryQuery {
            sort: vec![SortSpec {
                key,
                descending: true,
            }],
            limit: Some(PAGE_SIZE as u32),
            ..Default::default()
        };
        // 文件夹排序没有可用的复合索引，走 id 列表通道，迭代次数降到 8。
        let iterations = if key == SortKey::Folder { 8 } else { 30 };
        let (p50, p95) = time_query(database, &root, query, iterations);
        println!(
            "[perf] sort single level {label} desc first page 150: p50={:.2}ms p95={:.2}ms",
            ms(p50),
            ms(p95)
        );
    }

    // schema v3 → v4 只新增这两个索引；在 20 万行上量一次建索引耗时。
    database
        .with_writer(|connection| {
            connection.execute_batch(
                "DROP INDEX IF EXISTS idx_images_active_modified;
                 DROP INDEX IF EXISTS idx_images_active_pixels;",
            )?;
            Ok(())
        })
        .expect("drop v4 indexes");
    let index_start = Instant::now();
    database
        .with_writer(|connection| {
            connection.execute_batch(
                "CREATE INDEX idx_images_active_modified
                     ON images(deleted_at, modified_at DESC, id DESC);
                 CREATE INDEX idx_images_active_pixels
                     ON images(deleted_at, (width * height) DESC, id DESC);
                 ANALYZE;",
            )?;
            Ok(())
        })
        .expect("rebuild v4 indexes");
    println!(
        "[perf] schema v3->v4 index build on {IMAGE_COUNT} rows: {:.2}ms",
        ms(index_start.elapsed())
    );

    // schema v7 → v8 只加一张覆盖索引。合成库只有 20 万行、且整库都在页缓存里，
    // 回表代价远低于真实库（1 GB、随机 IO），所以这里只验证“索引确实被选中、形状没退化”，
    // 真实库的 620 → 20 ms 见 `docs/guides/performance.md`。
    let measure_aggregates = |label: &str| {
        let mut counts = Vec::with_capacity(5);
        let mut folders = Vec::with_capacity(5);
        let mut tags = Vec::with_capacity(5);
        let mut facets = Vec::with_capacity(5);
        for _ in 0..5 {
            let start = Instant::now();
            database.counts().expect("counts");
            counts.push(start.elapsed());

            let start = Instant::now();
            query::list_folders(database).expect("list folders");
            folders.push(start.elapsed());

            let start = Instant::now();
            query::list_tags_page(database, None, 100, 0).expect("tags page");
            tags.push(start.elapsed());

            let start = Instant::now();
            query::filter_counts(database, no_filter_query.clone()).expect("facets");
            facets.push(start.elapsed());
        }
        println!(
            "[perf] {label}: counts={:.2}ms list_folders={:.2}ms list_tags={:.2}ms filter_counts={:.2}ms",
            ms(percentile(&counts, 0.5)),
            ms(percentile(&folders, 0.5)),
            ms(percentile(&tags, 0.5)),
            ms(percentile(&facets, 0.5))
        );
    };
    let set_visible_index = |enabled: bool| {
        database
            .with_writer(|connection| {
                if enabled {
                    connection.execute_batch(
                        "CREATE INDEX IF NOT EXISTS idx_images_visible_facets
                             ON images(deleted_at, purge_operation_id, folder_id, file_ext, score);
                         ANALYZE;",
                    )?;
                } else {
                    connection.execute_batch("DROP INDEX IF EXISTS idx_images_visible_facets;")?;
                }
                Ok(())
            })
            .expect("toggle v8 index");
    };

    // 交替测两轮：先“有索引”，再“没有”，最后重建。两次“有索引”分开量，
    // 才能区分索引本身的影响与缓存/顺序带来的漂移。
    measure_aggregates("aggregates with idx_images_visible_facets (round 1)");
    set_visible_index(false);
    measure_aggregates("aggregates without idx_images_visible_facets");
    let visible_index_start = Instant::now();
    set_visible_index(true);
    println!(
        "[perf] schema v7->v8 index build on {IMAGE_COUNT} rows: {:.2}ms",
        ms(visible_index_start.elapsed())
    );
    measure_aggregates("aggregates with idx_images_visible_facets (round 2)");
    let plan = database
        .with_reader(|connection| {
            let mut statement = connection.prepare(
                "EXPLAIN QUERY PLAN SELECT count(*) FROM images WHERE deleted_at IS NULL",
            )?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(3))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows.join(" | "))
        })
        .expect("plan");
    println!("[perf] plan counts(): {plan}");
}

#[test]
#[ignore = "高成本基准（非日常）：20 万行操作记录与待释放；用 cargo test --lib -- --ignored --nocapture perf_history_200k 运行"]
fn perf_history_200k() {
    use crate::library::{NoopReporter, history};

    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().join("perf-history-library");
    let (session, _manifest) = service::create(&root, "历史基准", 4).expect("create library");
    let database = session.db;

    let seed_start = Instant::now();
    database.with_writer(seed).expect("seed database");
    println!(
        "[perf] history seed {IMAGE_COUNT} images: {:.2}s",
        seed_start.elapsed().as_secs_f64()
    );

    let record_start = Instant::now();
    database
        .with_writer(|connection| {
            let transaction = connection.transaction()?;
            {
                let mut statement = transaction.prepare(
                    "INSERT INTO operations(kind, label, created_at, status, affected, error)
                     VALUES ('score', '评分', ?1, 'applied', 1, '')",
                )?;
                for index in 0..500_i64 {
                    statement.execute([index])?;
                    transaction.execute(
                        "INSERT INTO operation_chunks(operation_id, seq, format, data)
                         VALUES (?1, 0, 'triples', zeroblob(24))",
                        [transaction.last_insert_rowid()],
                    )?;
                }
            }
            transaction.commit()?;
            Ok(())
        })
        .expect("seed operations");
    println!(
        "[perf] insert 500 operations + chunks: {:.2}ms",
        ms(record_start.elapsed())
    );

    let list_start = Instant::now();
    let entries = history::list(&database).expect("history list");
    println!(
        "[perf] history list ({} entries): {:.2}ms",
        entries.len(),
        ms(list_start.elapsed())
    );

    let trim_start = Instant::now();
    let evicted = history::trim(&database, 100).expect("trim history");
    println!(
        "[perf] trim 500 -> 100 ({} evicted): {:.2}ms",
        evicted.len(),
        ms(trim_start.elapsed())
    );

    let mark_start = Instant::now();
    database
        .with_writer(|connection| {
            connection.execute(
                "UPDATE images SET deleted_at = 1, purge_operation_id = 500",
                [],
            )?;
            Ok(())
        })
        .expect("mark purge");
    println!(
        "[perf] mark {IMAGE_COUNT} rows for purge: {:.2}ms",
        ms(mark_start.elapsed())
    );

    let pending_start = Instant::now();
    let (count, bytes) = history::pending_summary(&database).expect("pending summary");
    println!(
        "[perf] pending summary ({} rows, {} bytes): {:.2}ms",
        count,
        bytes,
        ms(pending_start.elapsed())
    );

    let release_start = Instant::now();
    let report =
        crate::library::metadata::purge_by_operation(&database, &root, &[500], &NoopReporter)
            .expect("purge rows");
    println!(
        "[perf] release {} rows without files: {:.2}ms",
        report.deleted,
        ms(release_start.elapsed())
    );
}

fn folder_id(database: &crate::library::db::LibraryDb, path: &str) -> i64 {
    database
        .with_reader(|connection| {
            let id =
                connection.query_row("SELECT id FROM folders WHERE path = ?1", [path], |row| {
                    row.get(0)
                })?;
            Ok(id)
        })
        .expect("benchmark folder id")
}

fn time_query(
    database: &crate::library::db::LibraryDb,
    root: &std::path::Path,
    query: LibraryQuery,
    iterations: usize,
) -> (Duration, Duration) {
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let page = query::query_page(database, root, query.clone()).expect("query page");
        assert!(!page.items.is_empty());
        samples.push(start.elapsed());
    }
    (percentile(&samples, 0.5), percentile(&samples, 0.95))
}

#[test]
#[ignore = "高成本基准（非日常）：缩略图包 vs 逐文件读取；用 cargo test -- --ignored --nocapture perf_thumb_pack_read 运行"]
fn perf_thumb_pack_read() {
    let temp = tempfile::tempdir().expect("temp dir");
    let count = 150usize;
    let blob_size = 20 * 1024usize;
    let mut blobs = Vec::with_capacity(count);
    for index in 0..count {
        let mut data = vec![0u8; blob_size];
        let mut state = (index as u32).wrapping_mul(2654435761).wrapping_add(1);
        for byte in &mut data {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *byte = (state & 0xff) as u8;
        }
        blobs.push(data);
    }

    let files_dir = temp.path().join("files");
    std::fs::create_dir_all(&files_dir).expect("files dir");
    for (index, data) in blobs.iter().enumerate() {
        std::fs::write(files_dir.join(format!("{index:03}.jpg")), data).expect("write file");
    }

    let pack_path = temp.path().join("thumbs.pack");
    let mut pack = File::create(&pack_path).expect("create pack");
    let mut offsets = Vec::with_capacity(count);
    for data in &blobs {
        offsets.push(pack.stream_position().expect("position"));
        pack.write_all(data).expect("write pack");
    }
    pack.sync_all().expect("sync pack");
    drop(pack);

    let mut order: Vec<usize> = (0..count).collect();
    let mut state = 0x9e3779b9_u32;
    for index in (1..order.len()).rev() {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        order.swap(index, (state as usize) % (index + 1));
    }

    let mut pack_reader = File::open(&pack_path).expect("open pack");
    let mut buffer = vec![0u8; blob_size];
    let mut pack_samples = Vec::with_capacity(80);
    for _ in 0..80 {
        let start = Instant::now();
        for index in &order {
            pack_reader
                .seek(SeekFrom::Start(offsets[*index]))
                .expect("seek");
            pack_reader.read_exact(&mut buffer).expect("read pack");
        }
        pack_samples.push(start.elapsed());
    }

    let mut file_samples = Vec::with_capacity(80);
    for _ in 0..80 {
        let start = Instant::now();
        for index in &order {
            let path = files_dir.join(format!("{index:03}.jpg"));
            let data = std::fs::read(path).expect("read file");
            buffer[..data.len()].copy_from_slice(&data);
        }
        file_samples.push(start.elapsed());
    }

    let bytes = (count * blob_size) as f64 / 1048576.0;
    let pack_p50 = percentile(&pack_samples, 0.5);
    let file_p50 = percentile(&file_samples, 0.5);
    println!(
        "[perf] 150x20KB random reads: pack p50={:.2}ms ({:.0} MB/s), files p50={:.2}ms ({:.0} MB/s)",
        ms(pack_p50),
        bytes / pack_p50.as_secs_f64(),
        ms(file_p50),
        bytes / file_p50.as_secs_f64()
    );
}

/// 编码线程基准：同一批图、同一档位，比较 4 / 8 / 12 / 核数−1 的吞吐。
///
/// 不测磁盘：样本在内存里生成，只衡量解码 + 阶梯编码 + PSNR 的并行扩展性。
#[test]
#[ignore = "高成本基准（非日常）：编码线程；cargo test --lib -- --ignored --nocapture perf_encode_threads"]
fn perf_encode_threads() {
    use rayon::prelude::*;

    use crate::{
        imageops::optimize::{self, LadderOutcome, OptimizeOptions, OptimizePlan},
        import::decode,
    };

    let mut images = Vec::new();
    for seed in 0..16_u32 {
        let mut image = image::RgbImage::new(1200, 900);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            let value = ((x * 7 + y * 13 + seed * 29) % 256) as u8;
            *pixel = image::Rgb([
                value,
                value.wrapping_mul(3).wrapping_add((x % 31) as u8),
                value.wrapping_mul(5).wrapping_add((y % 17) as u8),
            ]);
        }
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("encode png");
        images.push(bytes);
    }

    let cores = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1);
    let mut candidates = vec![4_usize, 8, 12, cores.saturating_sub(1).max(1)];
    candidates.sort_unstable();
    candidates.dedup();
    for threads in candidates {
        let pool = super::encode_pool_with(threads, "基准").expect("build pool");
        let started = Instant::now();
        let output: u64 = pool.install(|| {
            images
                .par_iter()
                .map(|bytes| {
                    let decoded = decode::decode_bytes(bytes, Some("png")).expect("decode");
                    let plan = OptimizePlan {
                        start_quality: 85,
                        psnr_floor: 36.0,
                    };
                    match optimize::encode_ladder(
                        &decoded.image,
                        &plan,
                        &OptimizeOptions::default(),
                        bytes.len() as u64,
                        None,
                    )
                    .expect("encode")
                    {
                        LadderOutcome::Encoded(candidate) => candidate.bytes.len() as u64,
                        LadderOutcome::Skipped(_) => 0,
                    }
                })
                .sum()
        });
        let elapsed = started.elapsed().as_secs_f64();
        println!(
            "[perf] encode threads={threads} images={} elapsed={elapsed:.2}s rate={:.1} img/s output={} KB",
            images.len(),
            images.len() as f64 / elapsed,
            output / 1024
        );
    }
    println!(
        "[perf] cores={cores} default_budget={}",
        crate::concurrency::budget::default_limit()
    );
}

/// 批次大小基准：500 / 1000 / 2000 张一批，比较总耗时与提交耗时占比。
///
/// 每档都用一份全新的临时库：同一批图压第二次时新内容等于当前内容，会被判为
/// 「省得太少」而全部跳过，量不到写路径。
#[test]
#[ignore = "高成本基准（非日常）：批次大小；cargo test --lib -- --ignored --nocapture perf_optimize_batch_size"]
fn perf_optimize_batch_size() {
    use crate::{
        imageops::optimize::OptimizeOptions,
        library::{NoopReporter, history, optimize as optimize_write},
    };

    let count = 600_u32;
    for batch_size in [500_usize, 1000, 2000] {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "批次基准", 1).expect("create library");

        let mut ids = Vec::with_capacity(count as usize);
        for index in 0..count {
            let mut image = image::RgbImage::new(320, 240);
            for (x, y, pixel) in image.enumerate_pixels_mut() {
                *pixel = image::Rgb([
                    ((x * 3 + index) % 256) as u8,
                    ((y * 5 + index * 7) % 256) as u8,
                    ((x + y + index) % 256) as u8,
                ]);
            }
            // 每张图左上角盖一个 8×8 的唯一色块：JPEG 量化不会抹掉它，
            // 内容哈希不会重复（唯一约束会拒绝插入）。
            for y in 0..8 {
                for x in 0..8 {
                    image.put_pixel(
                        x,
                        y,
                        image::Rgb([(index & 0xff) as u8, (index >> 8) as u8, 200]),
                    );
                }
            }
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgb8(image)
                .write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::Png,
                )
                .expect("encode png");
            let hash = blake3::hash(&bytes).to_hex().to_string();
            std::fs::write(root.join(format!("{hash}.png")), &bytes).expect("write content");
            let id = session
                .db
                .with_writer(|connection| {
                    connection.execute(
                        "INSERT INTO images(
                            content_hash, file_ext, folder_id, file_size, width, height, format,
                            has_alpha, phash, colors4, colors8, score, description, imported_at,
                            modified_at
                         ) VALUES (?1, 'png', 1, ?2, 320, 240, 'png', 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                        rusqlite::params![hash, bytes.len() as i64],
                    )?;
                    Ok(connection.last_insert_rowid())
                })
                .expect("insert row");
            ids.push(id);
        }

        let operation = history::begin_live(
            &session.db,
            history::NewOperation::new(history::HistoryKind::Optimize, "批次基准"),
        )
        .expect("begin operation");
        let candidates =
            optimize_write::collect_candidates(&session.db, &ids).expect("collect candidates");
        let started = Instant::now();
        let report = optimize_write::optimize_with_batch_size(
            &session.db,
            &root,
            &session.thumbs,
            candidates,
            &OptimizeOptions::default(),
            operation,
            &NoopReporter,
            batch_size,
            &crate::concurrency::budget::ImageBudget::new(
                crate::concurrency::budget::default_limit(),
            ),
            &crate::concurrency::budget::ImageMemoryBudget::default(),
        )
        .expect("optimize");
        let elapsed = started.elapsed().as_secs_f64();
        history::finish_live(&session.db, operation, "").expect("finish operation");
        println!(
            "[perf] batch={batch_size} processed={} elapsed={elapsed:.2}s publish={}ms commit={}ms delete={}ms",
            report.processed, report.publish_ms, report.commit_ms, report.delete_ms
        );
    }
}

/// 预计耗时 vs 实际耗时：同一份临时库先估算、再执行，打印偏差比例。
///
/// 只用于同机同参数的近似校验，不做跨机器比较；复测：
/// `cargo test --lib -- --ignored --nocapture perf_estimate_vs_actual`。
#[test]
#[ignore = "高成本基准（非日常）：预计/实际耗时对比；cargo test --lib -- --ignored --nocapture perf_estimate_vs_actual"]
fn perf_estimate_vs_actual() {
    use crate::{
        imageops::optimize::OptimizeOptions,
        library::{
            NoopReporter, history, optimize as optimize_write,
            optimize_estimate::{self, OptimizeEstimate},
        },
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("library");
    let (session, _) = service::create(&root, "耗时对比", 1).expect("create library");

    let count = 400_u32;
    let mut ids = Vec::with_capacity(count as usize);
    for index in 0..count {
        let mut image = image::RgbImage::new(800, 600);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            let value = ((x * 5 + y * 3 + index * 11) % 256) as u8;
            *pixel = image::Rgb([
                value,
                value.wrapping_mul(3).wrapping_add((x % 29) as u8),
                value.wrapping_mul(7).wrapping_add((y % 23) as u8),
            ]);
        }
        for y in 0..8 {
            for x in 0..8 {
                image.put_pixel(
                    x,
                    y,
                    image::Rgb([(index & 0xff) as u8, (index >> 8) as u8, 90]),
                );
            }
        }
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("encode png");
        let hash = blake3::hash(&bytes).to_hex().to_string();
        std::fs::write(root.join(format!("{hash}.png")), &bytes).expect("write content");
        let id = session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO images(
                        content_hash, file_ext, folder_id, file_size, width, height, format,
                        has_alpha, phash, colors4, colors8, score, description, imported_at,
                        modified_at
                     ) VALUES (?1, 'png', 1, ?2, 800, 600, 'png', 0,
                        zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                    rusqlite::params![hash, bytes.len() as i64],
                )?;
                Ok(connection.last_insert_rowid())
            })
            .expect("insert row");
        ids.push(id);
    }

    let options = OptimizeOptions::default();
    let estimate: OptimizeEstimate = optimize_estimate::estimate(
        &session.db,
        &root,
        "perf-estimate",
        &ids,
        &options,
        200,
        "fingerprint",
        &NoopReporter,
        &crate::concurrency::resources::ResourceTable::new(),
        &crate::concurrency::budget::ImageBudget::new(crate::concurrency::budget::default_limit()),
        &crate::concurrency::budget::ImageMemoryBudget::default(),
        &crate::concurrency::resources::HolderInfo::new("perf-estimate", "估算"),
    )
    .expect("estimate")
    .expect("estimate not cancelled");

    let operation = history::begin_live(
        &session.db,
        history::NewOperation::new(history::HistoryKind::Optimize, "耗时对比"),
    )
    .expect("begin operation");
    let candidates =
        optimize_write::collect_candidates(&session.db, &ids).expect("collect candidates");
    let started = Instant::now();
    let report = optimize_write::optimize(
        &session.db,
        &root,
        &session.thumbs,
        candidates,
        &options,
        operation,
        &NoopReporter,
        &crate::concurrency::budget::ImageBudget::new(crate::concurrency::budget::default_limit()),
        &crate::concurrency::budget::ImageMemoryBudget::default(),
    )
    .expect("optimize");
    let actual = started.elapsed().as_secs_f64();
    history::finish_live(&session.db, operation, "").expect("finish operation");

    let estimated = estimate.estimated_seconds as f64;
    println!(
        "[perf] estimate={estimated:.1}s (work={}s write={}s finish={}s) actual={actual:.2}s ratio={:.2} processed={}",
        estimate.estimated_work_seconds,
        estimate.estimated_write_seconds,
        estimate.estimated_finish_seconds,
        if estimated > 0.0 {
            actual / estimated
        } else {
            0.0
        },
        report.processed
    );
    println!(
        "[perf] actual split: encode={}ms hash={}ms recalc={}ms write={}ms publish={}ms commit={}ms delete={}ms",
        report.encode_ms,
        report.hash_ms,
        report.recalc_ms,
        report.write_ms,
        report.publish_ms,
        report.commit_ms,
        report.delete_ms
    );
    assert!(report.processed > 0);
}

/// 抽样离散度：同一份库、同一档位连抽多次，打印节省空间与耗时的极差。
///
/// 库里的图片按真实库的尺寸构成生成（小图多、体积集中在大图），否则抽样误差被
/// 均匀数据抹平，测不出东西。复测：
/// `cargo test --lib -- --ignored --nocapture perf_estimate_sampling_spread`。
#[test]
#[ignore = "高成本基准（非日常）：抽样离散度；cargo test --lib -- --ignored --nocapture perf_estimate_sampling_spread"]
fn perf_estimate_sampling_spread() {
    use crate::{
        imageops::optimize::OptimizeOptions,
        library::{
            NoopReporter,
            optimize_estimate::{self, OptimizeEstimate},
        },
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("library");
    let (session, _) = service::create(&root, "抽样离散度", 1).expect("create library");

    // 尺寸构成大致照抄真实库 s5：多数小图，体积与像素集中在大图。
    let buckets: [(u32, u32, usize); 5] = [
        (400, 300, 96),
        (800, 600, 64),
        (1_600, 1_200, 40),
        (2_400, 1_800, 20),
        (3_200, 2_400, 12),
    ];
    let mut ids = Vec::new();
    let mut index = 0_u32;
    for (width, height, count) in buckets {
        for _ in 0..count {
            let mut image = image::RgbImage::new(width, height);
            for (x, y, pixel) in image.enumerate_pixels_mut() {
                // 一半图细节多（难压），一半图平滑（好压），让节省率分散。
                let detail = index.is_multiple_of(2);
                let value = if detail {
                    ((x * 7 + y * 13 + index * 31) % 256) as u8
                } else {
                    ((x * 255 / width.max(1) + index * 5) % 256) as u8
                };
                *pixel = image::Rgb([value, value.wrapping_mul(3), value.wrapping_mul(7)]);
            }
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgb8(image)
                .write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::Png,
                )
                .expect("encode png");
            let hash = blake3::hash(&bytes).to_hex().to_string();
            std::fs::write(root.join(format!("{hash}.png")), &bytes).expect("write content");
            let id = session
                .db
                .with_writer(|connection| {
                    connection.execute(
                        "INSERT INTO images(
                            content_hash, file_ext, folder_id, file_size, width, height, format,
                            has_alpha, phash, colors4, colors8, score, description, imported_at,
                            modified_at
                         ) VALUES (?1, 'png', 1, ?2, ?3, ?4, 'png', 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                        rusqlite::params![hash, bytes.len() as i64, width, height],
                    )?;
                    Ok(connection.last_insert_rowid())
                })
                .expect("insert row");
            ids.push(id);
            index += 1;
        }
    }

    let options = OptimizeOptions::default();
    let run = |budget: usize| -> OptimizeEstimate {
        optimize_estimate::estimate(
            &session.db,
            &root,
            "perf-spread",
            &ids,
            &options,
            budget,
            "fingerprint",
            &NoopReporter,
            &crate::concurrency::resources::ResourceTable::new(),
            &crate::concurrency::budget::ImageBudget::new(
                crate::concurrency::budget::default_limit(),
            ),
            &crate::concurrency::budget::ImageMemoryBudget::default(),
            &crate::concurrency::resources::HolderInfo::new("perf-spread", "估算"),
        )
        .expect("estimate")
        .expect("not cancelled")
    };

    // 全量抽样等于真值，用来算每次抽样的相对偏差。
    let truth = run(2_000);
    println!(
        "[perf] truth: images={} bytes={} savings={} seconds={} sample={} sample_ms={} work={}",
        truth.candidate_count,
        truth.candidate_bytes,
        truth.savings_bytes,
        truth.estimated_seconds,
        truth.sample_count,
        truth.sample_elapsed_ms,
        truth.estimated_work_seconds
    );
    let truth_savings = truth.savings_bytes.max(1) as f64;
    let truth_seconds = truth.estimated_seconds.max(1) as f64;
    assert!(truth.candidate_count > 0);

    let rounds = 10_usize;
    let mut savings_ratio = Vec::with_capacity(rounds);
    let mut seconds_ratio = Vec::with_capacity(rounds);
    for round in 0..rounds {
        let estimate = run(50);
        let savings = (estimate.savings_bytes as f64 - truth_savings) / truth_savings;
        let seconds = (estimate.estimated_seconds as f64 - truth_seconds) / truth_seconds;
        savings_ratio.push(savings);
        seconds_ratio.push(seconds);
        println!(
            "[perf] round={round} sample={} coverage={:.2}% sample_ms={} bytes={} savings={} ({:+.1}%) ±{} seconds={} ({:+.1}%) ±{} work={}",
            estimate.sample_count,
            estimate.coverage * 100.0,
            estimate.sample_elapsed_ms,
            estimate.sample_bytes,
            estimate.savings_bytes,
            savings * 100.0,
            estimate.savings_margin_bytes,
            estimate.estimated_seconds,
            seconds * 100.0,
            estimate.estimated_seconds_margin,
            estimate.estimated_work_seconds
        );
    }
    let spread = |values: &[f64]| -> (f64, f64, f64) {
        let min = values.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        (min * 100.0, sorted[sorted.len() / 2] * 100.0, max * 100.0)
    };
    let (savings_min, savings_median, savings_max) = spread(&savings_ratio);
    let (seconds_min, seconds_median, seconds_max) = spread(&seconds_ratio);
    println!(
        "[perf] savings: min={savings_min:+.1}% median={savings_median:+.1}% max={savings_max:+.1}% range={:.1}pt",
        savings_max - savings_min
    );
    println!(
        "[perf] seconds: min={seconds_min:+.1}% median={seconds_median:+.1}% max={seconds_max:+.1}% range={:.1}pt",
        seconds_max - seconds_min
    );
}
