//! MCP 响应与错误的统一路径脱敏。
//!
//! 只隐藏本机目录事实：图库根、程序根、系统临时目录与 `.artbank` 路径；
//! 本次调用返回的导出文件路径是模型的工作文件，必须原样保留。

use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use serde_json::Value;

// 占位符用私用区字符包住序号：正常响应里不会出现，替换期间也不会被别的规则误伤。
const PROTECT_OPEN: char = '\u{e000}';
const PROTECT_CLOSE: char = '\u{e001}';

#[derive(Debug, Default)]
pub struct Sanitizer {
    protected: Vec<String>,
    replacements: Vec<(String, String)>,
}

impl Sanitizer {
    pub fn new(library_root: Option<&Path>) -> Self {
        let mut sanitizer = Self::default();
        if let Some(root) = library_root {
            sanitizer.hide(root, "<library>");
            sanitizer.hide(
                &root.join(crate::library::paths::DATA_DIR),
                "<library>/.artbank",
            );
        }
        sanitizer.hide(&crate::app::paths::app_root(), "<app>");
        sanitizer.hide(&std::env::temp_dir(), "<temp>");
        sanitizer
            .replacements
            .sort_by_key(|item| std::cmp::Reverse(item.0.len()));
        sanitizer
    }

    /// 从工具返回值里取出导出文件路径，作为脱敏时唯一保留的目录事实。
    pub fn with_export_from(self, value: &Value) -> Self {
        let path = value
            .get("export")
            .and_then(|export| export.get("path"))
            .and_then(Value::as_str)
            .map(PathBuf::from);
        self.protect_export(path.as_deref())
    }

    fn hide(&mut self, path: &Path, replacement: &str) {
        for variant in path_variants(path) {
            self.replacements.push((variant, replacement.to_owned()));
        }
    }

    /// 导出文件与它的父目录都要豁免，否则会被“临时目录一律替换”抹掉。
    fn protect_export(mut self, path: Option<&Path>) -> Self {
        let Some(path) = path else {
            return self;
        };
        self.protected.extend(path_variants(path));
        if let Some(parent) = path.parent() {
            self.protected.extend(path_variants(parent));
        }
        self.protected
            .sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
        self.protected.dedup();
        self
    }

    pub fn apply(&self, text: &str) -> String {
        let placeholders = self.placeholders();
        let mut result = Cow::Borrowed(text);
        for (needle, placeholder) in self.protected.iter().zip(&placeholders) {
            result = replace_ignore_ascii_case(result, needle, placeholder);
        }
        for (needle, replacement) in &self.replacements {
            result = replace_ignore_ascii_case(result, needle, replacement);
        }
        for (needle, placeholder) in self.protected.iter().zip(&placeholders) {
            result = replace_ignore_ascii_case(result, placeholder, needle);
        }
        result.into_owned()
    }

    fn placeholders(&self) -> Vec<String> {
        (0..self.protected.len())
            .map(|index| format!("{PROTECT_OPEN}{index}{PROTECT_CLOSE}"))
            .collect()
    }

