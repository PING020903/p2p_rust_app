//! 会话主循环：run/run_engine/run_node——引擎入口与 select 事件循环（CLI/GUI 共用）。
//! 输入抽象 LineSource 已屏蔽模式差异；命令树来自 commands::build_tree。

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::io::IsTerminal;
#[cfg(windows)]
use std::process::Stdio;
use tokio::io::AsyncBufReadExt;

use colored::Colorize;
use libp2p::{Multiaddr, PeerId};

use crate::cmd_tree::CmdError;
use crate::lineio::{ConfirmMode, InputMsg, LineSource};
use crate::p2p::identity::LoginOutcome;
use crate::p2p::identity_service::{is_l2_signal, IdentityService, TextTag};
use crate::p2p::seam::{self, Event, SignalRegistry, BYE_HANDSHAKE_TIMEOUT};
use crate::p2p::load_discovery_mode;
use crate::p2p_app::chat::commands::{build_tree, run_terminal_escape};
use crate::p2p_app::chat::control::handle_control;
use crate::p2p_app::chat::ctx::{consume_ops, make_chat_ctx, peer_name, ChatCtx, Conversation};
use crate::p2p_app::chat::display;
use crate::p2p_app::chat::group::{
    group_topic, load_groups, Group, GroupPayload,
};
use crate::p2p_app::chat::handlers::{
    display_untrusted_text, on_chat_text, on_group_invite, on_group_leave, on_group_member_list,
    on_group_owner_transfer, on_peer_bye_signal, on_peer_hello_signal, on_trust_signal, AppCtx,
};
use crate::p2p_app::chat::payloads::{
    ChatTextPayload, TAG_CHAT_TEXT, TAG_GROUP_INVITE, TAG_GROUP_LEAVE, TAG_GROUP_MEMBER_LIST,
    TAG_GROUP_OWNER_TRANSFER,
};
use crate::p2p_app::chat::sidebar::{push_sidebar, trust_badge};
/// 解析 `/sendStrings` 后的行数参数（`/sendStrings <N>`，N = 后续内容行数）
fn parse_line_count(rest: &str) -> Result<usize, String> {
    let t = rest.trim();
    if t.is_empty() {
        return Err("缺少行数".to_string());
    }
    t.parse::<usize>()
        .map_err(|_| format!("行数须为数字，当前: {t}"))
}

/// 收集一行多行内容；返回 `Some(完整文本)` 当收满最后一行（空行原样保留、内容不解析）。
fn collect_multiline(buf: &mut String, remaining: &mut usize, line: &str) -> Option<String> {
    if !buf.is_empty() {
        buf.push('\n');
    }
    buf.push_str(line);
    *remaining -= 1;
    if *remaining == 0 {
        Some(std::mem::take(buf))
    } else {
        None
    }
}

/// 发送文本到当前焦点（群或 1v1）。普通消息路径与 `/sendStrings` 多行路径共用，
/// 信任门控/回显行为与交互终端一致（管道/脚本未互信自动放行）。
///
/// 返回 `Some((peer, text))` = Ask 模式未互信发送已发卡片、登记待决（两段式：答案
/// 经 AskAnswer 专道到达后由调用方重发）；`None` = 已直接处理（含 CLI Interactive
/// 内联 y/n 与 busy 拒绝）。
/// `pending_busy`：已有待决确认挂起时，Ask 分支不再发卡（单槽模型，防交叉污染）。
async fn send_focused_text(
    ctx: &mut ChatCtx<'_>,
    text: &str,
    pending_busy: bool,
) -> Option<(PeerId, String)> {
    if let Some(gid) = &ctx.focused_group {
        let g = match ctx.groups.get(gid) {
            Some(g) => g.clone(),
            None => {
                eprintln!("{}", "当前群不存在".yellow());
                return None;
            }
        };
        let payload = serde_json::to_vec(&GroupPayload::Text {
            group_id: g.id.clone(),
            text: text.to_string(),
            sender: ctx.identity.my_name().to_string(),
        })
        .unwrap_or_default();
        let _ = ctx
            .cmd_tx
            .send(seam::Cmd::Publish {
                topic: group_topic(&g.id),
                data: payload,
            })
            .await;
        display::outgoing_chat(&g.name, text, Some(&g.name));
        return None;
    }
    match *ctx.focused {
        Some(p) => {
            if !ctx.connected.contains(&p) {
                eprintln!("{}", "当前会话未连接，请用 /chat 重连".yellow());
                return None;
            }
            let name = ctx
                .conversations
                .get(&p)
                .map(|c| c.name.clone())
                .unwrap_or_default();
            let who = if name.is_empty() {
                p.to_string()
            } else {
                name
            };
            // D3：未互信联系人发送确认（三态：Interactive 终端 y/n（内联，终端串行固有）/
            // **Ask 系统消息卡片（两段式：登记待决，答案经 AskAnswer 专道）** /
            // Auto 管道放行但文案如实——发出≠送达，对端未互信时会静默丢弃）
            if !ctx.identity.effective_trusted(&p) {
                match ctx.mode {
                    ConfirmMode::Interactive if !ctx.conversations[&p].send_confirmed => {
                        println!(
                            "{}",
                            format!("对方 {who} 未互信（需双方 /trust），确认发送？(y/n)").yellow()
                        );
                        let ans = match ctx.input.next_raw_line().await {
                            Some(l) => l.trim().to_string(),
                            None => String::new(),
                        };
                        if !ans.eq_ignore_ascii_case("y") {
                            println!("{}", "已取消发送".dimmed());
                            return None;
                        }
                        ctx.conversations.get_mut(&p).unwrap().send_confirmed = true;
                    }
                    ConfirmMode::Ask if !ctx.conversations[&p].send_confirmed => {
                        if pending_busy {
                            eprintln!(
                                "{}",
                                "已有确认挂起，消息未发送（先处理上方请求卡片）".yellow()
                            );
                            return None;
                        }
                        // GUI 两段式：发系统消息卡片（答案经 InputMsg::AskAnswer 专道），
                        // 原文随待决登记——y 后重发（send_confirmed 置位，D3 复查自然通过）
                        crate::sink::ask(crate::uievent::AskRequest {
                            id: crate::uievent::next_ask_id(),
                            kind: crate::uievent::AskKind::UntrustedSend {
                                name: who.clone(),
                            },
                            secret: false,
                        });
                        return Some((p, text.to_string()));
                    }
                    ConfirmMode::Auto => {
                        println!(
                            "{}",
                            format!("对方 {who} 未互信：消息已发出（对端未互信时可能被忽略）")
                                .yellow()
                        );
                    }
                    ConfirmMode::Interactive | ConfirmMode::Ask => {}
                }
            }
            let payload = serde_cbor::to_vec(&ChatTextPayload {
                text: text.to_string(),
            })
            .unwrap_or_default();
            let _ = ctx
                .cmd_tx
                .send(seam::Cmd::Send {
                    peer: p,
                    tag: TAG_CHAT_TEXT.to_string(),
                    payload: Some(payload),
                })
                .await;
            display::outgoing_chat(&who, text, None);
            None
        }
        None => {
            eprintln!(
                "{}",
                "尚未选择会话，无法发送（先 /chat <角色> 或 /group <群名>）".yellow()
            );
            None
        }
    }
}

