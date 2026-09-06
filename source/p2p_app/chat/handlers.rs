//! chat 业务语义 handler（注册到 SignalRegistry）：hello/bye/chat.text/trust/群事件。

use std::collections::HashMap;

use colored::Colorize;
use libp2p::PeerId;

use crate::lineio::LineSource;
use crate::p2p::identity_service::{IdentityService, TextTag};
use crate::p2p::seam;
use crate::p2p_app::chat::ctx::Conversation;
use crate::p2p_app::chat::display;
use crate::p2p_app::chat::group::{
    dedup_members, fanout_member_list_async, group_topic, save_groups, Group,
};
use crate::p2p_app::chat::payloads::{
    ChatTextPayload, GroupInvitePayload, GroupLeavePayload, GroupMemberListPayload,
    GroupOwnerTransferPayload,
};

/// 事件处理上下文：收到 seam::Event::Signal 时一次性构造，供语义 handler 读写。
/// handler 是 async 的，可直接 await（TOFU 读输入 / 发命令）。
pub(crate) struct AppCtx<'a> {
    pub(crate) identity: &'a mut IdentityService,
    pub(crate) conversations: &'a mut HashMap<PeerId, Conversation>,
    pub(crate) groups: &'a mut HashMap<String, Group>,
    pub(crate) focused: &'a mut Option<PeerId>,
    pub(crate) input: &'a mut LineSource,
    pub(crate) interactive: bool,
    pub(crate) cmd_tx: &'a tokio::sync::mpsc::Sender<seam::Cmd>,
    pub(crate) file: &'a mut crate::file_transfer::FileTransferState,
}

/// 让 `AppCtx<'a>` 作为 L2 `SignalRegistry` 的上下文：GAT 暴露其带生命周期的类型
impl seam::SignalCtx for AppCtx<'_> {
    type Ctx<'a> = AppCtx<'a>;
}

/// hello（对方上线）：L2 处理存在 + 触发 L3 钩子（解析名字，分析谁上线）
pub(crate) async fn on_peer_hello_signal(
    ctx: &mut AppCtx<'_>,
    from: &PeerId,
    payload: Option<&[u8]>,
) -> bool {
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
pub(crate) async fn on_peer_bye_signal(
    ctx: &mut AppCtx<'_>,
    from: &PeerId,
    _payload: Option<&[u8]>,
) -> bool {
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

pub(crate) async fn on_chat_text(
    ctx: &mut AppCtx<'_>,
    from: &PeerId,
    payload: Option<&[u8]>,
) -> bool {
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
pub(crate) async fn display_untrusted_text(
    ctx: &mut AppCtx<'_>,
    from: &PeerId,
    payload: Option<&[u8]>,
) -> bool {
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
pub(crate) async fn on_trust_signal(
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

pub(crate) async fn on_group_invite(
    ctx: &mut AppCtx<'_>,
    from: &PeerId,
    payload: Option<&[u8]>,
) -> bool {
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

pub(crate) async fn on_group_leave(
    ctx: &mut AppCtx<'_>,
    from: &PeerId,
    payload: Option<&[u8]>,
) -> bool {
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

pub(crate) async fn on_group_member_list(
    ctx: &mut AppCtx<'_>,
    _from: &PeerId,
    payload: Option<&[u8]>,
) -> bool {
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

pub(crate) async fn on_group_owner_transfer(
    ctx: &mut AppCtx<'_>,
    from: &PeerId,
    payload: Option<&[u8]>,
) -> bool {
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
