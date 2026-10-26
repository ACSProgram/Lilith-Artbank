//! 压缩执行的实测校准：把上次执行的写入、fsync、提交与删除耗时写进本库 `meta` 表，
//! 下次估算优先用这些值替换保守常数。
//!
//! 估算本身仍然只读；校准值是“上次执行反馈”，不写临时文件，也不改 schema。

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::app::error::AppResult;
use crate::library::db::LibraryDb;

pub const META_KEY: &str = "optimize.calibration.v1";

/// 一次执行里与写入侧有关的实测数据；由 `library::optimize` 汇总后传进来。
#[derive(Debug, Clone, Copy, Default)]
pub struct RunSample {
    pub processed: u64,
    pub output_bytes: u64,
    pub batches: u64,
    /// 写临时文件 + fsync + 持久化 rename + 目录 flush 的墙钟秒数。
    pub write_secs: f64,
    /// 其中的 `sync_all` 部分，用来拆出每张 fsync 成本；估算里它单独按张计。
    pub fsync_secs: f64,
    pub commit_secs: f64,
    pub delete_secs: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimizeCalibration {
    /// 每输出字节的写盘秒数；不含 fsync（fsync 按张单独计）。
    pub write_secs_per_byte: f64,
    /// 每张的 fsync 秒数。
    pub fsync_secs_per_image: f64,
    /// 每批提交的秒数。
    pub commit_secs_per_batch: f64,
    /// 每张删除旧图与预览的秒数。
    pub delete_secs_per_image: f64,
}

/// 保守常数：没有本库校准记录时使用。
///
/// 取“普通 SSD”的量级（写入 300 MB/s、每张 fsync 1.5 ms），不是最坏磁盘。
/// 首次执行结束后会用实测值覆盖；慢盘（机械硬盘）的第一次估算会偏低，
/// 从第二次起按本库校准。
pub const CONSERVATIVE: OptimizeCalibration = OptimizeCalibration {
    write_secs_per_byte: 1.0 / (300.0 * 1024.0 * 1024.0),
    fsync_secs_per_image: 0.0015,
    commit_secs_per_batch: 0.02,
    delete_secs_per_image: 0.0005,
};

impl OptimizeCalibration {
    pub fn load(database: &LibraryDb) -> AppResult<Option<Self>> {
        let raw: Option<String> = database.with_reader(|connection| {
            Ok(connection
                .query_row("SELECT value FROM meta WHERE key = ?1", [META_KEY], |row| {
                    row.get(0)
                })
                .optional()?)
        })?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        // 记录损坏时退回保守常数，不让一条坏 JSON 卡住估算。
        Ok(serde_json::from_str::<Self>(&raw)
            .ok()
            .filter(|value| value.is_sane()))
    }

    pub fn save(&self, database: &LibraryDb) -> AppResult<()> {
        let value = serde_json::to_string(self)?;
        database.with_writer(|connection| {
            connection.execute(
                "INSERT INTO meta(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![META_KEY, value],
            )?;
            Ok(())
        })
    }

    /// 由一次执行推导；样本太少或数值明显异常时返回 `None`，保留上一次校准。
    pub fn from_run(run: &RunSample) -> Option<Self> {
        // 少于 20 张或没有批次的执行噪声太大，不值得覆盖已有校准。
        if run.processed < 20 || run.batches == 0 || run.output_bytes == 0 {
            return None;
        }
        let processed = run.processed as f64;
        // fsync 在估算里按张单独计价，这里要先从写盘耗时里扣掉，否则会被算两遍。
        let byte_write_secs = (run.write_secs - run.fsync_secs).max(0.0);
        let value = Self {
            write_secs_per_byte: byte_write_secs / run.output_bytes as f64,
            fsync_secs_per_image: run.fsync_secs / processed,
            commit_secs_per_batch: run.commit_secs / run.batches as f64,
            delete_secs_per_image: run.delete_secs / processed,
        };
        value.is_sane().then_some(value)
    }

    /// 每项都落在合理量级内；超出说明计时被别的任务干扰或磁盘异常。
    fn is_sane(&self) -> bool {
        let in_range =
            |value: f64, low: f64, high: f64| value.is_finite() && (low..=high).contains(&value);
        in_range(self.write_secs_per_byte, 1e-10, 1e-4)
            && in_range(self.fsync_secs_per_image, 1e-5, 0.2)
            && in_range(self.commit_secs_per_batch, 1e-3, 5.0)
            && in_range(self.delete_secs_per_image, 1e-6, 0.05)
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::library::service;

    fn session() -> (tempfile::TempDir, service::LibrarySession) {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "校准测试", 1).unwrap();
        (temp, session)
    }

    #[test]
    fn saves_and_loads_calibration_through_meta() {
        let (_temp, session) = session();
        assert_eq!(OptimizeCalibration::load(&session.db).unwrap(), None);

        let value = OptimizeCalibration {
            write_secs_per_byte: 2e-8,
            fsync_secs_per_image: 0.006,
            commit_secs_per_batch: 0.12,
            delete_secs_per_image: 0.001,
        };
        value.save(&session.db).unwrap();
        let loaded = OptimizeCalibration::load(&session.db).unwrap().unwrap();
        assert_eq!(loaded, value);

        // 再写一次是覆盖，不是插入第二行。
        let updated = OptimizeCalibration {
            commit_secs_per_batch: 0.2,
            ..value
        };
        updated.save(&session.db).unwrap();
        assert_eq!(
            OptimizeCalibration::load(&session.db).unwrap(),
            Some(updated)
        );
    }

    #[test]
    fn corrupt_or_out_of_range_records_fall_back_to_none() {
        let (_temp, session) = session();
        session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO meta(key, value) VALUES (?1, 'not json')",
                    [META_KEY],
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(OptimizeCalibration::load(&session.db).unwrap(), None);

        let absurd = OptimizeCalibration {
            write_secs_per_byte: 1.0,
            ..CONSERVATIVE
        };
        absurd.save(&session.db).unwrap();
        assert_eq!(OptimizeCalibration::load(&session.db).unwrap(), None);
    }

    #[test]
    fn derives_calibration_only_from_a_meaningful_run() {
        let run = RunSample {
            processed: 100,
            output_bytes: 10_000_000,
            batches: 1,
            write_secs: 0.5,
            fsync_secs: 0.2,
            commit_secs: 0.3,
            delete_secs: 0.1,
        };
        let value = OptimizeCalibration::from_run(&run).unwrap();
        // 写盘按字节的部分要先扣掉 fsync：(0.5 − 0.2) / 10 MB。
        assert!((value.write_secs_per_byte - 3e-8).abs() < 1e-12);
        assert!((value.fsync_secs_per_image - 0.002).abs() < 1e-9);
        assert!((value.commit_secs_per_batch - 0.3).abs() < 1e-9);
        assert!((value.delete_secs_per_image - 0.001).abs() < 1e-9);

        // 太少或没有输出字节的执行不覆盖校准。
        assert!(
            OptimizeCalibration::from_run(&RunSample {
                processed: 5,
                ..run
            })
            .is_none()
        );
        assert!(
            OptimizeCalibration::from_run(&RunSample {
                output_bytes: 0,
                ..run
            })
            .is_none()
        );
        assert!(OptimizeCalibration::from_run(&RunSample { batches: 0, ..run }).is_none());
    }
}
