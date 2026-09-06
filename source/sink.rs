//! 输出重定向 sink：线程局部输出端 + 同名宏重定向（println!/eprintln!/print!）。
//!
//! 工作方式（C 的 printf hook 思路）：
//! - 线程未安装 sink（CLI 模式、主线程）→ 回落 `::std::println!` 等，行为与原来逐字节一致
//! - 线程已 `install` 通道（GUI 引擎线程，单 worker 防任务漂移）→ 输出发送到通道（GUI 消费）
//!
//! 通道载荷为 [`EngineOut`] 统一枚举：文本行走 `Line`、结构化显示事件走 `Event`——
//! 单通道 FIFO 保证两者的相对顺序（气泡与系统提示不打乱时序）。
//!
//! **主动唤醒**：安装时可携带 notify 回调（GUI 传入 `ctx.request_repaint` 的闭包），
//! 每次 send 成功后调用——引擎输出即刻唤醒 UI 事件循环去 drain（延迟 ~0），
//! 纯事件驱动，无轮询。回调约束：只做"唤醒"，不得再触碰 sink（RefCell 借内调用，防重入）。
//!
//! 宏在 main.rs crate 根定义（文本作用域遮蔽 std 宏），整个 crate 的
//! println!/eprintln!/print! 自动路由，无需逐点改造；deps 不受影响（宏遮蔽仅 crate 内）。

use std::cell::RefCell;
use std::fmt::Display;
use tokio::sync::mpsc::UnboundedSender;

use crate::uievent::{AskRequest, EngineOut, UiEvent};

/// 输出通道 + 可选唤醒回调（GUI 引擎线程专用；CLI 不安装 → std 回落）
struct SinkChannel {
    tx: UnboundedSender<EngineOut>,
    /// send 成功后的唤醒回调（egui 类型封在闭包内，不穿透本模块签名）
    notify: Option<Box<dyn Fn() + Send>>,
}

thread_local! {
    static SINK: RefCell<Option<SinkChannel>> = const { RefCell::new(None) };
}

/// 当前线程安装输出通道与唤醒回调（GUI 引擎线程启动时调用；安装即启用结构化事件模式）
pub fn install(tx: UnboundedSender<EngineOut>, notify: Option<Box<dyn Fn() + Send>>) {
    SINK.with(|s| *s.borrow_mut() = Some(SinkChannel { tx, notify }));
}

/// 当前线程卸载输出通道
pub fn uninstall() {
    SINK.with(|s| *s.borrow_mut() = None);
}

/// 结构化事件模式是否开启（= 本线程已 install；显示路由据此分流 CLI 文本 / GUI 事件）
pub fn event_mode() -> bool {
    SINK.with(|s| s.borrow().is_some())
}

/// send 成功后唤醒（通道关闭/未装回调则静默）
fn notify_sent() {
    SINK.with(|slot| {
        if let Some(ch) = &*slot.borrow() {
            if let Some(n) = &ch.notify {
                n();
            }
        }
    });
}

/// 行输出（等价 println! 语义：带换行）
pub fn line(s: impl Display) {
    let s = s.to_string();
    SINK.with(|slot| match &*slot.borrow() {
        Some(ch) => {
            let _ = ch.tx.send(EngineOut::Line(s.clone()));
        }
        None => ::std::println!("{s}"),
    });
    notify_sent();
}

/// 错误行输出（等价 eprintln! 语义：带换行，写 stderr；通道模式加 ⚠ 标记）
pub fn err(s: impl Display) {
    let s = s.to_string();
    SINK.with(|slot| match &*slot.borrow() {
        Some(ch) => {
            let _ = ch.tx.send(EngineOut::Line(format!("⚠ {s}")));
        }
        None => ::std::eprintln!("{s}"),
    });
    notify_sent();
}

/// 部分行输出（等价 print! 语义：无换行，提示符场景）
pub fn raw(s: impl Display) {
    use std::io::Write;
    let s = s.to_string();
    SINK.with(|slot| match &*slot.borrow() {
        Some(ch) => {
            let _ = ch.tx.send(EngineOut::Line(s.clone()));
        }
        None => {
            ::std::print!("{s}");
            let _ = ::std::io::stdout().flush();
        }
    });
    notify_sent();
}

/// 结构化显示事件（事件模式外为 no-op——CLI 不消费结构化载荷）
pub fn event(e: UiEvent) {
    SINK.with(|slot| {
        if let Some(ch) = &*slot.borrow() {
            let _ = ch.tx.send(EngineOut::Event(e));
        }
    });
    notify_sent();
}

/// 引擎等待 GUI 作答的 Ask 请求（事件模式外为 no-op；单飞行——同一时刻最多一个）
pub fn ask(req: AskRequest) {
    event(UiEvent::Ask(req));
}
