use rusqlite::{Connection, TransactionBehavior, params};

use crate::{
    app::error::{AppError, AppResult},
    library::manifest::LibraryManifest,
};

pub const SCHEMA_VERSION: u32 = 5;

const SCHEMA_SQL: &str = r#"
CREATE TABLE meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE folders (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    sort_index INTEGER NOT NULL DEFAULT 0
) STRICT;

-- Windows 下文件夹路径大小写不敏感；NOCASE 只覆盖 ASCII，中文不受影响。
CREATE UNIQUE INDEX idx_folders_path_nocase ON folders(path COLLATE NOCASE);

CREATE TABLE images (
    id INTEGER PRIMARY KEY,
    content_hash TEXT NOT NULL UNIQUE CHECK (length(content_hash) = 64),
    file_ext TEXT NOT NULL,
    folder_id INTEGER NOT NULL REFERENCES folders(id) ON DELETE RESTRICT,
    file_size INTEGER NOT NULL CHECK (file_size >= 0),
    width INTEGER NOT NULL CHECK (width > 0),
    height INTEGER NOT NULL CHECK (height > 0),
    format TEXT NOT NULL,
    has_alpha INTEGER NOT NULL DEFAULT 0 CHECK (has_alpha IN (0, 1)),
    phash BLOB NOT NULL CHECK (length(phash) = 32),
    colors4 BLOB NOT NULL CHECK (length(colors4) = 16),
    colors8 BLOB NOT NULL CHECK (length(colors8) = 32),
    score INTEGER NOT NULL DEFAULT 0 CHECK (score BETWEEN 0 AND 5),
    description TEXT NOT NULL DEFAULT '',
    description_source TEXT NOT NULL DEFAULT 'user'
        CHECK (description_source IN ('user', 'ai', 'import')),
    description_model TEXT NOT NULL DEFAULT '',
    imported_at INTEGER NOT NULL,
    modified_at INTEGER NOT NULL,
    deleted_at INTEGER,
    thumb_segment INTEGER NOT NULL DEFAULT 0,
    thumb_offset INTEGER NOT NULL DEFAULT 0,
    thumb_length INTEGER NOT NULL DEFAULT 0,
    thumb_format TEXT NOT NULL DEFAULT 'jpeg',
    thumb_width INTEGER NOT NULL DEFAULT 0,
    thumb_height INTEGER NOT NULL DEFAULT 0,
    thumb_version INTEGER NOT NULL DEFAULT 1,
    -- 待释放：只标记，文件与行在操作记录过期或清空记录后才真正删除。
    purge_operation_id INTEGER,
    -- 导入批次；导入撤销按它找回本次新增的行。
    origin_operation_id INTEGER
) STRICT;

CREATE INDEX idx_images_active_score
    ON images(deleted_at, score DESC, id DESC);
CREATE INDEX idx_images_active_imported
    ON images(deleted_at, imported_at DESC, id DESC);
CREATE INDEX idx_images_active_size
    ON images(deleted_at, file_size DESC, id DESC);
CREATE INDEX idx_images_active_modified
    ON images(deleted_at, modified_at DESC, id DESC);
CREATE INDEX idx_images_active_pixels
    ON images(deleted_at, (width * height) DESC, id DESC);
CREATE INDEX idx_images_folder_imported
    ON images(folder_id, deleted_at, imported_at DESC, id DESC);
CREATE INDEX idx_images_trash
    ON images(deleted_at) WHERE deleted_at IS NOT NULL;

CREATE TABLE tags (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE COLLATE NOCASE
) STRICT;

CREATE TABLE image_tags (
    image_id INTEGER NOT NULL REFERENCES images(id) ON DELETE CASCADE,
    tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (image_id, tag_id)
) STRICT;

CREATE INDEX idx_image_tags_tag ON image_tags(tag_id, image_id);

