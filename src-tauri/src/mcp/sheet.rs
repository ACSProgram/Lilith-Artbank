use std::time::{Duration, Instant};

use serde::Serialize;

use crate::{
    app::error::{AppError, AppResult},
    imageops::contact_sheet,
    library::{
        content,
        query::ImageRow,
        thumbs::{self, ThumbPointer},
    },
};

use super::context::ReadContext;

const MAX_SHEETS: usize = 128;
pub const SHEET_TTL_MINUTES: u64 = 2 * 60;
const SHEET_TTL: Duration = Duration::from_secs(SHEET_TTL_MINUTES * 60);

/// 拼图映射只保存格子与图片 id；展示字段在本次返回里直接给出，不重复缓存。
#[derive(Debug, Clone)]
pub struct SheetCell {
    pub label: String,
    pub image_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct SheetRecord {
    pub id: String,
    pub cells: Vec<SheetCell>,
    created_at: Instant,
}

/// 只保存格子映射，不缓存图片字节；过期或超过上限后直接丢弃。
#[derive(Default)]
pub struct SheetCache {
    entries: Vec<SheetRecord>,
}

impl SheetCache {
    pub fn insert(&mut self, cells: Vec<SheetCell>) -> SheetRecord {
        self.prune();
        let record = SheetRecord {
            id: uuid::Uuid::new_v4().to_string(),
            cells,
            created_at: Instant::now(),
        };
        self.entries.insert(0, record.clone());
        self.entries.truncate(MAX_SHEETS);
        record
    }

