//! 空闲调度：只决定「什么时候压」，怎么压交给 `library::pack_gc`。
//!
//! 一个线程，每 30 秒醒一次。动手条件三条**同时**满足：
//!
//! 1. `activity::idle_for() >= 3 分钟`；
//! 2. 任务总线没有运行中或排队中的任务；
//! 3. 缩略图包垃圾率 > 20%。
//!
//! 顺序是有意的：前两条都是内存判断，零成本；只有它们都满足才去查第 3 条的 SQL。
//! 平时一次数据库查询都不跑，不会为了「有没有垃圾」每 30 秒扫一遍几十万行。
//!
//! 阈值与轮询间隔都走 `tuned()`：GUI 构建固定 3 分钟 / 30 秒，只有 `headless` 构建
//! 会被 `LILITH_IDLE_THRESHOLD_MS` / `LILITH_IDLE_POLL_MS` 覆盖，供验收把窗口缩到秒级。
//!
//! 不做「一次空闲窗口只跑一批」的代次计数：任务本身压到干净或被打断就结束，
//! 天然自限，不需要再套一层窗口状态。

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use tauri::{AppHandle, Manager};

use crate::app::{activity, state::AppState};

/// 多久没有命令调用算「人走开了」。
pub const IDLE_THRESHOLD: Duration = Duration::from_secs(180);
/// 轮询间隔；也是「一次空闲窗口能压多久」的分辨率。
const POLL: Duration = Duration::from_secs(30);
/// 关库时轮询的退出延迟上限。
const TICK: Duration = Duration::from_secs(1);

/// 当前构建生效的判定阈值：GUI 固定用 `IDLE_THRESHOLD`，无头构建可按环境变量缩短。
pub fn idle_threshold() -> Duration {
    tuned("LILITH_IDLE_THRESHOLD_MS", IDLE_THRESHOLD)
}

/// 当前构建生效的轮询间隔：GUI 固定用 `POLL`。
fn poll_interval() -> Duration {
    tuned("LILITH_IDLE_POLL_MS", POLL)
}

#[cfg(feature = "headless")]
fn tuned(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(default)
}

#[cfg(not(feature = "headless"))]
fn tuned(_name: &str, default: Duration) -> Duration {
    default
}

pub struct IdleLoop {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl IdleLoop {
    pub fn start(app: AppHandle) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::Builder::new()
            .name("idle-compact".to_owned())
            .spawn(move || {
                let mut waited = Duration::ZERO;
                let poll = poll_interval();
                loop {
                    // 1 秒切片：关库时不必等满 30 秒。
                    std::thread::sleep(TICK);
                    if flag.load(Ordering::Relaxed) {
                        break;
                    }
                    waited += TICK;
                    if waited < poll {
                        continue;
                    }
                    waited = Duration::ZERO;
                    tick(&app);
                }
            })
            .map_err(|error| log::warn!("无法启动空闲回收线程：{error}"))
            .ok();
        Self { stop, handle }
    }
}

impl Drop for IdleLoop {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// 前两条判据：都是内存判断，零成本。通过了才值得去查一次垃圾率。
fn gate(auto: bool, idle: Option<Duration>, busy: bool) -> bool {
    if !auto {
        return false;
    }
    let Some(idle) = idle else {
        return false;
    };
    idle >= idle_threshold() && !busy
}

fn tick(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    if !gate(
        state.settings.snapshot().maintenance.auto_compact_thumbs,
        activity::idle_for(),
        !state.tasks.is_idle(),
    ) {
        return;
    }
    // 第 3 条：到这一步才查 SQL。
    match state.pack_garbage_ratio() {
        Ok(ratio) if ratio > crate::library::pack_gc::TRIGGER_RATIO => {
            if let Err(error) = state.start_pack_compact(app.clone()) {
                log::debug!("自动回收缩略图包未启动：{error}");
            }
        }
        Ok(_) => {}
        Err(error) => log::debug!("读取缩略图包垃圾率失败：{error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每条判据缺一不提交；垃圾率那一条在 `tick` 里单独判（要查 SQL，不在这里测）。
    #[test]
    fn every_condition_is_required() {
        let long = Some(Duration::from_secs(200));
        assert!(gate(true, long, false));
        assert!(!gate(false, long, false), "关掉开关后不该动手");
        assert!(!gate(true, None, false), "从未活动过不算空闲");
        assert!(
            !gate(true, Some(Duration::from_secs(10)), false),
            "不到 3 分钟不动手"
        );
        assert!(!gate(true, long, true), "有任务在跑/排队时不动手");
    }
}
