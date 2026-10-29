//! 合成图库数据种子：性能基准与无头验收共用。
//!
//! 行内容的生成必须保持稳定——`perf_metadata_200k` 的基线数字直接建立在
//! 这里的哈希、主色与描述分布上；改动生成公式要同步复核基准。

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
};

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::{
    app::error::{AppError, AppResult},
    library::paths::{normalize_content_extension, normalize_folder},
};

/// 一次种子的输入。`image_folders` 为空时图片轮转分配到 `folders`；
/// 非空时这些路径连同父级也会补进文件夹表。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SeedSpec {
    pub folders: Vec<String>,
    pub image_folders: Vec<String>,
    pub tags: Vec<String>,
    pub images: usize,
    pub tags_per_image: usize,
    /// 文件扩展名轮换；同时决定行里的 `file_ext` 与 `format`。
    pub extensions: Vec<String>,
    /// 为每张图写一个 1 字节占位文件，文件名是内容哈希。
    pub write_files: bool,
}

impl Default for SeedSpec {
    fn default() -> Self {
        Self {
            folders: Vec::new(),
            image_folders: Vec::new(),
            tags: Vec::new(),
            images: 0,
            tags_per_image: 1,
            extensions: vec!["jpg".into()],
            write_files: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeedReport {
    pub folders: usize,
    pub images: usize,
    pub tags: usize,
    pub files: usize,
}

/// 把行与可选占位文件写入一个已打开的空图库。
pub(crate) fn seed(
    connection: &mut Connection,
    spec: &SeedSpec,
    library_root: &Path,
) -> AppResult<SeedReport> {
    let image_folders = if spec.image_folders.is_empty() {
        Vec::new()
    } else {
        spec.image_folders
            .iter()
            .map(|path| normalize_folder(path))
            .collect::<AppResult<Vec<_>>>()?
    };
    // 图片文件夹必须出现在文件夹表里；先合并再展开，父级与去重一次完成。
    let mut folder_inputs = spec.folders.clone();
    folder_inputs.extend(image_folders.iter().cloned());
    let folders = expand_folders(&folder_inputs)?;
    let image_folders = if image_folders.is_empty() {
        folders.clone()
    } else {
        image_folders
    };
    if spec.images > 0 && image_folders.is_empty() {
        return Err(AppError::message("种子需要至少一个图片文件夹"));
    }
    let extensions = if spec.extensions.is_empty() {
        vec!["jpg".to_owned()]
    } else {
        spec.extensions
            .iter()
            .map(|ext| normalize_content_extension(ext))
            .collect::<AppResult<Vec<_>>>()?
    };

    let transaction = connection.transaction()?;
    let mut folder_ids: HashMap<String, i64> = HashMap::new();
    {
        let mut insert = transaction
            .prepare("INSERT OR IGNORE INTO folders(path, created_at) VALUES (?1, ?2)")?;
        let mut lookup = transaction.prepare("SELECT id FROM folders WHERE path = ?1")?;
        for path in &folders {
            insert.execute(params![path, 1_i64])?;
            let id: i64 = lookup.query_row([path], |row| row.get(0))?;
            folder_ids.insert(path.clone(), id);
        }
    }
    {
        let mut insert = transaction.prepare("INSERT OR IGNORE INTO tags(name) VALUES (?1)")?;
        for name in &spec.tags {
            insert.execute([name])?;
        }
    }
    let tag_ids: Vec<i64> = {
        let mut statement = transaction.prepare("SELECT id FROM tags ORDER BY id")?;
        statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut files = Vec::with_capacity(if spec.write_files { spec.images } else { 0 });
    {
        let mut image_statement = transaction.prepare(
            "INSERT INTO images(
                content_hash, file_ext, folder_id, file_size, width, height, format,
                has_alpha, phash, colors4, colors8, score, description, imported_at, modified_at
             ) VALUES (?1, ?2, ?3, ?4, 1920, 1080, ?5, 0, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
        )?;
        let mut tag_link_statement =
            transaction.prepare("INSERT INTO image_tags(image_id, tag_id) VALUES (?1, ?2)")?;
        for index in 0..spec.images {
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
            let extension = &extensions[index % extensions.len()];
            let folder_path = &image_folders[index % image_folders.len()];
            let folder_id = folder_ids[folder_path];
            let file_size = if spec.write_files {
                1
            } else {
                100_000 + (index % 1_000) * 1_000
            };
            let score = index % 6;
            let description = if index % 10 == 0 {
                format!("人物速写 第{index}张")
            } else {
                format!("参考素材 编号{index}")
            };
            image_statement.execute(params![
                content_hash,
                extension,
                folder_id,
                file_size,
                format_for_extension(extension),
                phash.as_slice(),
                colors4.as_slice(),
                colors8.as_slice(),
                score,
                description,
                index as i64,
            ])?;
            let image_id = transaction.last_insert_rowid();
            if !tag_ids.is_empty() {
                for offset in 0..spec.tags_per_image {
                    let tag_id = tag_ids[(index + offset) % tag_ids.len()];
                    tag_link_statement.execute(params![image_id, tag_id])?;
                }
            }
            if spec.write_files {
                files.push((folder_path.clone(), format!("{content_hash}.{extension}")));
            }
        }
    }
    transaction.execute_batch("ANALYZE;")?;
    transaction.commit()?;

    for (folder, file_name) in &files {
        let directory = if folder.is_empty() {
            library_root.to_path_buf()
        } else {
            library_root.join(folder.replace('/', std::path::MAIN_SEPARATOR_STR))
        };
        fs::create_dir_all(&directory)?;
        fs::write(directory.join(file_name), [0_u8])?;
    }

    Ok(SeedReport {
        folders: folders.len(),
        images: spec.images,
        tags: spec.tags.len(),
        files: files.len(),
    })
}

/// 去重并补齐父路径；父级总在子级之前。
fn expand_folders(raw: &[String]) -> AppResult<Vec<String>> {
    let mut seen = HashSet::new();
    let mut expanded = Vec::new();
    for value in raw {
        let path = normalize_folder(value)?;
        if path.is_empty() {
            continue;
        }
        let mut prefix = String::new();
        for segment in path.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            if seen.insert(prefix.clone()) {
                expanded.push(prefix.clone());
            }
        }
    }
    Ok(expanded)
}

fn format_for_extension(extension: &str) -> &'static str {
    match extension {
        "jpg" | "jpeg" => "jpeg",
        "png" => "png",
        "webp" => "webp",
        "gif" => "gif",
        "bmp" => "bmp",
        "tga" => "tga",
        "tif" | "tiff" => "tiff",
        _ => "jpeg",
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::library::service;

    #[test]
    fn seeds_rows_and_placeholder_files() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "种子测试", 2).unwrap();
        let spec = SeedSpec {
            folders: vec!["人物/速写".into(), "场景".into()],
            tags: vec!["红".into(), "蓝".into()],
            images: 12,
            tags_per_image: 2,
            extensions: vec!["jpg".into(), "png".into()],
            write_files: true,
            ..SeedSpec::default()
        };
        let report = session
            .db
            .with_writer(|connection| seed(connection, &spec, &root))
            .unwrap();
        assert_eq!(report.folders, 3);
        assert_eq!(report.images, 12);
        assert_eq!(report.tags, 2);
        assert_eq!(report.files, 12);
        assert!(root.join("人物").join("速写").is_dir());

        let (_, trash, temp_count) = session.db.counts().unwrap();
        assert_eq!((trash, temp_count), (0, 0));
        let files = fs::read_dir(root.join("场景")).unwrap().count();
        assert_eq!(files, 4);
    }

    #[test]
    fn refuses_images_without_folders() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "种子测试", 2).unwrap();
        let spec = SeedSpec {
            images: 1,
            ..SeedSpec::default()
        };
        assert!(
            session
                .db
                .with_writer(|connection| seed(connection, &spec, &root))
                .is_err()
        );
    }
}
