//! 全局「最后一次用户活动」。
//!
//! 只记一个时间戳，不监听鼠标键盘、不查窗口是否最小化、不加窗口事件：空闲判定只
//! 依赖「多久没有命令调用」这一个信号。落点有三处：
//!
//! - `trace::read` / `trace::write` / `trace::write_async`，覆盖全部命令；
//! - `thumb://` 与 `img://` 协议入口——纯滚动浏览不发命令，只发缩略图请求；
//! - MCP 请求入口，MCP 调用也是人在操作。
//!
//! 一次原子写，成本可以忽略。

use std::{
    sync::atomic::{AtomicI64, Ordering},
    time::Duration,
};

use crate::library::service::now_ms;

/// 0 表示「本次会话从未活动过」。
static LAST_ACTIVITY: AtomicI64 = AtomicI64::new(0);

pub fn touch() {
    LAST_ACTIVITY.store(now_ms(), Ordering::Relaxed);
}

/// 距上次用户活动多久；从未活动过时返回 `None`。
pub fn idle_for() -> Option<Duration> {
    let last = LAST_ACTIVITY.load(Ordering::Relaxed);
    if last <= 0 {
        return None;
    }
    let elapsed = now_ms().saturating_sub(last);
    Some(Duration::from_millis(elapsed.max(0) as u64))
}

/// 换库与关库时重置，回到「从未活动」。
pub fn reset() {
    LAST_ACTIVITY.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试都改同一个全局量，串行执行避免互相看见对方的时间戳。
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn touch_resets_the_idle_clock() {
        let _guard = LOCK.lock().unwrap_or_else(|error| error.into_inner());
        reset();
        assert!(idle_for().is_none());
        touch();
        let elapsed = idle_for().expect("活动过之后必须有时间");
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    }

    #[test]
    fn activity_is_visible_from_another_thread() {
        let _guard = LOCK.lock().unwrap_or_else(|error| error.into_inner());
        reset();
        std::thread::spawn(touch).join().unwrap();
        assert!(idle_for().is_some());
    }

    #[test]
    fn reset_returns_to_never_active() {
        let _guard = LOCK.lock().unwrap_or_else(|error| error.into_inner());
        touch();
        reset();
        assert!(idle_for().is_none());
    }
}
