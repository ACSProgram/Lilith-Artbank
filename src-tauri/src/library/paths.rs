use std::path::{Component, Path, PathBuf};

use crate::app::error::{AppError, AppResult};

pub const DATA_DIR: &str = ".artbank";
/// 旧程序的图库数据目录名。新应用不读取；扫描时与 `.artbank` 一样跳过。
pub const LEGACY_DATA_DIR: &str = ".data";
pub const DATABASE_FILE: &str = "library.db";
pub const MANIFEST_FILE: &str = "library.json";
pub const THUMBS_DIR: &str = "thumbs";
pub const FEATURES_DIR: &str = "features";
pub const TEMP_DIR: &str = "temp";
pub const UNDO_DIR: &str = "undo";
pub const RECYCLE_DIR: &str = "recycle";
pub const OUTPUT_DIR: &str = "output";
/// 临时文件夹的默认内部目录名。迁移遇到同名普通文件夹时会追加后缀，
/// 因此读写都以 `folders.kind = 'temp'` 记录的路径为准，不要直接拼这个常量。
pub const TEMP_FOLDER_DIR: &str = ".artbank-temp";
pub const LIBRARY_FORMAT: &str = "lilith-artbank";
pub const LIBRARY_FORMAT_VERSION: u32 = 1;

const RESERVED_DIRECTORY_NAMES: [&str; 3] = [DATA_DIR, RECYCLE_DIR, OUTPUT_DIR];
const RESERVED_DEVICE_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// `.artbank/` 内的运行时路径。`recycle`、`output` 在库根下，用同名常量拼接，
/// 需要时通过 `root.join(RECYCLE_DIR)` 取得。
#[derive(Debug, Clone)]
pub struct LibraryPaths {
    pub root: PathBuf,
    pub data: PathBuf,
    pub database: PathBuf,
    pub manifest: PathBuf,
    pub thumbs: PathBuf,
    pub features: PathBuf,
    pub temp: PathBuf,
    pub undo: PathBuf,
}

impl LibraryPaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let data = root.join(DATA_DIR);
        Self {
            database: data.join(DATABASE_FILE),
            manifest: data.join(MANIFEST_FILE),
            thumbs: data.join(THUMBS_DIR),
            features: data.join(FEATURES_DIR),
            temp: data.join(TEMP_DIR),
            undo: data.join(UNDO_DIR),
            data,
            root,
        }
    }

    pub fn ensure_runtime_dirs(&self) -> AppResult<()> {
        std::fs::create_dir_all(&self.data)?;
        std::fs::create_dir_all(&self.thumbs)?;
        std::fs::create_dir_all(&self.features)?;
        std::fs::create_dir_all(&self.temp)?;
        std::fs::create_dir_all(&self.undo)?;
        Ok(())
    }

    pub fn content_file(&self, folder: &str, file_name: &str) -> PathBuf {
        if folder.is_empty() {
            self.root.join(file_name)
        } else {
            self.root
                .join(folder.replace('/', std::path::MAIN_SEPARATOR_STR))
                .join(file_name)
        }
    }

    pub fn folder_dir(&self, folder: &str) -> PathBuf {
        if folder.is_empty() {
            self.root.clone()
        } else {
            self.root
                .join(folder.replace('/', std::path::MAIN_SEPARATOR_STR))
        }
    }
}

/// 转义 LIKE 的通配符，配合 `ESCAPE '\'` 使用。
pub fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// 子树匹配的 LIKE 模式，配合 `ESCAPE '\'` 使用。
///
/// 返回 `<path>/%`，只匹配后代、不匹配自身；根目录（空串）没有后代前缀，
/// 调用方要先把它当作“不限制文件夹”处理。
pub fn subtree_like_pattern(path: &str) -> String {
    format!("{}/%", escape_like(path))
}

/// 图库内文件夹使用 `/` 分隔的相对路径；根目录用空字符串表示。
pub fn normalize_folder(value: &str) -> AppResult<String> {
    normalize_folder_with(value, false)
}

/// 已存进数据库的文件夹路径（图片文件定位、撤销/重做）使用。
///
/// 只额外允许临时文件夹的内部路径；其它保留目录仍然拒绝。
pub fn normalize_stored_folder(value: &str) -> AppResult<String> {
    normalize_folder_with(value, true)
}

fn normalize_folder_with(value: &str, allow_temp: bool) -> AppResult<String> {
    let raw = value.trim().replace('\\', "/");
    if raw.starts_with('/') || raw.contains(':') {
        return Err(AppError::message("文件夹必须是图库内的相对路径"));
    }
    let trimmed = raw.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    if trimmed.chars().count() > 512 {
        return Err(AppError::message("文件夹路径过长"));
    }
    if trimmed.chars().any(|ch| ch.is_control()) {
        return Err(AppError::message("文件夹路径包含控制字符"));
    }
    for segment in trimmed.split('/') {
        validate_folder_segment(segment, allow_temp)?;
    }
    let path = Path::new(trimmed);
    if path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(AppError::message("文件夹必须是图库内的相对路径"));
    }
    Ok(trimmed.to_owned())
}

