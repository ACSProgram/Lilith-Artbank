use std::{
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{
    app::error::{AppError, AppResult},
    imageops::{
        colors, edit,
        edit::{CropRect, Geometry, MAX_EDIT_PIXELS, OutputFormat, ResizeSpec},
        phash, thumbnail,
    },
    import::decode,
    library::{
        content::{self, ContentRow},
        db::LibraryDb,
        metadata,
        paths::LibraryPaths,
        service::now_ms,
        thumbs::ThumbPack,
        viewer,
    },
};

const UNCHANGED_HINT: &str = "没有需要保存的像素修改";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditRequest {
    pub image_id: i64,
    #[serde(default)]
    pub rotate: u8,
    #[serde(default)]
    pub flip_horizontal: bool,
    #[serde(default)]
    pub flip_vertical: bool,
    #[serde(default)]
    pub crop: Option<CropRect>,
    #[serde(default)]
    pub resize: Option<ResizeSpec>,
    #[serde(default)]
    pub output_format: Option<String>,
    #[serde(default)]
    pub quality: Option<u8>,
}

impl EditRequest {
    fn geometry(&self) -> Geometry {
        Geometry {
            rotate: self.rotate,
            flip_horizontal: self.flip_horizontal,
            flip_vertical: self.flip_vertical,
            crop: self.crop,
            resize: self.resize,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditResult {
    pub image_id: i64,
    pub content_hash: String,
    pub file_ext: String,
    pub file_size: i64,
    pub width: u32,
    pub height: u32,
    pub format: String,
    pub has_alpha: bool,
    pub thumb_version: i64,
    pub changed: bool,
    pub old_file_removed: bool,
}

/// 临时文件句柄：作用域结束时自动删除，成功发布后 `disarm` 交给目标文件。
struct TempFile(Option<PathBuf>);

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl TempFile {
    fn new(path: PathBuf) -> Self {
        Self(Some(path))
    }

    fn path(&self) -> &Path {
        self.0.as_deref().expect("临时文件路径在 disarm 前始终可用")
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

/// 编辑保存：新内容 → 新文件 → 单事务更新行 → 提交后清理旧文件。
pub fn save(
    database: &LibraryDb,
    root: &Path,
    thumbs: &ThumbPack,
    request: &EditRequest,
) -> AppResult<EditResult> {
    let geometry = request.geometry();
    geometry.validate()?;
    let row = content::load(database, request.image_id)?;
    if row.deleted_at.is_some() {
        return Err(AppError::message("回收站中的图片不能编辑"));
    }
    if row.pixel_count() > MAX_EDIT_PIXELS {
        return Err(AppError::message(format!(
            "图片为 {}×{}，超过编辑上限 {} 百万像素",
            row.width,
            row.height,
            MAX_EDIT_PIXELS / 1_000_000
        )));
    }

    let output_format =
        OutputFormat::parse(request.output_format.as_deref(), &row.format, row.has_alpha)?;
    let quality = request.quality.unwrap_or(edit::DEFAULT_QUALITY);
    if !(1..=100).contains(&quality) {
        return Err(AppError::message("输出质量必须在 1-100 之间"));
    }
    if !edit::has_changes(&geometry, &row.format, output_format) {
        return Err(AppError::message(UNCHANGED_HINT));
    }

    let paths = LibraryPaths::new(root);
    let source = row.path(&paths)?;
    let decoded = decode::decode(&source)?;
    if u64::from(decoded.width) * u64::from(decoded.height) > MAX_EDIT_PIXELS {
        return Err(AppError::message(format!(
            "原图解码后为 {}×{}，超过编辑上限",
            decoded.width, decoded.height
        )));
    }

    let processed = edit::apply(decoded.image, &geometry)?;
    let (width, height) = (processed.width(), processed.height());
    let has_alpha = output_format != OutputFormat::Jpeg && processed.color().has_alpha();

    let mut temp = TempFile::new(paths.temp.join(format!(
        "edit-{}-{}.{}",
        row.id,
        uuid::Uuid::new_v4(),
        output_format.extension()
    )));
    let (new_hash, new_size) = write_encoded(temp.path(), &processed, output_format, quality)?;
    if new_hash == row.content_hash {
        return Ok(unchanged(&row, false));
    }

    let destination = metadata::content_path(
        &paths,
        &row.folder_path,
        &new_hash,
        output_format.extension(),
    )?;
    let thumb = thumbnail::encode(&processed)?;
    let hash = phash::compute(&processed);
    let (colors4, colors8) = colors::dominant(&processed);
    let file_ext = output_format.extension().to_owned();
    let file_size =
        i64::try_from(new_size).map_err(|_| AppError::message("编辑结果大小超出数据库范围"))?;

    let mut published = false;
    let commit = database.with_writer(|connection| {
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let conflict: Option<i64> = transaction
            .query_row(
                "SELECT id FROM images WHERE content_hash = ?1 AND id <> ?2",
                params![new_hash, row.id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(other) = conflict {
            return Err(AppError::message(format!(
                "编辑结果与图片 #{other} 的内容相同，已拒绝保存；请调整裁剪或输出设置"
            )));
        }

        published = publish(temp.path(), &destination)?;
        let location = thumbs.append(&thumb.bytes)?;
        let now = now_ms();

        let changed = transaction.execute(
            "UPDATE images SET
                content_hash = ?1, file_ext = ?2, file_size = ?3, width = ?4, height = ?5,
                format = ?6, has_alpha = ?7, phash = ?8, colors4 = ?9, colors8 = ?10,
                modified_at = ?11, thumb_segment = ?12, thumb_offset = ?13,
                thumb_length = ?14, thumb_format = 'webp', thumb_width = ?15,
                thumb_height = ?16, thumb_version = thumb_version + 1
             WHERE id = ?17",
            params![
                new_hash,
                file_ext,
                file_size,
                width,
                height,
                output_format.name(),
                has_alpha,
                hash.as_slice(),
                colors4.as_slice(),
                colors8.as_slice(),
                now,
                location.segment,
                i64::try_from(location.offset)
                    .map_err(|_| AppError::message("缩略图偏移超出数据库范围"))?,
                location.length,
                thumb.width,
                thumb.height,
                row.id,
            ],
        )?;
        if changed == 0 {
            return Err(AppError::message("图片不存在或已被删除"));
        }
        // 内容已变，旧的 embedding/pose 不再有效。
        transaction.execute("DELETE FROM feature_blobs WHERE image_id = ?1", [row.id])?;
        crate::library::import::ensure_thumb_segment(&transaction, location.segment, now)?;
        if let Ok(size) = thumbs.segment_size_bytes(location.segment) {
            crate::library::import::update_thumb_segment_size(
                &transaction,
                location.segment,
                size,
            )?;
        }
        transaction.commit()?;
        Ok(())
    });

    if let Err(error) = commit {
        if published {
            let _ = std::fs::remove_file(&destination);
        }
        return Err(error);
    }
    temp.disarm();

    // 旧哈希已不再被任何行引用；删除失败不影响保存结果，留给后续维护清理。
    let still_referenced: bool = database.with_reader(|connection| {
        Ok(connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM images WHERE content_hash = ?1)",
            [&row.content_hash],
            |row| row.get(0),
        )?)
    })?;
    let old_file_removed = if !still_referenced {
        match metadata::remove_content_file(root, &source) {
            Ok(()) => true,
            Err(error) => {
                log::warn!(
                    "旧原图清理失败 image={} path={}: {error}",
                    row.id,
                    source.display()
                );
                false
            }
        }
    } else {
        false
    };
    let _ = std::fs::remove_file(viewer::preview_path(&paths, row.id, &row.content_hash));

    Ok(EditResult {
        image_id: row.id,
        content_hash: new_hash,
        file_ext,
        file_size,
        width,
        height,
        format: output_format.name().to_owned(),
        has_alpha,
        thumb_version: row.thumb_version + 1,
        changed: true,
        old_file_removed,
    })
}

fn unchanged(row: &ContentRow, old_file_removed: bool) -> EditResult {
    EditResult {
        image_id: row.id,
        content_hash: row.content_hash.clone(),
        file_ext: row.file_ext.clone(),
        file_size: row.file_size,
        width: row.width,
        height: row.height,
        format: row.format.clone(),
        has_alpha: row.has_alpha,
        thumb_version: row.thumb_version,
        changed: false,
        old_file_removed,
    }
}

struct HashingFile {
    file: File,
    hasher: blake3::Hasher,
    size: u64,
}

impl HashingFile {
    fn create(path: &Path) -> AppResult<Self> {
        Ok(Self {
            file: File::create(path)?,
            hasher: blake3::Hasher::new(),
            size: 0,
        })
    }

    fn finish(mut self) -> AppResult<(String, u64)> {
        self.file.flush()?;
        self.file.sync_all()?;
        Ok((self.hasher.finalize().to_hex().to_string(), self.size))
    }
}

impl Write for HashingFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.file.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        self.size += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn write_encoded(
    path: &Path,
    image: &image::DynamicImage,
    format: OutputFormat,
    quality: u8,
) -> AppResult<(String, u64)> {
    let mut writer = HashingFile::create(path)?;
    edit::encode(image, format, quality, &mut writer)?;
    writer.finish()
}

fn publish(temp: &Path, destination: &Path) -> AppResult<bool> {
    if destination.is_file() {
        std::fs::remove_file(temp)?;
        return Ok(false);
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::rename(temp, destination) {
        Ok(()) => Ok(true),
        Err(_) if destination.is_file() => {
            let _ = std::fs::remove_file(temp);
            Ok(false)
        }
        Err(error) => Err(AppError::message(format!("发布编辑结果失败：{error}"))),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::params;
    use tempfile::tempdir;

    use super::*;
    use crate::library::service;

    struct Fixture {
        _temp: tempfile::TempDir,
        session: service::LibrarySession,
        root: PathBuf,
        image_id: i64,
        old_hash: String,
        old_path: PathBuf,
    }

    fn fixture() -> Fixture {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "编辑测试", 1).unwrap();
        let mut image = image::RgbaImage::new(24, 16);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = image::Rgba([(x * 9) as u8, (y * 13) as u8, 70, 255]);
        }
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let path = root.join(format!("{hash}.png"));
        fs::write(&path, &bytes).unwrap();

        let image_id = session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO images(
                        content_hash, file_ext, folder_id, file_size, width, height, format,
                        has_alpha, phash, colors4, colors8, score, description, imported_at,
                        modified_at, thumb_version
                     ) VALUES (?1, 'png', 1, ?2, 24, 16, 'png', 1,
                        zeroblob(32), zeroblob(16), zeroblob(32), 4, '保留描述', 1, 1, 3)",
                    params![hash, bytes.len() as i64],
                )?;
                let id = connection.last_insert_rowid();
                connection.execute("INSERT INTO tags(name) VALUES ('保留标签')", [])?;
                connection.execute(
                    "INSERT INTO image_tags(image_id, tag_id)
                     SELECT ?1, id FROM tags WHERE name = '保留标签'",
                    [id],
                )?;
                connection.execute(
                    "INSERT INTO collections(id, name, sort_order, created_at, updated_at)
                     VALUES (1, '保留集合', 0, 1, 1)",
                    [],
                )?;
                connection.execute(
                    "INSERT INTO collection_items(collection_id, image_id, position)
                     VALUES (1, ?1, 0)",
                    [id],
                )?;
                connection.execute(
                    "INSERT INTO feature_blobs(image_id, kind, model, dim, pack_name,
                        pack_offset, pack_length, updated_at)
                     VALUES (?1, 'embedding', 'm', 3, 'f.bin', 0, 12, 1)",
                    [id],
                )?;
                Ok(id)
            })
            .unwrap();

        Fixture {
            _temp: temp,
            session,
            root,
            image_id,
            old_hash: hash,
            old_path: path,
        }
    }

    fn rotate_request(image_id: i64) -> EditRequest {
        EditRequest {
            image_id,
            rotate: 1,
            flip_horizontal: false,
            flip_vertical: false,
            crop: None,
            resize: None,
            output_format: None,
            quality: Some(90),
        }
    }

    #[test]
    fn saves_new_content_and_preserves_metadata() {
        let fixture = fixture();
        let preview = viewer::preview_path(
            &LibraryPaths::new(&fixture.root),
            fixture.image_id,
            &fixture.old_hash,
        );
        fs::create_dir_all(preview.parent().unwrap()).unwrap();
        fs::write(&preview, b"old-preview").unwrap();

        let result = save(
            &fixture.session.db,
            &fixture.root,
            &fixture.session.thumbs,
            &rotate_request(fixture.image_id),
        )
        .unwrap();
        assert!(result.changed);
        assert!(result.old_file_removed);
        assert_eq!((result.width, result.height), (16, 24));
        assert_eq!(result.thumb_version, 4);
        assert_ne!(result.content_hash, fixture.old_hash);
        assert!(!fixture.old_path.exists());
        assert!(!preview.exists());
        assert!(
            LibraryPaths::new(&fixture.root)
                .content_file("", &format!("{}.png", result.content_hash))
                .is_file()
        );

        let details =
            metadata::image_details(&fixture.session.db, &fixture.root, fixture.image_id).unwrap();
        assert_eq!((details.width, details.height), (16, 24));
        assert_eq!(details.content_hash, result.content_hash);
        assert_eq!(details.score, 4);
        assert_eq!(details.description, "保留描述");
        assert_eq!(details.tags.len(), 1);
        assert_eq!(details.thumb_version, 4);

        let leftovers: (i64, i64, i64) = fixture
            .session
            .db
            .with_reader(|connection| {
                Ok((
                    connection.query_row("SELECT count(*) FROM collection_items", [], |row| {
                        row.get(0)
                    })?,
                    connection
                        .query_row("SELECT count(*) FROM feature_blobs", [], |row| row.get(0))?,
                    connection.query_row(
                        "SELECT length(phash) FROM images WHERE id = ?1",
                        [fixture.image_id],
                        |row| row.get(0),
                    )?,
                ))
            })
            .unwrap();
        assert_eq!(leftovers, (1, 0, 32));

        let pointer = crate::library::thumbs::load_pointer(&fixture.session.db, fixture.image_id)
            .unwrap()
            .unwrap();
        let bytes = fixture.session.thumbs.read(pointer.location).unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert!(decoded.width() > 0 && decoded.height() > 0);
    }

    #[test]
    fn rejects_request_without_pixel_changes() {
        let fixture = fixture();
        let request = EditRequest {
            rotate: 0,
            ..rotate_request(fixture.image_id)
        };
        let error = save(
            &fixture.session.db,
            &fixture.root,
            &fixture.session.thumbs,
            &request,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("没有需要保存"));
        assert!(fixture.old_path.exists());
    }

    #[test]
    fn rejects_conflicting_content_hash() {
        let fixture = fixture();
        let geometry = Geometry {
            rotate: 1,
            ..Geometry::default()
        };
        let decoded = decode::decode(&fixture.old_path).unwrap();
        let processed = edit::apply(decoded.image, &geometry).unwrap();
        let mut bytes = Vec::new();
        edit::encode(&processed, OutputFormat::Png, 90, &mut bytes).unwrap();
        let hash = blake3::hash(&bytes).to_hex().to_string();
        fs::write(fixture.root.join(format!("{hash}.png")), &bytes).unwrap();
        fixture
            .session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO images(
                        content_hash, file_ext, folder_id, file_size, width, height, format,
                        has_alpha, phash, colors4, colors8, score, description, imported_at,
                        modified_at
                     ) VALUES (?1, 'png', 1, 1, 1, 1, 'png', 1,
                        zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                    [hash],
                )?;
                Ok(())
            })
            .unwrap();

        let error = save(
            &fixture.session.db,
            &fixture.root,
            &fixture.session.thumbs,
            &rotate_request(fixture.image_id),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("已拒绝保存"), "{error}");

        let hash: String = fixture
            .session
            .db
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT content_hash FROM images WHERE id = ?1",
                    [fixture.image_id],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(hash, fixture.old_hash);
        assert!(fixture.old_path.exists());
        let temp = fixture.root.join(".artbank").join("temp");
        let leftovers = fs::read_dir(temp).unwrap().count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn rejects_trashed_images() {
        let fixture = fixture();
        metadata::set_trashed(&fixture.session.db, &[fixture.image_id], true).unwrap();
        let error = save(
            &fixture.session.db,
            &fixture.root,
            &fixture.session.thumbs,
            &rotate_request(fixture.image_id),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("回收站"));
    }
}
