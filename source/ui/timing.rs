//! 函数运行耗时组件：零依赖、线程安全，统计次数/最近/最大/平均。
//!
//! 三种用法：
//! - `TimingStats::record(label, duration)`：直接记录一个时长
//! - `Timer`：手动 `start()` / `stop_and_record(label, &stats)`
//! - `ScopeTimer`：作用域 guard，`let _t = ScopeTimer::start("chat.login", &stats);` 离开作用域自动记录
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 单标签样本统计
#[derive(Clone, Copy, Default)]
pub struct Sample {
    pub count: u64,
    pub last: Duration,
    pub max: Duration,
    pub total: Duration,
}

impl Sample {
    pub fn record(&mut self, d: Duration) {
        self.count += 1;
        self.last = d;
        if d > self.max {
            self.max = d;
        }
        self.total += d;
    }

    pub fn avg(&self) -> Duration {
        if self.count == 0 {
            Duration::ZERO
        } else {
            self.total / self.count as u32
        }
    }
}

/// 线程安全统计集：label → Sample
#[derive(Default)]
pub struct TimingStats {
    inner: Mutex<HashMap<&'static str, Sample>>,
}

impl TimingStats {
    pub fn record(&self, label: &'static str, d: Duration) {
        self.inner.lock().unwrap().entry(label).or_default().record(d);
    }

    pub fn get(&self, label: &'static str) -> Option<Sample> {
        self.inner.lock().unwrap().get(label).copied()
    }

    pub fn snapshot(&self) -> Vec<(&'static str, Sample)> {
        let mut v: Vec<_> = self
            .inner
            .lock()
            .unwrap()
            .iter()
            .map(|(k, s)| (*k, *s))
            .collect();
        v.sort_by(|a, b| a.0.cmp(b.0));
        v
    }

    pub fn clear(&self) {
        self.inner.lock().unwrap().clear();
    }
}

/// 手动计时器（不持借用，适合跨 `&mut self` 闭包计时的场景）
pub struct Timer {
    start: Option<Instant>,
}

impl Timer {
    pub fn start() -> Self {
        Timer {
            start: Some(Instant::now()),
        }
    }

    pub fn stop_and_record(&mut self, label: &'static str, stats: &TimingStats) {
        if let Some(t) = self.start.take() {
            stats.record(label, t.elapsed());
        }
    }
}

/// 作用域计时 guard：`Drop` 时把耗时记录进 `TimingStats`
pub struct ScopeTimer<'a> {
    label: &'static str,
    start: Instant,
    stats: &'a TimingStats,
}

impl<'a> ScopeTimer<'a> {
    pub fn start(label: &'static str, stats: &'a TimingStats) -> Self {
        ScopeTimer {
            label,
            start: Instant::now(),
            stats,
        }
    }
}

impl Drop for ScopeTimer<'_> {
    fn drop(&mut self) {
        self.stats.record(self.label, self.start.elapsed());
    }
}
