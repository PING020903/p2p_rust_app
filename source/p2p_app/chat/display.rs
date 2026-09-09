//! 聊天消息显示路由：CLI 文本格式化 / GUI 结构化事件，前端分流唯一入口。
//!
//! 聊天消息的四个显示漏斗（1v1 收/发、群收/发）统一经本模块：
//! - CLI（未装 sink）：格式化文本走 sink::line，颜色/前缀与历史逐字节一致（e2e 兜底）
//! - GUI（引擎线程已 install sink）：构造 [`ChatMessage`] 事件，气泡渲染由 GUI 决定
//! 规则（谁出现在前缀、未信任标记、焦点判定）只在事件载荷与文本格式各定义一次。

use colored::Colorize;

use crate::sink;
use crate::uievent::{
    ChatMessage, ContactView, DiscoveredView, FileTransferView, GroupView, SettingsView,
    SidebarState, UiEvent,
};

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

/// 侧栏快照推送（联系人 + 已发现节点 + 群列表；CLI 无事件通道时 no-op）
pub fn sidebar(
    contacts: Vec<ContactView>,
    discovered: Vec<DiscoveredView>,
    groups: Vec<GroupView>,
) {
    if sink::event_mode() {
        sink::event(UiEvent::Sidebar(SidebarState {
            contacts,
            discovered,
            groups,
        }));
    }
}

/// 本机监听地址推送（单条，GUI 去重累积；启动时每条监听地址一次，/listen 重查后重发；CLI no-op）
pub fn listen_addr(addr: String) {
    if sink::event_mode() {
        sink::event(UiEvent::ListenAddr(addr));
    }
}

/// 文件传输进度/终态路由（GUI 传输卡片数据源）。
/// - CLI：打印 `cli_text`（调用方预构造的历史文案，含颜色——逐字节保持 e2e 契约）；None = CLI 静默
/// - GUI：发 [`UiEvent::FileTransfer`] 结构化事件（不进滚动区，避免与卡片重复展示）
pub fn file_transfer(view: FileTransferView, cli_text: Option<String>) {
    if sink::event_mode() {
        sink::event(UiEvent::FileTransfer(view));
        return;
    }
    if let Some(text) = cli_text {
        sink::line(text);
    }
}

/// 设置页快照推送（GUI 设置面板数据源；CLI no-op——文本提示由调用方照旧打印）。
/// 内部读取 confirm_file_receive/discovery_mode 权威态，调用方只给 my_id 与下载目录。
pub fn push_settings(my_id: &libp2p::PeerId, download_dir: &std::path::Path) {
    if !sink::event_mode() {
        return;
    }
    let view = SettingsView {
        download_dir: download_dir.display().to_string(),
        confirm_file_receive: crate::p2p::settings::load_confirm_file_receive(my_id),
        discovery_mode: crate::p2p::load_discovery_mode(my_id).name().to_string(),
    };
    sink::event(UiEvent::Settings(view));
}