/// GUI 进程内引擎入口：input 由调用方提供（LineSource::Channel），输出走线程局部 sink。
/// `pre` 为 GUI 登录表单产出的凭据（Some → 直建会话；None 走 CLI 文本登录，兼容保留）
pub async fn run_engine(
    input: LineSource,
    pre: Option<LoginOutcome>,
) -> Result<(), Box<dyn Error>> {
    run_node(input, pre).await
}

pub fn run() {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{}", format!("无法创建 tokio 运行时: {e}").red());
            return;
        }
    };
    rt.block_on(async {
        let input = LineSource::Stdin(tokio::io::BufReader::new(tokio::io::stdin()).lines());
        if let Err(e) = run_node(input, None).await {
            eprintln!("{}", format!("节点运行错误: {e}").red());
        }
    });
}


/// 会话内待决确认（两跳状态机：phase1 登记 → 答案到达 → phase2 执行）
///
/// `via_window` 标记答案面：true=CLI 确认子窗口作答（confirm 通道回传），主窗口行**照常流转**
/// （聊天/命令不被劫持——子窗口作答期间 B 可正常给 A 发消息）；false=答案走主窗口行
/// （Ask 卡片回程 / 非 Windows 退化 / Auto e2e 密码行，终端串行劫持语义）。
enum PendingConfirm {
    Tofu {
        peer: PeerId,
        name: String,
        via_window: bool,
    },
    Backup {
        via_window: bool,
    },
    /// 文件接收 offer 挂起（at 供接收确认超时判定）
    FileReceive {
        from: PeerId,
        file_id: u64,
        name: String,
        size: u64,
        at: std::time::Instant,
        via_window: bool,
    },
    /// 未互信发送挂起（仅 Ask/GUI 两段式：原文随待决暂存，y 后重发；CLI Interactive 走内联 y/n）
    UntrustedSend { peer: PeerId, text: String },
}

impl PendingConfirm {
    /// 答案面是否为确认子窗口（true 时主窗口行不劫持）
    fn via_window(&self) -> bool {
        match self {
            PendingConfirm::Tofu { via_window, .. }
            | PendingConfirm::Backup { via_window, .. }
            | PendingConfirm::FileReceive { via_window, .. } => *via_window,
            // 未互信发送仅 Ask/GUI 登记（答案走 AskAnswer 专道，不经子窗口也不经行劫持）
            PendingConfirm::UntrustedSend { .. } => false,
        }
    }
}

/// 确认答案（CLI 确认子窗口任务回传；GUI 卡片答案经 input 路由到达同一第二阶段）。
/// 构造点全部在 cfg(windows) 子窗口任务——非 Windows 编译下仅模式匹配消费。
#[cfg_attr(not(windows), allow(dead_code))]
enum ConfirmAnswer {
    Tofu { peer: PeerId, trusted: bool },
    Backup { password: String },
    FileReceive {
        from: PeerId,
        file_id: u64,
        name: String,
        size: u64,
        accepted: bool,
    },
}

/// 拉起 CLI TOFU 确认子窗口（CREATE_NEW_CONSOLE；答案以退出码回传：0=信任 / 其它=拒绝）
/// 仅 Windows；非 Windows 退化为主窗口阻塞确认（known boundary）
#[cfg(windows)]
fn spawn_tofu_window(
    tx: tokio::sync::mpsc::UnboundedSender<ConfirmAnswer>,
    name: String,
    fingerprint: String,
    peer: PeerId,
) {
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = tokio::spawn(async move {
        let status = tokio::process::Command::new(exe)
            .args([
                "--confirm-tofu",
                &name,
                &fingerprint,
                &peer.to_string(),
            ])
            // stdio 全 null：不继承主窗口控制台句柄（实测继承会扣住主窗口键盘输入，
            // 子窗口退出才涌出）；子入口 attach_console_stdio 自挂 CONIN$/CONOUT$
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NEW_CONSOLE)
            .status()
            .await;
        let trusted = status.map(|s| s.success()).unwrap_or(false);
        let _ = tx.send(ConfirmAnswer::Tofu { peer, trusted });
    });
}

/// 拉起 CLI 密码确认子窗口（rpassword 不回显；答案写入结果文件由父进程读取）
/// 仅 Windows；非 Windows 退化为主窗口阻塞确认（known boundary）
#[cfg(windows)]
fn spawn_secret_window(
    tx: tokio::sync::mpsc::UnboundedSender<ConfirmAnswer>,
    title: String,
) {
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let result_file = std::env::temp_dir().join(format!(
        "p2p_confirm_secret_{}_{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0),
    ));
    let _ = tokio::spawn(async move {
        let status = tokio::process::Command::new(exe)
            .args(["--confirm-secret", &title, &result_file.display().to_string()])
            // stdio 全 null：同 spawn_tofu_window——不继承主窗口控制台句柄
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NEW_CONSOLE)
            .status()
            .await;
        let password = if status.map(|s| s.success()).unwrap_or(false) {
            std::fs::read_to_string(&result_file).unwrap_or_default()
        } else {
            String::new()
        };
        let _ = std::fs::remove_file(&result_file);
        if !password.is_empty() {
            let _ = tx.send(ConfirmAnswer::Backup { password });
        }
    });
}
/// 拉起 CLI 文件接收确认子窗口（CREATE_NEW_CONSOLE；答案以退出码回传：0=接收 / 其它=拒绝）
/// 仅 Windows；非 Windows 退化为主窗口 pending 路由（known boundary）
#[cfg(windows)]
fn spawn_file_window(
    tx: tokio::sync::mpsc::UnboundedSender<ConfirmAnswer>,
    from: PeerId,
    file_id: u64,
    name: String,
    size: u64,
) {
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = tokio::spawn(async move {
        let status = tokio::process::Command::new(exe)
            .args([
                "--confirm-file",
                &from.to_string(),
                &file_id.to_string(),
                &name,
                &size.to_string(),
            ])
            // stdio 全 null：不继承主窗口控制台句柄（实测继承会扣住主窗口键盘输入，
            // 子窗口退出才涌出）；子入口 attach_console_stdio 自挂 CONIN$/CONOUT$
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NEW_CONSOLE)
            .status()
            .await;
        let accepted = status.map(|s| s.success()).unwrap_or(false);
        let _ = tx.send(ConfirmAnswer::FileReceive {
            from,
            file_id,
            name,
            size,
            accepted,
        });
    });
}

/// 会话循环调试跟踪（`P2P_DEBUG_SESSION=1` 开启）：每个 select 臂触发追加一行到
/// `<cache_dir>/debug/session_<pid>.log`（毫秒时间戳 + 摘要）。
/// 定位"输入吞噬/循环卡死/事件丢失"类问题的运行时证据；默认关闭（file: None，log 零成本），
/// 不改动任何业务逻辑——e2e/正常路径逐字节不受影响。
struct SessionTrace {
    file: Option<std::fs::File>,
}

