use std::{collections::HashSet, path::Path};

use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    app::error::{AppError, AppResult},
    library::{
        db::LibraryDb,
        import::{ensure_folder, next_sort_index},
        paths::{
            LibraryPaths, join_folder, normalize_folder, normalize_folder_name, parent_path,
            subtree_like_pattern,
        },
        query::{self, FolderInfo},
        service::now_ms,
    },
};

fn folder_id(connection: &Connection, path: &str) -> AppResult<Option<i64>> {
    connection
        .query_row(
            "SELECT id FROM folders WHERE path = ?1 COLLATE NOCASE",
            [path],
            |row| row.get(0),
        )
        .optional()
        .map_err(AppError::from)
}

fn folder_path(connection: &Connection, id: i64) -> AppResult<String> {
    if id <= 0 {
        return Err(AppError::message("文件夹 id 无效"));
    }
    connection
        .query_row("SELECT path FROM folders WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()?
        .ok_or_else(|| AppError::message("文件夹不存在"))
}

fn ensure_inside_root(root: &Path, directory: &Path) -> AppResult<()> {
    let canonical_root = root.canonicalize()?;
    let canonical_directory = directory.canonicalize()?;
    if !canonical_directory.starts_with(&canonical_root) {
        return Err(AppError::message("文件夹位于图库目录之外"));
    }
    Ok(())
}

pub fn create(db: &LibraryDb, root: &Path, path: &str) -> AppResult<Vec<FolderInfo>> {
    let path = normalize_folder(path)?;
    if path.is_empty() {
        return Err(AppError::message("根目录已经存在"));
    }
    let target = LibraryPaths::new(root).folder_dir(&path);
    db.with_writer(|connection| {
        if folder_id(connection, &path)?.is_some() {
            return Err(AppError::message("文件夹已存在"));
        }
        // 物理目录先落地；ensure_folder 再从根到叶补齐缺失的数据库行。
        std::fs::create_dir_all(&target)?;
        ensure_inside_root(root, &target)?;
        ensure_folder(connection, &path, now_ms())?;
        Ok(())
    })?;
    query::list_folders(db)
}

pub fn rename(
    db: &LibraryDb,
    root: &Path,
    folder_id_value: i64,
    name: &str,
) -> AppResult<Vec<FolderInfo>> {
    let name = normalize_folder_name(name)?;
    let paths = LibraryPaths::new(root);
    let (old_path, new_path) = db.with_writer(|connection| {
        let old_path = folder_path(connection, folder_id_value)?;
        if old_path.is_empty() {
            return Err(AppError::message("不能重命名图库根目录"));
        }
        let parent = parent_path(&old_path);
        let new_path = join_folder(parent, &name);
        if old_path == new_path {
            return Ok((old_path, new_path));
        }
        if let Some(existing) = folder_id(connection, &new_path)?
            && existing != folder_id_value
        {
            return Err(AppError::message("目标文件夹已存在"));
        }
        Ok((old_path, new_path))
    })?;
    if old_path == new_path {
        return query::list_folders(db);
    }

    let old_dir = paths.folder_dir(&old_path);
    let new_dir = paths.folder_dir(&new_path);
    if !old_dir.is_dir() {
        return Err(AppError::message(format!(
            "文件夹目录不存在：{}",
            old_dir.display()
        )));
    }
    if !new_path.eq_ignore_ascii_case(&old_path) && new_dir.exists() {
        return Err(AppError::message("目标文件夹目录已存在"));
    }
    rename_directory(&old_dir, &new_dir, new_path.eq_ignore_ascii_case(&old_path))?;
    ensure_inside_root(root, &new_dir)?;

    let update = db.with_writer(|connection| {
        update_subtree_paths(connection, &old_path, &new_path, folder_id_value)
    });
    if let Err(error) = update {
        if let Err(rollback) = rename_directory(&new_dir, &old_dir, true) {
            log::error!(
                "回滚文件夹重命名失败：{rollback}；目录位于 {}",
                new_dir.display()
            );
        }
        return Err(error);
    }
    query::list_folders(db)
}

pub fn move_to(
    db: &LibraryDb,
    root: &Path,
    folder_id_value: i64,
    parent_id: i64,
) -> AppResult<Vec<FolderInfo>> {
    let paths = LibraryPaths::new(root);
    let (old_path, new_path) = db.with_writer(|connection| {
        let old_path = folder_path(connection, folder_id_value)?;
        if old_path.is_empty() {
            return Err(AppError::message("不能移动图库根目录"));
        }
        let parent_path = folder_path(connection, parent_id)?;
        if parent_path == old_path || parent_path.starts_with(&format!("{old_path}/")) {
            return Err(AppError::message("不能把文件夹移动到自身或子文件夹"));
        }
        let leaf = old_path
            .rsplit_once('/')
            .map_or(old_path.as_str(), |(_, leaf)| leaf);
        let new_path = join_folder(&parent_path, leaf);
        if new_path == old_path {
            return Ok((old_path, new_path));
        }
        if let Some(existing) = folder_id(connection, &new_path)?
            && existing != folder_id_value
        {
            return Err(AppError::message("目标位置已经有同名文件夹"));
        }
        Ok((old_path, new_path))
    })?;
    if old_path == new_path {
        return query::list_folders(db);
    }

    let old_dir = paths.folder_dir(&old_path);
    let new_dir = paths.folder_dir(&new_path);
    if !old_dir.is_dir() {
        return Err(AppError::message(format!(
            "文件夹目录不存在：{}",
            old_dir.display()
        )));
    }
    if let Some(parent) = new_dir.parent() {
        std::fs::create_dir_all(parent)?;
        ensure_inside_root(root, parent)?;
    }
    if new_dir.exists() {
        return Err(AppError::message("目标文件夹目录已存在"));
    }
    std::fs::rename(&old_dir, &new_dir)?;
    ensure_inside_root(root, &new_dir)?;

    let update = db.with_writer(|connection| {
        update_subtree_paths(connection, &old_path, &new_path, folder_id_value)?;
        let next = next_sort_index(connection, &new_path)?;
        connection.execute(
            "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
            params![next, folder_id_value],
        )?;
        Ok(())
    });
    if let Err(error) = update {
        if let Err(rollback) = std::fs::rename(&new_dir, &old_dir) {
            log::error!(
                "回滚文件夹移动失败：{rollback}；目录位于 {}",
                new_dir.display()
            );
        }
        return Err(error);
    }
    query::list_folders(db)
}

/// 把同一父级下的若干文件夹移动到指定位置；只调整 `sort_index`，不动目录。
pub fn reorder(
    db: &LibraryDb,
    folder_ids: &[i64],
    parent_id: i64,
    sort_index: i64,
) -> AppResult<Vec<FolderInfo>> {
    if folder_ids.is_empty() {
        return Err(AppError::message("没有要调整顺序的文件夹"));
    }
    let mut moved = HashSet::new();
    for id in folder_ids {
        if *id <= 0 || !moved.insert(*id) {
            return Err(AppError::message("文件夹 id 无效或重复"));
        }
    }
    db.with_writer(|connection| {
        let parent = folder_path(connection, parent_id)?;
        // 同级文件夹一次读出来：手动排序只在拖放时发生，几千行路径的读取代价可以接受。
        let mut statement =
            connection.prepare("SELECT id, path FROM folders ORDER BY sort_index, id")?;
        let folders = statement
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);

        let mut siblings: Vec<i64> = folders
            .iter()
            .filter(|(_, path)| !path.is_empty() && parent_path(path) == parent)
            .map(|(id, _)| *id)
            .collect();
        if let Some(foreign) = moved.iter().find(|id| !siblings.contains(id)) {
            return Err(AppError::message(format!(
                "文件夹 {foreign} 不在目标位置，只能调整同一级文件夹的顺序"
            )));
        }

        siblings.retain(|id| !moved.contains(id));
        let insert_at = sort_index.clamp(0, siblings.len() as i64) as usize;
        siblings.splice(insert_at..insert_at, folder_ids.iter().copied());

        let transaction = connection.unchecked_transaction()?;
        for (position, id) in siblings.iter().enumerate() {
            transaction.execute(
                "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
                params![position as i64, id],
            )?;
        }
        transaction.commit()?;
        Ok(())
    })?;
    query::list_folders(db)
}

