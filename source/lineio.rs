//! 输入 I/O 基础设施：LineSource 输入源抽象 + InputMsg 统一输入协议。
//!
//! 与输出侧（sink/uievent）对称——引擎的显示输出走 `EngineOut`，
//! 用户/前端的输入走 `InputMsg`。协议核心（p2p/）只使用 LineSource 读取，
//! 不感知 `Control`（结构化控制动作仅 chat.rs 应用编排层消费）。

use std::error::Error;

use libp2p::PeerId;

/// 输入行迭代器（input 被管道接管时逐行读取）
pub type StdinLines = tokio::io::Lines<tokio::io::BufReader<tokio::io::Stdin>>;

/// 统一输入消息：
/// - `Line`：CLI 语义行——按 `/` 前缀分流（命令树 vs 文本消息），终端/管道逐行产出
/// - `ChatText`：GUI 文本框的纯聊天文本——**绕过命令解析**直接发送到当前焦点（多行原样）
/// - `Control`：结构化控制动作（GUI 点击/按钮）——复刻对应命令的非文本逻辑，
///   不经命令文本解析（名字歧义/注入问题在类型层根除）；CLI/e2e 永不产出
pub enum InputMsg {
    Line(String),
    ChatText(String),
    Control(Control),
}

/// 结构化控制动作（GUI 原生操作 → 引擎；每个变体复刻一条命令的语义）
#[derive(Debug, Clone)]
pub enum Control {
    /// 切 1v1 会话（复刻 /chat：已连接仅聚焦；未连接建会话 + 拨号/待 mDNS。
    /// `name` 用于会话名填充与提示文案，信任判定仍按 peer）
    FocusPeer { peer: PeerId, name: String },
    /// 切群会话（复刻 /group <群名>：聚焦 + 拨号群成员）
    FocusGroup(String),
    /// 信任/取消信任（复刻 /trust：trust() + 清 send_confirmed + 在线发确认/撤销信号）
    Trust { peer: PeerId, trusted: bool },
    /// 添加联系人（复刻 /dial：解析地址 → registered 登记 → 拨号；
    /// `name` 非空时预登记会话名——GUI 侧栏显示名生效，CLI 不传名零影响）
    Dial { addr: String, name: String },
}

impl Control {
    /// 用户动作描述（交互日志用，不暴露凭据类内容）
    pub fn describe(&self) -> String {
        match self {
            Control::FocusPeer { name, .. } => format!("切换会话: {name}"),
            Control::FocusGroup(g) => format!("切换群聊: {g}"),
            Control::Trust { trusted, .. } => {
                if *trusted {
                    "信任联系人".to_string()
                } else {
                    "取消信任".to_string()
                }
            }
            Control::Dial { name, .. } => {
                if name.is_empty() {
                    "添加联系人".to_string()
                } else {
                    format!("添加联系人: {name}")
                }
            }
        }
    }

    /// 调试 trace 明细（key=value；交互日志用 [`Control::describe`]，runtime 诊断用本方法）
    pub fn trace_detail(&self) -> String {
        match self {
            Control::FocusPeer { peer, name } => {
                format!("action=focus_peer peer={peer} name={name}")
            }
            Control::FocusGroup(g) => format!("action=focus_group group={g}"),
            Control::Trust { peer, trusted } => {
                format!("action=trust peer={peer} trusted={trusted}")
            }
            Control::Dial { name, .. } => format!(
                "action=dial name={}",
                if name.is_empty() { "-" } else { name }
            ),
        }
    }
}

/// 输入源抽象：CLI/e2e 读终端或管道（Stdin，逐行产出 Line）；GUI 读 UI 输入通道（Channel）。
pub enum LineSource {
    Stdin(StdinLines),
    Channel(tokio::sync::mpsc::UnboundedReceiver<InputMsg>),
}

impl LineSource {
    /// 聊天循环输入：Stdin 每行包装为 Line；Channel 原样透传 GUI 消息
    pub async fn next_input(&mut self) -> Option<InputMsg> {
        match self {
            LineSource::Stdin(lines) => {
                lines.next_line().await.ok().flatten().map(InputMsg::Line)
            }
            LineSource::Channel(rx) => rx.recv().await,
        }
    }

    /// 交互提示场景的原始行读取（登录/确认；ChatText 亦取其文本）。
    /// 控制动作不作为交互答案——继续等待文本行（防御：提示符阶段不应有 Control）。
    pub async fn next_raw_line(&mut self) -> Option<String> {
        loop {
            match self.next_input().await {
                Some(InputMsg::Line(s)) => return Some(s),
                Some(InputMsg::ChatText(t)) => return Some(t),
                Some(InputMsg::Control(_)) => continue,
                None => return None,
            }
        }
    }

    /// 带提示符读取一行（登录/确认等交互场景；I/O 属输入抽象自身）
    pub async fn prompt(&mut self, prompt: &str) -> Result<String, Box<dyn Error>> {
        use std::io::Write;
        print!("{prompt}");
        std::io::stdout().flush()?;
        self.next_raw_line()
            .await
            .ok_or_else(|| -> Box<dyn Error> { "输入结束".into() })
    }

    /// 带提示符读取密码：交互终端不回显（rpassword）；管道环境（测试/脚本）退回行读取
    pub async fn prompt_secret(
        &mut self,
        interactive: bool,
        prompt: &str,
    ) -> Result<String, Box<dyn Error>> {
        if interactive {
            Ok(rpassword::prompt_password(prompt)?)
        } else {
            self.prompt(prompt).await
        }
    }
}
