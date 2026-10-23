use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    time::{Duration, Instant},
};

use rusqlite::{Connection, params};

use crate::{app::error::AppResult, library::service};

const IMAGE_COUNT: i64 = 200_000;
const TAG_COUNT: i64 = 200;
const TAGS_PER_IMAGE: i64 = 3;
const PAGE_SIZE: i64 = 150;

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
    {
        let mut folder_statement =
            transaction.prepare("INSERT INTO folders(path, created_at) VALUES (?1, ?2)")?;
        for index in 0..20 {
            folder_statement.execute(params![format!("folder{index}"), 1_i64])?;
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
        let phash = vec![0x5a_u8; 32];
        let colors4 = vec![200_u8; 16];
        let colors8 = vec![180_u8; 32];
        for index in 0..IMAGE_COUNT {
            let content_hash = format!("{index:064x}");
            let folder_id = 2 + (index % 20);
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
                phash,
                colors4,
                colors8,
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
    let seed_elapsed = database
        .with_writer(|connection| seed(connection))
        .expect("seed database");
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