    /// 递归处理 JSON 里的所有字符串；数值、布尔与 null 原样保留。
    pub fn apply_value(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.apply(text),
            Value::Array(items) => {
                for item in items {
                    self.apply_value(item);
                }
            }
            Value::Object(object) => {
                for item in object.values_mut() {
                    self.apply_value(item);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
}

/// 一条路径在文本里可能出现的写法：原样、正斜杠、URL 编码，以及 `file://` 前缀。
/// 返回值按长度降序：长路径先替换，短路径不会先把它啃掉。
fn path_variants(path: &Path) -> Vec<String> {
    let trimmed = path
        .to_string_lossy()
        .trim_end_matches(['/', '\\'])
        .to_owned();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let forward = trimmed.replace('\\', "/");
    let encoded = forward.replace(' ', "%20");
    let mut variants = vec![
        trimmed,
        forward.clone(),
        encoded.clone(),
        format!("file:///{forward}"),
        format!("file://{forward}"),
        format!("file:///{encoded}"),
        format!("file://{encoded}"),
    ];
    variants.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    variants.dedup();
    variants
}

/// 大小写不敏感地替换；没有命中时原样把借来的字符串传下去，不做无谓拷贝。
fn replace_ignore_ascii_case<'a>(
    text: Cow<'a, str>,
    needle: &str,
    replacement: &str,
) -> Cow<'a, str> {
    if needle.is_empty() || !contains_ignore_ascii_case(&text, needle) {
        return text;
    }
    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;
    while cursor < text.len() {
        if let Some(candidate) = text.get(cursor..cursor + needle.len())
            && candidate.eq_ignore_ascii_case(needle)
        {
            result.push_str(replacement);
            cursor += needle.len();
            continue;
        }
        let ch = text[cursor..].chars().next().expect("游标落在字符边界");
        result.push(ch);
        cursor += ch.len_utf8();
    }
    Cow::Owned(result)
}

fn contains_ignore_ascii_case(text: &str, needle: &str) -> bool {
    let mut cursor = 0;
    while cursor < text.len() {
        if let Some(candidate) = text.get(cursor..cursor + needle.len())
            && candidate.eq_ignore_ascii_case(needle)
        {
            return true;
        }
        cursor += text[cursor..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::*;

    #[test]
    fn hides_library_and_app_roots() {
        let sanitizer = Sanitizer::new(Some(Path::new(r"G:\Lilith Image Library")));
        let text = sanitizer.apply(
            r"打开 G:\Lilith Image Library\人物\a.png，配置在 G:\Lilith Image Library\.artbank\library.db",
        );
        assert!(text.contains("<library>"), "{text}");
        assert!(!text.contains("G:\\Lilith Image Library"), "{text}");
        assert!(
            !text.to_ascii_lowercase().contains("lilith image library"),
            "{text}"
        );
    }

    #[test]
    fn replaces_case_insensitively_and_in_file_urls() {
        let sanitizer = Sanitizer::new(Some(Path::new(r"G:\Art Bank")));
        let text = sanitizer.apply("g:/art bank/a.png file:///G:/Art%20Bank/a.png");
        assert_eq!(text.matches("<library>").count(), 2, "{text}");
    }

    #[test]
    fn keeps_the_export_path_and_its_directory() {
        let export = std::env::temp_dir()
            .join("lilith-artbank")
            .join("mcp-export")
            .join("one.jsonl");
        let mut value = json!({ "export": { "path": export.to_string_lossy() } });
        let sanitizer = Sanitizer::new(None).with_export_from(&value);
        let text = sanitizer.apply(&format!(
            "导出到 {}；图库在 {}",
            export.display(),
            std::env::temp_dir().join("library").display()
        ));
        assert!(
            text.contains(&export.to_string_lossy().to_string()),
            "{text}"
        );
        assert!(text.contains("mcp-export"), "{text}");
        assert!(text.contains("<temp>"), "{text}");

        // 同一个字符串经 JSON 脱敏后也要原样留下。
        sanitizer.apply_value(&mut value);
        assert_eq!(
            value["export"]["path"],
            export.to_string_lossy().to_string()
        );
    }

    #[test]
    fn sanitizes_nested_json_strings() {
        let sanitizer = Sanitizer::new(Some(Path::new(r"G:\Library")));
        let mut value = json!({
            "error": r"无法读取 G:\Library\a.png",
            "items": [r"G:\Library\b.png", 3],
            "nested": { "folder": r"G:\Library\人物" }
        });
        sanitizer.apply_value(&mut value);
        assert_eq!(value["error"], "无法读取 <library>\\a.png");
        assert_eq!(value["items"][0], "<library>\\b.png");
        assert_eq!(value["items"][1], 3);
        assert_eq!(value["nested"]["folder"], "<library>\\人物");
    }
}
