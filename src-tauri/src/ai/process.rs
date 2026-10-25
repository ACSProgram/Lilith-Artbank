use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

use serde::Serialize;

use crate::app::{
    error::{AppError, AppResult},
    paths,
    settings::AiSettings,
};

/// 启动尝试上限；监督线程与状态文案共用同一数字。
pub const MAX_START_ATTEMPTS: u32 = 3;

/// 一次 Python 服务启动所需的完整信息；路径在构建时就解析并校验。
#[derive(Debug, Clone)]
pub struct AiLaunchSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub working_dir: PathBuf,
    pub log_path: PathBuf,
}

pub fn launch_spec(settings: &AiSettings) -> AppResult<AiLaunchSpec> {
    let python = settings
        .python_path()
        .ok_or_else(|| AppError::message("请先在设置中配置 Python 路径"))?;
    let script = settings
        .server_script()
        .ok_or_else(|| AppError::message("请先在设置中配置 AI 服务脚本"))?;
    let program = paths::resolve(python);
    let script = paths::resolve(script);
    if !program.is_file() {
        return Err(AppError::message(format!(
            "Python 不存在：{}",
            program.display()
        )));
    }
    if !script.is_file() {
        return Err(AppError::message(format!(
            "AI 服务脚本不存在：{}",
            script.display()
        )));
    }

    let mut args = vec![
        script.into_os_string(),
        OsString::from("--port"),
        OsString::from(settings.port.to_string()),
    ];
    if let Some(model_dir) = paths::clip_model_dir(
        settings.model_path.as_deref(),
        settings.clip_model_path.as_deref(),
    ) {
        if !model_dir.join("config.json").is_file() {
            log::warn!("CLIP 模型配置不存在：{}", model_dir.display());
        }
        args.push(OsString::from("--model-dir"));
        args.push(model_dir.into_os_string());
    }

    Ok(AiLaunchSpec {
        program,
        args,
        working_dir: paths::app_root(),
        log_path: paths::ai_server_log_path(),
    })
}

