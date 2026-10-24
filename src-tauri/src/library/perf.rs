use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    time::{Duration, Instant},
};

use rusqlite::{Connection, params};

use crate::{
    app::error::AppResult,
    library::{
        query::{self, LibraryFilters, LibraryQuery, QueryCursor, SortKey, SortSpec},
        service,
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
    let transaction = connection.transaction()?;
    let mut leaf_folder_ids = Vec::with_capacity(TREE_FOLDERS as usize);
    {
        let mut folder_statement =
            transaction.prepare("INSERT INTO folders(path, created_at) VALUES (?1, ?2)")?;
        for top in 0..20 {
            let top_path = format!("folder{top}");
            folder_statement.execute(params![top_path, 1_i64])?;
            for middle in 0..10 {
                let middle_path = format!("{top_path}/set{middle}");
                folder_statement.execute(params![middle_path, 1_i64])?;
                for leaf in 0..50 {
                    let leaf_path = format!("{middle_path}/leaf{leaf}");
                    folder_statement.execute(params![leaf_path, 1_i64])?;
                    leaf_folder_ids.push(transaction.last_insert_rowid());
                }
            }
        }
    }
    {
        let mut tag_statement = transaction.prepare("INSERT INTO tags(name) VALUES (?1)")?;
        for index in 0..TAG_COUNT {
            tag_statement.execute(params![format!("标签{index}")])?;
        }
    }

    let tag_ids: Vec<i64> = {
        let mut statement = transaction.prepare("SELECT id FROM tags ORDER BY id")?;
        statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };

    {
        let mut image_statement = transaction.prepare(
            "INSERT INTO images(
                content_hash, file_ext, folder_id, file_size, width, height, format,
                has_alpha, phash, colors4, colors8, score, description, imported_at, modified_at
             ) VALUES (?1, 'jpg', ?2, ?3, 1920, 1080, 'jpeg', 0, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
        )?;
        let mut tag_link_statement =
            transaction.prepare("INSERT INTO image_tags(image_id, tag_id) VALUES (?1, ?2)")?;
        for index in 0..IMAGE_COUNT {
            // 相似度基准需要分布开的哈希与主色；全部相同会让分段索引退化成单一大组。
            let mut phash = [0_u8; 32];
            let mut state = index as u64 + 0x9e37_79b9_7f4a_7c15;
            for chunk in phash.chunks_exact_mut(8) {
                state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
                let mut mixed = state;
                mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                mixed ^= mixed >> 31;
                chunk.copy_from_slice(&mixed.to_le_bytes());
            }
            let colors4: Vec<u8> = (0..16)
                .map(|byte| ((index as u64 * 7 + byte * 13) % 256) as u8)
                .collect();
            let colors8: Vec<u8> = (0..32)
                .map(|byte| ((index as u64 * 11 + byte * 19) % 256) as u8)
                .collect();
            let content_hash = format!("{index:064x}");
            let folder_id = leaf_folder_ids[(index % TREE_FOLDERS) as usize];
            let file_size = 100_000 + (index % 1_000) * 1_000;
            let score = index % 6;
            let description = if index % 10 == 0 {
                format!("人物速写 第{index}张")
            } else {
                format!("参考素材 编号{index}")
            };
            image_statement.execute(params![
                content_hash,
                folder_id,
                file_size,
                phash.as_slice(),
                colors4.as_slice(),
                colors8.as_slice(),
                score,
                description,
                index,
            ])?;
            let image_id = transaction.last_insert_rowid();
            for offset in 0..TAGS_PER_IMAGE {
                let tag_id = tag_ids[((index + offset) % TAG_COUNT) as usize];
                tag_link_statement.execute(params![image_id, tag_id])?;
            }
        }
    }
    transaction.execute_batch("ANALYZE;")?;
    transaction.commit()?;
    Ok(start.elapsed())
}

#[test]
#[ignore = "20 万行规模基准；用 cargo test -- --ignored --nocapture perf_metadata_200k 运行"]
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
    let groups = index.phash_groups(6).expect("phash groups");
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
    println!(
        "[perf] reopen library + counts: {:.2}ms (active={})",
        ms(reopen_elapsed),
        reopened.info.image_count
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
        sort: SortSpec {
            key: SortKey::Score,
            descending: true,
        },
        cursor: Some(QueryCursor {
            sort_key: "2".into(),
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
        sort: SortSpec {
            key: SortKey::CollectionPosition,
            descending: false,
        },
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
#[ignore = "缩略图包 vs 逐文件读取；用 cargo test -- --ignored --nocapture perf_thumb_pack_read 运行"]
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
