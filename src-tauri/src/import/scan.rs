use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use crate::{
    import::job::ImportFailure,
    library::paths::{DATA_DIR, LEGACY_DATA_DIR, OUTPUT_DIR, RECYCLE_DIR, TEMP_FOLDER_DIR},
};

pub const SUPPORTED_EXTENSIONS: [&str; 9] = [
    "jpg", "jpeg", "png", "bmp", "webp", "tif", "tiff", "tga", "gif",
];

const MAX_DEPTH: usize = 64;
const MAX_FILES: usize = 100_000;

pub struct ScannedFile {
    pub path: PathBuf,
    /// 相对输入目录的文件夹路径（`/` 分隔）；文件输入与平铺模式为空。
    pub relative_folder: String,
}

pub struct ScanResult {
    pub files: Vec<ScannedFile>,
    pub failures: Vec<ImportFailure>,
    pub truncated: bool,
}

/// 把用户选择的文件/目录展开为图片文件列表。
///
/// 跳过 `.artbank` 与旧程序的 `.data`；`recycle`、`output` 只在位于当前图库内时跳过。
/// 不跟随符号链接，避免环。
pub fn collect(
    inputs: &[PathBuf],
    library_root: &Path,
    cancelled: impl Fn() -> bool,
) -> ScanResult {
    let root = library_root
        .canonicalize()
        .unwrap_or_else(|_| library_root.to_path_buf());
    let mut result = ScanResult {
        files: Vec::new(),
        failures: Vec::new(),
        truncated: false,
    };
    let mut seen = HashSet::new();

    for input in inputs {
        if cancelled() || result.truncated {
            break;
        }
        let Ok(metadata) = std::fs::metadata(input) else {
            result.failures.push(ImportFailure {
                path: input.to_string_lossy().into_owned(),
                reason: "无法读取路径".into(),
            });
            continue;
        };
        if metadata.is_file() {
            if is_supported(input) {
                push_file(&mut result, &mut seen, input.to_path_buf(), String::new());
            } else {
                result.failures.push(ImportFailure {
                    path: input.to_string_lossy().into_owned(),
                    reason: "不支持的图片格式".into(),
                });
            }
        } else if metadata.is_dir() {
            walk(&mut result, &mut seen, input, input, &root, 0, &cancelled);
        }
    }

    result
}

fn walk(
    result: &mut ScanResult,
    seen: &mut HashSet<String>,
    directory: &Path,
    base: &Path,
    library_root: &Path,
    depth: usize,
    cancelled: &impl Fn() -> bool,
) {
    if depth > MAX_DEPTH || result.truncated || cancelled() {
        return;
    }
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            result.failures.push(ImportFailure {
                path: directory.to_string_lossy().into_owned(),
                reason: format!("无法读取目录：{error}"),
            });
            return;
        }
    };

    let mut items: Vec<_> = entries.filter_map(Result::ok).collect();
    items.sort_by_key(|entry| entry.file_name());

    for entry in items {
        if result.truncated || cancelled() {
            return;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if should_skip_directory(&path, library_root) {
                continue;
            }
            walk(
                result,
                seen,
                &path,
                base,
                library_root,
                depth + 1,
                cancelled,
            );
        } else if file_type.is_file() && is_supported(&path) {
            let relative = relative_folder(&path, base);
            push_file(result, seen, path, relative);
        }
    }
}

fn relative_folder(path: &Path, base: &Path) -> String {
    let Some(parent) = path.parent() else {
        return String::new();
    };
    let Ok(relative) = parent.strip_prefix(base) else {
        return String::new();
    };
    relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

fn push_file(
    result: &mut ScanResult,
    seen: &mut HashSet<String>,
    path: PathBuf,
    relative_folder: String,
) {
    if result.files.len() >= MAX_FILES {
        result.truncated = true;
        return;
    }
    if seen.insert(path_key(&path)) {
        result.files.push(ScannedFile {
            path,
            relative_folder,
        });
    }
}

fn should_skip_directory(path: &Path, library_root: &Path) -> bool {
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name == DATA_DIR || name == LEGACY_DATA_DIR {
        return true;
    }
    if name == RECYCLE_DIR || name == OUTPUT_DIR || name.starts_with(TEMP_FOLDER_DIR) {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        return canonical.starts_with(library_root);
    }
    false
}

fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| SUPPORTED_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// 去重键：Windows 路径大小写不敏感，`/` 与 `\` 等价，所以统一成小写反斜杠形式。
fn path_key(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\").to_lowercase()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn scans_directories_and_skips_library_internals() {
        let temp = tempdir().unwrap();
        let library = temp.path().join("library");
        let source = library.join("导入源");
        fs::create_dir_all(source.join("子目录")).unwrap();
        fs::create_dir_all(library.join(".artbank").join("temp")).unwrap();
        fs::create_dir_all(library.join(".data")).unwrap();
        fs::create_dir_all(library.join("recycle")).unwrap();
        fs::write(source.join("a.png"), b"x").unwrap();
        fs::write(source.join("子目录").join("b.JPG"), b"x").unwrap();
        fs::write(source.join("readme.txt"), b"x").unwrap();
        fs::write(library.join(".artbank").join("hidden.png"), b"x").unwrap();
        fs::write(library.join(".data").join("legacy.png"), b"x").unwrap();
        fs::write(library.join("recycle").join("trash.png"), b"x").unwrap();

        let result = collect(std::slice::from_ref(&library), &library, || false);
        let names: Vec<String> = result
            .files
            .iter()
            .map(|file| {
                file.path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();

        assert_eq!(names.len(), 2);
        assert!(names.contains(&"a.png".to_owned()));
        assert!(names.contains(&"b.JPG".to_owned()));
        assert!(!names.contains(&"hidden.png".to_owned()));
        assert!(!names.contains(&"legacy.png".to_owned()));
        assert!(!names.contains(&"trash.png".to_owned()));
    }

    #[test]
    fn reports_relative_folders_under_each_input() {
        let temp = tempdir().unwrap();
        let first = temp.path().join("素材A");
        let second = temp.path().join("素材B");
        fs::create_dir_all(first.join("人物").join("速写")).unwrap();
        fs::create_dir_all(second.join("人物").join("色彩")).unwrap();
        fs::write(first.join("人物").join("速写").join("a.png"), b"x").unwrap();
        fs::write(second.join("人物").join("色彩").join("b.png"), b"x").unwrap();
        fs::write(first.join("root.png"), b"x").unwrap();

        let result = collect(&[first, second], temp.path(), || false);
        let mut pairs: Vec<(String, String)> = result
            .files
            .iter()
            .map(|file| {
                (
                    file.path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    file.relative_folder.clone(),
                )
            })
            .collect();
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                ("a.png".to_owned(), "人物/速写".to_owned()),
                ("b.png".to_owned(), "人物/色彩".to_owned()),
                ("root.png".to_owned(), String::new()),
            ]
        );
    }

    #[test]
    fn reports_unsupported_files_and_missing_paths() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("note.txt");
        fs::write(&file, b"x").unwrap();
        let missing = temp.path().join("missing.png");

        let result = collect(&[file, missing], temp.path(), || false);
        assert_eq!(result.files.len(), 0);
        assert_eq!(result.failures.len(), 2);
    }

    #[test]
    fn stops_when_cancelled() {
        let temp = tempdir().unwrap();
        let mut inputs = Vec::new();
        for index in 0..10 {
            let path = temp.path().join(format!("{index}.png"));
            fs::write(&path, b"x").unwrap();
            inputs.push(path);
        }

        let result = collect(&inputs, temp.path(), || true);
        assert!(result.files.is_empty());
    }
}