fn open_log_file(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ProcessState {
    #[default]
    Stopped,
    Starting,
    Running,
    External,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessStatus {
    pub state: ProcessState,
    pub owned: bool,
    pub message: Option<String>,
    pub attempts: u32,
}

#[derive(Default)]
pub struct AiProcess {
    child: Option<Child>,
    state: ProcessState,
    message: Option<String>,
    attempts: u32,
    owned: bool,
}

impl AiProcess {
    pub fn status(&self) -> ProcessStatus {
        ProcessStatus {
            state: self.state,
            owned: self.owned,
            message: self.message.clone(),
            attempts: self.attempts,
        }
    }

    pub fn is_owned_running(&self) -> bool {
        self.owned && self.child.is_some()
    }

    pub fn state(&self) -> ProcessState {
        self.state
    }

    pub fn set_starting(&mut self, message: impl Into<String>) {
        self.state = ProcessState::Starting;
        self.message = Some(message.into());
    }

    pub fn mark_online(&mut self, owned: bool) {
        self.state = if owned {
            ProcessState::Running
        } else {
            ProcessState::External
        };
        self.owned = owned;
        self.message = None;
        self.attempts = 0;
    }

    pub fn mark_failed(&mut self, message: impl Into<String>) {
        self.state = ProcessState::Failed;
        self.message = Some(message.into());
    }

    pub fn mark_stopped(&mut self, message: Option<String>) {
        self.state = ProcessState::Stopped;
        self.message = message;
        self.attempts = 0;
        self.owned = false;
        self.child = None;
    }

    pub fn reset_failure(&mut self) {
        if self.state == ProcessState::Failed {
            self.state = ProcessState::Stopped;
            self.message = None;
            self.attempts = 0;
        }
    }

    pub fn note_attempt(&mut self, attempt: u32) {
        self.attempts = attempt;
        self.state = ProcessState::Starting;
        self.message = Some(format!(
            "正在启动 AI 服务（第 {attempt}/{MAX_START_ATTEMPTS} 次尝试）"
        ));
    }

    pub fn spawn(&mut self, spec: &AiLaunchSpec) -> AppResult<()> {
        if let Some(child) = self.child.as_mut()
            && child.try_wait()?.is_none()
        {
            return Ok(());
        }
        self.child = None;

        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(&spec.working_dir)
            .env("PYTHONUNBUFFERED", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .stdin(Stdio::null());
        match open_log_file(&spec.log_path) {
            Ok(file) => match file.try_clone() {
                Ok(stderr) => {
                    command
                        .stdout(Stdio::from(file))
                        .stderr(Stdio::from(stderr));
                }
                Err(error) => {
                    log::warn!(
                        "无法复制 AI 服务日志句柄 {}：{error}",
                        spec.log_path.display()
                    );
                    command.stdout(Stdio::null()).stderr(Stdio::null());
                }
            },
            Err(error) => {
                log::warn!("无法打开 AI 服务日志 {}：{error}", spec.log_path.display());
                command.stdout(Stdio::null()).stderr(Stdio::null());
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW：托管服务不弹控制台窗口。
            command.creation_flags(0x0800_0000);
        }
        let child = command.spawn().map_err(|error| {
            AppError::message(format!(
                "无法启动 AI 服务（{}）：{error}",
                spec.program.display()
            ))
        })?;
        self.child = Some(child);
        self.owned = true;
        let message = if self.attempts <= 1 {
            "AI 服务进程已启动，等待健康检查…".to_owned()
        } else {
            format!(
                "AI 服务进程已启动（第 {}/{MAX_START_ATTEMPTS} 次尝试），等待健康检查…",
                self.attempts
            )
        };
        self.set_starting(message);
        Ok(())
    }

    /// 返回 true 表示子进程仍在运行。子进程退出时更新为 Failed。
    pub fn poll_child(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            return false;
        };
        let outcome = child.try_wait();
        match outcome {
            Ok(Some(status)) => {
                self.child = None;
                self.mark_failed(format!("AI 服务进程已退出：{status}"));
                false
            }
            Ok(None) => true,
            Err(error) => {
                self.child = None;
                self.mark_failed(format!("无法读取 AI 服务进程状态：{error}"));
                false
            }
        }
    }

    /// `wait_for_exit`：已发过 shutdown 请求时先等进程自行退出，超时再强杀。
    pub fn stop(&mut self, wait_for_exit: bool) {
        if let Some(mut child) = self.child.take() {
            if wait_for_exit {
                for _ in 0..20 {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                        Err(_) => break,
                    }
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        self.mark_stopped(Some("AI 服务已停止".into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_python_command_with_resolved_model_dir() {
        let temp = tempfile::tempdir().unwrap();
        let python = temp.path().join("python.exe");
        let script = temp.path().join("main.py");
        std::fs::write(&python, b"").unwrap();
        std::fs::write(&script, b"").unwrap();
        let model_root = temp.path().join("models");
        let model_dir = model_root.join("siglip2-test");
        std::fs::create_dir_all(&model_dir).unwrap();
        std::fs::write(model_dir.join("config.json"), b"{}").unwrap();

        let settings = AiSettings {
            python_path: Some(python.to_string_lossy().into_owned()),
            server_script: Some(script.to_string_lossy().into_owned()),
            model_path: Some(model_root.to_string_lossy().into_owned()),
            clip_model_path: Some("siglip2-test".into()),
            port: 19000,
            ..Default::default()
        };
        let spec = launch_spec(&settings).unwrap();
        assert_eq!(spec.program, python);
        assert_eq!(spec.args[0], script.into_os_string());
        assert_eq!(spec.args[1], OsString::from("--port"));
        assert_eq!(spec.args[2], OsString::from("19000"));
        assert_eq!(spec.args[3], OsString::from("--model-dir"));
        assert_eq!(spec.args[4], model_dir.into_os_string());
    }

    #[test]
    fn reports_missing_python_before_spawning() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("main.py");
        std::fs::write(&script, b"").unwrap();
        let settings = AiSettings {
            python_path: Some(
                temp.path()
                    .join("missing.exe")
                    .to_string_lossy()
                    .into_owned(),
            ),
            server_script: Some(script.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let error = launch_spec(&settings).unwrap_err().to_string();
        assert!(error.contains("Python 不存在"));
    }
}
