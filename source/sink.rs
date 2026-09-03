//! 输出重定向 sink：线程局部输出端 + 同名宏重定向（println!/eprintln!/print!）。
//!
//! 工作方式（C 的 printf hook 思路）：
//! - 线程未安装 sink（CLI 模式、主线程）→ 回落 `::std::println!` 等，行为与原来逐字节一致
//! - 线程已 `install` 通道（GUI 引擎线程，单 worker 防任务漂移）→ 输出发送到通道（滚动区消费）
//!
//! 宏在 main.rs crate 根定义（文本作用域遮蔽 std 宏），整个 crate 的
//! println!/eprintln!/print! 自动路由，无需逐点改造；deps 不受影响（宏遮蔽仅 crate 内）。

use std::cell::RefCell;
use std::fmt::Display;
use tokio::sync::mpsc::UnboundedSender;

thread_local! {
    static SINK: RefCell<Option<UnboundedSender<String>>> = const { RefCell::new(None) };
}

/// 当前线程安装输出通道（GUI 引擎线程启动时调用）
pub fn install(tx: UnboundedSender<String>) {
    SINK.with(|s| *s.borrow_mut() = Some(tx));
}

/// 当前线程卸载输出通道
pub fn uninstall() {
    SINK.with(|s| *s.borrow_mut() = None);
}

/// 行输出（等价 println! 语义：带换行）
pub fn line(s: impl Display) {
    let s = s.to_string();
    SINK.with(|slot| match &*slot.borrow() {
        Some(tx) => {
            let _ = tx.send(s.clone());
        }
        None => ::std::println!("{s}"),
    });
}

/// 错误行输出（等价 eprintln! 语义：带换行，写 stderr；通道模式加 ⚠ 标记）
pub fn err(s: impl Display) {
    let s = s.to_string();
    SINK.with(|slot| match &*slot.borrow() {
        Some(tx) => {
            let _ = tx.send(format!("⚠ {s}"));
        }
        None => ::std::eprintln!("{s}"),
    });
}

/// 部分行输出（等价 print! 语义：无换行，提示符场景）
pub fn raw(s: impl Display) {
    use std::io::Write;
    let s = s.to_string();
    SINK.with(|slot| match &*slot.borrow() {
        Some(tx) => {
            let _ = tx.send(s.clone());
        }
        None => {
            ::std::print!("{s}");
            let _ = ::std::io::stdout().flush();
        }
    });
}
