//! GUI 结构化显示事件类型：引擎 → GUI 的显示语义载荷。
//!
//! 与 sink 同层（显示边界基础设施）：sink 单通道统一承载
//! [`EngineOut::Line`]（原文本输出，保序）与 [`EngineOut::Event`]（结构化事件）。
//! p2p 协议核心不感知本模块——只有应用层显示路由（p2p_app/chat/display）构造事件。

/// 聊天消息气泡载荷（1v1 与群共用）
#[derive(Debug, Clone)]
pub struct ChatMessage {
    /// 显示名：对方联系人名/群内自报名/节点ID；本侧消息为会话对端名或群名
    pub from: String,
    pub text: String,
    /// true = 我发送的（GUI 右侧气泡；CLI 绿色）
    pub outgoing: bool,
    /// 当前焦点会话（非焦点在 CLI 带名前缀；GUI 可做"冒泡"提示）
    pub focused: bool,
    /// 群名（1v1 为 None）
    pub group: Option<String>,
    /// 未信任标记（CLI 黄色 [未信任] 前缀；GUI 黄色描边）
    pub untrusted: bool,
}

impl ChatMessage {
    /// 还原 CLI 文本形态（无色；供 GUI 日志落盘与文本渲染兜底）。
    /// 与 display::incoming_chat/outgoing_chat 的 CLI 分支保持同一语义：
    /// 1v1 焦点 `[对方]`、非焦点 `[名字]`；群焦点 `[{谁}]`、非焦点 `[{群名}] [{谁}]`。
    pub fn to_cli_line(&self) -> String {
        match (self.outgoing, self.untrusted) {
            (true, _) => format!(
                "[我 -> {}] {}",
                self.group.as_deref().unwrap_or(&self.from),
                self.text
            ),
            (false, true) => match &self.group {
                Some(g) => format!("[未信任] [{g}] [{}]: {}", self.from, self.text),
                None => format!("[未信任] {}: {}", self.from, self.text),
            },
            (false, false) => match (&self.group, self.focused) {
                (Some(_), true) => format!("[{}] {}", self.from, self.text),
                (Some(g), false) => format!("[{g}] [{}] {}", self.from, self.text),
                (None, true) => format!("[对方] {}", self.text),
                (None, false) => format!("[{}] {}", self.from, self.text),
            },
        }
    }
}

/// 结构化显示事件（随界面功能扩展；文本行永远走 Line，事件只承载结构化语义）
#[derive(Debug, Clone)]
pub enum UiEvent {
    Chat(ChatMessage),
}

/// 引擎输出统一项：单通道 FIFO 保证 Line 与 Event 的相对顺序
#[derive(Debug, Clone)]
pub enum EngineOut {
    /// 普通文本行（= 原 sink line/err/raw 语义，含 "⚠ " 前缀约定）
    Line(String),
    /// 结构化显示事件
    Event(UiEvent),
}
