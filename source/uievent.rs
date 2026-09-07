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

/// 侧栏联系人条目（信任徽标/在线/焦点由引擎侧判定，GUI 只渲染）
#[derive(Debug, Clone)]
pub struct ContactView {
    pub peer_id: String,
    pub name: String,
    pub online: bool,
    /// 当前焦点会话（高亮）
    pub focused: bool,
    /// 互信（我信任 且 对方信任我）
    pub effective_trusted: bool,
    /// 我已信任（对方未确认）
    pub i_trust: bool,
    /// TOFU 指纹（信任确认卡片展示用——D4：信任前人工核对）
    pub fingerprint: String,
}

/// 侧栏群条目
#[derive(Debug, Clone)]
pub struct GroupView {
    pub name: String,
    pub focused: bool,
    pub member_count: usize,
}

/// 侧栏"已发现节点"条目（registered 地址表中有、联系人簿中没有——未握手的节点）
#[derive(Debug, Clone)]
pub struct DiscoveredView {
    pub peer_id: String,
    pub online: bool,
}

/// 侧栏快照（联系人 + 已发现节点 + 群；GUI 左栏整体替换渲染）
#[derive(Debug, Clone, Default)]
pub struct SidebarState {
    pub contacts: Vec<ContactView>,
    pub discovered: Vec<DiscoveredView>,
    pub groups: Vec<GroupView>,
}

/// Ask 类型：引擎等待 GUI 系统消息卡片作答的请求（逐步扩充；答案经 InputMsg::Line 回程）
#[derive(Debug, Clone)]
pub enum AskKind {
    /// TOFU 首次接触指纹核对（答案 y=信任并记录 / n=仅记录不信任）
    TofuConfirm {
        peer_id: String,
        name: String,
        fingerprint: String,
    },
    /// /backup 解锁密码（secret=true；答案即密码行）
    BackupPassword,
}

/// Ask 请求：引擎单飞行（同时最多一个）；id 自增防御错位
#[derive(Debug, Clone)]
pub struct AskRequest {
    pub id: u64,
    pub kind: AskKind,
    /// true = 答案为密码类（GUI masked 输入、trace 打码）
    pub secret: bool,
}

use std::sync::atomic::{AtomicU64, Ordering};

static ASK_SEQ: AtomicU64 = AtomicU64::new(1);

/// Ask 请求自增 id（引擎单飞行，id 仅供防御性配对与 trace）
pub fn next_ask_id() -> u64 {
    ASK_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// 结构化显示事件（随界面功能扩展；文本行永远走 Line，事件只承载结构化语义）
#[derive(Debug, Clone)]
pub enum UiEvent {
    Chat(ChatMessage),
    Sidebar(SidebarState),
    /// 本机新增一条可分享监听地址（GUI 去重累积、"我的地址"点击复制）
    ListenAddr(String),
    /// 引擎等待 GUI 作答（系统消息区域渲染卡片；答案经 InputMsg::Line 回程）
    Ask(AskRequest),
    /// 助记词展示（/backup 解锁成功后；系统消息区域大字卡片 + 复制按钮）
    MnemonicShow { phrase: String },
}

/// 引擎输出统一项：单通道 FIFO 保证 Line 与 Event 的相对顺序
#[derive(Debug, Clone)]
pub enum EngineOut {
    /// 普通文本行（= 原 sink line/err/raw 语义，含 "⚠ " 前缀约定）
    Line(String),
    /// 结构化显示事件
    Event(UiEvent),
}
