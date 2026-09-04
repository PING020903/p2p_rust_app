use colored::Colorize;
use libp2p::{multiaddr::Protocol, Multiaddr, PeerId};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error;
use std::io::IsTerminal;
use std::path::PathBuf;
use tokio::io::AsyncBufReadExt;

use crate::cmd_tree::{CmdError, CmdTree, ROOT};
use crate::p2p::{cache_dir, load_discovery_mode, save_discovery_mode, save_download_dir, DiscoveryMode};
use crate::lineio::{Control, InputMsg, LineSource};
use crate::p2p::identity::LoginOutcome;
use crate::p2p::identity_service::{is_l2_signal, IdentityService, TextTag};
use crate::p2p_app::chat::display;
use crate::p2p::seam::{self, is_global_ipv6_listen, Event, SignalRegistry, BYE_HANDSHAKE_TIMEOUT};

// ---- 语义注册表（L3 应用层）：text=Custom(tag) 承载协议语义，binary 承载负载 ----
//
// 基础语义 Hello/Bye 由 L2（IdentityService）直接在事件分支处理；
// chat 业务注册为自定义语义 tag，按 tag 分发到注册的 handler。
// 新应用（文件传输/固件升级）注册自己的 tag + handler，不动核心。

/// chat 应用注册的自定义语义标签
const TAG_CHAT_TEXT: &str = "chat.text";
const TAG_GROUP_INVITE: &str = "chat.group_invite";
const TAG_GROUP_LEAVE: &str = "chat.group_leave";
const TAG_GROUP_MEMBER_LIST: &str = "chat.group_member_list";
const TAG_GROUP_OWNER_TRANSFER: &str = "chat.group_owner_transfer";

/// chat 业务负载结构（各自 tag 的 binary 负载，cbor 序列化）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatTextPayload {
    text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GroupInvitePayload {
    group_id: String,
    group_name: String,
    version: u64,
    members: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GroupLeavePayload {
    group_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GroupMemberListPayload {
    group_id: String,
    version: u64,
    members: Vec<String>,
}

/// 群主退群时一步顺位转移：携带新群主 + 移除群主后的名单（版本门控整体替换）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GroupOwnerTransferPayload {
    group_id: String,
    new_creator: String,
    version: u64,
    members: Vec<String>,
}

/// 命令 handler 产出的异步动作：同步逻辑跑在指令树 handler 里，真正需要 `.await`
/// 的 I/O（发命令给传输适配层 / 读密码）排进 `ChatCtx.ops`，由主循环统一消费。
/// 这本质是"同步生产者 → 异步消费者"的 ring buffer 解耦。
pub(crate) enum AsyncOp {
    Cmd(seam::Cmd),
    Backup,
    /// 查询本机监听地址并打印（/listen）
    Listen,
}

/// 命令上下文：一次性持有全部可变状态，供指令树 handler 直接读写。
/// 每次解析一行命令前临时构造（借用随本次处理结束释放），quit 置位表示请求退出。
struct ChatCtx<'a> {
    identity: &'a mut IdentityService,
    cmd_tx: &'a tokio::sync::mpsc::Sender<seam::Cmd>,
    input: &'a mut LineSource,
    interactive: bool,
    conversations: &'a mut HashMap<PeerId, Conversation>,
    groups: &'a mut HashMap<String, Group>,
    focused: &'a mut Option<PeerId>,
    focused_group: &'a mut Option<String>,
    connected: &'a HashSet<PeerId>,
    registered: &'a mut HashMap<PeerId, Vec<Multiaddr>>,
    /// 待消费的异步动作队列（VecDeque 即可增长的环状缓冲）
    ops: VecDeque<AsyncOp>,
    quit: bool,
    file: &'a mut crate::file_transfer::FileTransferState,
}
impl<'a> ChatCtx<'a> {
    /// 按名字/节点ID 解析目标 peer：会话名 → 联系人名（L2）→ 直接解析节点ID
    fn resolve(&self, target: &str) -> Option<PeerId> {
        self.conversations
            .iter()
            .find(|(_, c)| c.name == target)
            .map(|(p, _)| *p)
            .or_else(|| self.identity.contact_by_name(target))
            .or_else(|| target.parse::<PeerId>().ok())
    }

    /// 按群名解析群 id
    fn group_id(&self, name: &str) -> Option<String> {
        self.groups
            .iter()
            .find(|(_, g)| g.name == name)
            .map(|(id, _)| id.clone())
    }
}

/// 构造一次性会话上下文（借用随本次处理结束释放；命令/文本/Control 各输入分支共用）
#[allow(clippy::too_many_arguments)]
fn make_chat_ctx<'a>(
    identity: &'a mut IdentityService,
    cmd_tx: &'a tokio::sync::mpsc::Sender<seam::Cmd>,
    input: &'a mut LineSource,
    interactive: bool,
    conversations: &'a mut HashMap<PeerId, Conversation>,
    groups: &'a mut HashMap<String, Group>,
    focused: &'a mut Option<PeerId>,
    focused_group: &'a mut Option<String>,
    connected: &'a HashSet<PeerId>,
    registered: &'a mut HashMap<PeerId, Vec<Multiaddr>>,
    file: &'a mut crate::file_transfer::FileTransferState,
) -> ChatCtx<'a> {
    ChatCtx {
        identity,
        cmd_tx,
        input,
        interactive,
        conversations,
        groups,
        focused,
        focused_group,
        connected,
        registered,
        ops: VecDeque::new(),
        quit: false,
        file,
    }
}

/// 消费 ctx 排队的异步动作（同步生产者 → 异步消费者；命令与 Control 分支共用）
async fn consume_ops(ctx: &mut ChatCtx<'_>) {
    while let Some(op) = ctx.ops.pop_front() {
        match op {
            AsyncOp::Cmd(c) => {
                if let Err(e) = ctx.cmd_tx.send(c).await {
                    eprintln!("{}", format!("命令发送失败: {e}").red());
                }
            }
            AsyncOp::Backup => {
                if let Err(e) = ctx.identity.backup(ctx.input, ctx.interactive).await {
                    eprintln!("{}", format!("备份失败: {e}").red());
                }
            }
            AsyncOp::Listen => {
                let (tx, rx) = tokio::sync::oneshot::channel();
                if ctx.cmd_tx.send(seam::Cmd::GetListenAddr(tx)).await.is_ok() {
                    if let Ok(addrs) = rx.await {
                        print_listen_addrs(&addrs, ctx.identity.my_id());
                    }
                }
            }
        }
    }
}

/// 结构化控制动作（GUI 点击/按钮）处理：复刻对应命令的非文本逻辑。
/// 名字一律 `peer_name()`（会话名 → 联系人名 → 节点ID）；提示行照打（CLI 同款文案）；
/// 指纹信息照打（核对弹窗属弹窗批，本轮按钮与 /trust 同粒度直接执行）。
async fn handle_control(ctx: &mut ChatCtx<'_>, c: Control) {
    match c {
        Control::FocusPeer { peer, name } => {
            *ctx.focused_group = None;
            if ctx.connected.contains(&peer) {
                // 已连接：仅切换焦点
                *ctx.focused = Some(peer);
                let conv_name = ctx
                    .conversations
                    .get(&peer)
                    .map(|c| c.name.clone())
                    .unwrap_or_default();
                let who = if conv_name.is_empty() { name } else { conv_name };
                let badge = trust_badge(
                    ctx.identity.effective_trusted(&peer),
                    ctx.identity.is_verified(&peer),
                );
                println!(
                    "{}",
                    format!("已切换到会话: {who}（{peer}）{badge}").green()
                );
            } else {
                // 未连接：建/复用会话并拨号（或待 mDNS 发现）
                ctx.conversations.entry(peer).or_insert_with(Conversation::new);
                *ctx.focused = Some(peer);
                let conv = ctx.conversations.get_mut(&peer).unwrap();
                if conv.name.is_empty() {
                    conv.name = name.clone();
                }
                match ctx.registered.get(&peer) {
                    Some(addrs) if !addrs.is_empty() => {
                        println!("{}", format!("正在连接 {name}...").cyan());
                        ctx.conversations.get_mut(&peer).unwrap().pending_dial = false;
                        push_cmd(&mut ctx.ops, seam::Cmd::DialPeer(peer));
                    }
                    _ => {
                        ctx.conversations.get_mut(&peer).unwrap().pending_dial = true;
                        println!(
                            "{}",
                            format!("{name} 暂无已知地址，等待 mDNS 发现，发现后自动连接").cyan()
                        );
                    }
                }
            }
        }
        Control::FocusGroup(gname) => {
            match ctx.group_id(&gname) {
                Some(gid) => {
                    *ctx.focused_group = Some(gid.clone());
                    *ctx.focused = None;
                    let g = ctx.groups[&gid].clone();
                    // 聚焦即连：拨号群成员（常驻群维持 mesh，普通群按需连接）
                    dial_group_members(
                        &mut ctx.ops,
                        &g,
                        ctx.identity.my_id(),
                        ctx.connected,
                        ctx.registered,
                    );
                    println!(
                        "{}",
                        format!("已切换到群聊: {}（输入直接发群里）", g.name).green()
                    );
                }
                None => eprintln!("{}", format!("未知群: {gname}").yellow()),
            }
        }
        Control::Trust { peer, trusted } => {
            let name = peer_name(&peer, ctx.conversations, ctx.identity);
            if trusted {
                // D4：信任前展示节点ID + 指纹，供人工复核（信息行进时间线/终端）
                println!("{}", "请核对对方身份:".yellow());
                println!("  节点ID: {peer}");
                println!("  指纹: {}", ctx.identity.fingerprint(&peer).dimmed());
                ctx.identity.trust(&peer, &name, true);
                // 对称信任：通知对方"我已信任你"；离线则静默跳过（重连时 hello 自愈补发）
                if ctx.connected.contains(&peer) {
                    let my_name = ctx.identity.my_name().to_string();
                    let bin = serde_cbor::to_vec(&my_name).unwrap_or_default();
                    push_cmd(
                        &mut ctx.ops,
                        seam::Cmd::Send {
                            peer,
                            tag: TextTag::TrustConfirm.as_str().to_string(),
                            payload: Some(bin),
                        },
                    );
                }
                println!("{}", format!("已信任: {name}").green());
            } else {
                ctx.identity.trust(&peer, &name, false);
                // 对称信任：取消后 D3 需重新生效，清掉本会话的已确认标记
                if let Some(conv) = ctx.conversations.get_mut(&peer) {
                    conv.send_confirmed = false;
                }
                if ctx.connected.contains(&peer) {
                    let my_name = ctx.identity.my_name().to_string();
                    let bin = serde_cbor::to_vec(&my_name).unwrap_or_default();
                    push_cmd(
                        &mut ctx.ops,
                        seam::Cmd::Send {
                            peer,
                            tag: TextTag::TrustRevoke.as_str().to_string(),
                            payload: Some(bin),
                        },
                    );
                }
                println!("{}", format!("已取消信任: {name}").yellow());
            }
        }
    }
}

