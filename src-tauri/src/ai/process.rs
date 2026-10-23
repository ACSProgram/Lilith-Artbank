use std::{
    process::{Child, Command, Stdio},
    time::Duration,
};

use serde::Serialize;

use crate::app::error::{AppError, AppResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ProcessState {
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

pub struct AiProcess {
    child: Option<Child>,
    state: ProcessState,
    message: Option<String>,
    attempts: u32,
    owned: bool,
}

impl Default for AiProcess {
    fn default() -> Self {
        Self {
            child: None,
            state: ProcessState::Stopped,
            message: None,
            attempts: 0,
            owned: false,
        }
    }
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
        self.message = Some(format!("正在启动 AI 服务（第 {attempt}/3 次尝试）"));
    }

    pub fn spawn(&mut self, executable: &str, port: u16) -> AppResult<()> {
        if let Some(child) = self.child.as_mut()
            && child.try_wait()?.is_none()
        {
            return Ok(());
        }
        self.child = None;
        let path = std::path::Path::new(executable);
        if !path.is_file() {
            return Err(AppError::message(format!(
                "AI 服务可执行文件不存在：{executable}"
            )));
        }
        let mut command = Command::new(path);
        command
            .arg("--port")
            .arg(port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(parent) = path.parent() {
            command.current_dir(parent);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW：托管服务不弹控制台窗口。
            command.creation_flags(0x0800_0000);
        }
        let child = command
            .spawn()
            .map_err(|error| AppError::message(format!("无法启动 AI 服务：{error}")))?;
        self.child = Some(child);
        self.owned = true;
        self.set_starting("AI 服务进程已启动，等待健康检查…");
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

    pub fn stop(&mut self, graceful: bool) {
        if let Some(mut child) = self.child.take() {
            if graceful {
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
