//! GUI 运行日志组件：线程安全环形缓冲 + 可选文件落盘（追加 + 逐行 flush）。
//!
//! 双实例分工（由调用方建时间戳目录并分别 enable_file）：
//! - runtime.log  软件运行日志（启停/控制台成败/管线延迟/帧耗时/超阈值告警/退出）
//! - interact.log 用户交互输入输出（子进程每行输出 + 用户每次输入，带时间戳）
//!
//! 时间戳用 chrono 取**本地时区**（Windows/Linux 一致），文件夹名与行内时间戳均可读、可排序。

use std::collections::VecDeque;
use std::fmt::Display;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Trace => "TRACE",
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

/// 一条日志
#[derive(Clone)]
pub struct Entry {
    pub ts: SystemTime,
    pub level: Level,
    pub tag: String,
    pub msg: String,
}

struct Inner {
    buf: VecDeque<Entry>,
    file: Option<File>,
}

/// 线程安全日志存储
pub struct LogStore {
    inner: Mutex<Inner>,
    max: usize,
}

impl LogStore {
    pub fn new(max: usize) -> Self {
        LogStore {
            inner: Mutex::new(Inner {
                buf: VecDeque::new(),
                file: None,
            }),
            max,
        }
    }

    /// 记录：入内存环形缓冲（超上限丢弃最旧）+ 落盘（如已 enable_file）
    pub fn log(&self, level: Level, tag: &str, msg: impl Display) {
        let entry = Entry {
            ts: SystemTime::now(),
            level,
            tag: tag.to_string(),
            msg: msg.to_string(),
        };
        let mut g = self.inner.lock().unwrap();
        if g.buf.len() >= self.max {
            g.buf.pop_front();
        }
        g.buf.push_back(entry.clone());
        if let Some(f) = g.file.as_mut() {
            let line = format!(
                "[{}][{}][{}] {}\n",
                fmt_ts(entry.ts),
                entry.level.as_str(),
                entry.tag,
                entry.msg
            );
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
    }

    /// 启用文件落盘（追加模式；路径父目录须已存在）
    pub fn enable_file(&self, path: &Path) -> Result<(), String> {
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("打开日志文件 {} 失败: {e}", path.display()))?;
        self.inner.lock().unwrap().file = Some(f);
        Ok(())
    }

    /// 每帧取走全部新增（供 UI 面板渲染）
    pub fn drain(&self, out: &mut Vec<Entry>) {
        let mut g = self.inner.lock().unwrap();
        out.extend(g.buf.drain(..));
    }

    pub fn clear(&self) {
        self.inner.lock().unwrap().buf.clear();
    }
}

// ---- 本地时间戳（chrono）----

/// 文件夹时间戳：YYYYMMDD-HHMMSS（本地时区）
pub fn now_folder_ts() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

/// 行内时间戳：HH:MM:SS.mmm（本地时区）
pub fn fmt_ts(t: SystemTime) -> String {
    let local = chrono::DateTime::<chrono::Utc>::from(t).with_timezone(&chrono::Local);
    local.format("%H:%M:%S%.3f").to_string()
}