/// 信任徽标（对称信任）：[互信] 双向信任 / [我信任] 单方已信任 / [未信任] 默认
fn trust_badge(effective: bool, my_verified: bool) -> colored::ColoredString {
    if effective {
        "  [互信]".green()
    } else if my_verified {
        "  [我信任/对方未确认]".yellow()
    } else {
        "  [未信任]".yellow()
    }
}

/// 向命令队列排入"发命令"动作（字段级借用，可在 handler 持有其它字段借用时调用）
fn push_cmd(ops: &mut VecDeque<AsyncOp>, cmd: seam::Cmd) {
    ops.push_back(AsyncOp::Cmd(cmd));
}

/// 终端逃逸：`cmd/<命令>` 走 cmd.exe，`ps/<命令>` 走 PowerShell，`sh/<命令>` 走 POSIX sh。
/// stdout/stderr 继承到真实终端（cls 可真清屏），input 置 null 不与应用抢输入。
async fn run_terminal_escape(program: &str, args: &[&str], rest: &str) {
    let status = tokio::process::Command::new(program)
        .args(args)
        .arg(rest)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .await;
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("{}", format!("命令退出码: {s}").yellow()),
        Err(e) => eprintln!("{}", format!("无法执行 {program}: {e}").yellow()),
    }
}

/// 侧栏快照推送：联系人（信任徽标/在线/焦点）+ 群列表。
/// 推送点：命令处理后（/trust /chat /group 等）与每个传输事件处理后。
/// CLI 无事件通道时 no-op（display::sidebar 内部判定）。
fn push_sidebar(
    identity: &IdentityService,
    groups: &HashMap<String, Group>,
    connected: &HashSet<PeerId>,
    focused: &Option<PeerId>,
    focused_group: &Option<String>,
) {
    use crate::uievent::{ContactView, GroupView};
    let contacts = identity
        .contact_entries()
        .into_iter()
        .filter_map(|e| {
            let peer: PeerId = e.peer_id.parse().ok()?;
            let name = if e.name.is_empty() {
                e.peer_id.chars().take(10).collect()
            } else {
                e.name
            };
            Some(ContactView {
                peer_id: e.peer_id,
                name,
                online: connected.contains(&peer),
                focused: *focused == Some(peer),
                effective_trusted: e.verified && e.their_trust,
                i_trust: e.verified,
            })
        })
        .collect();
    let mut gvs: Vec<GroupView> = groups
        .values()
        .map(|g| GroupView {
            name: g.name.clone(),
            focused: focused_group.as_deref() == Some(g.id.as_str()),
            member_count: g.members.len(),
        })
        .collect();
    gvs.sort_by(|a, b| a.name.cmp(&b.name));
    display::sidebar(contacts, gvs);
}

/// 打印本机可分享地址：全局 IPv6 直连地址（标题一次 + 逐条列出），其余监听地址另列（/listen）
fn print_listen_addrs(addrs: &[Multiaddr], peer_id: &PeerId) {
    let globals: Vec<&Multiaddr> = addrs
        .iter()
        .filter(|a| is_global_ipv6_listen(a))
        .collect();
    if globals.is_empty() {
        println!(
            "{}",
            "本机暂无全局 IPv6 直连地址（跨城市需中继，后续支持；若刚启动可稍后重试 /listen）"
                .yellow()
        );
    } else {
        println!(
            "{}",
            "全局IPv6直连地址（任选一条分享，对方 /dial 即连；需路由器放行该端口）:".cyan()
        );
        for a in globals {
            println!("{}", format!("  {a}/p2p/{peer_id}").cyan());
        }
        println!(
            "{}",
            "（若分享的地址失效，重新 /listen 获取最新）".dimmed()
        );
    }
    let others: Vec<&Multiaddr> = addrs
        .iter()
        .filter(|a| !is_global_ipv6_listen(a))
        .collect();
    if !others.is_empty() {
        println!("{}", "其他监听地址:".dimmed());
        for a in others {
            println!("  {a}/p2p/{peer_id}");
        }
    }
}

/// 事件处理上下文：收到 seam::Event::Signal 时一次性构造，供语义 handler 读写。
/// handler 是 async 的，可直接 await（TOFU 读输入 / 发命令）。
pub(crate) struct AppCtx<'a> {
    identity: &'a mut IdentityService,
    conversations: &'a mut HashMap<PeerId, Conversation>,
    groups: &'a mut HashMap<String, Group>,
    focused: &'a mut Option<PeerId>,    pub(crate) input: &'a mut LineSource,
    pub(crate) interactive: bool,
    pub(crate) cmd_tx: &'a tokio::sync::mpsc::Sender<seam::Cmd>,
    pub(crate) file: &'a mut crate::file_transfer::FileTransferState,
}

/// 让 `AppCtx<'a>` 作为 L2 `SignalRegistry` 的上下文：GAT 暴露其带生命周期的类型
impl seam::SignalCtx for AppCtx<'_> {
    type Ctx<'a> = AppCtx<'a>;
}

// ---- chat 业务语义 handler（注册到 SignalRegistry）----
// ---- 语义 handler（注册到 SignalRegistry）----
//
// L2 存在语义：hello/bye 由 L2 映射（TOFU/联系人簿，默认行为），
// L3 通过钩子分析"谁上线/下线"并反应。chat 业务由 L3 注册 handler。

/// hello（对方上线）：L2 处理存在 + 触发 L3 钩子（解析名字，分析谁上线）
async fn on_peer_hello_signal(ctx: &mut AppCtx<'_>, from: &PeerId, payload: Option<&[u8]>) -> bool {
    let Some(bytes) = payload else {
        return false;
    };
    let Ok(name) = serde_cbor::from_slice::<String>(bytes) else {
        return false;
    };
    // 分离字段借用，让钩子闭包能访问 conversations 而不与 handle_peer_hello 冲突
    let conversations = &mut *ctx.conversations;
    let ok = ctx
        .identity
        .handle_peer_hello(ctx.input, ctx.interactive, from, &name, |peer, name| {
            let conv = conversations.entry(*peer).or_insert_with(Conversation::new);
            conv.name = name.to_string();
            println!("{}", format!("对方已上线: {name}").green());
        })
        .await
        .is_ok();
    // 对称信任自愈：hello 处理完（verified 已定型）后，向对方重报当前信任态，重连后重新同步
    let my_name = ctx.identity.my_name().to_string();
    let my_name_bin = serde_cbor::to_vec(&my_name).unwrap_or_default();
    let trust_tag = if ctx.identity.is_verified(from) {
        TextTag::TrustConfirm.as_str()
    } else {
        TextTag::TrustRevoke.as_str()
    };
    let _ = ctx
        .cmd_tx
        .send(seam::Cmd::Send {
            peer: *from,
            tag: trust_tag.to_string(),
            payload: Some(my_name_bin),
        })
        .await;
    ok
}

/// bye（对方下线）：L2 处理存在 + 触发 L3 钩子（标记会话 + 打印），再发 MarkBye
async fn on_peer_bye_signal(ctx: &mut AppCtx<'_>, from: &PeerId, _payload: Option<&[u8]>) -> bool {
    let conversations = &mut *ctx.conversations;
    let cmd_tx = ctx.cmd_tx;
    ctx.identity.handle_peer_bye(from, |peer| {
        if let Some(conv) = conversations.get_mut(peer) {
            conv.bye = true;
        }
        println!("{}", "对方已正常退出".yellow());
    });
    // L1 策略：标记 bye → 不再心跳、断开后不重连
    let _ = cmd_tx.send(seam::Cmd::MarkBye(*from)).await;
    true
}

/// 展示 chat.text 消息：显示路由分流（CLI 文本 / GUI 结构化事件）。
/// trusted 焦点 `[对方]`、非焦点 `[名字]` 前缀；untrusted 带 `[未信任]` 标记——
/// 前缀规则集中在 display::incoming_chat 与 ChatMessage::to_cli_line。
fn show_chat_text(from: &PeerId, text: &str, conv_name: &str, focused: bool, untrusted: bool) {
    let who = if conv_name.is_empty() {
        from.to_string()
    } else {
        conv_name.to_string()
    };
    display::incoming_chat(&who, text, focused, None, untrusted);
}