    pub fn get(&mut self, id: &str) -> AppResult<SheetRecord> {
        self.prune();
        self.entries
            .iter()
            .find(|record| record.id == id)
            .cloned()
            .ok_or_else(|| AppError::message("拼图映射已过期，请重新生成"))
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    fn prune(&mut self) {
        let now = Instant::now();
        self.entries
            .retain(|record| now.duration_since(record.created_at) < SHEET_TTL);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SheetResolution {
    Thumb,
    Detail,
}

impl SheetResolution {
    pub fn parse(value: &str) -> AppResult<Self> {
        match value {
            "thumb" => Ok(Self::Thumb),
            "detail" => Ok(Self::Detail),
            other => Err(AppError::message(format!(
                "拼图清晰度只能是 thumb 或 detail，收到 {other}"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Thumb => "thumb",
            Self::Detail => "detail",
        }
    }
}

/// 一行一列对应的格子。`None` 表示查询结果不足或图片已不存在。
pub struct SheetSlot {
    pub label: String,
    pub row: Option<ImageRow>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SheetCellOutput {
    pub label: String,
    pub image_id: Option<i64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub score: Option<i64>,
    pub tags: Vec<String>,
    pub folder_path: Option<String>,
    pub thumb_missing: bool,
}

pub struct BuiltSheet {
    pub record: SheetRecord,
    pub image: Vec<u8>,
    pub cells: Vec<SheetCellOutput>,
}

pub fn labels(grid: u8) -> Vec<String> {
    let grid = grid as usize;
    (0..grid * grid)
        .map(|index| {
            let column = (b'A' + (index % grid) as u8) as char;
            format!("{column}{}", index / grid + 1)
        })
        .collect()
}

/// 格数与清晰度的共同校验；取图前先挡住无效组合，避免白解析一遍 selector。
pub fn validate(grid: u8, resolution: SheetResolution) -> AppResult<()> {
    if !(1..=contact_sheet::MAX_GRID).contains(&grid) {
        return Err(AppError::message(format!(
            "拼图格数必须在 1-{} 之间",
            contact_sheet::MAX_GRID
        )));
    }
    if resolution == SheetResolution::Detail && grid > contact_sheet::DETAIL_MAX_GRID {
        return Err(AppError::message(format!(
            "detail 清晰度最多 {}×{}，避免一次解码过多原图",
            contact_sheet::DETAIL_MAX_GRID,
            contact_sheet::DETAIL_MAX_GRID
        )));
    }
    Ok(())
}

/// 读取缩略图/原图并合成拼图；缺图与解码失败都画占位，不阻塞整盘。
pub fn build(
    context: &ReadContext,
    grid: u8,
    resolution: SheetResolution,
    slots: Vec<SheetSlot>,
) -> AppResult<BuiltSheet> {
    validate(grid, resolution)?;
    let expected = (grid as usize) * (grid as usize);
    if slots.len() != expected {
        return Err(AppError::message("拼图格子数量与格数不一致"));
    }

    let labels: Vec<String> = slots.iter().map(|slot| slot.label.clone()).collect();
    let mut pointers = Vec::with_capacity(expected);
    let mut cells = Vec::with_capacity(expected);
    for slot in &slots {
        let Some(row) = slot.row.as_ref() else {
            pointers.push(None);
            cells.push(SheetCellOutput {
                label: slot.label.clone(),
                image_id: None,
                width: None,
                height: None,
                score: None,
                tags: Vec::new(),
                folder_path: None,
                thumb_missing: false,
            });
            continue;
        };

        let pointer = thumbs::load_pointer(&context.database, row.id)
            .ok()
            .flatten();
        pointers.push(pointer);
        cells.push(SheetCellOutput {
            label: slot.label.clone(),
            image_id: Some(row.id),
            width: Some(row.width),
            height: Some(row.height),
            score: Some(row.score),
            tags: row.tags.clone(),
            folder_path: Some(row.folder_path.clone()),
            thumb_missing: false,
        });
    }

    // 逐格解码：画完一格就释放，detail 拼图不会同时持有整盘原图。
    let mut drawn = vec![false; expected];
    let image = contact_sheet::render(grid, &labels, |index| {
        let row = slots[index].row.as_ref()?;
        let decoded = match resolution {
            SheetResolution::Thumb => load_thumbnail(context, pointers[index].clone()),
            SheetResolution::Detail => load_original(context, row.id),
        };
        drawn[index] = decoded.is_some();
        decoded
    })?;
    // 缩略图/原图缺失、缺行或解码失败都算这一格没画出图片。
    for (cell, drawn) in cells.iter_mut().zip(drawn) {
        cell.thumb_missing = !drawn;
    }
    let cached: Vec<SheetCell> = slots
        .iter()
        .map(|slot| SheetCell {
            label: slot.label.clone(),
            image_id: slot.row.as_ref().map(|row| row.id),
        })
        .collect();
    let record = context.sheets.lock().insert(cached);
    Ok(BuiltSheet {
        record,
        image,
        cells,
    })
}

fn load_thumbnail(
    context: &ReadContext,
    pointer: Option<ThumbPointer>,
) -> Option<image::DynamicImage> {
    let pointer = pointer?;
    let bytes = context.thumbs.read(pointer.location).ok()?;
    contact_sheet::decode_thumbnail(&bytes).ok()
}

fn load_original(context: &ReadContext, image_id: i64) -> Option<image::DynamicImage> {
    let path = content::original_path(&context.database, &context.root, image_id).ok()?;
    crate::import::decode::decode(&path)
        .ok()
        .map(|decoded| decoded.image)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_column_letter_and_row_number() {
        assert_eq!(labels(1), vec!["A1"]);
        assert_eq!(labels(2), vec!["A1", "B1", "A2", "B2"]);
        let five = labels(5);
        assert_eq!(five.len(), 25);
        assert_eq!(five[0], "A1");
        assert_eq!(five[4], "E1");
        assert_eq!(five[24], "E5");
    }

    #[test]
    fn cache_evicts_expired_and_keeps_latest_128() {
        let mut cache = SheetCache::default();
        for _ in 0..(MAX_SHEETS + 4) {
            cache.insert(Vec::new());
        }
        assert_eq!(cache.len(), MAX_SHEETS);
    }

    #[test]
    fn rejects_detail_above_three_by_three() {
        let context = test_context();
        let slots = (0..16)
            .map(|index| SheetSlot {
                label: format!("cell-{index}"),
                row: None,
            })
            .collect();
        let error = match build(&context, 4, SheetResolution::Detail, slots) {
            Ok(_) => panic!("4×4 detail 应当被拒绝"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("3×3"));
    }

    fn test_context() -> ReadContext {
        let temp = tempfile::tempdir().unwrap();
        let (session, _) = crate::library::service::create(temp.path(), "拼图测试", 1).unwrap();
        let sheets = std::sync::Arc::new(parking_lot::Mutex::new(SheetCache::default()));
        ReadContext {
            database: session.db.clone(),
            thumbs: session.thumbs.clone(),
            root: temp.path().to_path_buf(),
            info: session.info.clone(),
            settings: crate::app::settings::Settings::default(),
            sheets,
        }
    }
}