/// 单个文件夹名称（不含分隔符），用于新建与重命名。
pub fn normalize_folder_name(value: &str) -> AppResult<String> {
    let name = value.trim();
    if name.is_empty() {
        return Err(AppError::message("文件夹名称不能为空"));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(AppError::message("文件夹名称不能包含路径分隔符"));
    }
    validate_folder_segment(name, false)?;
    Ok(name.to_owned())
}

fn validate_folder_segment(segment: &str, allow_temp: bool) -> AppResult<()> {
    if segment.is_empty() {
        return Err(AppError::message("文件夹路径包含空目录名"));
    }
    if segment == "." || segment == ".." {
        return Err(AppError::message("文件夹必须是图库内的相对路径"));
    }
    if segment.chars().count() > 255 {
        return Err(AppError::message("文件夹名称过长"));
    }
    if segment.chars().any(|ch| ch.is_control()) {
        return Err(AppError::message("文件夹名称包含控制字符"));
    }
    if segment
        .chars()
        .any(|ch| matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '/' | '\\'))
    {
        return Err(AppError::message("文件夹名称包含 Windows 不允许的字符"));
    }
    if segment.ends_with(' ') || segment.ends_with('.') {
        return Err(AppError::message("文件夹名称不能以空格或点结尾"));
    }
    let lower = segment.to_lowercase();
    if allow_temp && lower.starts_with(TEMP_FOLDER_DIR) {
        return Ok(());
    }
    if RESERVED_DIRECTORY_NAMES.iter().any(|name| lower == *name)
        || lower.starts_with(TEMP_FOLDER_DIR)
    {
        return Err(AppError::message(format!(
            "「{segment}」是图库保留目录名，不能用作文件夹"
        )));
    }
    let stem = segment
        .split('.')
        .next()
        .unwrap_or(segment)
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    if RESERVED_DEVICE_NAMES.iter().any(|name| stem == *name) {
        return Err(AppError::message(format!(
            "「{segment}」是 Windows 保留设备名，不能用作文件夹"
        )));
    }
    Ok(())
}

pub fn join_folder(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

/// 上级路径；根目录与一级文件夹都返回空串。
pub fn parent_path(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

pub fn normalize_content_extension(value: &str) -> AppResult<String> {
    let ext = value.trim().trim_start_matches('.').to_ascii_lowercase();
    if ext.is_empty() || ext.len() > 16 || !ext.chars().all(|ch| ch.is_ascii_alphanumeric()) {
        return Err(AppError::message("图片扩展名无效"));
    }
    Ok(ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_folder_paths() {
        assert_eq!(normalize_folder("").unwrap(), "");
        assert_eq!(normalize_folder(" 人物\\速写 ").unwrap(), "人物/速写");
        assert!(normalize_folder("../外面").is_err());
        assert!(normalize_folder(r"C:\绝对").is_err());
        assert!(normalize_folder("/root").is_err());
        assert!(normalize_folder("人物//速写").is_err());
    }

    #[test]
    fn rejects_windows_reserved_names_and_characters() {
        for value in [
            "CON",
            "con",
            "CON.txt",
            "人物/AUX/速写",
            "a:b",
            "人物/速写?",
            "人物/结尾空格 /速写",
            "人物/结尾点./速写",
            ".artbank",
            "recycle",
            "人物/output",
        ] {
            assert!(normalize_folder(value).is_err(), "{value} 应被拒绝");
        }
        assert!(normalize_folder_name("速写").is_ok());
        assert!(normalize_folder_name("a/b").is_err());
        assert!(normalize_folder_name("NUL").is_err());

        assert!(normalize_folder(".artbank-temp").is_err());
        assert_eq!(
            normalize_stored_folder(".artbank-temp").unwrap(),
            ".artbank-temp"
        );
        assert!(normalize_stored_folder(".artbank").is_err());
        assert!(normalize_stored_folder("recycle").is_err());
    }

    #[test]
    fn builds_subtree_like_patterns() {
        assert_eq!(subtree_like_pattern("人物"), "人物/%");
        assert_eq!(
            subtree_like_pattern(r"a\b%c_d"),
            r"a\\b\%c\_d/%",
            "通配符与反斜杠都要转义"
        );
        assert_eq!(join_folder("人物", "速写"), "人物/速写");
        assert_eq!(join_folder("", "速写"), "速写");
    }
}
