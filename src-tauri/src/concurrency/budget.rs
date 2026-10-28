//! 全局图像工作预算：所有解码、编码、缩略图、pHash/主色与临时文件写入共用一个上限。

use std::{sync::Arc, time::Duration};

use parking_lot::{Condvar, Mutex};

use crate::app::error::{AppError, AppResult};

struct BudgetState {
    limit: usize,
    in_use: usize,
}

struct Inner {
    state: Mutex<BudgetState>,
    ready: Condvar,
}

pub struct ImageBudget {
    inner: Arc<Inner>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetError {
    Cancelled,
}

impl Default for ImageBudget {
    fn default() -> Self {
        Self::from_setting(None)
    }
}

impl ImageBudget {
    pub fn from_setting(value: Option<usize>) -> Self {
        Self::new(value.unwrap_or_else(default_limit))
    }

    pub fn new(limit: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(BudgetState {
                    limit: limit.clamp(1, 128),
                    in_use: 0,
                }),
                ready: Condvar::new(),
            }),
        }
    }

    pub fn current_limit(&self) -> usize {
        self.inner.state.lock().limit
    }

    pub fn update_limit(&self, limit: usize) {
        let mut state = self.inner.state.lock();
        state.limit = limit.clamp(1, 128);
        drop(state);
        self.inner.ready.notify_all();
    }

    pub fn update_from_setting(&self, value: Option<usize>) {
        self.update_limit(value.unwrap_or_else(default_limit));
    }

    /// 拿一个图像工作许可；等待期间可取消。
    pub fn acquire(&self, cancel: &dyn Fn() -> bool) -> Result<ImagePermit, BudgetError> {
        let mut state = self.inner.state.lock();
        loop {
            if state.in_use < state.limit {
                state.in_use += 1;
                return Ok(ImagePermit {
                    inner: self.inner.clone(),
                });
            }
            if cancel() {
                return Err(BudgetError::Cancelled);
            }
            self.inner
                .ready
                .wait_for(&mut state, Duration::from_millis(200));
        }
    }

    pub fn pool(&self, context: &str) -> AppResult<rayon::ThreadPool> {
        let threads = self.current_limit();
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads.max(1))
            .build()
            .map_err(|error| AppError::message(format!("无法创建{context}线程池：{error}")))
    }
}

pub struct ImagePermit {
    inner: Arc<Inner>,
}

impl Drop for ImagePermit {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock();
        state.in_use = state.in_use.saturating_sub(1);
        drop(state);
        self.inner.ready.notify_all();
    }
}

pub fn default_limit() -> usize {
    std::thread::available_parallelism()
        .map(|value| value.get().saturating_sub(1))
        .unwrap_or(1)
        .max(1)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use super::*;

    #[test]
    fn permits_never_exceed_limit() {
        let budget = Arc::new(ImageBudget::new(3));
        let current = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();
        for _ in 0..8 {
            let budget = budget.clone();
            let current = current.clone();
            let max = max.clone();
            threads.push(std::thread::spawn(move || {
                for _ in 0..200 {
                    let _permit = budget.acquire(&|| false).unwrap();
                    let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                    max.fetch_max(now, Ordering::SeqCst);
                    std::thread::yield_now();
                    current.fetch_sub(1, Ordering::SeqCst);
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert!(max.load(Ordering::SeqCst) <= 3);
        assert_eq!(budget.inner.state.lock().in_use, 0);
    }

    #[test]
    fn cancellation_and_limit_change_wake_waiters() {
        let budget = Arc::new(ImageBudget::new(1));
        let first = budget.acquire(&|| false).unwrap();
        let waiter = {
            let budget = budget.clone();
            std::thread::spawn(move || budget.acquire(&|| true).map(|_| ()))
        };
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(waiter.join().unwrap(), Err(BudgetError::Cancelled));

        let waiter = {
            let budget = budget.clone();
            std::thread::spawn(move || budget.acquire(&|| false).map(|_| ()))
        };
        std::thread::sleep(Duration::from_millis(40));
        budget.update_limit(2);
        assert_eq!(waiter.join().unwrap(), Ok(()));
        drop(first);
    }
}