async fn on_chat_text(ctx: &mut AppCtx<'_>, from: &PeerId, payload: Option<&[u8]>) -> bool {
    let Some(bytes) = payload else {
        return false;
    };
    let Ok(p) = serde_cbor::from_slice::<ChatTextPayload>(bytes) else {
        return false;
    };
    let conv = ctx
        .conversations
        .entry(*from)
        .or_insert_with(Conversation::new);
    let focused = *ctx.focused == Some(*from);
    show_chat_text(from, &p.text, &conv.name, focused, false);
    true
}

/// 未互信 `chat.text` 钩子（测试专用，经 P2P_E2E_UNTRUSTED_HOOK=1 启用）：
/// 未互信时也显示，带 `[未信任]` 标记。用于验证"未互信处理是每端本地策略"的边界。
async fn display_untrusted_text(ctx: &mut AppCtx<'_>, from: &PeerId, payload: Option<&[u8]>) -> bool {
    let Some(bytes) = payload else {
        return false;
    };
    let Ok(p) = serde_cbor::from_slice::<ChatTextPayload>(bytes) else {
        return false;
    };
    let conv = ctx
        .conversations
        .entry(*from)
        .or_insert_with(Conversation::new);
    let focused = *ctx.focused == Some(*from);
    show_chat_text(from, &p.text, &conv.name, focused, true);
    true
}

/// L2 信任信号处理（trust.confirm=true / trust.revoke=false）：对端告知"我信任你/取消信任你"
async fn on_trust_signal(
    ctx: &mut AppCtx<'_>,
    from: &PeerId,
    payload: Option<&[u8]>,
    trusted: bool,
) -> bool {
    let Some(bytes) = payload else {
        return false;
    };
    let Ok(name) = serde_cbor::from_slice::<String>(bytes) else {
        return false;
    };
    ctx.identity.on_peer_trust_signal(from, &name, trusted);
    if trusted {
        println!("{}", format!("对方已信任你: {name}").green());
    } else {
        println!("{}", format!("对方已取消信任: {name}").yellow());
    }
    true
}

async fn on_group_invite(ctx: &mut AppCtx<'_>, from: &PeerId, payload: Option<&[u8]>) -> bool {
    let Some(bytes) = payload else {
        return false;
    };
    let Ok(p) = serde_cbor::from_slice::<GroupInvitePayload>(bytes) else {
        return false;
    };
    // 群主（邀请者 from）发来的邀请：携带当前版本 + 全量名单，入群即一致。
    // 名单先归一化去重（幽灵/重复防御）
    let mut members = p.members.clone();
    dedup_members(&mut members);
    if !ctx.groups.contains_key(&p.group_id) || ctx.groups[&p.group_id].version < p.version {
        ctx.groups.insert(
            p.group_id.clone(),
            Group {
                id: p.group_id.clone(),
                name: p.group_name.clone(),
                members: members.clone(),
                version: p.version,
                creator: from.to_string(),
                resident: false, // 入群默认非常驻
            },
        );
        let _ = save_groups(ctx.identity.my_id(), &ctx.groups);
        let _ = ctx
            .cmd_tx
            .send(seam::Cmd::Subscribe {
                topic: group_topic(&p.group_id),
            })
            .await;
    }
    let sender = ctx
        .identity
        .contact_name(from)
        .unwrap_or_else(|| from.to_string());
    println!(
        "{}",
        format!(
            "被邀请加入群聊: {}（邀请者 {sender}，成员 {} 人）",
            p.group_name,
            p.members.len()
        )
        .green()
    );
    true
}

async fn on_group_leave(ctx: &mut AppCtx<'_>, from: &PeerId, payload: Option<&[u8]>) -> bool {
    let Some(bytes) = payload else {
        return false;
    };
    let Ok(p) = serde_cbor::from_slice::<GroupLeavePayload>(bytes) else {
        return false;
    };
    // 成员主动退群：校验发送者确为成员，移除并推进版本，向剩余成员扇出
    let is_member = ctx
        .groups
        .get(&p.group_id)
        .map(|g| g.members.iter().any(|m| m == &from.to_string()))
        .unwrap_or(false);
    if !is_member {
        return false;
    }
    if let Some(g) = ctx.groups.get_mut(&p.group_id) {
        g.version += 1;
        g.members.retain(|m| m != &from.to_string());
        dedup_members(&mut g.members);
        let _ = save_groups(ctx.identity.my_id(), &ctx.groups);
        let name = ctx
            .identity
            .contact_name(from)
            .unwrap_or_else(|| from.to_string());
        let g = &ctx.groups[&p.group_id];
        let my_id = ctx.identity.my_id().to_string();
        let remaining: Vec<PeerId> = g
            .members
            .iter()
            .filter(|m| m.as_str() != &my_id)
            .filter_map(|m| m.parse().ok())
            .collect();
        fanout_member_list_async(ctx.cmd_tx, &g.id, g.version, &g.members, &remaining).await;
        println!(
            "{}",
            format!(
                "成员 {name} 已退出群 {}（名单版本 {}）",
                g.name, g.version
            )
            .yellow()
        );
    }
    true
}

async fn on_group_member_list(ctx: &mut AppCtx<'_>, _from: &PeerId, payload: Option<&[u8]>) -> bool {
    let Some(bytes) = payload else {
        return false;
    };
    let Ok(p) = serde_cbor::from_slice::<GroupMemberListPayload>(bytes) else {
        return false;
    };
    // 群主 1v1 扇出名单：版本更高才整体替换（防乱序/重复）。名单先归一化去重
    let mut members = p.members.clone();
    dedup_members(&mut members);
    let newer = ctx
        .groups
        .get(&p.group_id)
        .map(|g| p.version > g.version)
        .unwrap_or(false);
    if newer {
        let gname = ctx
            .groups
            .get(&p.group_id)
            .map(|g| g.name.clone())
            .unwrap_or_default();
        if let Some(g) = ctx.groups.get_mut(&p.group_id) {
            g.version = p.version;
            g.members = members.clone();
        }
        let _ = save_groups(ctx.identity.my_id(), &ctx.groups);
        println!(
            "{}",
            format!(
                "群 {gname} 成员名单已更新（版本 {}，{} 人）",
                p.version,
                p.members.len()
            )
            .dimmed()
        );
    }
    true
}

async fn on_group_owner_transfer(ctx: &mut AppCtx<'_>, from: &PeerId, payload: Option<&[u8]>) -> bool {
    let Some(bytes) = payload else {
        return false;
    };
    let Ok(p) = serde_cbor::from_slice::<GroupOwnerTransferPayload>(bytes) else {
        return false;
    };
    // 群主退群顺位转移：版本更高才整体替换。门控放宽为"from 是群成员"——
    // 漏收中间转移的节点收到任一后续转移即可自愈，creator 不再永久错位。
    let mut members = p.members.clone();
    dedup_members(&mut members);
    let from_is_member = members.iter().any(|m| m == &from.to_string())
        || ctx
            .groups
            .get(&p.group_id)
            .map(|g| g.members.iter().any(|m| m == &from.to_string()))
            .unwrap_or(false);
    let new_in_list = members.iter().any(|m| m == &p.new_creator);
    let newer = ctx
        .groups
        .get(&p.group_id)
        .map(|g| p.version > g.version)
        .unwrap_or(false);
    if from_is_member && new_in_list && newer {
        let was_creator_of = ctx
            .groups
            .get(&p.group_id)
            .map(|g| g.name.clone())
            .unwrap_or_default();
        let new_is_me = p.new_creator == ctx.identity.my_id().to_string();
        if let Some(g) = ctx.groups.get_mut(&p.group_id) {
            g.version = p.version;
            g.creator = p.new_creator.clone();
            g.members = members.clone();
        }
        let _ = save_groups(ctx.identity.my_id(), &ctx.groups);
        if new_is_me {
            println!(
                "{}",
                format!(
                    "群 {was_creator_of} 的群主已转移给你，你已成为群主（可 /group add 邀请）"
                )
                .green()
            );
        } else {
            let nc_name = ctx
                .identity
                .contact_name(&p.new_creator.parse().unwrap_or(*from))
                .unwrap_or_else(|| p.new_creator.clone());
            println!(
                "{}",
                format!(
                    "群 {was_creator_of} 群主已顺位转移给 {nc_name}（名单版本 {}，{} 人）",
                    p.version,
                    p.members.len()
                )
                .dimmed()
            );
        }
    }
    true
}

/// 一个 1v1 会话：与某 peer 的聊天上下文（连接可多路共存）
struct Conversation {
    name: String,       // 对方角色名（Hello 更新；未知为空）
    greeted: bool,      // 是否已发过 Hello（重连后重置，避免漏问候）
    bye: bool,          // 对方已主动退出（不再心跳/重连）
    pending_dial: bool, // /chat 后尚无地址，等待 mDNS 发现自动拨号
    send_confirmed: bool, // 未信任联系人首次发消息是否已确认（D3）
}

impl Conversation {
    fn new() -> Self {
        Conversation {
            name: String::new(),
            greeted: false,
            bye: false,
            pending_dial: false,
            send_confirmed: false,
        }
    }
}