CREATE TABLE collections (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    sort_order INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE collection_items (
    collection_id INTEGER NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
    image_id INTEGER NOT NULL REFERENCES images(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    PRIMARY KEY (collection_id, image_id)
) STRICT;

CREATE INDEX idx_collection_items_order
    ON collection_items(collection_id, position);

CREATE TABLE thumb_segments (
    id INTEGER PRIMARY KEY,
    file_name TEXT NOT NULL UNIQUE,
    size INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
) STRICT;

-- 嵌入向量与姿势数据不放进主库：主库只记录它们在 feature pack 中的位置，
-- 避免几十万条 1536 维向量把元数据库撑到数百 MB。
-- dim 对 embedding 是向量维度，对 pose 是 landmark 个数（33）；包内记录长度以包头为准。
CREATE TABLE feature_blobs (
    image_id INTEGER NOT NULL REFERENCES images(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('embedding', 'pose')),
    model TEXT NOT NULL,
    dim INTEGER NOT NULL,
    pack_name TEXT NOT NULL,
    pack_offset INTEGER NOT NULL,
    pack_length INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (image_id, kind)
) STRICT;

CREATE VIRTUAL TABLE image_search USING fts5(
    description,
    content='images',
    content_rowid='id',
    tokenize='trigram'
);

CREATE TRIGGER images_search_insert AFTER INSERT ON images BEGIN
    INSERT INTO image_search(rowid, description) VALUES (new.id, new.description);
END;

CREATE TRIGGER images_search_delete AFTER DELETE ON images BEGIN
    INSERT INTO image_search(image_search, rowid, description)
    VALUES ('delete', old.id, old.description);
END;

CREATE TRIGGER images_search_update AFTER UPDATE OF description ON images BEGIN
    INSERT INTO image_search(image_search, rowid, description)
    VALUES ('delete', old.id, old.description);
    INSERT INTO image_search(rowid, description) VALUES (new.id, new.description);
END;
"#;

/// 操作记录相关结构：新建库与 v4→v5 迁移共用一份 DDL，避免两处漂移。
///
/// 要点：`operations.id` 用 `AUTOINCREMENT`，记录清理后 id 不复用（待释放标记与
/// `.artbank/undo/<id>/` 都按 id 引用）；两个标记列不建外键，记录行删除后标记仍要能被释放扫描读到。
const HISTORY_SCHEMA_SQL: &str = r#"
CREATE INDEX idx_images_purge
    ON images(purge_operation_id) WHERE purge_operation_id IS NOT NULL;

CREATE TABLE operations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    label TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    status TEXT NOT NULL,
    task_id TEXT,
    affected INTEGER NOT NULL DEFAULT 0,
    error TEXT NOT NULL DEFAULT '',
    undone_at INTEGER
) STRICT;

CREATE TABLE operation_chunks (
    operation_id INTEGER NOT NULL REFERENCES operations(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    format TEXT NOT NULL,
    data BLOB NOT NULL,
    PRIMARY KEY (operation_id, seq)
) STRICT;

CREATE TABLE undo_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    target_operation_id INTEGER NOT NULL,
    state TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    finished_at INTEGER
) STRICT;
"#;

pub fn configure(connection: &Connection) -> AppResult<()> {
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -65536;
         PRAGMA mmap_size = 268435456;",
    )?;
    Ok(())
}

pub fn read_version(connection: &Connection) -> AppResult<u32> {
    let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    Ok(version)
}

pub fn initialize(connection: &mut Connection, manifest: &LibraryManifest) -> AppResult<()> {
    let version = read_version(connection)?;
    if version != 0 {
        return Err(AppError::message("图库数据库已经初始化"));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_SQL)?;
    transaction.execute_batch(HISTORY_SCHEMA_SQL)?;
    let now = chrono::Utc::now().timestamp_millis();
    transaction.execute(
        "INSERT INTO meta(key, value) VALUES ('appFormat', ?1), ('schemaVersion', ?2), ('libraryId', ?3), ('createdAt', ?4)",
        params![
            crate::library::paths::LIBRARY_FORMAT,
            SCHEMA_VERSION.to_string(),
            manifest.library_id,
            manifest.created_at.to_string()
        ],
    )?;
    transaction.execute(
        "INSERT INTO folders(path, created_at) VALUES ('', ?1)",
        params![now],
    )?;
    transaction.execute(
        "INSERT INTO thumb_segments(id, file_name, size, created_at) VALUES (0, '', 0, ?1)",
        params![now],
    )?;
    transaction.execute_batch("INSERT INTO image_search(image_search) VALUES ('rebuild');")?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

pub fn ensure(connection: &mut Connection, manifest: &LibraryManifest) -> AppResult<()> {
    let version = read_version(connection)?;
    match version {
        0 => initialize(connection, manifest),
        1 => {
            verify_meta(connection, manifest)?;
            migrate_v1_to_v2(connection)?;
            migrate_v2_to_v3(connection)?;
            migrate_v3_to_v4(connection)?;
            migrate_v4_to_v5(connection)
        }
        2 => {
            verify_meta(connection, manifest)?;
            migrate_v2_to_v3(connection)?;
            migrate_v3_to_v4(connection)?;
            migrate_v4_to_v5(connection)
        }
        3 => {
            verify_meta(connection, manifest)?;
            migrate_v3_to_v4(connection)?;
            migrate_v4_to_v5(connection)
        }
        4 => {
            verify_meta(connection, manifest)?;
            migrate_v4_to_v5(connection)
        }
        SCHEMA_VERSION => {
            verify_meta(connection, manifest)?;
            Ok(())
        }
        other => Err(AppError::message(format!(
            "图库 schema 版本 {other} 高于当前程序支持的 {SCHEMA_VERSION}"
        ))),
    }
}

fn verify_meta(connection: &Connection, manifest: &LibraryManifest) -> AppResult<()> {
    let format: String = connection.query_row(
        "SELECT value FROM meta WHERE key = 'appFormat'",
        [],
        |row| row.get(0),
    )?;
    if format != crate::library::paths::LIBRARY_FORMAT {
        return Err(AppError::message("数据库不是 Lilith Artbank 图库"));
    }
    let library_id: String = connection.query_row(
        "SELECT value FROM meta WHERE key = 'libraryId'",
        [],
        |row| row.get(0),
    )?;
    if library_id != manifest.library_id {
        return Err(AppError::message("图库清单与数据库的 libraryId 不一致"));
    }
    Ok(())
}

/// 版本号同时写在 `PRAGMA user_version`（读取依据）与 `meta.schemaVersion`（人看），必须一起改。
fn mark_version(connection: &Connection, version: u32) -> AppResult<()> {
    connection.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'schemaVersion'",
        [version.to_string()],
    )?;
    connection.pragma_update(None, "user_version", version)?;
    Ok(())
}

