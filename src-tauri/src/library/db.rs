use std::{
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags};

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

    pub fn counts(&self) -> AppResult<(u64, u64)> {
        self.with_reader(|connection| {
            let active: u64 = connection.query_row(
                "SELECT count(*) FROM images WHERE deleted_at IS NULL",
                [],
                |row| row.get(0),
            )?;
            let trash: u64 = connection.query_row(
                "SELECT count(*) FROM images
                 WHERE deleted_at IS NOT NULL AND purge_operation_id IS NULL",
                [],
                |row| row.get(0),
            )?;
            Ok((active, trash))
        })
    }
}