/// 群：本地注册表（id/name/members）。**群主为中心**的单一权威模型：
/// 群主（creator）是成员表唯一权威——仅群主可邀请新成员、处理成员退群；
/// 每次成员变更版本 +1，并向最新名单所有成员 1v1 扇出全量名单（版本化整体替换）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Group {
    id: String,
    name: String,
    members: Vec<String>, // peer_id 字符串
    #[serde(default)]
    version: u64, // 成员变更计数，仅接受更高版本
    #[serde(default)]
    creator: String, // 群主 peer_id（唯一权威）
    /// 常驻接收（per-node 本地偏好，不随名单传播）：常驻群成员上线自动拨号维持 mesh，
    /// 普通群只在聚焦时按需连接（防"所有群都 mesh"的通讯风暴）
    #[serde(default)]
    resident: bool,
}

/// 群消息载荷（gossipsub data，JSON 编码）。
/// 群文本经 gossipsub 分发；成员名单由群主 1v1 扇出（见 GroupMemberList），不走 gossip
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum GroupPayload {
    Text {
        group_id: String,
        text: String,
        /// 发送者自己的显示名（Signed 签名保证来源真实，名字是展示元数据）
        sender: String,
    },
}

/// 群 topic 字符串（L1 不解释 topic，直接透传；订阅/发布/接收须用同一格式）
fn group_topic(group_id: &str) -> String {
    format!("/group/{group_id}/v1")
}

/// 群主向目标成员 1v1 扇出名单更新（版本化整体替换）：同步排入命令队列，主循环统一消费
fn fanout_member_list(
    ops: &mut VecDeque<AsyncOp>,
    group_id: &str,
    version: u64,
    members: &[String],
    targets: &[PeerId],
) {
    let payload = serde_cbor::to_vec(&GroupMemberListPayload {
        group_id: group_id.to_string(),
        version,
        members: members.to_vec(),
    })
    .unwrap_or_default();
    for p in targets {
        ops.push_back(AsyncOp::Cmd(seam::Cmd::Send {
            peer: *p,
            tag: TAG_GROUP_MEMBER_LIST.to_string(),
            payload: Some(payload.clone()),
        }));
    }
}

/// 事件 handler（async）用的异步扇出：直接 await cmd_tx
async fn fanout_member_list_async(
    cmd_tx: &tokio::sync::mpsc::Sender<seam::Cmd>,
    group_id: &str,
    version: u64,
    members: &[String],
    targets: &[PeerId],
) {
    let payload = serde_cbor::to_vec(&GroupMemberListPayload {
        group_id: group_id.to_string(),
        version,
        members: members.to_vec(),
    })
    .unwrap_or_default();
    for p in targets {
        let _ = cmd_tx
            .send(seam::Cmd::Send {
                peer: *p,
                tag: TAG_GROUP_MEMBER_LIST.to_string(),
                payload: Some(payload.clone()),
            })
            .await;
    }
}

/// 拨号群成员（跳过自己/已连接/无已知地址）：常驻群保持 mesh 与聚焦群按需连接的共用入口
fn dial_group_members(
    ops: &mut VecDeque<AsyncOp>,
    g: &Group,
    my_id: &PeerId,
    connected: &HashSet<PeerId>,
    registered: &HashMap<PeerId, Vec<Multiaddr>>,
) {
    let my_id_str = my_id.to_string();
    for m in &g.members {
        if m == &my_id_str {
            continue;
        }
        let Ok(pid) = m.parse::<PeerId>() else {
            continue;
        };
        if connected.contains(&pid) {
            continue;
        }
        if registered.get(&pid).map(|a| !a.is_empty()).unwrap_or(false) {
            ops.push_back(AsyncOp::Cmd(seam::Cmd::DialPeer(pid)));
        }
    }
}

fn groups_path(my_peer_id: &PeerId) -> PathBuf {
    let dir = cache_dir().unwrap_or_else(|_| PathBuf::from("."));
    dir.join(format!("groups_{my_peer_id}.json"))
}

fn load_groups(my_peer_id: &PeerId) -> HashMap<String, Group> {
    let path = groups_path(my_peer_id);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<Group>>(&s).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|mut g| {
            dedup_members(&mut g.members);
            (g.id.clone(), g)
        })
        .collect()
}

fn save_groups(my_peer_id: &PeerId, groups: &HashMap<String, Group>) -> Result<(), String> {
    let path = groups_path(my_peer_id);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建群目录失败: {e}"))?;
    }
    let list: Vec<&Group> = groups.values().collect();
    let s = serde_json::to_string_pretty(&list).map_err(|e| format!("群序列化失败: {e}"))?;
    std::fs::write(&path, s).map_err(|e| format!("写入群注册表失败: {e}"))
}

/// 保序去重成员名单（幽灵/重复防御：加载、接收名单、处理退群后统一归一化）
fn dedup_members(members: &mut Vec<String>) {
    let mut seen: HashSet<String> = HashSet::new();
    members.retain(|m| seen.insert(m.clone()));
}

/// 群主顺位转移的"下一位"：members 数组里群主之后的下一个成员；
/// 群主在末尾时回卷取第一个非群主成员；名单只有群主返回 None（解散）
fn next_creator(members: &[String], creator: &str) -> Option<String> {
    let pos = members.iter().position(|m| m == creator)?;
    members[pos + 1..]
        .iter()
        .find(|m| *m != creator)
        .or_else(|| members[..pos].iter().find(|m| *m != creator))
        .cloned()
}

/// 由 peer 解析显示名：先查 1v1 会话名，再查 L2 联系人，兜底完整节点ID
fn peer_name(
    peer: &PeerId,
    conversations: &HashMap<PeerId, Conversation>,
    identity: &IdentityService,
) -> String {
    if let Some(c) = conversations.get(peer) {
        if !c.name.is_empty() {
            return c.name.clone();
        }
    }
    if let Some(n) = identity.contact_name(peer) {
        return n;
    }
    peer.to_string()
}

/// 群主标签：`群主 {昵称} ({peerID})`（昵称用 peer_name 解析；解析失败直接显 raw id）
fn group_owner_label(
    g: &Group,
    conversations: &HashMap<PeerId, Conversation>,
    identity: &IdentityService,
) -> String {
    match g.creator.parse::<PeerId>() {
        Ok(owner) => format!("群主 {} ({owner})", peer_name(&owner, conversations, identity)),
        Err(_) => format!("群主 {}", g.creator),
    }
}

fn print_dial_template() {
    println!("{}", "地址格式:".yellow());
    println!("  /ip4/<IPv4地址>/tcp/<端口>/p2p/<节点ID>");
    println!("  /ip6/<IPv6地址>/tcp/<端口>/p2p/<节点ID>");
    println!("{}", "有效性规则:".yellow());
    println!("  <IPv4地址> 点分十进制 4 段，每段 0-255，如 192.168.31.10");
    println!("  <端口>     对方监听的端口号，0-65535");
    println!("  <节点ID>   12D3KooW 开头的串，代表对方节点身份");
    println!(
        "{}",
        "提示: 直接粘贴对方启动时打印的\"监听地址\"整行即可".dimmed()
    );
}

fn parse_dial_addr(input: &str) -> Result<Multiaddr, String> {
    let mut s = input.trim();
    for prefix in ["监听地址:", "监听地址："] {
        if let Some(stripped) = s.strip_prefix(prefix) {
            s = stripped.trim();
        }
    }
    if !s.starts_with('/') {
        return Err("地址须以 / 开头，格式: /ip4/<IPv4地址>/tcp/<端口>/p2p/<节点ID>".into());
    }
    let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();

    match parts.first() {
        Some(&"ip4") => {
            let ip = parts.get(1).ok_or("缺少 IP 地址: /ip4/ 后应跟 IPv4 地址")?;
            ip.parse::<std::net::Ipv4Addr>().map_err(|_| {
                format!("IPv4 地址无效: {ip}（应为 4 段点分十进制，每段 0-255）")
            })?;
        }
        Some(&"ip6") => {
            let ip = parts.get(1).ok_or("缺少 IP 地址: /ip6/ 后应跟 IPv6 地址")?;
            ip.parse::<std::net::Ipv6Addr>()
                .map_err(|_| format!("IPv6 地址无效: {ip}"))?;
        }
        Some(other) => {
            return Err(format!("地址须以 /ip4/ 或 /ip6/ 开头，当前是 /{other}/"))
        }
        None => return Err("地址为空".into()),
    }

    let tcp_pos = parts
        .iter()
        .position(|&p| p == "tcp")
        .ok_or("缺少 /tcp/<端口> 部分（如 .../tcp/12082/...）")?;
    let port_str = parts.get(tcp_pos + 1).ok_or("/tcp/ 后缺少端口号")?;
    port_str
        .parse::<u16>()
        .map_err(|_| format!("端口须为 0-65535 的数字，当前: {port_str}"))?;

    let p2p_pos = parts.iter().position(|&p| p == "p2p").ok_or(
        "缺少 /p2p/<节点ID> 部分（节点ID 在对方的监听地址里，12D3KooW 开头）",
    )?;
    let peer_str = parts.get(p2p_pos + 1).ok_or("/p2p/ 后缺少节点ID")?;
    peer_str
        .parse::<PeerId>()
        .map_err(|_| format!("节点ID无效: {peer_str}（应以 12D3KooW 开头）"))?;

    s.parse::<Multiaddr>().map_err(|e| format!("地址整体解析失败: {e}"))
}