fn migrate_v1_to_v2(connection: &mut Connection) -> AppResult<()> {
    let mut statement = connection.prepare(
        "SELECT path FROM folders GROUP BY path COLLATE NOCASE HAVING count(*) > 1
         ORDER BY path",
    )?;
    let conflicts = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    if !conflicts.is_empty() {
        return Err(AppError::message(format!(
            "图库存在大小写冲突的文件夹路径，无法升级 schema：{}",
            conflicts.join("、")
        )));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_folders_path_nocase
         ON folders(path COLLATE NOCASE)",
        [],
    )?;
    mark_version(&transaction, 2)?;
    transaction.commit()?;
    log::info!("图库 schema 已从 v1 升级到 v2");
    Ok(())
}

/// v2 → v3：文件夹增加手动排序位，按现有名称顺序初始化，升级后视觉顺序不变。
fn migrate_v2_to_v3(connection: &mut Connection) -> AppResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute(
        "ALTER TABLE folders ADD COLUMN sort_index INTEGER NOT NULL DEFAULT 0",
        [],
    )?;
    transaction.execute(
        "WITH ordered AS (
             SELECT id, ROW_NUMBER() OVER (ORDER BY path COLLATE NOCASE, id) - 1 AS position
             FROM folders
         )
         UPDATE folders SET sort_index = (SELECT position FROM ordered WHERE ordered.id = folders.id)",
        [],
    )?;
    mark_version(&transaction, 3)?;
    transaction.commit()?;
    log::info!("图库 schema 已从 v2 升级到 v3");
    Ok(())
}

/// v3 → v4：补两个排序索引（修改时间、像素尺寸），避免这两种排序退化为全表排序。
fn migrate_v3_to_v4(connection: &mut Connection) -> AppResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_images_active_modified
             ON images(deleted_at, modified_at DESC, id DESC);
         CREATE INDEX IF NOT EXISTS idx_images_active_pixels
             ON images(deleted_at, (width * height) DESC, id DESC);",
    )?;
    mark_version(&transaction, 4)?;
    transaction.commit()?;
    log::info!("图库 schema 已从 v3 升级到 v4");
    Ok(())
}

