//! 任务进度计量：按工作量单位推进、单调递增、带滚动吞吐与 ETA。
//!
//! 阶段加权已去掉：需要「一个任务一个百分比」的地方直接用计数派生，避免同一个任务
//! 在胶囊、进度条与图表上读出三个不同的数。

use std::{
    collections::VecDeque,
    sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    time::{Duration, Instant},
};

use parking_lot::Mutex;

const RATE_WINDOW: Duration = Duration::from_secs(10);

pub struct WorkMeter {
    total_units: AtomicU64,
    done_units: AtomicU64,
    explicit_percent: AtomicU8,
    finished: AtomicBool,
    samples: Mutex<VecDeque<(Instant, u64)>>,
}

impl Default for WorkMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkMeter {
    pub fn new() -> Self {
        Self {
            total_units: AtomicU64::new(0),
            done_units: AtomicU64::new(0),
            explicit_percent: AtomicU8::new(255),
            finished: AtomicBool::new(false),
            samples: Mutex::new(VecDeque::new()),
        }
    }

    pub fn set_total_units(&self, total: u64) {
        self.total_units.store(total, Ordering::Relaxed);
    }

    pub fn total_units(&self) -> u64 {
        self.total_units.load(Ordering::Relaxed)
    }

    #[allow(dead_code)]
    pub fn set_done_units(&self, done: u64) {
        self.done_units.store(done, Ordering::Relaxed);
        self.note_sample(done);
    }

    pub fn advance_units(&self, units: u64) -> u64 {
        let done = self.done_units.fetch_add(units, Ordering::Relaxed) + units;
        self.note_sample(done);
        done
    }

    pub fn done_units(&self) -> u64 {
        self.done_units.load(Ordering::Relaxed)
    }

    /// 显式百分比（单调不减、上限 99）；用于没有单位、只知道大概进度的阶段。
    pub fn set_percent(&self, percent: u8) {
        let mut current = self.explicit_percent.load(Ordering::Relaxed);
        loop {
            if current != 255 && percent <= current {
                return;
            }
            match self.explicit_percent.compare_exchange(
                current,
                percent.min(99),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(next) => current = next,
            }
        }
    }

    /// 当前百分比：显式值与「已完成单位 / 总单位」取较大者；两者都没有时为空。
    pub fn percent(&self) -> Option<u8> {
        if self.finished.load(Ordering::Relaxed) {
            return Some(100);
        }
        let explicit = match self.explicit_percent.load(Ordering::Relaxed) {
            255 => None,
            value => Some(value),
        };
        let total = self.total_units();
        let units = if total == 0 {
            None
        } else {
            let done = self.done_units().min(total);
            Some((done.saturating_mul(100) / total).min(99) as u8)
        };
        match (explicit, units) {
            (Some(left), Some(right)) => Some(left.max(right)),
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        }
    }

    /// 最近 10 秒的滚动吞吐（单位/秒）；样本不足时返回 `None`。
    pub fn rate(&self) -> Option<f64> {
        let samples = self.samples.lock();
        let (first_time, first_units) = *samples.front()?;
        let (last_time, last_units) = *samples.back()?;
        let elapsed = last_time.duration_since(first_time).as_secs_f64();
        if elapsed < 0.25 || last_units <= first_units {
            return None;
        }
        Some((last_units - first_units) as f64 / elapsed)
    }

    /// 按总单位与滚动吞吐估算剩余时间；样本不足时返回 `None`。
    pub fn eta(&self) -> Option<Duration> {
        let total = self.total_units();
        if total == 0 {
            return None;
        }
        let remaining = total.saturating_sub(self.done_units());
        let rate = self.rate()?;
        if rate <= 0.0 || remaining == 0 {
            return None;
        }
        Some(Duration::from_secs_f64(remaining as f64 / rate))
    }

    pub fn finish(&self) {
        self.finished.store(true, Ordering::Relaxed);
    }

    fn note_sample(&self, done: u64) {
        let now = Instant::now();
        let mut samples = self.samples.lock();
        samples.push_back((now, done));
        while samples
            .front()
            .is_some_and(|(time, _)| now.duration_since(*time) > RATE_WINDOW)
        {
            samples.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{thread::sleep, time::Duration};

    use super::*;

    #[test]
    fn percent_follows_units_is_monotonic_and_capped() {
        let meter = WorkMeter::new();
        meter.set_total_units(1_000);
        meter.advance_units(500);
        assert_eq!(meter.percent(), Some(50));
        meter.set_percent(60);
        meter.advance_units(100);
        assert_eq!(meter.percent(), Some(60));
        meter.set_done_units(1_000);
        assert_eq!(meter.percent(), Some(99));
        meter.set_percent(150);
        assert_eq!(meter.percent(), Some(99));
        meter.finish();
        assert_eq!(meter.percent(), Some(100));
    }

    #[test]
    fn rate_and_eta_use_recent_samples() {
        let meter = WorkMeter::new();
        meter.set_total_units(100);
        meter.set_done_units(0);
        sleep(Duration::from_millis(300));
        meter.advance_units(50);
        let rate = meter.rate().unwrap();
        assert!(rate > 0.0);
        assert!(meter.eta().is_some());
    }
}