fn build_tree<'a>() -> CmdTree<ChatCtx<'a>> {
    let mut tree: CmdTree<ChatCtx<'a>> = CmdTree::new();
    let dial = tree.register(ROOT, "dial", |ctx, args| {
        if args.is_empty() {
            print_dial_template();
            return;
        }
        let raw = args.join(" ");
        match parse_dial_addr(&raw) {
            Ok(ma) => {
                let target = ma.iter().find_map(|p| match p {
                    Protocol::P2p(pid) => Some(pid),
                    _ => None,
                });
                if let Some(p) = target {
                    let recorded = ctx.registered.entry(p).or_default();
                    if !recorded.contains(&ma) {
                        recorded.push(ma.clone());
                    }
                }
                push_cmd(&mut ctx.ops, seam::Cmd::Dial { addr: ma });
            }
            Err(reason) => {
                eprintln!("{}", format!("地址无效: {reason}").red());
                print_dial_template();
            }
        }
    });
    tree.set_help(dial, "连接对方节点，参数为对方的监听地址");
    let chat = tree.register(ROOT, "chat", |ctx, args| {
        if args.is_empty() {
            eprintln!(
                "{}",
                "用法: /chat <完整角色名 或 完整节点ID>（/list 查看已登记节点）".yellow()
            );
            return;
        }
        let target = args.join(" ");
        match ctx.resolve(&target) {
            Some(p) => {
                *ctx.focused_group = None;
                if ctx.connected.contains(&p) {
                    // 已连接：仅切换焦点
                    *ctx.focused = Some(p);
                    let name = ctx
                        .conversations
                        .get(&p)
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    let who = if name.is_empty() {
                        target.as_str()
                    } else {
                        name.as_str()
                    };
                    let badge = trust_badge(
                        ctx.identity.effective_trusted(&p),
                        ctx.identity.is_verified(&p),
                    );
                    println!(
                        "{}",
                        format!("已切换到会话: {who}（{p}）{badge}").green()
                    );
                } else {
                    // 未连接：建/复用会话并拨号（或待接）
                    ctx.conversations.entry(p).or_insert_with(Conversation::new);
                    *ctx.focused = Some(p);
                    let name = ctx.conversations[&p].name.clone();
                    if name.is_empty() {
                        ctx.conversations.get_mut(&p).unwrap().name = target.to_string();
                    }
                    match ctx.registered.get(&p) {
                        Some(addrs) if !addrs.is_empty() => {
                            println!("{}", format!("正在连接 {target}...").cyan());
                            ctx.conversations.get_mut(&p).unwrap().pending_dial = false;
                            push_cmd(&mut ctx.ops, seam::Cmd::DialPeer(p));
                        }
                        _ => {
                            ctx.conversations.get_mut(&p).unwrap().pending_dial = true;
                            println!(
                                "{}",
                                "该节点暂无已知地址，等待 mDNS 发现，发现后自动连接".cyan()
                            );
                        }
                    }
                }
            }
            None => eprintln!(
                "{}",
                format!(
                    "未知角色: {target}（须为完整角色名或完整节点ID，/list 查看）"
                )
                .yellow()
            ),
        }
    });
    tree.set_help(chat, "按完整角色名或完整节点ID发起 1v1 聊天");
    let list = tree.register(ROOT, "list", |ctx, _| {
        if ctx.registered.is_empty() {
            println!(
                "{}",
                "暂无已登记节点（等待 mDNS 发现或用 /dial 直连）".dimmed()
            );
        } else {
            println!("{}", "=== 已登记节点 ===".cyan());
            let mut entries: Vec<(String, &PeerId, usize)> = ctx
                .registered
                .iter()
                .map(|(p, addrs)| (p.to_string(), p, addrs.len()))
                .collect();
            entries.sort();
            for (id_str, p, addr_n) in entries {
                let pname = peer_name(p, ctx.conversations, ctx.identity);
                let who = if pname == p.to_string() {
                    "未知".to_string()
                } else {
                    pname
                };
                let state = if *ctx.focused == Some(*p) {
                    "当前会话"
                } else if ctx.connected.contains(p) {
                    "已连接"
                } else {
                    "离线"
                };
                let trust_badge = if ctx.identity.effective_trusted(p) {
                    "互信".green()
                } else if ctx.identity.is_verified(p) {
                    "我信任/对方未确认".yellow()
                } else {
                    "未信任".yellow()
                };
                println!(
                    "  {who}  {id_str}  [{}]  [{state}]  地址数 {addr_n}",
                    trust_badge
                );
            }
        }
        if !ctx.groups.is_empty() {
            println!("{}", "=== 群聊 ===".cyan());
            for g in ctx.groups.values() {
                let n = g.members.len();
                let focus = if ctx.focused_group.as_deref() == Some(g.id.as_str()) {
                    "  ← 当前群聊".green()
                } else {
                    "".dimmed()
                };
                let resident = if g.resident {
                    " [常驻]".green()
                } else {
                    "".dimmed()
                };
                let owner = group_owner_label(g, ctx.conversations, ctx.identity);
                println!(
                    "  {}（{} 人，名单版本 {}，群ID {}，{owner}）{resident}{focus}",
                    g.name, n, g.version, g.id
                );
            }
        }
    });
    tree.set_help(list, "列出已登记节点与状态");
    let quit = tree.register(ROOT, "quit", |ctx, _| {
        let peers: Vec<PeerId> = ctx
            .conversations
            .iter()
            .filter(|(p, c)| ctx.connected.contains(p) && !c.bye)
            .map(|(p, _)| *p)
            .collect();
        for p in peers {
            push_cmd(
                &mut ctx.ops,
                seam::Cmd::Send {
                    peer: p,
                    tag: TextTag::Bye.as_str().to_string(),
                    payload: None,
                },
            );
            println!("{}", format!("正在通知对方下线: {p}...").dimmed());
        }
        ctx.quit = true;
    });
    tree.set_help(quit, "退出聊天");
    let q = tree.register(ROOT, "q", |ctx, _| {
        let peers: Vec<PeerId> = ctx
            .conversations
            .iter()
            .filter(|(p, c)| ctx.connected.contains(p) && !c.bye)
            .map(|(p, _)| *p)
            .collect();
        for p in peers {
            push_cmd(
                &mut ctx.ops,
                seam::Cmd::Send {
                    peer: p,
                    tag: TextTag::Bye.as_str().to_string(),
                    payload: None,
                },
            );
            println!("{}", format!("正在通知对方下线: {p}...").dimmed());
        }
        ctx.quit = true;
    });
    tree.set_help(q, "退出聊天");
    let help = tree.register(ROOT, "help", |_, _| {});
    tree.set_help(
        help,
        "显示本帮助；/sendStrings <行数> 发送多行文本（随后输入恰好 N 行，内容不解析）；cmd/<命令>（cmd）、ps/<命令>（PowerShell）、sh/<命令>（POSIX sh）可透传给终端执行（如 cmd/cls 或 sh/clear 清屏）",
    );
    let backup = tree.register(ROOT, "backup", |ctx, _| {
        ctx.ops.push_back(AsyncOp::Backup);
    });
    tree.set_help(backup, "重新查看本身份助记词（需输入密码）");
    let trust = tree.register(ROOT, "trust", |ctx, args| {
        if args.is_empty() {
            eprintln!(
                "{}",
                "用法: /trust <角色名 或 节点ID>（加 ! 前缀取消信任）".yellow()
            );
            return;
        }
        let target = args.join(" ");
        let (untrust, target) = match target.strip_prefix('!') {
            Some(stripped) => (true, stripped.to_string()),
            None => (false, target),
        };
        match ctx.resolve(&target) {
            Some(p) => {
                // 名字统一走 peer_name（会话名 → 联系人名 → 节点ID），避免"未知"
                let name = peer_name(&p, ctx.conversations, ctx.identity);
                if untrust {
                    ctx.identity.trust(&p, &name, false);
                    // 对称信任：取消后 D3 需重新生效，清掉本会话的已确认标记
                    if let Some(conv) = ctx.conversations.get_mut(&p) {
                        conv.send_confirmed = false;
                    }
                    // 通知对方"我取消了对你的信任"；对方离线则静默跳过（重连时 hello 自愈补发）
                    if ctx.connected.contains(&p) {
                        let my_name = ctx.identity.my_name().to_string();
                        let bin = serde_cbor::to_vec(&my_name).unwrap_or_default();
                        ctx.ops.push_back(AsyncOp::Cmd(seam::Cmd::Send {
                            peer: p,
                            tag: TextTag::TrustRevoke.as_str().to_string(),
                            payload: Some(bin),
                        }));
                    }
                    println!("{}", format!("已取消信任: {name}").yellow());
                } else {
                    // D4：信任前展示节点ID + 指纹，供人工复核（允许重名时核对）
                    println!("{}", "请核对对方身份:".yellow());
                    println!("  节点ID: {p}");
                    println!("  指纹: {}", ctx.identity.fingerprint(&p).dimmed());
                    ctx.identity.trust(&p, &name, true);
                    // 对称信任：通知对方"我已信任你"；对方离线则静默跳过（重连时 hello 自愈补发）
                    if ctx.connected.contains(&p) {
                        let my_name = ctx.identity.my_name().to_string();
                        let bin = serde_cbor::to_vec(&my_name).unwrap_or_default();
                        ctx.ops.push_back(AsyncOp::Cmd(seam::Cmd::Send {
                            peer: p,
                            tag: TextTag::TrustConfirm.as_str().to_string(),
                            payload: Some(bin),
                        }));
                    }
                    println!("{}", format!("已信任: {name}").green());
                }
            }
            None => eprintln!("{}", "未知节点，无法标记信任（用 /list 查看）".yellow()),
        }
    });
    tree.set_help(trust, "标记/取消信任联系人（! 前缀取消；对称信任：双方 /trust 后才互信可收发消息）");
    let send = tree.register(ROOT, "send", |ctx, args| {
        if args.len() < 2 {
            eprintln!(
                "{}",
                "用法: /send <角色|节点ID> <文件路径>（须为已信任联系人）".yellow()
            );
            return;
        }
        let target = args[0].to_string();
        let path_str = args[1..].join(" ");
        match ctx.resolve(&target) {
            Some(peer) => {
                if !ctx.identity.effective_trusted(&peer) {
                    eprintln!(
                        "{}",
                        format!("{target} 尚未互信（需双方 /trust），文件传输被拒绝").yellow()
                    );
                    return;
                }
                let path = std::path::PathBuf::from(&path_str);
                if !path.exists() {
                    eprintln!("{}", format!("文件不存在: {path_str}").yellow());
                    return;
                }
                if let Err(e) =
                    crate::file_transfer::start_send(ctx.file, &mut ctx.ops, peer, &path)
                {
                    eprintln!("{}", format!("发送启动失败: {e}").yellow());
                }
            }
            None => eprintln!(
                "{}",
                format!("未知角色: {target}（须为完整角色名或完整节点ID）").yellow()
            ),
        }
    });
    tree.set_help(send, "发送文件给已信任联系人：/send <角色|节点ID> <路径>");
    let discover = tree.register(ROOT, "discover", |ctx, args| {
        let mode = match args.first() {
            Some(m) => match DiscoveryMode::parse(m) {
                Some(v) => v,
                None => {
                    eprintln!(
                        "{}",
                        "发现模式须为 advertise / stealth / off".yellow()
                    );
                    return;
                }
            },
            None => {
                eprintln!("{}", "用法: /discover <advertise|stealth|off>".yellow());
                return;
            }
        };
        match save_discovery_mode(ctx.identity.my_id(), mode) {
            Ok(()) => println!(
                "{}",
                format!(
                    "发现模式已设为 {}（下次进入聊天生效）",
                    mode.name()
                )
                .green()
            ),
            Err(e) => {
                eprintln!("{}", format!("保存失败: {e}").yellow())
            }
        }
    });
    tree.set_help(discover, "设置 mDNS 发现模式（下次进入聊天生效）");
    let download_dir = tree.register(ROOT, "download-dir", |ctx, args| {
        match args.first() {
            Some(path) => {
                match save_download_dir(ctx.identity.my_id(), path) {
                    Ok(()) => println!(
                        "{}",
                        format!("下载目录已设为 {}（下次进入聊天生效）", path).green()
                    ),
                    Err(e) => {
                        eprintln!("{}", format!("保存失败: {e}").yellow())
                    }
                }
            }
            None => {
                println!(
                    "{}",
                    format!("当前下载目录: {}", ctx.file.downloads_dir().display()).dimmed()
                );
            }
        }
    });
    tree.set_help(download_dir, "设置文件下载目录（缺省为下载到用户 Downloads，/download-dir <路径> 配置）");
    let listen = tree.register(ROOT, "listen", |ctx, _| {
        ctx.ops.push_back(AsyncOp::Listen);
    });
    tree.set_help(listen, "重新打印本机可分享的直连地址（IPv6 前缀变化后可重新获取）");
    // group 树：`/group <群名>` 聚焦由 group 节点处理，子命令注册为子节点（指令树最深命中）
    let group = tree.register(ROOT, "group", |ctx, args| {
        match args.first() {
            Some(name) => match ctx.group_id(name) {
                Some(gid) => {
                    *ctx.focused_group = Some(gid.clone());
                    *ctx.focused = None;
                    let g = ctx.groups[&gid].clone();
                    let gname = g.name.clone();
                    // 聚焦即连：拨号群成员（常驻群维持 mesh，普通群按需连接）
                    dial_group_members(
                        &mut ctx.ops,
                        &g,
                        ctx.identity.my_id(),
                        ctx.connected,
                        ctx.registered,
                    );
                    println!(
                        "{}",
                        format!("已切换到群聊: {gname}（输入直接发群里）").green()
                    );
                }
                None => eprintln!(
                    "{}",
                    format!("未知群: {name}（/group list 查看）").yellow()
                ),
            },
            None => eprintln!(
                "{}",
                "群聊: /group new <群名> 建群 | /group add <群名> <角色|节点ID> 加人(仅群主) | /group resident <群名> on|off 常驻接收 | /group leave <群名> 退群 | /group list 列群 | /group <群名> 聚焦".yellow()
            ),
        }
    });
    tree.set_help(group, "聚焦群聊（/group <群名>）；子命令 new/add/resident/leave/list");
    let g_new = tree.register(group, "new", |ctx, args| {
        match args.first() {
            Some(name) if !name.is_empty() => {
                if ctx.groups.values().any(|g| g.name == *name) {
                    eprintln!("{}", format!("已存在同名群: {name}").yellow());
                } else {
                    let id = format!("{:08x}", OsRng.next_u32());
                    let creator = ctx.identity.my_id().to_string();
                    ctx.groups.insert(
                        id.clone(),
                        Group {
                            id: id.clone(),
                            name: name.to_string(),
                            members: vec![creator.clone()],
                            version: 0,
                            creator: creator.clone(),
                            resident: false, // 默认非常驻，用户显式 /group resident on
                        },
                    );
                    push_cmd(
                        &mut ctx.ops,
                        seam::Cmd::Subscribe {
                            topic: group_topic(&id),
                        },
                    );
                    let _ = save_groups(ctx.identity.my_id(), &ctx.groups);
                    *ctx.focused_group = Some(id.clone());
                    *ctx.focused = None;
                    println!(
                        "{}",
                        format!("已创建并聚焦群聊: {name}（群ID {id}，你是群主）").green()
                    );
                }
            }
            _ => eprintln!("{}", "用法: /group new <群名>".yellow()),
        }
    });
    tree.set_help(g_new, "建群");
    let g_add = tree.register(group, "add", |ctx, args| {
        let (group, target) = match (args.first(), args.get(1)) {
            (Some(g), Some(t)) => (g.to_string(), t.to_string()),
            _ => {
                eprintln!("{}", "用法: /group add <群名> <角色|节点ID>（仅群主）".yellow());
                return;
            }
        };
        match ctx.group_id(&group) {
            Some(gid) => {
                let my_id = ctx.identity.my_id().to_string();
                if ctx.groups[&gid].creator != my_id {
                    eprintln!("{}", "仅群主可邀请新成员".yellow());
                    return;
                }
                match ctx.resolve(&target) {
                    Some(p) => {
                        if !ctx.identity.is_verified(&p) {
                            eprintln!(
                                "{}",
                                format!("{target} 尚未验证，请先 /trust {target}").yellow()
                            );
                        } else if ctx.groups[&gid].members.contains(&p.to_string()) {
                            // 已在名单中：仍重发邀请——对方 cache 可能被意外清理（群记录/topic 丢失），
                            // 重发让其重新入群+订阅；cache 完好者收等版本邀请无副作用（版本相等不重插）
                            let g = &ctx.groups[&gid];
                            let invite = serde_cbor::to_vec(&GroupInvitePayload {
                                group_id: g.id.clone(),
                                group_name: g.name.clone(),
                                version: g.version,
                                members: g.members.clone(),
                            })
                            .unwrap_or_default();
                            push_cmd(
                                &mut ctx.ops,
                                seam::Cmd::Send {
                                    peer: p,
                                    tag: TAG_GROUP_INVITE.to_string(),
                                    payload: Some(invite),
                                },
                            );
                            println!(
                                "{}",
                                format!("{target} 已在群 {group} 中，已重发邀请确认对方同步").dimmed()
                            );
                        } else {
                            let name = peer_name(&p, ctx.conversations, ctx.identity);
                            ctx.groups.get_mut(&gid).unwrap().version += 1;
                            ctx.groups.get_mut(&gid).unwrap().members.push(p.to_string());
                            let _ = save_groups(ctx.identity.my_id(), &ctx.groups);
                            // 邀请新成员（携带当前版本 + 全量名单，入群即一致）
                            let g = &ctx.groups[&gid];
                            let invite = serde_cbor::to_vec(&GroupInvitePayload {
                                group_id: g.id.clone(),
                                group_name: g.name.clone(),
                                version: g.version,
                                members: g.members.clone(),
                            })
                            .unwrap_or_default();
                            push_cmd(
                                &mut ctx.ops,
                                seam::Cmd::Send {
                                    peer: p,
                                    tag: TAG_GROUP_INVITE.to_string(),
                                    payload: Some(invite),
                                },
                            );
                            // 向其余成员（不含新人、不含自己）1v1 扇出名单更新
                            let g = &ctx.groups[&gid];
                            let others: Vec<PeerId> = g
                                .members
                                .iter()
                                .filter(|m| {
                                    m.as_str() != &p.to_string() && m.as_str() != &my_id
                                })
                                .filter_map(|m| m.parse().ok())
                                .collect();
                            fanout_member_list(
                                &mut ctx.ops,
                                &g.id,
                                g.version,
                                &g.members,
                                &others,
                            );
                            println!(
                                "{}",
                                format!(
                                    "已将 {name} 加入群 {group}（名单版本 {}）",
                                    g.version
                                )
                                .green()
                            );
                        }
                    }
                    None => eprintln!(
                        "{}",
                        format!("未知成员: {target}（须为已连接的角色名或节点ID）").yellow()
                    ),
                }
            }
            None => eprintln!("{}", format!("未知群: {group}（/group list 查看）").yellow()),
        }
    });
    tree.set_help(g_add, "加人（仅群主）");
    let g_resident = tree.register(group, "resident", |ctx, args| {
        let (group, enable) = match (args.first(), args.get(1)) {
            (Some(g), Some(&"on")) => (g.to_string(), true),
            (Some(g), Some(&"off")) => (g.to_string(), false),
            _ => {
                eprintln!("{}", "用法: /group resident <群名> on|off".yellow());
                return;
            }
        };
        match ctx.group_id(&group) {
            Some(gid) => {
                ctx.groups.get_mut(&gid).unwrap().resident = enable;
                let _ = save_groups(ctx.identity.my_id(), &ctx.groups);
                let name = ctx.groups[&gid].name.clone();
                if enable {
                    // 标记常驻：立即补连成员（上线后也会自动拨号）
                    let g = ctx.groups[&gid].clone();
                    dial_group_members(
                        &mut ctx.ops,
                        &g,
                        ctx.identity.my_id(),
                        ctx.connected,
                        ctx.registered,
                    );
                }
                println!(
                    "{}",
                    format!(
                        "群 {name} 已设为{}常驻（成员上线自动连接维持接收）",
                        if enable { "" } else { "非" }
                    )
                    .green()
                );
            }
            None => eprintln!("{}", format!("未知群: {group}（/group list 查看）").yellow()),
        }
    });
    tree.set_help(g_resident, "常驻接收 on/off（防通讯风暴）");
    let g_leave = tree.register(group, "leave", |ctx, args| {
        let group = match args.first() {
            Some(name) => name.to_string(),
            None => {
                eprintln!("{}", "用法: /group leave <群名>".yellow());
                return;
            }
        };
        match ctx.group_id(&group) {
            Some(gid) => {
                let creator: PeerId = match ctx.groups[&gid].creator.parse() {
                    Ok(c) => c,
                    Err(_) => {
                        eprintln!("{}", "该群缺少群主信息，无法退群".yellow());
                        return;
                    }
                };
                let my_id = *ctx.identity.my_id();
                if my_id == creator {
                    // 群主退群：一步顺位转移（名单 >1）或解散（仅自己）
                    let members = ctx.groups[&gid].members.clone();
                    if members.len() > 1 {
                        let new_creator =
                            match next_creator(&members, &creator.to_string()) {
                                Some(nc) => nc,
                                None => {
                                    eprintln!("{}", "无法确定继任群主，退群失败".yellow());
                                    return;
                                }
                            };
                        // 本地：换新群主、移除自己、版本 +1
                        {
                            let g = ctx.groups.get_mut(&gid).unwrap();
                            g.version += 1;
                            g.creator = new_creator.clone();
                            g.members.retain(|m| m != &creator.to_string());
                            dedup_members(&mut g.members);
                        }
                        let _ = save_groups(ctx.identity.my_id(), &ctx.groups);
                        // 1v1 扇出 GroupOwnerTransfer 给剩余成员（新名单 + 新群主）
                        let g = &ctx.groups[&gid];
                        let payload = serde_cbor::to_vec(&GroupOwnerTransferPayload {
                            group_id: g.id.clone(),
                            new_creator: new_creator.clone(),
                            version: g.version,
                            members: g.members.clone(),
                        })
                        .unwrap_or_default();
                        let targets: Vec<PeerId> = g
                            .members
                            .iter()
                            .filter_map(|m| m.parse().ok())
                            .collect();
                        for t in targets {
                            push_cmd(
                                &mut ctx.ops,
                                seam::Cmd::Send {
                                    peer: t,
                                    tag: TAG_GROUP_OWNER_TRANSFER.to_string(),
                                    payload: Some(payload.clone()),
                                },
                            );
                        }
                        let new_creator_peer: PeerId =
                            match new_creator.parse() {
                                Ok(p) => p,
                                Err(_) => {
                                    eprintln!("{}", "继任群主解析失败".yellow());
                                    return;
                                }
                            };
                        let new_name = ctx
                            .conversations
                            .get(&new_creator_peer)
                            .map(|c| c.name.clone())
                            .filter(|n| !n.is_empty())
                            .unwrap_or_else(|| new_creator_peer.to_string());
                        // 退订 + 本地删群
                        push_cmd(
                            &mut ctx.ops,
                            seam::Cmd::Unsubscribe {
                                topic: group_topic(&gid),
                            },
                        );
                        if ctx.focused_group.as_deref() == Some(gid.as_str()) {
                            *ctx.focused_group = None;
                        }
                        ctx.groups.remove(&gid);
                        let _ = save_groups(ctx.identity.my_id(), &ctx.groups);
                        println!(
                            "{}",
                            format!("已退出群聊 {group}，群主已顺位转移给 {new_name}").green()
                        );
                    } else {
                        // 仅自己：解散
                        push_cmd(
                            &mut ctx.ops,
                            seam::Cmd::Unsubscribe {
                                topic: group_topic(&gid),
                            },
                        );
                        if ctx.focused_group.as_deref() == Some(gid.as_str()) {
                            *ctx.focused_group = None;
                        }
                        ctx.groups.remove(&gid);
                        let _ = save_groups(ctx.identity.my_id(), &ctx.groups);
                        println!(
                            "{}",
                            format!("已解散群聊 {group}（你是唯一成员）").yellow()
                        );
                    }
                } else if !ctx.connected.contains(&creator) {
                    // 单写者一致性：群主不在线禁止退群（防止名单发散/幽灵）
                    eprintln!(
                        "{}",
                        format!("群主不在线，无法退群 {group}（请等群主上线后再试）").yellow()
                    );
                } else {
                    // 普通成员：通知群主划去自己
                    let leave = serde_cbor::to_vec(&GroupLeavePayload {
                        group_id: gid.clone(),
                    })
                    .unwrap_or_default();
                    push_cmd(
                        &mut ctx.ops,
                        seam::Cmd::Send {
                            peer: creator,
                            tag: TAG_GROUP_LEAVE.to_string(),
                            payload: Some(leave),
                        },
                    );
                    // 本地移除群记录并退订 topic
                    push_cmd(
                        &mut ctx.ops,
                        seam::Cmd::Unsubscribe {
                            topic: group_topic(&gid),
                        },
                    );
                    if ctx.focused_group.as_deref() == Some(gid.as_str()) {
                        *ctx.focused_group = None;
                    }
                    ctx.groups.remove(&gid);
                    let _ = save_groups(ctx.identity.my_id(), &ctx.groups);
                    println!(
                        "{}",
                        format!("已退出群聊 {group}（已通知群主）").yellow()
                    );
                }
            }
            None => eprintln!("{}", format!("未知群: {group}（/group list 查看）").yellow()),
        }
    });
    tree.set_help(g_leave, "退群（群主须在线；群主退群自动顺位转移）");
    let g_list = tree.register(group, "list", |ctx, _| {
        if ctx.groups.is_empty() {
            println!("{}", "暂无群聊（/group new <群名> 创建）".dimmed());
        } else {
            println!("{}", "=== 群聊 ===".cyan());
            for g in ctx.groups.values() {
                let n = g.members.len();
                let focus = if ctx.focused_group.as_deref() == Some(g.id.as_str()) {
                    "  ← 当前群聊".green()
                } else {
                    "".dimmed()
                };
                let resident = if g.resident {
                    " [常驻]".green()
                } else {
                    "".dimmed()
                };
                let owner = group_owner_label(g, ctx.conversations, ctx.identity);
                println!(
                    "  {}（{} 人，名单版本 {}，群ID {}，{owner}）{resident}{focus}",
                    g.name, n, g.version, g.id
                );
            }
        }
    });
    tree.set_help(g_list, "列群");
    tree
}

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
async fn send_focused_text(ctx: &mut ChatCtx<'_>, text: &str) {
    if let Some(gid) = &ctx.focused_group {
        let g = match ctx.groups.get(gid) {
            Some(g) => g.clone(),
            None => {
                eprintln!("{}", "当前群不存在".yellow());
                return;
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
        return;
    }
    match *ctx.focused {
        Some(p) => {
            if !ctx.connected.contains(&p) {
                eprintln!("{}", "当前会话未连接，请用 /chat 重连".yellow());
                return;
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
            // D3：未互信联系人首次发消息确认（仅交互终端；管道/e2e 自动放行）
            if !ctx.identity.effective_trusted(&p) {
                if ctx.interactive && !ctx.conversations[&p].send_confirmed {
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
                        return;
                    }
                    ctx.conversations.get_mut(&p).unwrap().send_confirmed = true;
                } else if !ctx.interactive {
                    println!("{}", format!("对方 {who} 未信任，消息仍已发送").yellow());
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
        }
        None => eprintln!(
            "{}",
            "尚未选择会话，无法发送（先 /chat <角色> 或 /group <群名>）".yellow()
        ),
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

pub async fn run_node(mut input: LineSource, pre: Option<LoginOutcome>) -> Result<(), Box<dyn Error>> {
    // 交互语义按输入源判定：Stdin 终端=交互（rpassword/y 确认）；Stdin 管道与 GUI 通道=管道语义
    let interactive = match &input {
        LineSource::Stdin(_) => std::io::stdin().is_terminal(),
        LineSource::Channel(_) => false,
    };

    // L2 身份基础：GUI 表单凭据直建会话（影子探测防同 ID 双在线），
    // 或 CLI 文本登录菜单产出凭据 → login_pre + 联系人簿（TOFU）
    let mut identity = match pre {
        Some(outcome) => IdentityService::login_pre(outcome).await?,
        None => crate::p2p_app::chat::cli::login::run(&mut input, interactive).await?,
    };
    let discovery_mode = load_discovery_mode(identity.my_id());
    println!(
        "{}",
        format!("发现模式: {}", discovery_mode.name()).dimmed()
    );
    // 文件传输应用状态（下载目录在构造时解析，见 FileTransferState::new）
    let mut file_state = crate::file_transfer::FileTransferState::new(identity.my_id());
    println!(
        "{}",
        format!("下载目录: {}", file_state.downloads_dir().display()).dimmed()
    );

    // L3 群注册表（登录后先读本地持久化）
    let mut groups: HashMap<String, Group> = load_groups(identity.my_id());
    let mut focused_group: Option<String> = None;

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
    use crate::file_transfer as ft;
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
        tokio::select! {
            msg = input.next_input() => {
                // None = 输入结束（EOF/通道关闭）：多行收集中则报未闭合丢弃
                let line = match msg {
                    // 结构化控制动作（GUI 点击/按钮）：不经命令文本解析，复刻对应命令逻辑
                    Some(InputMsg::Control(c)) => {
                        let mut ctx = make_chat_ctx(
                            &mut identity, &cmd_tx, &mut input, interactive,
                            &mut conversations, &mut groups, &mut focused, &mut focused_group,
                            &connected, &mut registered, &mut file_state,
                        );
                        handle_control(&mut ctx, c).await;
                        consume_ops(&mut ctx).await;
                        push_sidebar(ctx.identity, ctx.groups, ctx.connected, ctx.focused, &ctx.focused_group);
                        continue;
                    }
                    Some(InputMsg::ChatText(text)) => {
                        // GUI 文本框：纯聊天文本直发当前焦点（多行原样；以 / 开头也不解析为命令）
                        let text = text.trim();
                        if !text.is_empty() {
                            let mut ctx = make_chat_ctx(
                                &mut identity, &cmd_tx, &mut input, interactive,
                                &mut conversations, &mut groups, &mut focused, &mut focused_group,
                                &connected, &mut registered, &mut file_state,
                            );
                            send_focused_text(&mut ctx, &text).await;
                        }
                        continue;
                    }
                    Some(InputMsg::Line(l)) => l,
                    None => {
                        if let Some((_, remaining)) = &sendstrings {
                            eprintln!(
                                "{}",
                                format!("多行消息未闭合（还差 {remaining} 行时输入结束），已丢弃").yellow()
                            );
                        }
                        break;
                    }
                };
                // 多行收集态优先：不 trim、空行原样保留，正文以 / 开头也不解析为命令
                if let Some((mut buf, mut remaining)) = sendstrings.take() {
                    if let Some(content) = collect_multiline(&mut buf, &mut remaining, &line) {
                        let mut ctx = make_chat_ctx(
                            &mut identity, &cmd_tx, &mut input, interactive,
                            &mut conversations, &mut groups, &mut focused, &mut focused_group,
                            &connected, &mut registered, &mut file_state,
                        );
                        send_focused_text(&mut ctx, &content).await;
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
                    // 命令上下文：一次性借用全部状态，handler 同步改状态 + 排异步动作队列。
                    // 指令树每行重建（无状态 builder，开销可忽略）：其 `ChatCtx<'a>` 生命周期
                    // 随本次处理结束释放，借用不跨 select 迭代存活。
                    let mut ctx = make_chat_ctx(
                        &mut identity, &cmd_tx, &mut input, interactive,
                        &mut conversations, &mut groups, &mut focused, &mut focused_group,
                        &connected, &mut registered, &mut file_state,
                    );
                    let mut tree = build_tree();
                    if let Err(CmdError::NotFound) = tree.parse(cmd, &mut ctx) {
                        eprintln!("{}", format!("未知命令: {cmd}").yellow());
                    }
                    // 异步消费 handler 排队的动作（同步生产者 → 异步消费者）
                    consume_ops(&mut ctx).await;
                    if ctx.quit {
                        // 等 Bye 帧送达（传输任务独立处理），再关闭传输任务
                        tokio::time::sleep(BYE_HANDSHAKE_TIMEOUT).await;
                        let _ = ctx.cmd_tx.send(seam::Cmd::Shutdown).await;
                        break;
                    }
                    push_sidebar(
                        ctx.identity,
                        ctx.groups,
                        ctx.connected,
                        ctx.focused,
                        &ctx.focused_group,
                    );
                    continue;
                }
                // 非命令：作为消息发送给当前聊天对象（与 /sendStrings 共用同一发送逻辑）
                let mut ctx = make_chat_ctx(
                    &mut identity, &cmd_tx, &mut input, interactive,
                    &mut conversations, &mut groups, &mut focused, &mut focused_group,
                    &connected, &mut registered, &mut file_state,
                );
                send_focused_text(&mut ctx, line).await;
            }
            event = ev_rx.recv() => {
                match event {
                    Some(ev) => {
                        match ev {
                            Event::Connected(peer) => {
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
                                // 通道路由：control（L1 心跳）已被 seam 过滤，L3 只见信号帧。
                                // 无 match：构造应用上下文，按 tag 标签查 SignalRegistry 分发，
                                // await 注册的 handler（hello/bye/trust 由 L2 内化语义映射 + L3 钩子；
                                // chat.* 由 chat 业务 handler 处理）。
                                // L2 门禁（唯一收口）：内化信号（hello/bye/trust）一律放行；
                                // 业务信号（chat.*/file.*）须互信，否则走该 tag 的未互信钩子
                                // （L2 API `register_untrusted`；未注册 = 空函数 = 丢弃）
                                if !is_l2_signal(&tag) && !identity.effective_trusted(&from) {
                                    let mut actx = AppCtx {
                                        identity: &mut identity,
                                        conversations: &mut conversations,
                                        groups: &mut groups,
                                        focused: &mut focused,
                                        input: &mut input,
                                        interactive,
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
                                    input: &mut input,
                                    interactive,
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
                            Event::SendFailure { peer, error } => {
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
                            &identity, &groups, &connected, &focused, &focused_group,
                        );
                    }
                    None => break,
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEER: &str = "12D3KooWGpERtoeJ1M482Kkx7p9czC9yKYuXGsvUvDBG3589iPKq";

    fn valid_addr() -> String {
        format!("/ip4/192.168.31.10/tcp/12082/p2p/{PEER}")
    }

    #[test]
    fn accept_valid_ipv4() {
        assert!(parse_dial_addr(&valid_addr()).is_ok());
    }

    #[test]
    fn accept_valid_ipv6() {
        let a = format!("/ip6/::1/tcp/12082/p2p/{PEER}");
        assert!(parse_dial_addr(&a).is_ok());
    }

    #[test]
    fn strip_listen_label_prefix() {
        let a = format!("监听地址: {}", valid_addr());
        assert!(parse_dial_addr(&a).is_ok());
        let b = format!("监听地址：{}", valid_addr());
        assert!(parse_dial_addr(&b).is_ok());
    }

    #[test]
    fn reject_no_leading_slash() {
        let e = parse_dial_addr("ip4/1.2.3.4/tcp/1/p2p/x").unwrap_err();
        assert!(e.contains("以 / 开头"));
    }

    #[test]
    fn reject_bad_protocol() {
        let e = parse_dial_addr("/ipx/1.2.3.4/tcp/1").unwrap_err();
        assert!(e.contains("/ip4/ 或 /ip6/"));
    }

    #[test]
    fn reject_bad_ipv4() {
        let e = parse_dial_addr("/ip4/300.1.2.3/tcp/1/p2p/x").unwrap_err();
        assert!(e.contains("IPv4 地址无效"));
    }

    #[test]
    fn reject_missing_tcp() {
        let e = parse_dial_addr("/ip4/1.2.3.4/p2p/x").unwrap_err();
        assert!(e.contains("/tcp/"));
    }

    #[test]
    fn reject_bad_port() {
        let e = parse_dial_addr("/ip4/1.2.3.4/tcp/abc/p2p/x").unwrap_err();
        assert!(e.contains("端口"));
        let e = parse_dial_addr("/ip4/1.2.3.4/tcp/70000/p2p/x").unwrap_err();
        assert!(e.contains("端口"));
    }

    #[test]
    fn reject_missing_p2p() {
        let e = parse_dial_addr("/ip4/1.2.3.4/tcp/1").unwrap_err();
        assert!(e.contains("/p2p/"));
    }

    #[test]
    fn reject_bad_peer_id() {
        let e = parse_dial_addr("/ip4/1.2.3.4/tcp/1/p2p/not-a-peer-id").unwrap_err();
        assert!(e.contains("节点ID无效"));
    }

    #[test]
    fn dedup_members_keeps_order_and_removes_dups() {
        let mut m = vec!["A".into(), "B".into(), "A".into(), "C".into(), "B".into()];
        dedup_members(&mut m);
        assert_eq!(m, vec!["A", "B", "C"]);
        let mut single = vec!["X".into()];
        dedup_members(&mut single);
        assert_eq!(single, vec!["X"]);
    }

    #[test]
    fn next_creator_wraps_after_owner() {
        let members: Vec<String> = vec!["A".into(), "B".into(), "C".into()];
        assert_eq!(next_creator(&members, "A").as_deref(), Some("B"));
        assert_eq!(next_creator(&members, "B").as_deref(), Some("C"));
        // 群主在末尾：回卷取第一个非群主
        assert_eq!(next_creator(&members, "C").as_deref(), Some("A"));
        // 仅自己：无下一位（解散）
        let solo = vec!["A".into()];
        assert_eq!(next_creator(&solo, "A"), None);
        // 群主不在名单（数据异常防御）：不猜测继任者
        assert_eq!(next_creator(&members, "Z"), None);
    }

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
}

