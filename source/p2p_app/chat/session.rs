//! 会话主循环：run/run_engine/run_node——引擎入口与 select 事件循环（CLI/GUI 共用）。
//! 输入抽象 LineSource 已屏蔽模式差异；命令树来自 commands::build_tree。

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::io::IsTerminal;
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
            // D3：未互信联系人发送确认（三态：Interactive 终端 y/n / **Ask 系统消息卡片** /
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
                            return;
                        }
                        ctx.conversations.get_mut(&p).unwrap().send_confirmed = true;
                    }
                    ConfirmMode::Ask if !ctx.conversations[&p].send_confirmed => {
                        // GUI：发系统消息卡片（答案经 InputMsg::Line 回程，同 CLI 交互）
                        crate::sink::ask(crate::uievent::AskRequest {
                            id: crate::uievent::next_ask_id(),
                            kind: crate::uievent::AskKind::UntrustedSend {
                                name: who.clone(),
                            },
                            secret: false,
                        });
                        let ans = match ctx.input.next_raw_line().await {
                            Some(l) => l.trim().to_string(),
                            None => String::new(),
                        };
                        if !ans.eq_ignore_ascii_case("y") {
                            println!("{}", "已取消发送".dimmed());
                            return;
                        }
                        ctx.conversations.get_mut(&p).unwrap().send_confirmed = true;
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
    // 交互确认三态：Stdin 终端=Interactive（文字提示）；Stdin 管道=Auto（e2e 自动语义）；
    // Channel（GUI）=Ask（系统消息卡片 + InputMsg::Line 回程）
    let mode = ConfirmMode::of(&input, std::io::stdin().is_terminal());

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
        // 定时臂预条件：有等待确认的 offer 时，睡到最早过期时刻（无则该分支禁用）
        let offer_expiry = file_state
            .next_offer_expiry()
            .map(tokio::time::Instant::from_std);
        tokio::select! {
            _ = tokio::time::sleep_until(offer_expiry.unwrap_or_else(tokio::time::Instant::now)), if offer_expiry.is_some() => {
                // H2：offer 超时——对端不应答（网络突发/对端退出）时中止并清理，防状态悬挂
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
                    println!(
                        "{}",
                        format!("等待对端确认超时，已中止发送: {name}").yellow()
                    );
                }
            }
            msg = input.next_input() => {
                // None = 输入结束（EOF/通道关闭）：多行收集中则报未闭合丢弃
                let line = match msg {
                    // 结构化控制动作（GUI 点击/按钮）：不经命令文本解析，复刻对应命令逻辑
                    Some(InputMsg::Control(c)) => {
                        let mut ctx = make_chat_ctx(
                            &mut identity, &cmd_tx, &mut input, mode,
                            &mut conversations, &mut groups, &mut focused, &mut focused_group,
                            &connected, &mut registered, &mut file_state,
                        );
                        handle_control(&mut ctx, c).await;
                        consume_ops(&mut ctx).await;
                        push_sidebar(ctx.identity, ctx.groups, ctx.connected, ctx.focused, &ctx.focused_group, ctx.registered);
                        continue;
                    }
                    Some(InputMsg::ChatText(text)) => {
                        // GUI 文本框：纯聊天文本直发当前焦点（多行原样；以 / 开头也不解析为命令）
                        let text = text.trim();
                        if !text.is_empty() {
                            let mut ctx = make_chat_ctx(
                                &mut identity, &cmd_tx, &mut input, mode,
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
                            &mut identity, &cmd_tx, &mut input, mode,
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
                        &mut identity, &cmd_tx, &mut input, mode,
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
                                    // 拦截可见性：未互信来源的业务信号被丢弃时明确提示
                                    // （每条都提示——单方面信任的"不通"必须可诊断）
                                    let who = peer_name(
                                        &from,
                                        &conversations,
                                        &identity,
                                    );
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
                                        input: &mut input,
                                        mode,
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
                                    mode,
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
                            Event::Listening(addr) => {
                                // GUI"我的地址"去重累积（CLI 的监听地址行由 L1 打印照旧）
                                crate::p2p_app::chat::display::listen_addr(addr.to_string());
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
                            &identity, &groups, &connected, &focused, &focused_group, &registered,
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

