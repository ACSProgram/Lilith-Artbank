use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value};
use serde::{Deserialize, Serialize};

use crate::{
    app::error::{AppError, AppResult},
    library::{
        db::LibraryDb,
        history::{
            self, CollectionItemsRecord, CollectionOrderState, CollectionRenameRecord,
            CollectionReorderRecord, CollectionSnapshot, HistoryKind, NewOperation,
        },
        placeholders,
        service::now_ms,
    },
};

const SQL_CHUNK: usize = 400;
const MAX_IMAGES: usize = 1_000_000;
const MAX_COMBINE_COLLECTIONS: usize = 32;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionInfo {
    pub id: i64,
    pub name: String,
    pub sort_order: i64,
    pub image_count: u64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CombineMode {
    Union,
    Intersection,
    Difference,
}

pub fn list(database: &LibraryDb) -> AppResult<Vec<CollectionInfo>> {
    database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT collections.id, collections.name, collections.sort_order,
                    collections.created_at, collections.updated_at,
                    (SELECT count(*) FROM collection_items
                     JOIN images ON images.id = collection_items.image_id
                     WHERE collection_items.collection_id = collections.id
                       AND images.deleted_at IS NULL)
             FROM collections
             ORDER BY collections.sort_order, collections.id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(CollectionInfo {
                id: row.get(0)?,
                name: row.get(1)?,
                sort_order: row.get(2)?,
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
                image_count: row.get::<_, i64>(5)?.max(0) as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

pub fn create(database: &LibraryDb, name: &str) -> AppResult<CollectionInfo> {
    let name = validate_name(name)?;
    history::write(
        database,
        NewOperation::new(HistoryKind::CollectionCreate, format!("创建集合「{name}」")),
        |transaction, draft| {
            ensure_unique_name(transaction, &name, None)?;
            let sort_order = next_sort_order(transaction)?;
            let now = now_ms();
            transaction.execute(
                "INSERT INTO collections(name, sort_order, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?3)",
                params![name, sort_order, now],
            )?;
            let id = transaction.last_insert_rowid();
            draft.json(&CollectionSnapshot {
                id,
                name: name.clone(),
                sort_order,
                created_at: now,
                updated_at: now,
                items: Vec::new(),
            })?;
            draft.affected(1);
            Ok(CollectionInfo {
                id,
                name: name.clone(),
                sort_order,
                image_count: 0,
                created_at: now,
                updated_at: now,
            })
        },
    )
}

pub fn rename(database: &LibraryDb, collection_id: i64, name: &str) -> AppResult<()> {
    validate_id(collection_id)?;
    let name = validate_name(name)?;
    history::write(
        database,
        NewOperation::new(
            HistoryKind::CollectionRename,
            format!("重命名集合「{name}」"),
        ),
        |transaction, draft| {
            ensure_unique_name(transaction, &name, Some(collection_id))?;
            let old: Option<(String, i64)> = transaction
                .query_row(
                    "SELECT name, updated_at FROM collections WHERE id = ?1",
                    [collection_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((old_name, old_updated_at)) = old else {
                return Err(AppError::message("集合不存在"));
            };
            if old_name == name {
                return Ok(());
            }
            transaction.execute(
                "UPDATE collections SET name = ?1, updated_at = ?2 WHERE id = ?3",
                params![name, now_ms(), collection_id],
            )?;
            draft.json(&CollectionRenameRecord {
                id: collection_id,
                old_name,
                old_updated_at,
            })?;
            draft.affected(1);
            Ok(())
        },
    )
}

pub fn delete(database: &LibraryDb, collection_id: i64) -> AppResult<u64> {
    validate_id(collection_id)?;
    let name = collection_name(database, collection_id)?;
    history::write(
        database,
        NewOperation::new(HistoryKind::CollectionDelete, format!("删除集合「{name}」")),
        |transaction, draft| {
            let snapshot = history::collection_snapshot(transaction, collection_id)?
                .ok_or_else(|| AppError::message("集合不存在"))?;
            transaction.execute("DELETE FROM collections WHERE id = ?1", [collection_id])?;
            let affected = snapshot.items.len() as u64;
            draft.json(&snapshot)?;
            draft.affected(affected);
            Ok(affected)
        },
    )
}

pub fn reorder(database: &LibraryDb, collection_ids: &[i64]) -> AppResult<()> {
    let ids = validate_ids(collection_ids, 10_000)?;
    history::write(
        database,
        NewOperation::new(HistoryKind::CollectionReorder, "调整集合顺序"),
        |transaction, draft| {
            let existing: HashSet<i64> = transaction
                .prepare("SELECT id FROM collections")?
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<HashSet<_>>>()?;
            if existing.len() != ids.len() || !ids.iter().all(|id| existing.contains(id)) {
                return Err(AppError::message("排序必须包含全部集合"));
            }
            let now = now_ms();
            let mut rows = Vec::with_capacity(ids.len());
            for (index, id) in ids.iter().enumerate() {
                let (old_sort_order, old_updated_at): (i64, i64) = transaction.query_row(
                    "SELECT sort_order, updated_at FROM collections WHERE id = ?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                rows.push(CollectionOrderState {
                    id: *id,
                    sort_order: old_sort_order,
                    updated_at: old_updated_at,
                });
                transaction.execute(
                    "UPDATE collections SET sort_order = ?1, updated_at = ?2 WHERE id = ?3",
                    params![index as i64, now, id],
                )?;
            }
            draft.json(&CollectionReorderRecord { rows })?;
            draft.affected(ids.len() as u64);
            Ok(())
        },
    )
}

pub fn add_images(database: &LibraryDb, collection_id: i64, image_ids: &[i64]) -> AppResult<u64> {
    validate_id(collection_id)?;
    let ids = validate_ids(image_ids, MAX_IMAGES)?;
    let name = collection_name(database, collection_id)?;
    history::write(
        database,
        NewOperation::new(HistoryKind::CollectionAdd, format!("加入集合「{name}」")),
        |transaction, draft| {
            ensure_collection(transaction, collection_id)?;
            let old_updated_at: i64 = transaction.query_row(
                "SELECT updated_at FROM collections WHERE id = ?1",
                [collection_id],
                |row| row.get(0),
            )?;
            let mut position: i64 = transaction.query_row(
                "SELECT coalesce(max(position), -1) + 1 FROM collection_items
                 WHERE collection_id = ?1",
                [collection_id],
                |row| row.get(0),
            )?;
            let mut items = Vec::new();
            {
                let mut statement = transaction.prepare(
                    "INSERT INTO collection_items(collection_id, image_id, position)
                     SELECT ?1, ?2, ?3 WHERE EXISTS (SELECT 1 FROM images WHERE id = ?2)
                     ON CONFLICT(collection_id, image_id) DO NOTHING",
                )?;
                for image_id in &ids {
                    let changed = statement.execute(params![collection_id, image_id, position])?;
                    if changed > 0 {
                        items.push((*image_id, position));
                    }
                    position += 1;
                }
            }
            if items.is_empty() {
                return Ok(0);
            }
            transaction.execute(
                "UPDATE collections SET updated_at = ?1 WHERE id = ?2",
                params![now_ms(), collection_id],
            )?;
            draft.json(&CollectionItemsRecord {
                collection_id,
                items: items.clone(),
                old_updated_at,
            })?;
            draft.affected(items.len() as u64);
            Ok(items.len() as u64)
        },
    )
}

pub fn remove_images(
    database: &LibraryDb,
    collection_id: i64,
    image_ids: &[i64],
) -> AppResult<u64> {
    validate_id(collection_id)?;
    let ids = validate_ids(image_ids, MAX_IMAGES)?;
    let name = collection_name(database, collection_id)?;
    history::write(
        database,
        NewOperation::new(HistoryKind::CollectionRemove, format!("移出集合「{name}」")),
        |transaction, draft| {
            ensure_collection(transaction, collection_id)?;
            let old_updated_at: i64 = transaction.query_row(
                "SELECT updated_at FROM collections WHERE id = ?1",
                [collection_id],
                |row| row.get(0),
            )?;
            let mut items = Vec::new();
            for chunk in ids.chunks(SQL_CHUNK) {
                let marks = placeholders(chunk.len());
                let present: Vec<(i64, i64)> = {
                    let mut statement = transaction.prepare(&format!(
                        "SELECT image_id, position FROM collection_items
                         WHERE collection_id = ?1 AND image_id IN ({marks})"
                    ))?;
                    statement
                        .query_map(
                            params_from_iter(
                                std::iter::once(Value::Integer(collection_id))
                                    .chain(chunk.iter().map(|id| Value::Integer(*id))),
                            ),
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                };
                if present.is_empty() {
                    continue;
                }
                transaction.execute(
                    &format!(
                        "DELETE FROM collection_items
                         WHERE collection_id = ?1 AND image_id IN ({marks})"
                    ),
                    params_from_iter(
                        std::iter::once(Value::Integer(collection_id))
                            .chain(chunk.iter().map(|id| Value::Integer(*id))),
                    ),
                )?;
                items.extend(present);
            }
            if items.is_empty() {
                return Ok(0);
            }
            transaction.execute(
                "UPDATE collections SET updated_at = ?1 WHERE id = ?2",
                params![now_ms(), collection_id],
            )?;
            draft.json(&CollectionItemsRecord {
                collection_id,
                items: items.clone(),
                old_updated_at,
            })?;
            draft.affected(items.len() as u64);
            Ok(items.len() as u64)
        },
    )
}

pub fn combine(
    database: &LibraryDb,
    mode: CombineMode,
    collection_ids: &[i64],
    name: &str,
) -> AppResult<CollectionInfo> {
    let ids = validate_ids(collection_ids, MAX_COMBINE_COLLECTIONS)?;
    if ids.len() < 2 {
        return Err(AppError::message("集合运算至少需要两个集合"));
    }
    let name = validate_name(name)?;
    let mode_label = match mode {
        CombineMode::Union => "并集",
        CombineMode::Intersection => "交集",
        CombineMode::Difference => "差集",
    };
    history::write(
        database,
        NewOperation::new(
            HistoryKind::CollectionCombine,
            format!("集合{mode_label}「{name}」"),
        ),
        |transaction, draft| {
            ensure_unique_name(transaction, &name, None)?;

            let mut lists = Vec::with_capacity(ids.len());
            for collection_id in &ids {
                ensure_collection(transaction, *collection_id)?;
                let mut statement = transaction.prepare(
                    "SELECT image_id FROM collection_items
                     WHERE collection_id = ?1 ORDER BY position, image_id",
                )?;
                lists.push(
                    statement
                        .query_map([collection_id], |row| row.get::<_, i64>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?,
                );
            }

            let combined = combine_ids(mode, &lists);
            let sort_order = next_sort_order(transaction)?;
            let now = now_ms();
            transaction.execute(
                "INSERT INTO collections(name, sort_order, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?3)",
                params![name, sort_order, now],
            )?;
            let collection_id = transaction.last_insert_rowid();
            let items: Vec<(i64, i64)> = combined
                .iter()
                .enumerate()
                .map(|(position, image_id)| (*image_id, position as i64))
                .collect();
            {
                let mut statement = transaction.prepare(
                    "INSERT INTO collection_items(collection_id, image_id, position)
                     VALUES (?1, ?2, ?3)",
                )?;
                for (image_id, position) in &items {
                    statement.execute(params![collection_id, image_id, position])?;
                }
            }
            draft.json(&CollectionSnapshot {
                id: collection_id,
                name: name.clone(),
                sort_order,
                created_at: now,
                updated_at: now,
                items: items.clone(),
            })?;
            draft.affected(items.len() as u64);
            // imageCount 与 list() 保持同一口径：只统计未删除图片。
            let image_count: i64 = transaction.query_row(
                "SELECT count(*) FROM collection_items
                 JOIN images ON images.id = collection_items.image_id
                 WHERE collection_items.collection_id = ?1 AND images.deleted_at IS NULL",
                [collection_id],
                |row| row.get(0),
            )?;
            Ok(CollectionInfo {
                id: collection_id,
                name: name.clone(),
                sort_order,
                image_count: image_count.max(0) as u64,
                created_at: now,
                updated_at: now,
            })
        },
    )
}

/// 相似分组保存：一次事务创建全部集合，只记一条历史。
pub fn create_groups(
    database: &LibraryDb,
    groups: &[Vec<i64>],
    base_name: &str,
) -> AppResult<Vec<CollectionInfo>> {
    let base_name = base_name.trim();
    let base_name = if base_name.is_empty() {
        "相似组".to_owned()
    } else {
        base_name.to_owned()
    };
    validate_name(&base_name)?;
    let groups: Vec<Vec<i64>> = groups
        .iter()
        .filter(|group| !group.is_empty())
        .map(|group| validate_ids(group, MAX_IMAGES))
        .collect::<AppResult<Vec<_>>>()?;
    if groups.is_empty() {
        return Err(AppError::message("没有可保存的分组"));
    }
    history::write(
        database,
        NewOperation::new(
            HistoryKind::SimilarityGroups,
            format!("保存 {} 个相似组为集合", groups.len()),
        ),
        |transaction, draft| {
            let mut used: HashSet<String> = transaction
                .prepare("SELECT name FROM collections")?
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<HashSet<_>>>()?
                .into_iter()
                .map(|name| name.to_lowercase())
                .collect();
            let mut sort_order = next_sort_order(transaction)?;
            let now = now_ms();
            let mut snapshots = Vec::new();
            let mut infos = Vec::new();
            let mut affected = 0_u64;
            for group in &groups {
                let name = unique_name(&mut used, &base_name, infos.len());
                transaction.execute(
                    "INSERT INTO collections(name, sort_order, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?3)",
                    params![name, sort_order, now],
                )?;
                let collection_id = transaction.last_insert_rowid();
                let mut items = Vec::new();
                {
                    let mut statement = transaction.prepare(
                        "INSERT INTO collection_items(collection_id, image_id, position)
                         SELECT ?1, ?2, ?3 WHERE EXISTS (SELECT 1 FROM images WHERE id = ?2)
                         ON CONFLICT(collection_id, image_id) DO NOTHING",
                    )?;
                    for image_id in group {
                        let position = items.len() as i64;
                        if statement.execute(params![collection_id, image_id, position])? > 0 {
                            items.push((*image_id, position));
                        }
                    }
                }
                if items.is_empty() {
                    transaction
                        .execute("DELETE FROM collections WHERE id = ?1", [collection_id])?;
                    continue;
                }
                affected += items.len() as u64;
                snapshots.push(CollectionSnapshot {
                    id: collection_id,
                    name: name.clone(),
                    sort_order,
                    created_at: now,
                    updated_at: now,
                    items: items.clone(),
                });
                infos.push(CollectionInfo {
                    id: collection_id,
                    name,
                    sort_order,
                    image_count: items.len() as u64,
                    created_at: now,
                    updated_at: now,
                });
                sort_order += 1;
            }
            if snapshots.is_empty() {
                return Err(AppError::message("分组里的图片都已不存在"));
            }
            draft.json(&snapshots)?;
            draft.affected(affected);
            Ok(infos)
        },
    )
}

fn next_sort_order(connection: &Connection) -> AppResult<i64> {
    Ok(connection.query_row(
        "SELECT coalesce(max(sort_order), -1) + 1 FROM collections",
        [],
        |row| row.get(0),
    )?)
}

fn unique_name(used: &mut HashSet<String>, base: &str, index: usize) -> String {
    let mut name = format!("{base} {}", index + 1);
    let mut suffix = 2;
    while used.contains(&name.to_lowercase()) {
        name = format!("{base} {}-{suffix}", index + 1);
        suffix += 1;
    }
    used.insert(name.to_lowercase());
    name
}

fn collection_name(database: &LibraryDb, collection_id: i64) -> AppResult<String> {
    database.with_reader(|connection| {
        connection
            .query_row(
                "SELECT name FROM collections WHERE id = ?1",
                [collection_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| AppError::message("集合不存在"))
    })
}

fn combine_ids(mode: CombineMode, lists: &[Vec<i64>]) -> Vec<i64> {
    let first = &lists[0];
    match mode {
        CombineMode::Union => {
            let mut seen = HashSet::new();
            let mut result = Vec::new();
            for list in lists {
                for id in list {
                    if seen.insert(*id) {
                        result.push(*id);
                    }
                }
            }
            result
        }
        CombineMode::Intersection => {
            let others: Vec<HashSet<i64>> = lists[1..]
                .iter()
                .map(|list| list.iter().copied().collect())
                .collect();
            let mut seen = HashSet::new();
            first
                .iter()
                .copied()
                .filter(|id| seen.insert(*id) && others.iter().all(|set| set.contains(id)))
                .collect()
        }
        CombineMode::Difference => {
            let others: HashSet<i64> = lists[1..]
                .iter()
                .flat_map(|list| list.iter().copied())
                .collect();
            let mut seen = HashSet::new();
            first
                .iter()
                .copied()
                .filter(|id| seen.insert(*id) && !others.contains(id))
                .collect()
        }
    }
}

fn ensure_collection(connection: &Connection, collection_id: i64) -> AppResult<()> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM collections WHERE id = ?1)",
        [collection_id],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(AppError::message("集合不存在"));
    }
    Ok(())
}

fn ensure_unique_name(
    connection: &Connection,
    name: &str,
    exclude_id: Option<i64>,
) -> AppResult<()> {
    let duplicate: Option<i64> = connection
        .query_row(
            "SELECT id FROM collections
             WHERE name = ?1 COLLATE NOCASE AND id <> coalesce(?2, -1)",
            params![name, exclude_id],
            |row| row.get(0),
        )
        .optional()?;
    if duplicate.is_some() {
        return Err(AppError::message("已经有同名集合"));
    }
    Ok(())
}

fn validate_name(name: &str) -> AppResult<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::message("集合名称不能为空"));
    }
    if name.chars().count() > 120 {
        return Err(AppError::message("集合名称不能超过 120 个字符"));
    }
    if name.chars().any(char::is_control) {
        return Err(AppError::message("集合名称包含控制字符"));
    }
    Ok(name.to_owned())
}

fn validate_id(id: i64) -> AppResult<()> {
    if id <= 0 {
        return Err(AppError::message("集合 id 无效"));
    }
    Ok(())
}

fn validate_ids(values: &[i64], max: usize) -> AppResult<Vec<i64>> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for value in values {
        if *value <= 0 {
            return Err(AppError::message("id 无效"));
        }
        if seen.insert(*value) {
            result.push(*value);
        }
    }
    if result.is_empty() {
        return Err(AppError::message("没有提供 id"));
    }
    if result.len() > max {
        return Err(AppError::message(format!("一次最多处理 {max} 项")));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use rusqlite::params;
    use tempfile::tempdir;

    use super::*;
    use crate::library::{import::ensure_folder, service};

    struct Fixture {
        _temp: tempfile::TempDir,
        session: service::LibrarySession,
        images: Vec<i64>,
    }

    fn fixture() -> Fixture {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "集合测试", 1).unwrap();
        let images = session
            .db
            .with_writer(|connection| {
                let folder = ensure_folder(connection, "", 1)?;
                let mut ids = Vec::new();
                for index in 0..6 {
                    connection.execute(
                        "INSERT INTO images(
                            content_hash, file_ext, folder_id, file_size, width, height, format,
                            has_alpha, phash, colors4, colors8, score, description,
                            imported_at, modified_at
                         ) VALUES (?1, 'jpg', ?2, 1, 1, 1, 'jpeg', 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                        params![format!("{index:064x}"), folder],
                    )?;
                    ids.push(connection.last_insert_rowid());
                }
                Ok(ids)
            })
            .unwrap();
        Fixture {
            _temp: temp,
            session,
            images,
        }
    }

    fn collection_ids(database: &LibraryDb, collection_id: i64) -> Vec<i64> {
        database
            .with_reader(|connection| {
                let mut statement = connection.prepare(
                    "SELECT image_id FROM collection_items
                     WHERE collection_id = ?1 ORDER BY position, image_id",
                )?;
                Ok(statement
                    .query_map([collection_id], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .unwrap()
    }

    #[test]
    fn creates_renames_reorders_and_deletes() {
        let fixture = fixture();
        let first = create(&fixture.session.db, "参考").unwrap();
        let second = create(&fixture.session.db, "练习").unwrap();
        assert!(create(&fixture.session.db, "参考").is_err());
        rename(&fixture.session.db, first.id, "精选").unwrap();
        reorder(&fixture.session.db, &[second.id, first.id]).unwrap();
        let collections = list(&fixture.session.db).unwrap();
        assert_eq!(collections[0].id, second.id);
        assert_eq!(collections[1].name, "精选");
        assert_eq!(delete(&fixture.session.db, first.id).unwrap(), 0);
        assert_eq!(list(&fixture.session.db).unwrap().len(), 1);
    }

    #[test]
    fn adds_and_removes_images_without_duplicates() {
        let fixture = fixture();
        let collection = create(&fixture.session.db, "参考").unwrap();
        let ids = &fixture.images;
        assert_eq!(
            add_images(
                &fixture.session.db,
                collection.id,
                &[ids[0], ids[2], ids[1]]
            )
            .unwrap(),
            3
        );
        assert_eq!(
            add_images(&fixture.session.db, collection.id, &[ids[0], ids[3]]).unwrap(),
            1
        );
        assert_eq!(
            collection_ids(&fixture.session.db, collection.id),
            vec![ids[0], ids[2], ids[1], ids[3]]
        );
        assert_eq!(
            remove_images(&fixture.session.db, collection.id, &[ids[2], ids[5]]).unwrap(),
            1
        );
        assert_eq!(
            collection_ids(&fixture.session.db, collection.id),
            vec![ids[0], ids[1], ids[3]]
        );
        assert!(add_images(&fixture.session.db, 999, &[ids[0]]).is_err());
    }

    #[test]
    fn combines_collections_with_set_operations() {
        let fixture = fixture();
        let ids = &fixture.images;
        let first = create(&fixture.session.db, "A").unwrap();
        let second = create(&fixture.session.db, "B").unwrap();
        let third = create(&fixture.session.db, "C").unwrap();
        add_images(&fixture.session.db, first.id, &[ids[0], ids[1], ids[2]]).unwrap();
        add_images(&fixture.session.db, second.id, &[ids[1], ids[2], ids[3]]).unwrap();
        add_images(&fixture.session.db, third.id, &[ids[2], ids[4]]).unwrap();

        let union = combine(
            &fixture.session.db,
            CombineMode::Union,
            &[first.id, second.id, third.id],
            "并集",
        )
        .unwrap();
        assert_eq!(
            collection_ids(&fixture.session.db, union.id),
            vec![ids[0], ids[1], ids[2], ids[3], ids[4]]
        );

        let intersection = combine(
            &fixture.session.db,
            CombineMode::Intersection,
            &[first.id, second.id],
            "交集",
        )
        .unwrap();
        assert_eq!(
            collection_ids(&fixture.session.db, intersection.id),
            vec![ids[1], ids[2]]
        );

        let difference = combine(
            &fixture.session.db,
            CombineMode::Difference,
            &[first.id, second.id, third.id],
            "差集",
        )
        .unwrap();
        assert_eq!(
            collection_ids(&fixture.session.db, difference.id),
            vec![ids[0]]
        );
    }
}
