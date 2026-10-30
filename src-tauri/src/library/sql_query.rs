//! MCP 只读 SQL：专用只读连接、`mcp_*` TEMP 视图、执行上限与结果导出。
//!
//! 主库只读由 `SQLITE_OPEN_READ_ONLY` 保证；这里不设 `query_only`，因为视图要建在
//! 连接自己的 temp 库里。内部表只能经视图访问，由 authorizer 白名单兜底。

use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use parking_lot::Mutex;
use rusqlite::{
    Connection, OpenFlags,
    hooks::{AuthAction, AuthContext, Authorization},
    types::ValueRef,
};
use serde_json::{Map, Value, json};

use crate::app::error::{AppError, AppResult};

/// 视图字段口径只在这里维护；`library_info.sql` 直接读它。
pub const VIEW_NAMES: [&str; 10] = [
    "mcp_images",
    "mcp_trash_images",
    "mcp_temp_images",
    "mcp_folders",
    "mcp_trash_folders",
    "mcp_tags",
    "mcp_image_tags",
    "mcp_collections",
    "mcp_collection_items",
    "mcp_features",
];

pub const INLINE_MAX_ROWS: usize = 200;
pub const INLINE_MAX_BYTES: u64 = 256 * 1024;
pub const INLINE_TIMEOUT: Duration = Duration::from_secs(5);
pub const EXPORT_MAX_ROWS: u64 = 1_000_000;
pub const EXPORT_MAX_BYTES: u64 = 512 * 1024 * 1024;
pub const EXPORT_TIMEOUT: Duration = Duration::from_secs(60);
pub const EXPORT_PREVIEW_ROWS: usize = 20;
pub const EXPORT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// 视图定义。非聚合视图都带 `LIMIT -1`：SQLite 展开（flatten）视图时，部分底层表读取
/// 会丢掉 authorizer 的 view accessor，正常查询就会被判成“直读内部表”；禁止展开后，
/// 底层读取始终带着 `mcp_*` 视图名，白名单才可靠。带 GROUP BY 的聚合视图本身不会被展开。
const VIEW_SQL: &str = r#"
CREATE TEMP VIEW mcp_images (
    id, folder_id, folder, format, file_ext, file_bytes, width, height, has_alpha,
    rating, description, description_source, phash, colors4, colors8,
    imported_at_ms, modified_at_ms, optimized_at_ms, content_hash
) AS
SELECT
    i.id AS id,
    i.folder_id AS folder_id,
    f.path AS folder,
    i.format AS format,
    i.file_ext AS file_ext,
    i.file_size AS file_bytes,
    i.width AS width,
    i.height AS height,
    i.has_alpha AS has_alpha,
    i.score AS rating,
    i.description AS description,
    i.description_source AS description_source,
    hex(i.phash) AS phash,
    hex(i.colors4) AS colors4,
    hex(i.colors8) AS colors8,
    i.imported_at AS imported_at_ms,
    i.modified_at AS modified_at_ms,
    i.optimized_at AS optimized_at_ms,
    i.content_hash AS content_hash
FROM images i
JOIN folders f ON f.id = i.folder_id
WHERE i.deleted_at IS NULL
  AND f.kind = 'normal'
  AND f.trashed_at IS NULL
LIMIT -1;

CREATE TEMP VIEW mcp_trash_images (
    id, folder_id, folder, format, file_ext, file_bytes, width, height, has_alpha,
    rating, description, description_source, phash, colors4, colors8,
    imported_at_ms, modified_at_ms, deleted_at_ms
) AS
SELECT
    i.id AS id,
    i.folder_id AS folder_id,
    f.path AS folder,
    i.format AS format,
    i.file_ext AS file_ext,
    i.file_size AS file_bytes,
    i.width AS width,
    i.height AS height,
    i.has_alpha AS has_alpha,
    i.score AS rating,
    i.description AS description,
    i.description_source AS description_source,
    hex(i.phash) AS phash,
    hex(i.colors4) AS colors4,
    hex(i.colors8) AS colors8,
    i.imported_at AS imported_at_ms,
    i.modified_at AS modified_at_ms,
    i.deleted_at AS deleted_at_ms
FROM images i
JOIN folders f ON f.id = i.folder_id
WHERE i.deleted_at IS NOT NULL
  AND i.purge_operation_id IS NULL
  AND f.kind = 'normal'
LIMIT -1;

CREATE TEMP VIEW mcp_temp_images (
    id, folder, format, file_ext, file_bytes, width, height, has_alpha, rating,
    phash, colors4, colors8, imported_at_ms, modified_at_ms
) AS
SELECT
    i.id AS id,
    '临时' AS folder,
    i.format AS format,
    i.file_ext AS file_ext,
    i.file_size AS file_bytes,
    i.width AS width,
    i.height AS height,
    i.has_alpha AS has_alpha,
    i.score AS rating,
    hex(i.phash) AS phash,
    hex(i.colors4) AS colors4,
    hex(i.colors8) AS colors8,
    i.imported_at AS imported_at_ms,
    i.modified_at AS modified_at_ms
FROM images i
JOIN folders f ON f.id = i.folder_id
WHERE i.deleted_at IS NULL
  AND f.kind = 'temp'
LIMIT -1;

