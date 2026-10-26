use std::io::Write;

use serde_json::Value;

use crate::app::paths;

/// 单行日志的最大长度；工具错误或路径列表再长也只截断诊断信息。
const MAX_DETAIL: usize = 600;
const MAX_LOGGED_SOURCES: usize = 20;

/// stdio 小桥不能写 stdout（那是 MCP 协议通道），诊断信息统一追加到 logs/mcp.log。
pub fn log_line(scope: &str, message: &str) {
    let path = paths::mcp_log_path();
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        eprintln!("无法创建 MCP 日志目录 {}：{error}", parent.display());
        return;
    }
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let message = single_line(message);
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| writeln!(file, "{stamp} [{scope}] {message}"));
    if let Err(error) = result {
        eprintln!("无法写入 MCP 日志 {}：{error}", path.display());
    }
}

/// 每次工具调用的结果行；不记录图片描述、路径或整份返回值。
///
/// `Ok(None)` 表示工具返回值不是可摘要的 JSON（拼图、只读结果），只记状态与耗时。
pub fn tool(tool: &str, mode: &str, duration_ms: u128, outcome: Result<Option<&Value>, &str>) {
    match outcome {
        Ok(value) => {
            let status = value
                .and_then(|value| value.get("status"))
                .and_then(Value::as_str)
                .unwrap_or("ok");
            let mut parts = vec![
                format!("tool={tool}"),
                format!("mode={mode}"),
                format!("status={status}"),
            ];
            if let Some(value) = value {
                for key in ["selected", "affected", "willChange"] {
                    if let Some(number) = value.get(key).and_then(Value::as_u64) {
                        parts.push(format!("{key}={number}"));
                    }
                }
                if let Some(operation_id) = value.get("operationId").and_then(Value::as_i64) {
                    parts.push(format!("operation={operation_id}"));
                }
                if let Some(task_id) = value.get("taskId").and_then(Value::as_str) {
                    parts.push(format!("task={task_id}"));
                }
            }
            parts.push(format!("{duration_ms}ms"));
            log_line("tool", &parts.join(" "));
        }
        Err(error) => {
            log_line(
                "tool",
                &format!("tool={tool} mode={mode} status=error error={error} {duration_ms}ms"),
            );
        }
    }
}

/// 导入来源单独记录；总数写在路径之前，单行截断只会切掉路径尾部。
pub fn import_sources(
    paths: &[String],
    target: &str,
    mode: &str,
    task_id: &str,
    operation_id: Option<i64>,
) {
    log_line(
        "import",
        &import_line(paths, target, mode, task_id, operation_id),
    );
}

fn import_line(
    paths: &[String],
    target: &str,
    mode: &str,
    task_id: &str,
    operation_id: Option<i64>,
) -> String {
    let operation = operation_id.map_or_else(|| "-".to_owned(), |id| id.to_string());
    let listed = paths
        .iter()
        .take(MAX_LOGGED_SOURCES)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" | ");
    format!(
        "mode={mode} target={target} task={task_id} operation={operation} sources={} paths={listed}",
        paths.len()
    )
}

fn single_line(message: &str) -> String {
    let mut text = message.replace("\r\n", " ").replace(['\r', '\n'], " ");
    if text.chars().count() > MAX_DETAIL {
        text = text.chars().take(MAX_DETAIL).collect::<String>();
        text.push('…');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_line_strips_newlines_and_truncates() {
        assert_eq!(single_line("a\nb\r\nc"), "a b c");
        let long = "x".repeat(MAX_DETAIL + 10);
        let text = single_line(&long);
        assert_eq!(text.chars().count(), MAX_DETAIL + 1);
        assert!(text.ends_with('…'));
    }

    #[test]
    fn import_line_puts_the_total_before_the_paths() {
        let paths = (0..MAX_LOGGED_SOURCES + 5)
            .map(|index| format!("C:\\素材\\{index}.png"))
            .collect::<Vec<_>>();
        let line = import_line(&paths, "根目录", "ask", "job-1", Some(7));
        assert!(line.contains("sources=25"));
        assert!(line.contains("operation=7"));
        assert_eq!(line.matches(" | ").count(), MAX_LOGGED_SOURCES - 1);
        // 总数在路径之前，单行截断不会把「共 N 个路径」切掉。
        assert!(line.find("sources=25").unwrap() < line.find("paths=").unwrap());
    }
}