pub fn delete(db: &LibraryDb, root: &Path, folder_id_value: i64) -> AppResult<Vec<FolderInfo>> {
    let paths = LibraryPaths::new(root);
    let path = db.with_writer(|connection| {
        let path = folder_path(connection, folder_id_value)?;
        if path.is_empty() {
            return Err(AppError::message("不能删除图库根目录"));
        }
        let pattern = subtree_like_pattern(&path);
        let children: i64 = connection.query_row(
            "SELECT count(*) FROM folders WHERE path LIKE ?1 ESCAPE '\\'",
            [pattern],
            |row| row.get(0),
        )?;
        if children > 0 {
            return Err(AppError::message("文件夹还包含子文件夹，不能删除"));
        }
        let images: i64 = connection.query_row(
            "SELECT count(*) FROM images WHERE folder_id = ?1",
            [folder_id_value],
            |row| row.get(0),
        )?;
        if images > 0 {
            return Err(AppError::message("文件夹中还有图片，不能删除"));
        }
        Ok(path)
    })?;

    let directory = paths.folder_dir(&path);
    if directory.exists() {
        ensure_inside_root(root, &directory)?;
        if std::fs::read_dir(&directory)?.next().is_some() {
            return Err(AppError::message("文件夹目录不是空的，不能删除"));
        }
        std::fs::remove_dir(&directory)?;
    }

    if let Err(error) = db.with_writer(|connection| {
        connection.execute("DELETE FROM folders WHERE id = ?1", [folder_id_value])?;
        Ok(())
    }) {
        if let Err(rollback) = std::fs::create_dir_all(&directory) {
            log::error!(
                "回滚文件夹删除失败：{rollback}；目录应位于 {}",
                directory.display()
            );
        }
        return Err(error);
    }
    query::list_folders(db)
}

