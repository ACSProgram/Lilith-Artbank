//! 全局图像工作预算：所有解码、编码、缩略图、pHash/主色与临时文件写入共用一个上限。
//!
//! 线程许可只限制并发张数；字节预算限制同一时刻的解码内存。导入、压缩、估算、
//! 查看器预览、AI 输入解码与 MCP 拼图共用同一份字节额度，否则“15 个线程各拿一张
//! 20 MP 大图”会把峰值撑到数 GB，多个任务并行时还会叠加。

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

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

/// 图像解码内存的默认上限：大图少并发、小图多并发，峰值压在 1.5 GiB 附近。
/// 全进程一份，导入、压缩、估算、查看器、AI 输入与 MCP 拼图都从这份额度里申请。
pub const DEFAULT_MEMORY_LIMIT: u64 = 1536 * 1024 * 1024;

struct MemoryState {
    limit: u64,
    in_use: u64,
}

struct MemoryInner {
    state: Mutex<MemoryState>,
    ready: Condvar,
    peak: AtomicU64,
}

/// 字节预算：按“预计同时占用的解码内存”排队，而不是只数线程。
///
/// 一份额度服务全进程的图像解码：导入、压缩、估算、查看器预览、AI 输入与 MCP 拼图。
pub struct ImageMemoryBudget {
    inner: Arc<MemoryInner>,
}

impl Default for ImageMemoryBudget {
    fn default() -> Self {
        Self::new(DEFAULT_MEMORY_LIMIT)
    }
}

impl ImageMemoryBudget {
    pub fn new(limit: u64) -> Self {
        Self {
            inner: Arc::new(MemoryInner {
                state: Mutex::new(MemoryState {
                    limit: limit.max(1),
                    in_use: 0,
                }),
                ready: Condvar::new(),
                peak: AtomicU64::new(0),
            }),
        }
    }

    pub fn limit(&self) -> u64 {
        self.inner.state.lock().limit
    }

    #[cfg(test)]
    pub fn in_use(&self) -> u64 {
        self.inner.state.lock().in_use
    }

    /// 本次进程生命周期内的占用峰值；用于调试日志，不参与准入。
    pub fn peak(&self) -> u64 {
        self.inner.peak.load(Ordering::Relaxed)
    }

    /// 申请 `bytes` 额度；等待期间可取消。
    ///
    /// 单张估算超过上限时允许独占：先等当前占用归零，再直接放行超限额度，
    /// 这样超大图不会因为永远凑不够额度而饿死，同时不会和别的解码叠加。
    pub fn acquire(
        &self,
        bytes: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<MemoryPermit, BudgetError> {
        let mut state = self.inner.state.lock();
        let oversize = bytes > state.limit;
        loop {
            let fits = if oversize {
                state.in_use == 0
            } else {
                state.in_use.saturating_add(bytes) <= state.limit
            };
            if fits {
                state.in_use = state.in_use.saturating_add(bytes);
                self.inner.peak.fetch_max(state.in_use, Ordering::Relaxed);
                return Ok(MemoryPermit {
                    inner: self.inner.clone(),
                    bytes,
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
}

pub struct MemoryPermit {
    inner: Arc<MemoryInner>,
    bytes: u64,
}

impl Drop for MemoryPermit {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock();
        state.in_use = state.in_use.saturating_sub(self.bytes);
        drop(state);
        self.inner.ready.notify_all();
    }
}

/// 单张图片的工作内存估算：整张解码图与随之产生的缩放缓冲。
///
/// `optimize` 为 true 时按压缩路径算：像素 × 16、源文件字节与 PSNR 回读缓冲；
/// 其余解码（导入、查看器预览、AI 输入、MCP 拼图）按像素 × 5 + 8 MiB 开销。
/// 系数取保守值，实测后再校准。
pub fn estimate_working_bytes(width: u32, height: u32, source_size: u64, optimize: bool) -> u64 {
    const OVERHEAD: u64 = 8 * 1024 * 1024;
    let pixels = u64::from(width) * u64::from(height);
    let per_pixel = if optimize { 16 } else { 5 };
    pixels
        .saturating_mul(per_pixel)
        .saturating_add(if optimize { source_size } else { 0 })
        .saturating_add(OVERHEAD)
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

    #[test]
    fn memory_permits_stay_within_limit() {
        let budget = Arc::new(ImageMemoryBudget::new(1000));
        let first = budget.acquire(600, &|| false).unwrap();
        let waiter = {
            let budget = budget.clone();
            std::thread::spawn(move || {
                let _permit = budget.acquire(600, &|| false).unwrap();
                budget.in_use()
            })
        };
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(budget.in_use(), 600, "第二份额度必须等待");
        drop(first);
        assert_eq!(waiter.join().unwrap(), 600);
        assert_eq!(budget.in_use(), 0);
        assert!(budget.peak() <= 1000);
    }

    #[test]
    fn oversized_image_takes_an_exclusive_permit() {
        let budget = Arc::new(ImageMemoryBudget::new(1000));
        let small = budget.acquire(400, &|| false).unwrap();
        let big = {
            let budget = budget.clone();
            std::thread::spawn(move || {
                let _permit = budget.acquire(4000, &|| false).unwrap();
                budget.in_use()
            })
        };
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(budget.in_use(), 400, "超大图要等当前占用归零");
        drop(small);
        assert_eq!(big.join().unwrap(), 4000);
        assert_eq!(budget.in_use(), 0);
        assert_eq!(budget.peak(), 4000);
    }

    #[test]
    fn memory_wait_is_cancellable() {
        let budget = Arc::new(ImageMemoryBudget::new(100));
        let _permit = budget.acquire(100, &|| false).unwrap();
        let waiter = {
            let budget = budget.clone();
            std::thread::spawn(move || budget.acquire(50, &|| true).map(|_| ()))
        };
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(waiter.join().unwrap(), Err(BudgetError::Cancelled));
        assert_eq!(budget.in_use(), 100);
    }

    #[test]
    fn estimate_grows_with_pixels_and_source_size() {
        let small = estimate_working_bytes(1000, 1000, 200_000, false);
        let large = estimate_working_bytes(4000, 3000, 200_000, false);
        assert!(large > small);
        let optimized = estimate_working_bytes(1000, 1000, 2_000_000, true);
        assert!(optimized > small, "压缩路径要额外算源文件与像素缓冲");
    }
}