CREATE TEMP VIEW mcp_folders (id, path, sort_index, created_at_ms) AS
SELECT id, path, sort_index, created_at AS created_at_ms
FROM folders
WHERE kind = 'normal'
  AND trashed_at IS NULL
  AND purge_operation_id IS NULL
LIMIT -1;

CREATE TEMP VIEW mcp_trash_folders (id, path, trashed_at_ms) AS
SELECT id, path, trashed_at AS trashed_at_ms
FROM folders
WHERE kind = 'normal'
  AND trashed_at IS NOT NULL
  AND purge_operation_id IS NULL
LIMIT -1;

CREATE TEMP VIEW mcp_tags (id, name, image_count) AS
SELECT t.id AS id, t.name AS name, count(i.id) AS image_count
FROM tags t
LEFT JOIN image_tags it ON it.tag_id = t.id
LEFT JOIN images i ON i.id = it.image_id AND i.deleted_at IS NULL
LEFT JOIN folders f
    ON f.id = i.folder_id AND f.kind = 'normal' AND f.trashed_at IS NULL
GROUP BY t.id, t.name;

CREATE TEMP VIEW mcp_image_tags (image_id, tag_id, tag_name) AS
SELECT it.image_id AS image_id, it.tag_id AS tag_id, t.name AS tag_name
FROM image_tags it
JOIN tags t ON t.id = it.tag_id
JOIN images i ON i.id = it.image_id
JOIN folders f ON f.id = i.folder_id
WHERE i.deleted_at IS NULL
  AND f.kind = 'normal'
  AND f.trashed_at IS NULL
LIMIT -1;

CREATE TEMP VIEW mcp_collections (
    id, name, sort_order, item_count, created_at_ms, updated_at_ms
) AS
SELECT
    c.id AS id,
    c.name AS name,
    c.sort_order AS sort_order,
    count(ci.image_id) AS item_count,
    c.created_at AS created_at_ms,
    c.updated_at AS updated_at_ms
FROM collections c
LEFT JOIN collection_items ci ON ci.collection_id = c.id
GROUP BY c.id, c.name, c.sort_order, c.created_at, c.updated_at;

CREATE TEMP VIEW mcp_collection_items (collection_id, image_id, position) AS
SELECT collection_id, image_id, position
FROM collection_items
LIMIT -1;

CREATE TEMP VIEW mcp_features (
    image_id, kind, model, dim, updated_at_ms
) AS
SELECT
    fb.image_id AS image_id,
    fb.kind AS kind,
    fb.model AS model,
    fb.dim AS dim,
    fb.updated_at AS updated_at_ms
FROM feature_blobs fb
JOIN images i ON i.id = fb.image_id
JOIN folders f ON f.id = i.folder_id
WHERE i.deleted_at IS NULL
  AND f.kind = 'normal'
  AND f.trashed_at IS NULL
LIMIT -1;
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Jsonl,
    Csv,
}

impl ExportFormat {
    pub fn parse(value: Option<&str>) -> AppResult<Self> {
        match value
            .unwrap_or("jsonl")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "jsonl" => Ok(Self::Jsonl),
            "csv" => Ok(Self::Csv),
            other => Err(AppError::message(format!(
                "不支持的导出格式：{other}；只支持 jsonl 与 csv"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jsonl => "jsonl",
            Self::Csv => "csv",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum QueryKind {
    Inline,
    Export(ExportFormat),
}

#[derive(Debug, Clone)]
pub struct ExportFile {
    pub path: PathBuf,
    pub format: ExportFormat,
    pub rows: u64,
    pub bytes: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct QueryOutcome {
    pub columns: Vec<String>,
    pub rows: Vec<Value>,
    pub row_count: u64,
    pub bytes: u64,
    pub truncated: bool,
    pub export: Option<ExportFile>,
}

impl QueryOutcome {
    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "columns": self.columns,
            "rows": self.rows,
            "rowCount": self.row_count,
            "bytes": self.bytes,
            "truncated": self.truncated,
        });
        if let Some(export) = &self.export {
            value["export"] = json!({
                "path": export.path.to_string_lossy(),
                "format": export.format.as_str(),
                "rows": export.rows,
                "bytes": export.bytes,
                "truncated": export.truncated,
            });
            value["note"] = Value::String(format!(
                "rows 只是前 {EXPORT_PREVIEW_ROWS} 行预览；完整结果已写入 export.path 的 {} 文件，可用自己的 Python 读取。truncated=true 表示触发了导出上限，文件不是全量。",
                export.format.as_str().to_uppercase()
            ));
        } else if self.truncated {
            value["note"] = Value::String(format!(
                "结果超过内联上限（{INLINE_MAX_ROWS} 行 / {} KiB / {} 秒），已截断；需要完整结果时加 export=true。",
                INLINE_MAX_BYTES / 1024,
                INLINE_TIMEOUT.as_secs()
            ));
        }
        value
    }
}

struct SandboxConnection {
    path: PathBuf,
    generation: u64,
    connection: Connection,
}

/// 进程内唯一的只读 SQL 连接；同一时刻只跑一条查询。
pub struct SqlSandbox {
    state: Mutex<Option<SandboxConnection>>,
    interrupt: Mutex<Option<rusqlite::InterruptHandle>>,
    generation: AtomicU64,
}

impl Default for SqlSandbox {
    fn default() -> Self {
        Self::new()
    }
}

impl SqlSandbox {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(None),
            interrupt: Mutex::new(None),
            generation: AtomicU64::new(0),
        }
    }

    /// 关库/换库时中断正在跑的查询，并丢弃空闲连接（Windows 下文件可能还要被删除）。
    ///
    /// 查询还在跑时不能取走句柄，否则下一次关库就打断不了；连接真被丢弃时，
    /// 句柄跟它一起丢（`try_lock` 失败说明 `run` 正在用连接，它拿到结果后会自己收尾）。
    pub fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        let mut interrupt = self.interrupt.lock();
        if let Some(handle) = interrupt.as_ref() {
            handle.interrupt();
        }
        if let Some(mut state) = self.state.try_lock()
            && state.take().is_some()
        {
            interrupt.take();
        }
    }