impl SessionTrace {
    fn enable() -> Self {
        let file = std::env::var("P2P_DEBUG_SESSION")
            .ok()
            .and_then(|_| crate::p2p::cache_dir().ok())
            .and_then(|dir| {
                let d = dir.join("debug");
                std::fs::create_dir_all(&d).ok()?;
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(d.join(format!("session_{}.log", std::process::id())))
                    .ok()
            });
        SessionTrace { file }
    }

    /// tick 臂预条件：仅 trace 开启时存在（每秒一行"tick"——空窗期判循环死活的证据）
    fn enabled(&self) -> bool {
        self.file.is_some()
    }

    fn log(&mut self, msg: &str) {
        if let Some(f) = &mut self.file {
            use std::io::Write;
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let _ = writeln!(f, "{ts} {msg}");
        }
    }
}

/// 确认第二阶段执行器（Line 路由与 AskAnswer 专道共用）：按待决变体落账。
/// 传入的 `pc` 已被调用方 take（pending_confirm 此时为 None）；
/// `answer` 为答案文本（y/n/密码原文——BOM 剥离在各分支内做）。
#[allow(clippy::too_many_arguments)]
async fn execute_confirm(
    pc: PendingConfirm,
    answer: &str,
    identity: &mut IdentityService,
    conversations: &mut HashMap<PeerId, Conversation>,
    groups: &mut HashMap<String, Group>,
    focused: &mut Option<PeerId>,
    focused_group: &mut Option<String>,
    connected: &HashSet<PeerId>,
    registered: &mut HashMap<PeerId, Vec<Multiaddr>>,
    input: &mut LineSource,
    mode: ConfirmMode,
    cmd_tx: &tokio::sync::mpsc::Sender<seam::Cmd>,
    file_state: &mut crate::p2p_app::file_transfer::FileTransferState,
) {
    match pc {
        PendingConfirm::Tofu { peer, name, .. } => {
            let trusted = answer.trim().eq_ignore_ascii_case("y");
            identity.complete_tofu(&peer, &name, trusted);
            // 补跑 hello 钩子：会话名更新 + 上线提示
            if let Some(conv) = conversations.get_mut(&peer) {
                if conv.name.is_empty() {
                    conv.name = name.clone();
                }
            }
            println!("{}", format!("对方已上线: {name}").green());
            // 信任重报（自愈）：落账后向对方重报当前信任态
            let my_name = identity.my_name().to_string();
            let trust_tag = if identity.is_verified(&peer) {
                TextTag::TrustConfirm.as_str()
            } else {
                TextTag::TrustRevoke.as_str()
            };
            let _ = cmd_tx
                .send(seam::Cmd::Send {
                    peer,
                    tag: trust_tag.to_string(),
                    payload: Some(serde_cbor::to_vec(&my_name).unwrap_or_default()),
                })
                .await;
            push_sidebar(identity, groups, connected, focused, focused_group, registered);
        }
        PendingConfirm::Backup { .. } => {
            identity.backup_complete(answer.trim(), mode);
        }
        PendingConfirm::FileReceive {
            from,
            file_id,
            name,
            size,
            ..
        } => {
            // 主窗口作答路径（非 Windows 退化；GUI 卡片答案走 AskAnswer 专道）
            let accept =
                answer.trim_start_matches('\u{feff}').trim().eq_ignore_ascii_case("y");
            let mut actx = AppCtx {
                identity,
                conversations,
                groups,
                focused,
                mode,
                hello_pending: None,
                file_pending: None,
                cmd_tx,
                file: file_state,
            };
            crate::p2p_app::file_transfer::complete_file_receive(
                &mut actx, &from, file_id, name, size, accept,
            )
            .await;
        }
        PendingConfirm::UntrustedSend { peer, text } => {
            let confirmed = answer.trim().eq_ignore_ascii_case("y");
            if !confirmed {
                println!("{}", "已取消发送".dimmed());
                return;
            }
            if let Some(conv) = conversations.get_mut(&peer) {
                conv.send_confirmed = true;
            }
            // 复用发送路径：send_confirmed 已置位，D3 复查自然通过（pending_busy=false）
            let mut ctx = make_chat_ctx(
                identity, cmd_tx, input, mode, conversations, groups, focused, focused_group,
                connected, registered, file_state,
            );
            send_focused_text(&mut ctx, &text, false).await;
        }
    }
}