/// v4 → v5：操作记录、待释放标记与导入来源；只新增表、列与索引，不改现有行。
fn migrate_v4_to_v5(connection: &mut Connection) -> AppResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "ALTER TABLE images ADD COLUMN purge_operation_id INTEGER;
         ALTER TABLE images ADD COLUMN origin_operation_id INTEGER;",
    )?;
    transaction.execute_batch(HISTORY_SCHEMA_SQL)?;
    mark_version(&transaction, SCHEMA_VERSION)?;
    transaction.commit()?;
    log::info!("图库 schema 已从 v4 升级到 v{SCHEMA_VERSION}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> LibraryManifest {
        LibraryManifest {
            format: crate::library::paths::LIBRARY_FORMAT.into(),
            format_version: crate::library::paths::LIBRARY_FORMAT_VERSION,
            library_id: "test-library".into(),
            name: "测试".into(),
            created_at: 1,
        }
    }

    #[test]
    fn initializes_schema_and_search() {
        let mut connection = Connection::open_in_memory().unwrap();
        configure(&connection).unwrap();
        initialize(&mut connection, &manifest()).unwrap();
        assert_eq!(read_version(&connection).unwrap(), SCHEMA_VERSION);

        let now = chrono::Utc::now().timestamp_millis();
        connection
            .execute(
                "INSERT INTO images(
                    content_hash, file_ext, folder_id, file_size, width, height, format,
                    has_alpha, phash, colors4, colors8, score, description, imported_at, modified_at
                 ) VALUES (?1, 'jpg', 1, 10, 4, 3, 'jpeg', 0, zeroblob(32), zeroblob(16), zeroblob(32), 3, '一张人物速写', ?2, ?2)",
                params!["a".repeat(64), now],
            )
            .unwrap();

        let found: i64 = connection
            .query_row(
                "SELECT rowid FROM image_search WHERE image_search MATCH '人物速'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(found, 1);

        connection
            .execute(
                "UPDATE images SET description = '新的描述' WHERE id = 1",
                [],
            )
            .unwrap();
        let old_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM image_search WHERE image_search MATCH '人物速'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_count, 0);
        let new_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM image_search WHERE image_search MATCH '新的描'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(new_count, 1);
    }

    #[test]
    fn refuses_double_initialization() {
        let mut connection = Connection::open_in_memory().unwrap();
        configure(&connection).unwrap();
        initialize(&mut connection, &manifest()).unwrap();
        assert!(initialize(&mut connection, &manifest()).is_err());
        ensure(&mut connection, &manifest()).unwrap();
    }

    fn downgrade_to_v1(connection: &Connection) {
        downgrade_to_v4(connection);
        connection
            .execute("DROP INDEX idx_folders_path_nocase", [])
            .unwrap();
        connection
            .execute("ALTER TABLE folders DROP COLUMN sort_index", [])
            .unwrap();
        connection.pragma_update(None, "user_version", 1).unwrap();
        connection
            .execute(
                "UPDATE meta SET value = '1' WHERE key = 'schemaVersion'",
                [],
            )
            .unwrap();
    }

    fn downgrade_to_v2(connection: &Connection) {
        downgrade_to_v4(connection);
        connection
            .execute("ALTER TABLE folders DROP COLUMN sort_index", [])
            .unwrap();
        connection.pragma_update(None, "user_version", 2).unwrap();
        connection
            .execute(
                "UPDATE meta SET value = '2' WHERE key = 'schemaVersion'",
                [],
            )
            .unwrap();
    }

    fn downgrade_to_v4(connection: &Connection) {
        connection
            .execute_batch(
                "DROP INDEX idx_images_purge;
             ALTER TABLE images DROP COLUMN purge_operation_id;
             ALTER TABLE images DROP COLUMN origin_operation_id;
             DROP TABLE undo_runs;
             DROP TABLE operation_chunks;
             DROP TABLE operations;",
            )
            .unwrap();
        connection.pragma_update(None, "user_version", 4).unwrap();
        connection
            .execute(
                "UPDATE meta SET value = '4' WHERE key = 'schemaVersion'",
                [],
            )
            .unwrap();
    }

    #[test]
    fn migrates_v1_to_v2_and_adds_case_insensitive_index() {
        let mut connection = Connection::open_in_memory().unwrap();
        configure(&connection).unwrap();
        initialize(&mut connection, &manifest()).unwrap();
        downgrade_to_v1(&connection);
        assert_eq!(read_version(&connection).unwrap(), 1);

        ensure(&mut connection, &manifest()).unwrap();
        assert_eq!(read_version(&connection).unwrap(), SCHEMA_VERSION);
        let version: String = connection
            .query_row(
                "SELECT value FROM meta WHERE key = 'schemaVersion'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION.to_string());
        let indexes: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_master
                 WHERE type = 'index' AND name = 'idx_folders_path_nocase'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(indexes, 1);

        connection
            .execute(
                "INSERT INTO folders(path, created_at) VALUES ('People', 1)",
                [],
            )
            .unwrap();
        assert!(
            connection
                .execute(
                    "INSERT INTO folders(path, created_at) VALUES ('people', 2)",
                    []
                )
                .is_err()
        );
    }

    #[test]
    fn migrates_v2_to_v3_and_orders_folders_by_name() {
        let mut connection = Connection::open_in_memory().unwrap();
        configure(&connection).unwrap();
        initialize(&mut connection, &manifest()).unwrap();
        downgrade_to_v2(&connection);
        assert_eq!(read_version(&connection).unwrap(), 2);

        // 插入顺序与名称顺序不一致，迁移后应按名称重新编号。
        for (path, created) in [("gamma", 1), ("Alpha", 2), ("beta", 3)] {
            connection
                .execute(
                    "INSERT INTO folders(path, created_at) VALUES (?1, ?2)",
                    params![path, created],
                )
                .unwrap();
        }

        ensure(&mut connection, &manifest()).unwrap();
        assert_eq!(read_version(&connection).unwrap(), SCHEMA_VERSION);

        let mut statement = connection
            .prepare("SELECT path, sort_index FROM folders ORDER BY sort_index")
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                (String::new(), 0),
                ("Alpha".to_owned(), 1),
                ("beta".to_owned(), 2),
                ("gamma".to_owned(), 3),
            ]
        );
    }

    #[test]
    fn new_library_has_folder_sort_index() {
        let mut connection = Connection::open_in_memory().unwrap();
        configure(&connection).unwrap();
        initialize(&mut connection, &manifest()).unwrap();
        connection
            .execute(
                "INSERT INTO folders(path, created_at) VALUES ('作品', 1)",
                [],
            )
            .unwrap();
        let index: i64 = connection
            .query_row(
                "SELECT sort_index FROM folders WHERE path = '作品'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index, 0);
    }

    #[test]
    fn refuses_v1_migration_on_case_conflicts() {
        let mut connection = Connection::open_in_memory().unwrap();
        configure(&connection).unwrap();
        initialize(&mut connection, &manifest()).unwrap();
        downgrade_to_v1(&connection);
        connection
            .execute(
                "INSERT INTO folders(path, created_at) VALUES ('People', 1)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO folders(path, created_at) VALUES ('people', 2)",
                [],
            )
            .unwrap();

        let error = ensure(&mut connection, &manifest()).unwrap_err();
        assert!(error.to_string().contains("大小写冲突"));
        assert_eq!(read_version(&connection).unwrap(), 1);
    }

    #[test]
    fn new_library_has_operation_tables_and_purge_columns() {
        let mut connection = Connection::open_in_memory().unwrap();
        configure(&connection).unwrap();
        initialize(&mut connection, &manifest()).unwrap();

        for table in ["operations", "operation_chunks", "undo_runs"] {
            let found: i64 = connection
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(found, 1, "缺少表 {table}");
        }
        let columns: i64 = connection
            .query_row(
                "SELECT count(*) FROM pragma_table_info('images')
                 WHERE name IN ('purge_operation_id', 'origin_operation_id')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(columns, 2);
        let indexes: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_master
                 WHERE type = 'index' AND name = 'idx_images_purge'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(indexes, 1);
    }

    #[test]
    fn migrates_v4_to_v5_and_keeps_existing_rows() {
        let mut connection = Connection::open_in_memory().unwrap();
        configure(&connection).unwrap();
        initialize(&mut connection, &manifest()).unwrap();
        downgrade_to_v4(&connection);
        assert_eq!(read_version(&connection).unwrap(), 4);

        connection
            .execute(
                "INSERT INTO images(
                    content_hash, file_ext, folder_id, file_size, width, height, format,
                    has_alpha, phash, colors4, colors8, score, description, imported_at, modified_at
                 ) VALUES (?1, 'jpg', 1, 10, 4, 3, 'jpeg', 0, zeroblob(32), zeroblob(16),
                           zeroblob(32), 3, '迁移前描述', 1, 1)",
                [("a".repeat(64))],
            )
            .unwrap();

        ensure(&mut connection, &manifest()).unwrap();
        assert_eq!(read_version(&connection).unwrap(), SCHEMA_VERSION);
        let version: String = connection
            .query_row(
                "SELECT value FROM meta WHERE key = 'schemaVersion'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION.to_string());

        let (hash, score, purge): (String, i64, Option<i64>) = connection
            .query_row(
                "SELECT content_hash, score, purge_operation_id FROM images WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(hash, "a".repeat(64));
        assert_eq!(score, 3);
        assert_eq!(purge, None);

        connection
            .execute(
                "INSERT INTO operations(kind, label, created_at, status)
                 VALUES ('trash', '移入回收站', 1, 'applied')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO operation_chunks(operation_id, seq, format, data)
                 VALUES (1, 0, 'ids', X'0000000000000000')",
                [],
            )
            .unwrap();
    }
}