    pub fn run(&self, database: &Path, sql: &str, kind: QueryKind) -> AppResult<QueryOutcome> {
        ensure_select(sql)?;
        let generation = self.generation.load(Ordering::SeqCst);
        let mut state = self.state.lock();
        let needs_open = state
            .as_ref()
            .is_none_or(|current| current.path != database || current.generation != generation);
        if needs_open {
            let connection = open_connection(database)?;
            self.interrupt
                .lock()
                .replace(connection.get_interrupt_handle());
            *state = Some(SandboxConnection {
                path: database.to_path_buf(),
                generation,
                connection,
            });
        }
        let outcome = {
            let current = state.as_ref().expect("SQL 连接刚建立");
            execute(&current.connection, sql, kind, || {
                self.generation.load(Ordering::SeqCst) != generation
            })
        };
        if self.generation.load(Ordering::SeqCst) != generation {
            // 关库/换库发生在这次查询期间：连接一起丢掉，别让只读句柄继续占着库文件。
            drop(state.take());
            self.interrupt.lock().take();
            if outcome.is_err() {
                return Err(AppError::message("图库已关闭或已切换，查询已中止"));
            }
        }
        outcome
    }
}

fn open_connection(database: &Path) -> AppResult<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = Connection::open_with_flags(database, flags)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    // 视图只存在这个连接的 temp 库；放内存里，关连接即消失。
    connection.execute_batch("PRAGMA temp_store = MEMORY;")?;
    connection.execute_batch(VIEW_SQL)?;
    install_authorizer(&connection)?;
    Ok(connection)
}

fn install_authorizer(connection: &Connection) -> AppResult<()> {
    let mut real_tables = HashSet::new();
    {
        let mut statement = connection.prepare(
            "SELECT name FROM sqlite_master UNION ALL SELECT name FROM sqlite_temp_master",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        for name in rows {
            real_tables.insert(name?.to_ascii_lowercase());
        }
    }
    let allowed_views: HashSet<String> = VIEW_NAMES.iter().map(|name| name.to_string()).collect();
    connection.authorizer(Some(move |context: AuthContext<'_>| {
        match context.action {
            AuthAction::Select | AuthAction::Recursive => Authorization::Allow,
            AuthAction::Read { table_name, .. } => {
                let table = table_name.to_ascii_lowercase();
                let accessor = context.accessor.map(str::to_ascii_lowercase);
                // 视图名本身、经视图展开的底层读取、以及用户 CTE 都放行；
                // 直读内部表、sqlite_master / sqlite_temp_master 一律拒绝。
                if matches!(
                    table.as_str(),
                    "sqlite_master" | "sqlite_temp_master" | "sqlite_schema" | "sqlite_temp_schema"
                ) {
                    return Authorization::Deny;
                }
                if allowed_views.contains(&table)
                    || accessor.is_some_and(|name| allowed_views.contains(&name))
                    || !real_tables.contains(&table)
                {
                    Authorization::Allow
                } else {
                    Authorization::Deny
                }
            }
            AuthAction::Function { function_name } => {
                if denied_function(function_name) {
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            }
            _ => Authorization::Deny,
        }
    }));
    Ok(())
}

/// 函数策略：内置函数默认放行，只按名字拒绝危险的那几个。
fn denied_function(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "load_extension"
            | "readfile"
            | "writefile"
            | "fsdir"
            | "zipfile"
            | "sqlite_dbpage"
            | "sqlite_dbptr"
            | "sqlite_dbdata"
            | "sqlite_dbfile"
            | "edit"
            | "shell"
            | "system"
            | "eval"
    ) || name.starts_with("pragma_")
}

/// 只接受单条 `SELECT` 或以 `WITH` 开头的查询；先去掉前导空白与注释。
pub fn ensure_select(sql: &str) -> AppResult<()> {
    let body = strip_leading_comments(sql);
    let word: String = body
        .chars()
        .take_while(|ch| ch.is_ascii_alphabetic())
        .collect();
    if word.eq_ignore_ascii_case("select") || word.eq_ignore_ascii_case("with") {
        Ok(())
    } else {
        Err(AppError::message(
            "只允许单条 SELECT 或以 WITH 开头的查询；EXPLAIN、PRAGMA、ATTACH、写操作与 DDL 都不开放",
        ))
    }
}

fn strip_leading_comments(sql: &str) -> &str {
    let mut rest = sql.trim_start();
    loop {
        if let Some(stripped) = rest.strip_prefix("--") {
            match stripped.find('\n') {
                Some(index) => rest = stripped[index + 1..].trim_start(),
                None => return "",
            }
        } else if let Some(stripped) = rest.strip_prefix("/*") {
            match stripped.find("*/") {
                Some(index) => rest = stripped[index + 2..].trim_start(),
                None => return "",
            }
        } else {
            return rest;
        }
    }
}