pub async fn run_node(mut input: LineSource, pre: Option<LoginOutcome>) -> Result<(), Box<dyn Error>> {
    // 交互确认三态：Stdin 终端=Interactive（文字提示 + 确认子窗口）；Stdin 管道=Auto（e2e 自动语义）；
    // Channel（GUI）=Ask（系统消息卡片 + InputMsg::Line 回程）
    let mode = ConfirmMode::of(&input, std::io::stdin().is_terminal());
    // CLI 交互终端：禁用 QuickEdit——确认子窗口抢焦点后用户点击拖选会冻结控制台输入
    #[cfg(windows)]
    if mode == ConfirmMode::Interactive && crate::disable_quick_edit() {
        println!("{}", "已禁用控制台快速编辑（防点击拖选冻结输入）".dimmed());
    }
    // 确认应答通道：CLI 确认子窗口任务经此回传答案（GUI 卡片答案走 input 路由）
    let (confirm_tx, mut confirm_rx) = tokio::sync::mpsc::unbounded_channel::<ConfirmAnswer>();
    // 非 Windows：确认子窗口不存在，confirm_tx 无生产者（抑制未用告警）
    #[cfg(not(windows))]
    let _ = &confirm_tx;
    // 待决确认状态机：登记后由 input 行（CLI/GUI 答案）或 confirm 臂（CLI 子窗口）驱动第二阶段
    let mut pending_confirm: Option<PendingConfirm> = None;

    // L2 身份基础：GUI 表单凭据直建会话（影子探测防同 ID 双在线），
    // 或 CLI 文本登录菜单产出凭据 → login_pre + 联系人簿（TOFU）
    let mut identity = match pre {
        Some(outcome) => IdentityService::login_pre(outcome).await?,
        None => crate::p2p_app::chat::cli::login::run(&mut input, mode).await?,
    };
    let discovery_mode = load_discovery_mode(identity.my_id());
    println!(
        "{}",
        format!("发现模式: {}", discovery_mode.name()).dimmed()
    );
    // 文件传输应用状态（下载目录在构造时解析，见 FileTransferState::new）
    let mut file_state = crate::p2p_app::file_transfer::FileTransferState::new(identity.my_id());
    println!(
        "{}",
        format!("下载目录: {}", file_state.downloads_dir().display()).dimmed()
    );
    // 设置页快照（GUI 数据源；CLI no-op）
    crate::p2p_app::chat::display::push_settings(
        identity.my_id(),
        file_state.downloads_dir(),
    );

    // L3 群注册表（登录后先读本地持久化）
    let mut groups: HashMap<String, Group> = load_groups(identity.my_id());
    let mut focused_group: Option<String> = None;

    // 会话循环调试跟踪（P2P_DEBUG_SESSION=1 开启，见 SessionTrace）
    let mut trace = SessionTrace::enable();
    trace.log("session start");

    // L1 传输任务经 L2 适配层（seam）：L3 只见 Cmd/Event（tag+payload），
    // 不接触 Frame/control。事件用无界通道——传输任务永不因应用阻塞
    // （应用卡在 TOFU/密码等交互 await 时，心跳仍由传输任务独立维持）
    let transport = seam::spawn_transport(identity.keypair().clone(), discovery_mode)?;
    let cmd_tx = transport.cmd_tx;
    let mut ev_rx = transport.ev_rx;

    // 订阅已保存群的 gossipsub topic
    for g in groups.values() {
        let _ = cmd_tx
            .send(seam::Cmd::Subscribe {
                topic: group_topic(&g.id),
            })
            .await;
    }

    let mut conversations: HashMap<PeerId, Conversation> = HashMap::new();
    let mut focused: Option<PeerId> = None;
    let mut connected: HashSet<PeerId> = HashSet::new();
    let mut registered: HashMap<PeerId, Vec<Multiaddr>> = HashMap::new();

    // trace tick 定时器（仅 trace 开启时的 select 臂）：每秒一行 tick——
    // 输入空窗期循环死活的直接证据（tick 连续=循环活、输入管道问题；tick 停=循环卡死）
    let mut trace_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + std::time::Duration::from_secs(1),
        std::time::Duration::from_secs(1),
    );

    // 语义注册表（L2 seam 提供）：L2 存在语义（hello/bye/trust 内化 text）+ L3 chat 业务 handler。
    // 收到信号 tag 即查表分发（无 match）。L2 内化信号用 TextTag 常量注册（L3 不触碰）。
    let mut registry: SignalRegistry<AppCtx<'static>> = SignalRegistry::new();
    registry.register(TextTag::Hello.as_str(), |ctx, from, payload| {
        Box::pin(on_peer_hello_signal(ctx, from, payload))
    });
    registry.register(TextTag::Bye.as_str(), |ctx, from, payload| {
        Box::pin(on_peer_bye_signal(ctx, from, payload))
    });
    // L2 信任信号（对称信任：对方告知"我信任你/我取消信任你"）
    registry.register(TextTag::TrustConfirm.as_str(), |ctx, from, payload| {
        Box::pin(on_trust_signal(ctx, from, payload, true))
    });
    registry.register(TextTag::TrustRevoke.as_str(), |ctx, from, payload| {
        Box::pin(on_trust_signal(ctx, from, payload, false))
    });
    registry.register(TAG_CHAT_TEXT, |ctx, from, payload| {
        Box::pin(on_chat_text(ctx, from, payload))
    });
    // 测试专用：P2P_E2E_UNTRUSTED_HOOK=1 时注册未互信 chat.text 钩子（带 [未信任] 标记显示），
    // 用于验证"未互信信号处理是每端本地策略、协议互通"的边界（A 注册显示 / B 未注册丢弃）
    if std::env::var("P2P_E2E_UNTRUSTED_HOOK").is_ok() {
        registry.register_untrusted(TAG_CHAT_TEXT, |ctx, from, payload| {
            Box::pin(display_untrusted_text(ctx, from, payload))
        });
    }
    registry.register(TAG_GROUP_INVITE, |ctx, from, payload| {
        Box::pin(on_group_invite(ctx, from, payload))
    });
    registry.register(TAG_GROUP_LEAVE, |ctx, from, payload| {
        Box::pin(on_group_leave(ctx, from, payload))
    });
    registry.register(TAG_GROUP_MEMBER_LIST, |ctx, from, payload| {
        Box::pin(on_group_member_list(ctx, from, payload))
    });
    registry.register(TAG_GROUP_OWNER_TRANSFER, |ctx, from, payload| {
        Box::pin(on_group_owner_transfer(ctx, from, payload))
    });
    // 文件传输应用（L3 应用②）注册 file.* 语义
    use crate::p2p_app::file_transfer as ft;
    registry.register(ft::TAG_FILE_OFFER, |ctx, from, payload| {
        Box::pin(ft::on_file_offer(ctx, from, payload))
    });
    registry.register(ft::TAG_FILE_ACCEPT, |ctx, from, payload| {
        Box::pin(ft::on_file_accept(ctx, from, payload))
    });
    registry.register(ft::TAG_FILE_REJECT, |ctx, from, payload| {
        Box::pin(ft::on_file_reject(ctx, from, payload))
    });
    registry.register(ft::TAG_FILE_CHUNK, |ctx, from, payload| {
        Box::pin(ft::on_file_chunk(ctx, from, payload))
    });
    registry.register(ft::TAG_FILE_ACK, |ctx, from, payload| {
        Box::pin(ft::on_file_ack(ctx, from, payload))
    });
    registry.register(ft::TAG_FILE_FINISH, |ctx, from, payload| {
        Box::pin(ft::on_file_finish(ctx, from, payload))
    });
    registry.register(ft::TAG_FILE_COMPLETE, |ctx, from, payload| {
        Box::pin(ft::on_file_complete(ctx, from, payload))
    });
    registry.register(ft::TAG_FILE_ABORT, |ctx, from, payload| {
        Box::pin(ft::on_file_abort(ctx, from, payload))
    });

    println!(
        "{}",
        "命令以 / 开头（/help 查看详情，/list 查看节点，/chat <角色> 发起聊天）；/sendStrings <行数> 发送多行文本；cmd/、ps/、sh/ 可直控终端；其余输入作为消息发送给当前聊天对象".dimmed()
    );

    // `/sendStrings <N>` 多行收集态：缓冲内容 + 剩余行数（None = 未在收集）
    let mut sendstrings: Option<(String, usize)> = None;

    loop {
        // 定时臂预条件：发送侧有等待确认的 offer，或接收确认挂起中——睡到最早到期时刻
        // （两者皆无则该分支禁用）
        let offer_expiry = file_state
            .next_offer_expiry()
            .map(tokio::time::Instant::from_std);
        let confirm_expiry = match &pending_confirm {
            Some(PendingConfirm::FileReceive { at, .. }) => {
                Some(tokio::time::Instant::from_std(
                    *at + crate::p2p_app::file_transfer::OFFER_TIMEOUT,
                ))
            }
            _ => None,
        };
        let timer_deadline = match (offer_expiry, confirm_expiry) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        tokio::select! {        // 确认应答臂：CLI 确认子窗口任务回传答案 → 执行第二阶段（不冻结会话循环）
        answer = confirm_rx.recv(), if !confirm_rx.is_closed() => {
            trace.log("confirm: answer");
            match answer {                Some(ConfirmAnswer::Tofu { peer, trusted }) => {
                    let name = match &pending_confirm {
                        Some(PendingConfirm::Tofu { name, .. }) => name.clone(),
                        _ => String::new(),
                    };
                    identity.complete_tofu(&peer, &name, trusted);
                    // 补跑 hello 钩子：会话名更新 + 上线提示
                    if let Some(conv) = conversations.get_mut(&peer) {
                        if conv.name.is_empty() {
                            conv.name = name.clone();
                        }
                    }
                    println!("{}", format!("对方已上线: {name}").green());
                    // 信任重报（自愈）：落账后向对方重报当前信任态
                    let my_name = identity.my_name().to_string();
                    let trust_tag = if identity.is_verified(&peer) {
                        TextTag::TrustConfirm.as_str()
                    } else {
                        TextTag::TrustRevoke.as_str()
                    };
                    let _ = cmd_tx
                        .send(seam::Cmd::Send {
                            peer,
                            tag: trust_tag.to_string(),
                            payload: Some(serde_cbor::to_vec(&my_name).unwrap_or_default()),
                        })
                        .await;
                    push_sidebar(
                        &identity, &groups, &connected, &focused, &focused_group,
                        &registered,
                    );
                    pending_confirm = None;
                }
                Some(ConfirmAnswer::Backup { password }) => {
                    identity.backup_complete(&password, mode);
                    pending_confirm = None;
                }
                Some(ConfirmAnswer::FileReceive { from, file_id, name, size, accepted }) => {
                    trace.log(&format!("confirm: file id={file_id} ok={accepted}"));
                    // 仅当挂起的正是该 offer 才落账（迟到的子窗口答案忽略）
                    let matches = matches!(
                        &pending_confirm,
                        Some(PendingConfirm::FileReceive { file_id: fid, .. }) if *fid == file_id
                    );
                    if matches {
                        let mut actx = AppCtx {
                            identity: &mut identity,
                            conversations: &mut conversations,
                            groups: &mut groups,
                            focused: &mut focused,
                            mode,
                            hello_pending: None,
                            file_pending: None,
                            cmd_tx: &cmd_tx,
                            file: &mut file_state,
                        };
                        ft::complete_file_receive(&mut actx, &from, file_id, name, size, accepted)
                            .await;
                        pending_confirm = None;
                    }
                }
                None => {}
            }
        }
            _ = tokio::time::sleep_until(timer_deadline.unwrap_or_else(tokio::time::Instant::now)), if timer_deadline.is_some() => {
                trace.log("timer: fired");
                // H2：发送侧 offer 超时——对端不应答（网络突发/对端退出）时中止并清理，防状态悬挂
                for (peer, file_id, name) in
                    file_state.expire_stale_offers(crate::p2p_app::file_transfer::OFFER_TIMEOUT)
                {
                    let _ = cmd_tx
                        .send(seam::Cmd::Send {
                            peer,
                            tag: ft::TAG_FILE_ABORT.to_string(),
                            payload: Some(
                                serde_cbor::to_vec(&crate::p2p_app::file_transfer::FileAbortPayload {
                                    file_id,
                                    reason: "等待对端确认超时".into(),
                                })
                                .unwrap_or_default(),
                            ),
                        })
                        .await;
                    // 发送卡片终态（GUI）；CLI 文案逐字节保持
                    let view = crate::uievent::FileTransferView {
                        peer: peer.to_string(),
                        file_id,
                        peer_name: peer_name(&peer, &conversations, &identity),
                        name: name.clone(),
                        outgoing: true,
                        total: 0,
                        sent: 0,
                        done: true,
                        ok: false,
                        saved_path: None,
                        error: Some("等待对端确认超时".into()),
                    };
                    crate::p2p_app::chat::display::file_transfer(
                        view,
                        Some(format!("等待对端确认超时，已中止发送: {name}").yellow().to_string()),
                    );
                }
                // 接收确认超时：挂起的 offer 未作答 → 自动拒绝并清槽
                // （防单槽 pending 被未答 offer 永久占位，挡住后续 TOFU/Backup/文件确认）
                let expired = match &pending_confirm {
                    Some(PendingConfirm::FileReceive { from, file_id, name, at, .. })
                        if at.elapsed() >= crate::p2p_app::file_transfer::OFFER_TIMEOUT =>
                    {
                        Some((*from, *file_id, name.clone()))
                    }
                    _ => None,
                };
                if let Some((from, file_id, name)) = expired {
                    let _ = cmd_tx
                        .send(seam::Cmd::Send {
                            peer: from,
                            tag: ft::TAG_FILE_REJECT.to_string(),
                            payload: Some(
                                serde_cbor::to_vec(&ft::FileRejectPayload {
                                    file_id,
                                    reason: "对方未及时确认，已自动拒绝".into(),
                                })
                                .unwrap_or_default(),
                            ),
                        })
                        .await;
                    pending_confirm = None;
                    println!(
                        "{}",
                        format!("文件 {name} 确认超时，已自动拒绝").yellow()
                    );
                }
            }
            msg = input.next_input() => {
                // None = 输入结束（EOF/通道关闭）：多行收集中则报未闭合丢弃
                let line = match msg {
                    // 待决确认路由：登记期间到达的文本行 = 该确认的答案（终端串行语义）。
                    // 仅当答案面是主窗口行（Ask 卡片回程/非 Windows 退化/Auto e2e）才劫持；
                    // via_window（CLI 确认子窗口作答中）时主窗口行照常流转——聊天/命令畅通
                    Some(InputMsg::Line(l)) => {
                        trace.log(&format!(
                            "in: {}",
                            l.trim().chars().take(40).collect::<String>()
                        ));
                        // 待决确认路由（stdin 行作答）：仅 Interactive 非 Windows 退化与
                        // Auto e2e 密码行——Ask（GUI）下 Line 一律是命令（卡片答案走
                        // AskAnswer 专道）；via_window（子窗口）时主窗口行照常流转
                        if matches!(&pending_confirm, Some(pc) if !pc.via_window())
                            && mode != ConfirmMode::Ask
                        {
                            if let Some(pc) = pending_confirm.take() {
                                execute_confirm(
                                    pc,
                                    &l,
                                    &mut identity, &mut conversations, &mut groups,
                                    &mut focused, &mut focused_group, &connected,
                                    &mut registered, &mut input, mode, &cmd_tx, &mut file_state,
                                )
                                .await;
                                continue;
                            }
                        }
                        l
                    }
                    // 卡片答案专道（GUI Ask）：类型层分流——答案≠输入行。
                    // 无对应待决（已超时/已被消费）= 迟到答案，丢弃并提示
                    Some(InputMsg::AskAnswer { text }) => {
                        trace.log(&format!(
                            "ask-answer: {}",
                            text.trim().chars().take(20).collect::<String>()
                        ));
                        if matches!(&pending_confirm, Some(pc) if !pc.via_window()) {
                            if let Some(pc) = pending_confirm.take() {
                                execute_confirm(
                                    pc,
                                    &text,
                                    &mut identity, &mut conversations, &mut groups,
                                    &mut focused, &mut focused_group, &connected,
                                    &mut registered, &mut input, mode, &cmd_tx, &mut file_state,
                                )
                                .await;
                            }
                        } else {
                            eprintln!(
                                "{}",
                                "确认答案已失效（无对应待决项），已忽略".dimmed()
                            );
                        }
                        continue;
                    }
                    // 结构化控制动作（GUI 点击/按钮）：不经命令文本解析，复刻对应命令逻辑
                    Some(InputMsg::Control(c)) => {
                        trace.log(&format!("ctl: {}", c.describe()));
                        let mut ctx = make_chat_ctx(
                            &mut identity, &cmd_tx, &mut input, mode,
                            &mut conversations, &mut groups, &mut focused, &mut focused_group,
                            &connected, &mut registered, &mut file_state,
                        );
                        handle_control(&mut ctx, c).await;
                        if consume_ops(&mut ctx).await.is_some() {
                            // Backup 进入等待密码挂起：登记待决确认
                            pending_confirm = Some(PendingConfirm::Backup {
                                via_window: cfg!(windows) && mode == ConfirmMode::Interactive,
                            });
                            #[cfg(windows)]
                            if mode == ConfirmMode::Interactive {
                                spawn_secret_window(
                                    confirm_tx.clone(),
                                    "备份助记词：输入解锁密码".to_string(),
                                );
                            }
                            // 非 Windows：无密码子窗口——pending 走主窗口行作答（known boundary）
                        }
                        push_sidebar(ctx.identity, ctx.groups, ctx.connected, ctx.focused, &ctx.focused_group, ctx.registered);
                        continue;
                    }
                    Some(InputMsg::ChatText(text)) => {
                        // GUI 文本框：纯聊天文本直发当前焦点（多行原样；以 / 开头也不解析为命令）
                        trace.log(&format!(
                            "txt: {}",
                            text.trim().chars().take(40).collect::<String>()
                        ));
                        let text = text.trim();
                        if !text.is_empty() {
                            let mut ctx = make_chat_ctx(
                                &mut identity, &cmd_tx, &mut input, mode,
                                &mut conversations, &mut groups, &mut focused, &mut focused_group,
                                &connected, &mut registered, &mut file_state,
                            );
                            // Ask 未互信发送两段式：返回 Some = 已发卡片，登记待决（原文随存）
                            if let Some((peer, text)) =
                                send_focused_text(&mut ctx, &text, pending_confirm.is_some()).await
                            {
                                pending_confirm = Some(PendingConfirm::UntrustedSend {
                                    peer,
                                    text,
                                });
                            }
                        }
                        continue;
                    }
                    None => {
                        if let Some((_, remaining)) = &sendstrings {
                            eprintln!(
                                "{}",
                                format!("多行消息未闭合（还差 {remaining} 行时输入结束），已丢弃").yellow()
                            );
                        }
                        trace.log("exit: input eof");
                        break;
                    }
                };
                // 多行收集态优先：不 trim、空行原样保留，正文以 / 开头也不解析为命令
                if let Some((mut buf, mut remaining)) = sendstrings.take() {
                    if let Some(content) = collect_multiline(&mut buf, &mut remaining, &line) {
                        let mut ctx = make_chat_ctx(
                            &mut identity, &cmd_tx, &mut input, mode,
                            &mut conversations, &mut groups, &mut focused, &mut focused_group,
                            &connected, &mut registered, &mut file_state,
                        );
                        if let Some((peer, text)) =
                            send_focused_text(&mut ctx, &content, pending_confirm.is_some()).await
                        {
                            pending_confirm = Some(PendingConfirm::UntrustedSend { peer, text });
                        }
                    } else {
                        sendstrings = Some((buf, remaining));
                    }
                    continue;
                }
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                // cmd/...、ps/...、sh/...：终端逃逸，绕过应用直控当前终端（清屏/跑命令）
                if let Some(rest) = line.strip_prefix("cmd/") {
                    run_terminal_escape("cmd", &["/c"], rest).await;
                    continue;
                }
                if let Some(rest) = line.strip_prefix("ps/") {
                    run_terminal_escape("powershell", &["-Command"], rest).await;
                    continue;
                }
                if let Some(rest) = line.strip_prefix("sh/") {
                    run_terminal_escape("sh", &["-c"], rest).await;
                    continue;
                }
                // /sendStrings <N>：多行文本（行数声明；GUI 自动计数并逐行写入，内容零解析）
                if let Some(rest) = line.strip_prefix("/sendStrings") {
                    match parse_line_count(rest) {
                        Ok(0) => {
                            eprintln!("{}", "多行内容不能为空".yellow());
                            continue;
                        }
                        Ok(n) => {
                            sendstrings = Some((String::new(), n));
                            continue;
                        }
                        Err(reason) => {
                            eprintln!(
                                "{}",
                                format!("{reason}（用法: /sendStrings <行数>，随后输入恰好 N 行）").yellow()
                            );
                            continue;
                        }
                    }
                }
                if let Some(cmd) = line.strip_prefix('/') {
                    trace.log(&format!("cmd: {}", cmd.chars().take(40).collect::<String>()));
                    // 命令上下文：一次性借用全部状态，handler 同步改状态 + 排异步动作队列。
                    // 指令树每行重建（无状态 builder，开销可忽略）：其 `ChatCtx<'a>` 生命周期
                    // 随本次处理结束释放，借用不跨 select 迭代存活。
                    let mut ctx = make_chat_ctx(
                        &mut identity, &cmd_tx, &mut input, mode,
                        &mut conversations, &mut groups, &mut focused, &mut focused_group,
                        &connected, &mut registered, &mut file_state,
                    );
                    let mut tree = build_tree();
                    if let Err(CmdError::NotFound) = tree.parse(cmd, &mut ctx) {
                        eprintln!("{}", format!("未知命令: {cmd}").yellow());
                    }
                    // 异步消费 handler 排队的动作（同步生产者 → 异步消费者）
                    if consume_ops(&mut ctx).await.is_some() {
                        // Backup 进入等待密码挂起：登记待决确认
                        pending_confirm = Some(PendingConfirm::Backup {
                            via_window: cfg!(windows) && mode == ConfirmMode::Interactive,
                        });
                        #[cfg(windows)]
                        if mode == ConfirmMode::Interactive {
                            spawn_secret_window(
                                confirm_tx.clone(),
                                "备份助记词：输入解锁密码".to_string(),
                            );
                        }
                        // 非 Windows：无密码子窗口——pending 走主窗口行作答（known boundary）
                    }
                    if ctx.quit {
                        // 等 Bye 帧送达（传输任务独立处理），再关闭传输任务
                        tokio::time::sleep(BYE_HANDSHAKE_TIMEOUT).await;
                        let _ = ctx.cmd_tx.send(seam::Cmd::Shutdown).await;
                        trace.log("exit: quit");
                        break;
                    }
                    push_sidebar(
                        ctx.identity,
                        ctx.groups,
                        ctx.connected,
                        ctx.focused,
                        &ctx.focused_group,
                        ctx.registered,
                    );
                    continue;
                }
                // 非命令：作为消息发送给当前聊天对象（与 /sendStrings 共用同一发送逻辑）
                let mut ctx = make_chat_ctx(
                    &mut identity, &cmd_tx, &mut input, mode,
                    &mut conversations, &mut groups, &mut focused, &mut focused_group,
                    &connected, &mut registered, &mut file_state,
                );
                trace.log("chat begin");
                if let Some((peer, text)) =
                    send_focused_text(&mut ctx, line, pending_confirm.is_some()).await
                {
                    pending_confirm = Some(PendingConfirm::UntrustedSend { peer, text });
                }
                trace.log("chat done");
            }
            // trace tick 臂（仅 trace 开启时启用）：每秒一行，判循环死活
            _ = trace_tick.tick(), if trace.enabled() => {
                trace.log("tick");
            }
            event = ev_rx.recv() => {
                match event {
                    Some(ev) => {
                        match ev {
                            Event::Connected(peer) => {
                                trace.log(&format!("ev: connected {peer}"));
                                connected.insert(peer);
                                if let Some(conv) = conversations.get_mut(&peer) {
                                    conv.pending_dial = false;
                                }
                                // 仅在没有 1v1/群焦点时自动聚焦首个连接，避免连上群成员时抢焦点
                                if focused.is_none() && focused_group.is_none() {
                                    focused = Some(peer);
                                    let name = conversations
                                        .get(&peer)
                                        .map(|c| c.name.clone())
                                        .unwrap_or_default();
                                    if !name.is_empty() {
                                        let badge = trust_badge(
                                            identity.effective_trusted(&peer),
                                            identity.is_verified(&peer),
                                        );
                                        println!(
                                            "{}",
                                            format!("已切换到会话: {}（{peer}）{badge}", name)
                                                .green()
                                        );
                                    }
                                }
                                println!("{}", format!("已连接对端: {peer}").green());
                                let conv = conversations
                                    .entry(peer)
                                    .or_insert_with(Conversation::new);
                                if !conv.greeted {
                                    // hello 存在信号：tag="hello" + payload=cbor(名字)，帧组装由 seam 负责
                                    let my_name = identity.my_name().to_string();
                                    let name_bin =
                                        serde_cbor::to_vec(&my_name).unwrap_or_default();
                                    let _ = cmd_tx
                                        .send(seam::Cmd::Send {
                                            peer,
                                            tag: TextTag::Hello.as_str().to_string(),
                                            payload: Some(name_bin),
                                        })
                                        .await;
                                    conv.greeted = true;
                                }
                            }
                            Event::Disconnected { peer, bye } => {
                                connected.remove(&peer);
                                if let Some(conv) = conversations.get_mut(&peer) {
                                    conv.greeted = false;
                                }
                                if focused == Some(peer) {
                                    focused = None;
                                    eprintln!(
                                        "{}",
                                        format!(
                                            "当前会话已断开（{peer}），用 /chat 重新选择"
                                        )
                                        .yellow()
                                    );
                                }
                                if bye {
                                    registered.remove(&peer);
                                    println!("{}", "对方已正常退出，不进行重连".dimmed());
                                }
                            }
                            Event::Discovered { peer, addr } => {
                                let recorded = registered.entry(peer).or_default();
                                if !recorded.contains(&addr) {
                                    recorded.push(addr.clone());
                                }
                                // 待接呼叫 或 常驻群成员：上线即拨号（决策归 L3，动作经 DialPeer 命令）
                                let pending_dial = conversations
                                    .get(&peer)
                                    .map(|c| c.pending_dial)
                                    .unwrap_or(false);
                                let resident_member = groups.values().any(|g| {
                                    g.resident
                                        && g.members.iter().any(|m| m == &peer.to_string())
                                });
                                if (pending_dial || resident_member)
                                    && !connected.contains(&peer)
                                {
                                    println!(
                                        "{}",
                                        format!("发现可连接节点，拨号 {peer}").cyan()
                                    );
                                    let _ = cmd_tx.send(seam::Cmd::DialPeer(peer)).await;
                                }
                            }
                            Event::Signal { from, tag, payload } => {
                                trace.log(&format!("sig: {tag} from {from}"));
                                // 通道路由：control（L1 心跳）已被 seam 过滤，L3 只见信号帧。
                                // 无 match：构造应用上下文，按 tag 标签查 SignalRegistry 分发，
                                // await 注册的 handler（hello/bye/trust 由 L2 内化语义映射 + L3 钩子；
                                // chat.* 由 chat 业务 handler 处理）。
                                // L2 门禁（唯一收口）：内化信号（hello/bye/trust）一律放行；
                                // 业务信号（chat.*/file.*）须互信，否则走该 tag 的未互信钩子
                                // （L2 API `register_untrusted`；未注册 = 空函数 = 丢弃）
                                if !is_l2_signal(&tag) && !identity.effective_trusted(&from) {
                                    // 拦截可见性：未互信来源的业务信号被丢弃时明确提示
                                    // （每条都提示——单方面信任的"不通"必须可诊断）
                                    let who = peer_name(&from, &conversations, &identity);
                                    println!(
                                        "{}",
                                        format!(
                                            "收到未信任方 {who} 的业务消息已拦截（互信后可见）: {tag}"
                                        )
                                        .yellow()
                                    );
                                    let mut actx = AppCtx {
                                        identity: &mut identity,
                                        conversations: &mut conversations,
                                        groups: &mut groups,
                                        focused: &mut focused,
                                        mode,
                                        hello_pending: None,
                                        file_pending: None,
                                        cmd_tx: &cmd_tx,
                                        file: &mut file_state,
                                    };
                                    registry
                                        .handle_untrusted(&tag, &from, payload.as_deref(), &mut actx)
                                        .await;
                                    continue;
                                }
                                let mut actx = AppCtx {
                                    identity: &mut identity,
                                    conversations: &mut conversations,
                                    groups: &mut groups,
                                    focused: &mut focused,
                                    mode,
                                    hello_pending: None,
                                    file_pending: None,
                                    cmd_tx: &cmd_tx,
                                    file: &mut file_state,
                                };
                                let handled = registry
                                    .dispatch(&tag, &from, payload.as_deref(), &mut actx)
                                    .await;
                                if !handled {
                                    eprintln!(
                                        "{}",
                                        format!("未处理的自定义语义: {tag}").yellow()
                                    );
                                }
                                // hello 两段式：TOFU 首触挂起 → 登记待决确认
                                // （Interactive 同时拉起确认子窗口；Ask 卡片已由 L2 发出）
                                if let Some((peer, name)) = actx.hello_pending.take() {
                                    pending_confirm = Some(PendingConfirm::Tofu {
                                        peer,
                                        name: name.clone(),
                                        via_window: cfg!(windows)
                                            && mode == ConfirmMode::Interactive,
                                    });
                                    #[cfg(windows)]
                                    if mode == ConfirmMode::Interactive {
                                        let fp = identity.fingerprint(&peer);
                                        spawn_tofu_window(
                                            confirm_tx.clone(),
                                            name.clone(),
                                            fp,
                                            peer,
                                        );
                                    }
                                    #[cfg(not(windows))]
                                    if mode == ConfirmMode::Interactive {
                                        // 非 Windows 退化：记录为未信任（known boundary）
                                        identity.complete_tofu(&peer, &name, false);
                                        println!(
                                            "{}",
                                            "非 Windows 暂不支持交互确认，已记录为未信任（可稍后 /trust 升级）"
                                                .yellow()
                                        );
                                        pending_confirm = None;
                                    }
                                    continue;
                                }
                                // 文件 offer 两段式：phase1 登记 → 登记待决确认
                                // （Interactive 拉起 --confirm-file 子窗口；Ask 卡片答案经 input 回程）
                                if let Some(offer) = actx.file_pending.take() {
                                    if pending_confirm.is_some() {
                                        // 忙拒：另一确认挂起中——单槽模型不覆盖既有待决
                                        let _ = cmd_tx
                                            .send(seam::Cmd::Send {
                                                peer: offer.from,
                                                tag: ft::TAG_FILE_REJECT.to_string(),
                                                payload: Some(serde_cbor::to_vec(
                                                    &ft::FileRejectPayload {
                                                        file_id: offer.file_id,
                                                        reason: "正在等待其他确认，请稍后重发"
                                                            .into(),
                                                    },
                                                )
                                                .unwrap_or_default()),
                                            })
                                            .await;
                                        eprintln!(
                                            "{}",
                                            format!(
                                                "已拒收文件 {}（另一确认处理中）",
                                                offer.name
                                            )
                                            .yellow()
                                        );
                                    } else {
                                        pending_confirm = Some(PendingConfirm::FileReceive {
                                            from: offer.from,
                                            file_id: offer.file_id,
                                            name: offer.name.clone(),
                                            size: offer.size,
                                            at: std::time::Instant::now(),
                                            via_window: cfg!(windows)
                                                && mode == ConfirmMode::Interactive,
                                        });
                                        #[cfg(windows)]
                                        if mode == ConfirmMode::Interactive {
                                            spawn_file_window(
                                                confirm_tx.clone(),
                                                offer.from,
                                                offer.file_id,
                                                offer.name,
                                                offer.size,
                                            );
                                        }
                                        #[cfg(not(windows))]
                                        if mode == ConfirmMode::Interactive {
                                            println!(
                                                "{}",
                                                "非 Windows 暂无确认子窗口：请在主窗口输入 y（接收）/ n（拒绝）"
                                                    .yellow()
                                            );
                                        }
                                        // Ask：不发窗口——卡片答案经 InputMsg::Line 走 input 待决路由
                                    }
                                    continue;
                                }
                            }
                            Event::Gossip { source, data } => {
                                let Ok(payload) =
                                    serde_json::from_slice::<GroupPayload>(&data)
                                else {
                                    continue;
                                };
                                let group_id = match &payload {
                                    GroupPayload::Text { group_id, .. } => group_id.clone(),
                                };
                                let Some(g) = groups.get(&group_id) else {
                                    continue;
                                };
                                match payload {
                                    GroupPayload::Text {
                                        group_id,
                                        text,
                                        sender,
                                    } => {
                                        // 本地注册表模型：成员由群主背书（加人时须已验证联系人）。
                                        // 接收端依赖 Signed 签名保证来源真实；显示名用发送者自报，
                                        // 回退到本地方言名/节点ID
                                        let who = if !sender.is_empty() {
                                            sender
                                        } else {
                                            peer_name(&source, &conversations, &identity)
                                        };
                                        let focused =
                                            focused_group.as_deref() == Some(group_id.as_str());
                                        display::incoming_chat(
                                            &who,
                                            &text,
                                            focused,
                                            Some(&g.name),
                                            false,
                                        );
                                    }
                                }
                            }
                            Event::Listening(addr) => {
                                // GUI"我的地址"去重累积（CLI 的监听地址行由 L1 打印照旧）
                                crate::p2p_app::chat::display::listen_addr(addr.to_string());
                            }
                            Event::SendFailure { peer, error } => {
                                trace.log(&format!("ev: send-fail {peer} {error}"));
                                let bye = conversations
                                    .get(&peer)
                                    .map(|c| c.bye)
                                    .unwrap_or(false);
                                if bye || focused != Some(peer) {
                                    eprintln!(
                                        "{}",
                                        format!(
                                            "发送到 {peer} 失败（对方正在退出或已离线）: {error}"
                                        )
                                        .dimmed()
                                    );
                                } else {
                                    eprintln!("{}", format!("发送到 {peer} 失败: {error}").red());
                                }
                            }
                        }
                        // 侧栏快照：任何事件后都可能改变联系人/群/连接状态
                        push_sidebar(
                            &identity, &groups, &connected, &focused, &focused_group, &registered,
                        );
                    }
                    None => {
                        trace.log("ev: closed");
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // parse_dial_addr/群域（dedup/next_creator）单测随迁 dial.rs/group.rs（步1）

    #[test]
    fn sendstrings_parse_count() {
        assert_eq!(parse_line_count(" 3"), Ok(3));
        assert_eq!(parse_line_count("\t5"), Ok(5));
        assert!(parse_line_count("").is_err()); // 缺少行数
        assert!(parse_line_count("  ").is_err());
        assert!(parse_line_count("abc").is_err()); // 非数字
    }

    #[test]
    fn sendstrings_collect_preserves_lines_verbatim() {
        // 收满 N 行后返回完整文本；空行/以 / 开头的内容原样保留
        let mut buf = String::new();
        let mut remaining = 3;
        assert!(collect_multiline(&mut buf, &mut remaining, "/开头行").is_none());
        assert_eq!(remaining, 2);
        assert!(collect_multiline(&mut buf, &mut remaining, "").is_none()); // 空行保留
        assert_eq!(remaining, 1);
        let done = collect_multiline(&mut buf, &mut remaining, "含\"引号行\"").unwrap();
        assert_eq!(done, "/开头行\n\n含\"引号行\"");
        assert_eq!(remaining, 0);
        assert!(buf.is_empty()); // std::mem::take 已清空
    }

    #[test]
    fn sendstrings_single_line() {
        let mut buf = String::new();
        let mut remaining = 1;
        let done = collect_multiline(&mut buf, &mut remaining, "仅一行").unwrap();
        assert_eq!(done, "仅一行");
    }

    #[test]
    fn discovered_views_excludes_known_contacts() {
        use crate::p2p_app::chat::sidebar::discovered_views;
        use crate::uievent::DiscoveredView;
        use std::collections::HashSet;
        // 两个合法节点 ID（BIP39 测试向量派生，互不相同）
        let pa: PeerId = "12D3KooWGpERtoeJ1M482Kkx7p9czC9yKYuXGsvUvDBG3589iPKq"
            .parse()
            .unwrap();
        let pb: PeerId = "12D3KooWPCyWnZCXR3VGdrQjLr5d8TBaAHD956XZvo6xoCXYB5AR"
            .parse()
            .unwrap();
        let mut registered = HashMap::new();
        registered.insert(pa, vec![]);
        registered.insert(pb, vec![]);
        let mut known = HashSet::new();
        known.insert(pa.to_string());
        let mut connected = HashSet::new();
        connected.insert(pb);
        let views: Vec<DiscoveredView> = discovered_views(&registered, &known, &connected);
        // 簿内联系人（pa）不出现；未握手节点（pb）出现且带在线态
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].peer_id, pb.to_string());
        assert!(views[0].online);
    }
}

