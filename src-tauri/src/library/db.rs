use std::{
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::{
    app::error::{AppError, AppResult},
    library::schema,
};

/// 单写连接 + 多读连接。SQLite 的 WAL 允许读写并行，写仍然必须串行。
pub struct LibraryDb {
    writer: Mutex<Connection>,
    readers: Vec<Mutex<Connection>>,
    cursor: AtomicUsize,
}

/// 活跃图片总数；`?1` 是临时文件夹 id（没有临时文件夹时传 -1）。
///
/// 单独提出来是为了让测试能断言查询计划：这条聚合必须走
/// `idx_images_visible_facets` 或直接扫表，不能再按像素索引逐行回表。
const ACTIVE_COUNT_SQL: &str =
    "SELECT count(*) FROM images WHERE deleted_at IS NULL AND folder_id <> ?1";

impl LibraryDb {
    pub fn open(path: &Path, read_connections: u32) -> AppResult<Self> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE;
        let writer = Connection::open_with_flags(path, flags)?;
        schema::configure(&writer)?;

        let read_connections = read_connections.clamp(1, 16) as usize;
        let mut readers = Vec::with_capacity(read_connections);
        for _ in 0..read_connections {
            let connection = Connection::open_with_flags(path, flags)?;
            schema::configure(&connection)?;
            connection.pragma_update(None, "query_only", true)?;
            readers.push(Mutex::new(connection));
        }

        Ok(Self {
            writer: Mutex::new(writer),
            readers,
            cursor: AtomicUsize::new(0),
        })
    }

    pub fn create(path: &Path, read_connections: u32) -> AppResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE;
        let writer = Connection::open_with_flags(path, flags)?;
        schema::configure(&writer)?;
        drop(writer);
        Self::open(path, read_connections)
    }

    pub fn with_writer<T>(
        &self,
        operation: impl FnOnce(&mut Connection) -> AppResult<T>,
    ) -> AppResult<T> {
        let mut connection = self.writer.lock();
        operation(&mut connection)
    }

    /// 在破坏性写任务（压缩、永久删除、释放）里临时把 `synchronous` 提到 `FULL`，
    /// 让提交本身落盘；结束后无论成败都恢复原值。
    ///
    /// 默认仍是 `NORMAL`：普通编辑不值得为每次提交多付一次 WAL fsync。
    pub fn with_durable_writer<T>(
        &self,
        operation: impl FnOnce(&mut Connection) -> AppResult<T>,
    ) -> AppResult<T> {
        self.with_writer(|connection| {
            let previous: i64 =
                connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
            connection.execute_batch("PRAGMA synchronous = FULL;")?;
            let result = operation(connection);
            // 恢复失败不能盖掉真正的错误：先取回结果，再决定返回什么。
            let restore = connection.execute_batch(&format!("PRAGMA synchronous = {previous};"));
            match (result, restore) {
                (Ok(value), Ok(())) => Ok(value),
                (Ok(_), Err(error)) => Err(error.into()),
                (Err(error), _) => Err(error),
            }
        })
    }

    pub fn with_reader<T>(
        &self,
        operation: impl FnOnce(&Connection) -> AppResult<T>,
    ) -> AppResult<T> {
        if self.readers.is_empty() {
            return Err(AppError::message("图库没有可用的读取连接"));
        }
        let index = self.cursor.fetch_add(1, Ordering::Relaxed) % self.readers.len();
        let connection = self.readers[index].lock();
        operation(&connection)
    }

    /// `(正常图片, 回收站, 临时文件夹)`；临时图片只计入第三个值。
    pub fn counts(&self) -> AppResult<(u64, u64, u64)> {
        self.with_reader(|connection| {
            let temp_id: Option<i64> = connection
                .query_row("SELECT id FROM folders WHERE kind = 'temp'", [], |row| {
                    row.get(0)
                })
                .optional()?;
            let temp_id = temp_id.unwrap_or(-1);
            let active: u64 =
                connection.query_row(ACTIVE_COUNT_SQL, [temp_id], |row| row.get(0))?;
            let trash: u64 = connection.query_row(
                "SELECT count(*) FROM images
                 WHERE deleted_at IS NOT NULL AND purge_operation_id IS NULL AND folder_id <> ?1",
                [temp_id],
                |row| row.get(0),
            )?;
            let temp: u64 = connection.query_row(
                "SELECT count(*) FROM images WHERE deleted_at IS NULL AND folder_id = ?1",
                [temp_id],
                |row| row.get(0),
            )?;
            Ok((active, trash, temp))
        })
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn synchronous(db: &LibraryDb) -> i64 {
        db.with_writer(|connection| {
            Ok(connection.pragma_query_value(None, "synchronous", |row| row.get(0))?)
        })
        .unwrap()
    }

    /// 破坏性写任务用 FULL 提交，结束后无论成败都恢复 NORMAL。
    #[test]
    fn durable_writer_temporarily_raises_synchronous_then_restores() {
        let temp = tempdir().unwrap();
        let db = LibraryDb::create(&temp.path().join("library.db"), 1).unwrap();
        assert_eq!(synchronous(&db), 1, "默认应是 NORMAL");

        db.with_durable_writer(|connection| {
            let level: i64 =
                connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
            assert_eq!(level, 2, "事务期间应是 FULL");
            Ok(())
        })
        .unwrap();
        assert_eq!(synchronous(&db), 1);

        let failed: AppResult<()> =
            db.with_durable_writer(|_| Err(crate::app::error::AppError::message("模拟失败")));
        assert!(failed.is_err());
        assert_eq!(synchronous(&db), 1, "失败也要恢复原值");
    }

    /// 计数聚合不能退回“按像素索引逐行回表”的旧计划（真实库上那是 39 万次回表）。
    #[test]
    fn active_count_plan_avoids_per_row_indexes() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = crate::library::service::create(&root, "计划测试", 1).unwrap();
        let plan = session
            .db
            .with_reader(|connection| {
                let mut statement =
                    connection.prepare(&format!("EXPLAIN QUERY PLAN {ACTIVE_COUNT_SQL}"))?;
                let rows = statement
                    .query_map([-1_i64], |row| row.get::<_, String>(3))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows.join("\n"))
            })
            .unwrap();
        assert!(
            !plan.contains("idx_images_active"),
            "计数仍在逐行回表：{plan}"
        );
    }
}