/// 执行一条已校验的查询：超时与关库/换库都靠 progress handler 打断。
fn execute(
    connection: &Connection,
    sql: &str,
    kind: QueryKind,
    invalidated: impl Fn() -> bool,
) -> AppResult<QueryOutcome> {
    let (max_rows, max_bytes, timeout) = match kind {
        QueryKind::Inline => (INLINE_MAX_ROWS as u64, INLINE_MAX_BYTES, INLINE_TIMEOUT),
        QueryKind::Export(_) => (EXPORT_MAX_ROWS, EXPORT_MAX_BYTES, EXPORT_TIMEOUT),
    };
    let deadline = Instant::now() + timeout;
    let timed_out = Arc::new(AtomicBool::new(false));
    let flag = timed_out.clone();
    connection.progress_handler(
        10_000,
        Some(move || {
            if Instant::now() >= deadline {
                flag.store(true, Ordering::Relaxed);
                true
            } else {
                false
            }
        }),
    );

    let result: AppResult<QueryOutcome> = (|| {
        let mut statement = connection.prepare(sql).map_err(map_query_error)?;
        let columns = unique_columns(&statement)?;
        match kind {
            QueryKind::Inline => collect_inline(&mut statement, columns, max_rows, max_bytes),
            QueryKind::Export(format) => {
                export_rows(&mut statement, columns, format, max_rows, max_bytes)
            }
        }
    })();

    connection.progress_handler(0, None::<fn() -> bool>);

    if let Err(error) = result {
        if timed_out.load(Ordering::Relaxed) {
            return Err(AppError::message(format!(
                "查询超过 {} 秒上限，已中止；请收紧条件或改用导出",
                timeout.as_secs()
            )));
        }
        if invalidated() {
            return Err(AppError::message("图库已关闭或已切换，查询已中止"));
        }
        return Err(error);
    }
    result
}

fn unique_columns(statement: &rusqlite::Statement<'_>) -> AppResult<Vec<String>> {
    let mut seen = HashSet::new();
    let mut columns = Vec::new();
    for name in statement.column_names() {
        let name = name.trim();
        if name.is_empty() {
            return Err(AppError::message("结果列名不能为空；请给表达式加 AS 别名"));
        }
        if !seen.insert(name.to_owned()) {
            return Err(AppError::message(format!(
                "结果列名重复：{name}；请给每一列起唯一别名"
            )));
        }
        columns.push(name.to_owned());
    }
    Ok(columns)
}

/// 内联结果：先判上限再入列，因此会多读一行来确认确实还有数据，截断时不留半行。
fn collect_inline(
    statement: &mut rusqlite::Statement<'_>,
    columns: Vec<String>,
    max_rows: u64,
    max_bytes: u64,
) -> AppResult<QueryOutcome> {
    let mut rows = statement.query([]).map_err(map_query_error)?;
    let mut values = Vec::new();
    let mut bytes = 0_u64;
    let mut truncated = false;
    while let Some(row) = rows.next().map_err(map_query_error)? {
        let value = row_to_json(row, &columns)?;
        let size = serde_json::to_vec(&value)?.len() as u64;
        if values.len() as u64 >= max_rows || (!values.is_empty() && bytes + size > max_bytes) {
            truncated = true;
            break;
        }
        bytes += size;
        values.push(value);
    }
    Ok(QueryOutcome {
        columns,
        row_count: values.len() as u64,
        rows: values,
        bytes,
        truncated,
        export: None,
    })
}

fn export_rows(
    statement: &mut rusqlite::Statement<'_>,
    columns: Vec<String>,
    format: ExportFormat,
    max_rows: u64,
    max_bytes: u64,
) -> AppResult<QueryOutcome> {
    cleanup_exports();
    let dir = export_dir();
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!(
        "{}.{}",
        uuid::Uuid::new_v4().simple(),
        format.as_str()
    ));
    let file = fs::File::create(&path)?;
    let mut writer = std::io::BufWriter::new(file);
    let mut bytes = 0_u64;
    let mut written = 0_u64;
    let mut truncated = false;
    let mut preview = Vec::new();

    let result: AppResult<()> = (|| {
        if format == ExportFormat::Csv {
            let header = columns
                .iter()
                .map(|name| csv_cell(ValueRef::Text(name.as_bytes())))
                .collect::<Vec<_>>()
                .join(",");
            let mut line = header.into_bytes();
            line.extend_from_slice(b"\r\n");
            writer.write_all(&line)?;
            bytes += line.len() as u64;
        }
        let mut rows = statement.query([]).map_err(map_query_error)?;
        while let Some(row) = rows.next().map_err(map_query_error)? {
            // JSONL 必然要行对象；CSV 只为预览那几行构一份，避免百万行都做一次 JSON 映射。
            let mut json = None;
            let buffer = match format {
                ExportFormat::Jsonl => {
                    let value = row_to_json(row, &columns)?;
                    let mut buffer = serde_json::to_vec(&value)?;
                    buffer.push(b'\n');
                    json = Some(value);
                    buffer
                }
                ExportFormat::Csv => csv_line(row, columns.len())?,
            };
            if written >= max_rows || (written > 0 && bytes + buffer.len() as u64 > max_bytes) {
                truncated = true;
                break;
            }
            writer.write_all(&buffer)?;
            bytes += buffer.len() as u64;
            written += 1;
            if preview.len() < EXPORT_PREVIEW_ROWS {
                preview.push(match json {
                    Some(value) => value,
                    None => row_to_json(row, &columns)?,
                });
            }
        }
        writer.flush()?;
        Ok(())
    })();

    match result {
        Ok(()) => Ok(QueryOutcome {
            columns,
            rows: preview,
            row_count: written,
            bytes,
            truncated,
            export: Some(ExportFile {
                path,
                format,
                rows: written,
                bytes,
                truncated,
            }),
        }),
        Err(error) => {
            drop(writer);
            let _ = fs::remove_file(&path);
            Err(error)
        }
    }
}

