//! 聊天消息显示路由：CLI 文本格式化 / GUI 结构化事件，前端分流唯一入口。
//!
//! 聊天消息的四个显示漏斗（1v1 收/发、群收/发）统一经本模块：
//! - CLI（未装 sink）：格式化文本走 sink::line，颜色/前缀与历史逐字节一致（e2e 兜底）
//! - GUI（引擎线程已 install sink）：构造 [`ChatMessage`] 事件，气泡渲染由 GUI 决定
//! 规则（谁出现在前缀、未信任标记、焦点判定）只在事件载荷与文本格式各定义一次。

use colored::Colorize;

use crate::sink;
use crate::uievent::{ChatMessage, ContactView, GroupView, SidebarState, UiEvent};

/// 收到聊天消息（1v1 与群共用）
/// - `from_display`：对方显示名（联系人名/群内自报名/节点ID）
/// - `focused`：是否当前焦点会话（焦点不显示名前缀——CLI 语义）
/// - `group`：群名（1v1 传 None）
pub fn incoming_chat(from_display: &str, text: &str, focused: bool, group: Option<&str>, untrusted: bool) {
    if sink::event_mode() {
        sink::event(UiEvent::Chat(ChatMessage {
            from: from_display.to_string(),
            text: text.to_string(),
            outgoing: false,
            focused,
            group: group.map(str::to_string),
            untrusted,
        }));
        return;
    }
    match group {
        // 群消息：焦点 `[{谁}]`，非焦点 `[{群名}] [{谁}]` 双前缀（无未信任组合，防御性保留）
        Some(g) => {
            if untrusted {
                sink::line(format!("[未信任] [{g}] [{from_display}]: {text}").yellow());
            } else if focused {
                sink::line(format!("[{from_display}] {text}").bright_cyan());
            } else {
                sink::line(format!("[{g}] [{from_display}] {text}").bright_cyan());
            }
        }
        // 1v1：焦点 `[对方]`，非焦点 `[名字]` 前缀，未信任 `[未信任]` 标记
        None => {
            if untrusted {
                sink::line(format!("[未信任] {from_display}: {text}").yellow());
            } else if focused {
                sink::line(format!("[对方] {text}").bright_cyan());
            } else {
                sink::line(format!("[{from_display}] {text}").bright_cyan());
            }
        }
    }
}

/// 发出聊天消息（send_focused_text 唯一回显点；本侧消息必然处于焦点会话）
pub fn outgoing_chat(who: &str, text: &str, group: Option<&str>) {
    if sink::event_mode() {
        sink::event(UiEvent::Chat(ChatMessage {
            from: who.to_string(),
            text: text.to_string(),
            outgoing: true,
            focused: true,
            group: group.map(str::to_string),
            untrusted: false,
        }));
        return;
    }
    sink::line(format!("[我 -> {who}] {text}").green());
}

/// 侧栏快照推送（联系人 + 群列表；CLI 无事件通道时 no-op）
pub fn sidebar(contacts: Vec<ContactView>, groups: Vec<GroupView>) {
    if sink::event_mode() {
        sink::event(UiEvent::Sidebar(SidebarState { contacts, groups }));
    }
}