fn rename_directory(old: &Path, new: &Path, case_only: bool) -> AppResult<()> {
    if !case_only {
        return std::fs::rename(old, new).map_err(AppError::from);
    }
    // Windows 不允许直接把 Foo 改成 foo（目标被视为已存在），用临时名中转。
    let parent = old.parent().unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(".artbank-rename-{}", uuid::Uuid::new_v4()));
    std::fs::rename(old, &temporary)?;
    match std::fs::rename(&temporary, new) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::rename(&temporary, old);
            Err(error.into())
        }
    }
}

fn update_subtree_paths(
    connection: &Connection,
    old_path: &str,
    new_path: &str,
    folder_id_value: i64,
) -> AppResult<()> {
    let transaction = connection.unchecked_transaction()?;
    let pattern = subtree_like_pattern(old_path);
    transaction.execute(
        "UPDATE folders
         SET path = ?1 || substr(path, length(?2) + 1)
         WHERE path LIKE ?3 ESCAPE '\\' AND id <> ?4",
        params![new_path, old_path, pattern, folder_id_value],
    )?;
    transaction.execute(
        "UPDATE folders SET path = ?1 WHERE id = ?2",
        params![new_path, folder_id_value],
    )?;
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, sync::Arc};

    use tempfile::tempdir;

    use super::*;
    use crate::library::service;

    fn fixture() -> (tempfile::TempDir, Arc<LibraryDb>, PathBuf) {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "文件夹测试", 1).unwrap();
        let database = session.db.clone();
        (temp, database, root)
    }

    #[test]
    fn creates_nested_folders_with_ancestors() {
        let (_temp, db, root) = fixture();
        let folders = create(&db, &root, "人物/速写/手部").unwrap();
        let paths: Vec<&str> = folders.iter().map(|item| item.path.as_str()).collect();
        assert!(paths.contains(&"人物"));
        assert!(paths.contains(&"人物/速写"));
        assert!(paths.contains(&"人物/速写/手部"));
        assert!(root.join("人物").join("速写").join("手部").is_dir());
        assert!(create(&db, &root, "人物").is_err());
        assert!(create(&db, &root, "CON").is_err());
    }

    #[test]
    fn renames_and_moves_subtrees() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "人物/速写").unwrap();
        let people = db
            .with_reader(|connection| folder_id(connection, "人物"))
            .unwrap()
            .unwrap();
        let sketch = db
            .with_reader(|connection| folder_id(connection, "人物/速写"))
            .unwrap()
            .unwrap();
        let scenery = create(&db, &root, "风景").unwrap();
        let scenery_id = scenery
            .iter()
            .find(|item| item.path == "风景")
            .map(|item| item.id)
            .unwrap();

        rename(&db, &root, sketch, "草稿").unwrap();
        assert!(root.join("人物").join("草稿").is_dir());
        assert!(!root.join("人物").join("速写").exists());
        let sketch = db
            .with_reader(|connection| folder_id(connection, "人物/草稿"))
            .unwrap()
            .unwrap();

        move_to(&db, &root, sketch, scenery_id).unwrap();
        assert!(root.join("风景").join("草稿").is_dir());
        let moved = db
            .with_reader(|connection| folder_id(connection, "风景/草稿"))
            .unwrap();
        assert_eq!(moved, Some(sketch));

        assert!(move_to(&db, &root, people, people).is_err());
        assert!(move_to(&db, &root, people, scenery_id).is_ok());
    }

    #[test]
    fn renames_case_only_and_rejects_conflicts() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "Sketches").unwrap();
        let id = db
            .with_reader(|connection| folder_id(connection, "Sketches"))
            .unwrap()
            .unwrap();
        rename(&db, &root, id, "sketches").unwrap();
        let names: Vec<String> = fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|name| name == "sketches"));
        assert!(!names.iter().any(|name| name == "Sketches"));
        assert!(create(&db, &root, "SKETCHES").is_err());
    }

    #[test]
    fn reorders_siblings_and_rejects_foreign_folders() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "一").unwrap();
        create(&db, &root, "二").unwrap();
        create(&db, &root, "三").unwrap();
        create(&db, &root, "组/子").unwrap();
        let id = |path: &str| {
            db.with_reader(|connection| folder_id(connection, path))
                .unwrap()
                .unwrap()
        };
        let root_id = id("");
        let (one, two, three) = (id("一"), id("二"), id("三"));
        let group_child = id("组/子");

        let order = |db: &LibraryDb| {
            db.with_reader(|connection| {
                let mut statement = connection.prepare(
                    "SELECT path FROM folders WHERE instr(path, '/') = 0 AND path <> ''
                     ORDER BY sort_index, id",
                )?;
                let paths = statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(paths)
            })
            .unwrap()
        };

        reorder(&db, &[three], root_id, 0).unwrap();
        assert_eq!(order(&db), vec!["三", "一", "二", "组"]);

        reorder(&db, &[one], root_id, 10).unwrap();
        reorder(&db, &[two], root_id, 0).unwrap();
        assert_eq!(order(&db), vec!["二", "三", "组", "一"]);

        assert!(reorder(&db, &[group_child], root_id, 0).is_err());
        assert!(reorder(&db, &[one, two], three, 0).is_err());
    }

    #[test]
    fn deletes_only_empty_folders() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "空文件夹").unwrap();
        create(&db, &root, "有子级/子级").unwrap();
        let empty = db
            .with_reader(|connection| folder_id(connection, "空文件夹"))
            .unwrap()
            .unwrap();
        let parent = db
            .with_reader(|connection| folder_id(connection, "有子级"))
            .unwrap()
            .unwrap();

        assert!(delete(&db, &root, parent).is_err());
        delete(&db, &root, empty).unwrap();
        assert!(!root.join("空文件夹").exists());
        assert!(
            db.with_reader(|connection| folder_id(connection, "空文件夹"))
                .unwrap()
                .is_none()
        );

        let untracked = root.join("外部目录");
        fs::create_dir_all(&untracked).unwrap();
        fs::write(untracked.join("keep.txt"), b"x").unwrap();
        create(&db, &root, "外部目录").unwrap();
        let external = db
            .with_reader(|connection| folder_id(connection, "外部目录"))
            .unwrap()
            .unwrap();
        assert!(delete(&db, &root, external).is_err());
        assert!(untracked.join("keep.txt").exists());
    }
}