pub fn export_dir() -> PathBuf {
    std::env::temp_dir()
        .join("lilith-artbank")
        .join("mcp-export")
}

/// 清理超过 TTL 的导出文件；导出前、应用启动时与空闲轮询各调一次。
pub fn cleanup_exports() {
    cleanup_dir_at(&export_dir(), SystemTime::now());
}

fn cleanup_dir_at(dir: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let age = now.duration_since(modified).unwrap_or_default();
        if age > EXPORT_TTL && metadata.is_file() {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn row_to_json(row: &rusqlite::Row<'_>, columns: &[String]) -> AppResult<Value> {
    let mut object = Map::with_capacity(columns.len());
    for (index, name) in columns.iter().enumerate() {
        object.insert(
            name.clone(),
            value_ref_to_json(row.get_ref(index).map_err(map_query_error)?),
        );
    }
    Ok(Value::Object(object))
}

fn value_ref_to_json(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(number) => json!(number),
        ValueRef::Real(number) => real_to_json(number),
        ValueRef::Text(bytes) => Value::String(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => Value::String(STANDARD.encode(bytes)),
    }
}

fn real_to_json(number: f64) -> Value {
    if number.is_finite() {
        serde_json::Number::from_f64(number)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    } else {
        Value::String(non_finite_label(number).to_owned())
    }
}

fn non_finite_label(number: f64) -> &'static str {
    if number.is_nan() {
        "NaN"
    } else if number.is_sign_positive() {
        "Infinity"
    } else {
        "-Infinity"
    }
}

/// CSV 一行：必须按查询列顺序取单元格，JSON 对象的键顺序不保证与 SELECT 一致。
fn csv_line(row: &rusqlite::Row<'_>, columns: usize) -> AppResult<Vec<u8>> {
    let mut line = String::new();
    for index in 0..columns {
        if index > 0 {
            line.push(',');
        }
        line.push_str(&csv_cell(row.get_ref(index).map_err(map_query_error)?));
    }
    line.push_str("\r\n");
    Ok(line.into_bytes())
}

fn csv_cell(value: ValueRef<'_>) -> String {
    let mut text = match value {
        ValueRef::Null => String::new(),
        ValueRef::Integer(number) => number.to_string(),
        ValueRef::Real(number) => {
            if number.is_finite() {
                number.to_string()
            } else {
                non_finite_label(number).to_owned()
            }
        }
        ValueRef::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        ValueRef::Blob(bytes) => STANDARD.encode(bytes),
    };
    // 文本单元格以表格公式字符开头时加单引号，数字保持可解析。
    if matches!(value, ValueRef::Text(_))
        && text
            .chars()
            .next()
            .is_some_and(|ch| matches!(ch, '=' | '+' | '-' | '@' | '\t' | '\r'))
    {
        text.insert(0, '\'');
    }
    if text.contains(['"', ',', '\r', '\n']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text
    }
}

fn map_query_error(error: rusqlite::Error) -> AppError {
    use rusqlite::ffi::ErrorCode;
    // 分号后还有语句时 prepare 会拒绝；给一句可读的说明，别把英文原文抛给模型。
    if matches!(error, rusqlite::Error::MultipleStatement) {
        return AppError::message("只允许一条语句；需要多条时分开调用");
    }
    if let Some(code) = error.sqlite_error_code() {
        match code {
            ErrorCode::AuthorizationForStatementDenied => {
                return AppError::message(
                    "SQL 不允许访问内部表或危险函数；只能查询 mcp_* 视图，且只接受只读 SELECT",
                );
            }
            ErrorCode::OperationInterrupted => {
                return AppError::message("查询已中止");
            }
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => {
                return AppError::message("图库正忙，请稍后重试");
            }
            _ => {}
        }
    }
    AppError::message(format!("SQL 执行失败：{error}"))
}

/// `library_info.sql`：字段、类型、单位与用法只维护这一份。
pub fn schema_docs() -> Value {
    json!({
        "tool": "library_sql",
        "usage": "只读单条 SELECT；只能查询下面的 mcp_* 视图，直接读取内部表、sqlite_master、PRAGMA、ATTACH 与写操作都会被拒绝。小结果内联返回；需要完整结果时加 export=true，把结果写成 JSONL（默认）或 CSV 文件并返回真实路径。",
        "views": {
            "mcp_images": view_doc(&[
                field("id", "INTEGER", "图片 id"),
                field("folder_id", "INTEGER", "文件夹 id"),
                field("folder", "TEXT", "图库内相对文件夹路径；根目录是空字符串"),
                field("format", "TEXT", "图片格式（jpeg/png/webp/...）"),
                field("file_ext", "TEXT", "文件扩展名"),
                field("file_bytes", "INTEGER", "原图字节数"),
                field("width", "INTEGER", "像素宽"),
                field("height", "INTEGER", "像素高"),
                field("has_alpha", "INTEGER", "是否带 alpha（0/1）"),
                field("rating", "INTEGER", "评分 0-5"),
                field("description", "TEXT", "描述；可能为空"),
                field("description_source", "TEXT", "user / ai / import"),
                field("phash", "TEXT", "64 位 pHash 的十六进制"),
                field("colors4", "TEXT", "4 色主色的十六进制"),
                field("colors8", "TEXT", "8 色主色的十六进制"),
                field("imported_at_ms", "INTEGER", "导入时间（毫秒时间戳）"),
                field("modified_at_ms", "INTEGER", "修改时间（毫秒时间戳）"),
                field("optimized_at_ms", "INTEGER", "上次空间优化时间；0 表示没有"),
                field("content_hash", "TEXT", "BLAKE3 内容哈希"),
            ]),
            "mcp_trash_images": view_doc(&[
                field("id", "INTEGER", "图片 id"),
                field("folder_id", "INTEGER", "文件夹 id"),
                field("folder", "TEXT", "原相对文件夹路径"),
                field("deleted_at_ms", "INTEGER", "移入回收站时间（毫秒时间戳）"),
            ]),
            "mcp_temp_images": view_doc(&[
                field("id", "INTEGER", "临时图片 id"),
                field("folder", "TEXT", "固定为「临时」"),
                field("imported_at_ms", "INTEGER", "导入时间（毫秒时间戳）"),
            ]),
            "mcp_folders": view_doc(&[
                field("id", "INTEGER", "文件夹 id"),
                field("path", "TEXT", "图库内相对路径；根目录用空字符串表示"),
                field("sort_index", "INTEGER", "同级排序位"),
                field("created_at_ms", "INTEGER", "创建时间（毫秒时间戳）"),
            ]),
            "mcp_trash_folders": view_doc(&[
                field("id", "INTEGER", "文件夹 id"),
                field("path", "TEXT", "原相对路径"),
                field("trashed_at_ms", "INTEGER", "移入回收站时间（毫秒时间戳）"),
            ]),
            "mcp_tags": view_doc(&[
                field("id", "INTEGER", "标签 id"),
                field("name", "TEXT", "标签名"),
                field("image_count", "INTEGER", "正常图片数；回收站与临时图片不计"),
            ]),
            "mcp_image_tags": view_doc(&[
                field("image_id", "INTEGER", "正常图片 id"),
                field("tag_id", "INTEGER", "标签 id"),
                field("tag_name", "TEXT", "标签名"),
            ]),
            "mcp_collections": view_doc(&[
                field("id", "INTEGER", "集合 id"),
                field("name", "TEXT", "集合名"),
                field("sort_order", "INTEGER", "排序位"),
                field("item_count", "INTEGER", "成员数"),
                field("created_at_ms", "INTEGER", "创建时间（毫秒时间戳）"),
                field("updated_at_ms", "INTEGER", "更新时间（毫秒时间戳）"),
            ]),
            "mcp_collection_items": view_doc(&[
                field("collection_id", "INTEGER", "集合 id"),
                field("image_id", "INTEGER", "图片 id"),
                field("position", "INTEGER", "集合内位置"),
            ]),
            "mcp_features": view_doc(&[
                field("image_id", "INTEGER", "正常图片 id"),
                field("kind", "TEXT", "embedding / pose"),
                field("model", "TEXT", "生成模型名"),
                field("dim", "INTEGER", "embedding 维度，或 pose 的关键点个数"),
                field("updated_at_ms", "INTEGER", "生成时间（毫秒时间戳）"),
            ]),
        },
        "fieldConventions": "rating 代替 score；时间用 _ms；大小用 _bytes；pHash 与主色是 hex() 字符串。不暴露绝对路径、pack 名、向量本体与内部表。",
        "limits": {
            "inlineRows": INLINE_MAX_ROWS,
            "inlineBytes": INLINE_MAX_BYTES,
            "inlineSeconds": INLINE_TIMEOUT.as_secs(),
            "exportRows": EXPORT_MAX_ROWS,
            "exportBytes": EXPORT_MAX_BYTES,
            "exportSeconds": EXPORT_TIMEOUT.as_secs(),
            "exportPreviewRows": EXPORT_PREVIEW_ROWS,
            "exportTtlSeconds": EXPORT_TTL.as_secs(),
        },
        "export": "export=true 时结果写入系统临时目录下的 JSONL（默认）或 CSV 文件；JSONL 一行一个对象，CSV 为 UTF-8、无 BOM、RFC 4180 引号。NULL 在 JSONL 是 null、CSV 是空字段；BLOB 用 base64；非有限 REAL 写成字符串 NaN / Infinity。导出路径可以直接交给外部 Python 读取，但不接受指定任意写入目录。",
    })
}

fn field(name: &str, kind: &str, note: &str) -> Value {
    json!({ "name": name, "type": kind, "note": note })
}

fn view_doc(required: &[Value]) -> Value {
    json!({ "required": required, "columns": "只增不减；改名视为破坏性变更" })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::Value;
    use tempfile::TempDir;

    use super::*;
    use crate::library::service;

    struct Fixture {
        _temp: TempDir,
        database: PathBuf,
        sandbox: SqlSandbox,
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "SQL 测试", 1).unwrap();
        session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO folders(path, created_at) VALUES ('作品', 1)",
                    [],
                )?;
                let works = connection.last_insert_rowid();
                connection.execute(
                    "INSERT INTO folders(path, created_at) VALUES ('作品/速写', 2)",
                    [],
                )?;
                let sketches = connection.last_insert_rowid();
                connection.execute(
                    "INSERT INTO collections(name, sort_order, created_at, updated_at)
                     VALUES ('精选', 0, 1, 1)",
                    [],
                )?;
                connection.execute("INSERT INTO tags(name) VALUES ('人物')", [])?;
                for (id, folder, score) in [(1_i64, works, 5_i64), (2, sketches, 3)] {
                    connection.execute(
                        "INSERT INTO images(
                            id, content_hash, file_ext, folder_id, file_size, width, height,
                            format, has_alpha, phash, colors4, colors8, score, description,
                            imported_at, modified_at
                         ) VALUES (?1, ?2, 'png', ?3, ?4, ?5, ?6, 'png', 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), ?7, '说明', ?8, ?8)",
                        rusqlite::params![
                            id,
                            format!("{id:0>64}"),
                            folder,
                            1000 + id,
                            40,
                            30,
                            score,
                            10 + id
                        ],
                    )?;
                }
                connection.execute("INSERT INTO image_tags(image_id, tag_id) VALUES (1, 1)", [])?;
                Ok(())
            })
            .unwrap();
        let database = root.join(".artbank").join("library.db");
        Fixture {
            _temp: temp,
            database,
            sandbox: SqlSandbox::new(),
        }
    }

    fn inline(fixture: &Fixture, sql: &str) -> QueryOutcome {
        fixture
            .sandbox
            .run(&fixture.database, sql, QueryKind::Inline)
            .unwrap()
    }

    fn error(fixture: &Fixture, sql: &str) -> String {
        fixture
            .sandbox
            .run(&fixture.database, sql, QueryKind::Inline)
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn views_are_queryable_and_hide_internal_details() {
        let fixture = fixture();
        let outcome = inline(
            &fixture,
            "SELECT id, folder, rating FROM mcp_images ORDER BY id",
        );
        assert_eq!(outcome.rows.len(), 2);
        assert_eq!(outcome.rows[0]["folder"], "作品");
        assert_eq!(outcome.rows[1]["folder"], "作品/速写");
        assert_eq!(outcome.rows[0]["rating"], 5);

        for (label, sql) in [
            ("folders", "SELECT count(*) AS n FROM mcp_folders"),
            ("tags", "SELECT count(*) AS n FROM mcp_tags"),
            ("image_tags", "SELECT count(*) AS n FROM mcp_image_tags"),
            ("collections", "SELECT count(*) AS n FROM mcp_collections"),
            (
                "collection_items",
                "SELECT count(*) AS n FROM mcp_collection_items",
            ),
            ("features", "SELECT count(*) AS n FROM mcp_features"),
            ("trash_images", "SELECT count(*) AS n FROM mcp_trash_images"),
            ("temp_images", "SELECT count(*) AS n FROM mcp_temp_images"),
            (
                "trash_folders",
                "SELECT count(*) AS n FROM mcp_trash_folders",
            ),
        ] {
            fixture
                .sandbox
                .run(&fixture.database, sql, QueryKind::Inline)
                .unwrap_or_else(|error| panic!("{label}: {error}"));
        }
        let counts = inline(
            &fixture,
            "SELECT (SELECT count(*) FROM mcp_folders) AS f,
                    (SELECT count(*) FROM mcp_tags) AS t,
                    (SELECT count(*) FROM mcp_collections) AS c",
        );
        assert_eq!(counts.rows[0]["f"], 3);
        assert_eq!(counts.rows[0]["t"], 1);
        assert_eq!(counts.rows[0]["c"], 1);

        let join = fixture
            .sandbox
            .run(
                &fixture.database,
                "WITH tagged AS (SELECT image_id FROM mcp_image_tags)
                 SELECT count(*) AS n FROM tagged JOIN mcp_images ON mcp_images.id = tagged.image_id",
                QueryKind::Inline,
            )
            .expect("cte join");
        assert_eq!(join.rows[0]["n"], 1);
    }

    #[test]
    fn invalidate_drops_the_idle_connection_and_the_next_query_reopens() {
        let fixture = fixture();
        assert_eq!(inline(&fixture, "SELECT 1 AS one").rows[0]["one"], 1);
        assert!(fixture.sandbox.state.lock().is_some());
        fixture.sandbox.invalidate();
        assert!(
            fixture.sandbox.state.lock().is_none(),
            "关库后应丢弃只读连接"
        );
        // 重新查询会自动开一条新连接，视图照常在。
        assert_eq!(
            inline(&fixture, "SELECT count(*) AS n FROM mcp_images").rows[0]["n"],
            2
        );
    }

    #[test]
    fn direct_tables_schema_and_dangerous_statements_are_rejected() {
        let fixture = fixture();
        for sql in [
            "SELECT * FROM images",
            "SELECT * FROM sqlite_master",
            "SELECT * FROM sqlite_temp_master",
            "PRAGMA table_info(images)",
            "ATTACH DATABASE 'x.db' AS x",
            "DELETE FROM images",
            "SELECT load_extension('x')",
            "SELECT readfile('x')",
            "EXPLAIN SELECT 1",
        ] {
            let message = error(&fixture, sql);
            assert!(!message.is_empty(), "{sql}");
            assert!(
                message.contains("只允许")
                    || message.contains("不允许")
                    || message.contains("SQL 执行失败")
                    || message.contains("SQL 不能为空")
                    || message.contains("Result"),
                "{sql} 应被拒绝：{message}"
            );
        }
    }

    #[test]
    fn second_statement_is_rejected_with_a_readable_message() {
        let fixture = fixture();
        let message = error(&fixture, "SELECT 1 AS one; SELECT 2 AS two");
        assert!(message.contains("只允许一条语句"), "{message}");
    }

    #[test]
    fn allowed_functions_and_select_constant() {
        let fixture = fixture();
        let outcome = inline(
            &fixture,
            "SELECT 1 AS one, lower('A') AS low, phash AS h FROM mcp_images WHERE id = 1",
        );
        assert_eq!(outcome.rows[0]["one"], 1);
        assert_eq!(outcome.rows[0]["low"], "a");
        assert_eq!(outcome.rows[0]["h"], "0".repeat(64));
    }

    #[test]
    fn column_names_must_be_unique_and_non_empty() {
        let fixture = fixture();
        assert!(error(&fixture, "SELECT id, id FROM mcp_images").contains("重复"));
        assert!(error(&fixture, "SELECT id AS \"\" FROM mcp_images").contains("列名不能为空"));
    }

    #[test]
    fn inline_limits_report_truncation_before_cutting_the_row() {
        let fixture = fixture();
        let outcome = inline(
            &fixture,
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 500)
             SELECT x FROM n",
        );
        assert_eq!(outcome.row_count, INLINE_MAX_ROWS as u64);
        assert!(outcome.truncated);
        assert_eq!(outcome.rows.len(), INLINE_MAX_ROWS);
    }

    #[test]
    fn json_export_writes_parseable_lines_and_maps_values() {
        let fixture = fixture();
        let outcome = fixture
            .sandbox
            .run(
                &fixture.database,
                "SELECT id AS \"=id\", '=SUM(A1)' AS formula, 9e999 AS real_inf, x'00ff' AS blob FROM mcp_images ORDER BY id",
                QueryKind::Export(ExportFormat::Jsonl),
            )
            .unwrap();
        let export = outcome.export.expect("应生成导出文件");
        assert_eq!(export.rows, 2);
        assert!(!export.truncated);
        let text = fs::read_to_string(&export.path).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["formula"], "=SUM(A1)");
        assert_eq!(lines[0]["blob"], STANDARD.encode([0_u8, 255]));
        assert_eq!(lines[0]["real_inf"], "Infinity");
        assert_eq!(lines[0]["=id"], 1);
        fs::remove_file(export.path).ok();
    }

    #[test]
    fn csv_export_quotes_and_blocks_formula_injection() {
        let fixture = fixture();
        let outcome = fixture
            .sandbox
            .run(
                &fixture.database,
                "SELECT '=1+1' AS formula, 'a,b' AS comma, 'x\"y' AS quote, '-5' AS text_neg, -5 AS num_neg FROM mcp_images WHERE id = 1",
                QueryKind::Export(ExportFormat::Csv),
            )
            .unwrap();
        let export = outcome.export.expect("应生成导出文件");
        let text = fs::read_to_string(&export.path).unwrap();
        assert!(
            text.starts_with("formula,comma,quote,text_neg,num_neg\r\n"),
            "{text}"
        );
        assert!(text.contains("'=1+1"), "{text}");
        assert!(text.contains("\"a,b\""), "{text}");
        assert!(text.contains("\"x\"\"y\""), "{text}");
        // 文本负数防公式注入；数值列保持可解析。
        assert!(text.contains("'-5"), "{text}");
        assert!(text.contains(",-5\r\n"), "{text}");
        fs::remove_file(export.path).ok();
    }

    #[test]
    fn export_limits_and_preview() {
        let fixture = fixture();
        let outcome = fixture
            .sandbox
            .run(
                &fixture.database,
                "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 30)
                 SELECT x FROM n",
                QueryKind::Export(ExportFormat::Jsonl),
            )
            .unwrap();
        assert_eq!(outcome.rows.len(), EXPORT_PREVIEW_ROWS);
        let export = outcome.export.expect("应生成导出文件");
        assert_eq!(export.rows, 30);
        assert!(!export.truncated);
        fs::remove_file(export.path).ok();
    }

    #[test]
    fn expired_exports_are_cleaned_up() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("stale.jsonl");
        fs::write(&path, b"old").unwrap();
        cleanup_dir_at(
            temp.path(),
            SystemTime::now() + EXPORT_TTL + Duration::from_secs(1),
        );
        assert!(!path.exists());
    }

    #[test]
    fn schema_docs_lists_every_view() {
        let docs = schema_docs();
        let views = docs["views"].as_object().unwrap();
        assert_eq!(views.len(), VIEW_NAMES.len());
        for name in VIEW_NAMES {
            assert!(views.contains_key(name), "缺少视图文档 {name}");
        }
    }
}
