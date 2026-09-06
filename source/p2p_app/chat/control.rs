//! 结构化控制动作（GUI 点击/按钮）处理：复刻对应命令的非文本逻辑。
//!
//! 名字一律 `peer_name()`（会话名 → 联系人名 → 节点ID）；提示行照打（CLI 同款文案）；
//! 指纹信息照打（核对弹窗属弹窗批，本实现与 /trust 同粒度直接执行）。

use colored::Colorize;

use crate::lineio::Control;
use crate::p2p::identity_service::TextTag;
use crate::p2p::seam;
use crate::p2p_app::chat::ctx::{peer_name, push_cmd, ChatCtx, Conversation};
use crate::p2p_app::chat::dial::{parse_dial_addr, print_dial_template};
use crate::p2p_app::chat::group::dial_group_members;
use crate::p2p_app::chat::sidebar::trust_badge;

pub(crate) async fn handle_control(ctx: &mut ChatCtx<'_>, c: Control) {
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
        Control::Dial { addr, name } => {
            // 复刻 /dial：解析地址 → registered 登记 → 拨号；name 预登记会话名
            match parse_dial_addr(&addr) {
                Ok(ma) => {
                    if let Some(p) = ma.iter().find_map(|seg| match seg {
                        libp2p::multiaddr::Protocol::P2p(pid) => Some(pid),
                        _ => None,
                    }) {
                        let recorded = ctx.registered.entry(p).or_default();
                        if !recorded.contains(&ma) {
                            recorded.push(ma.clone());
                        }
                        if !name.is_empty() {
                            let conv = ctx.conversations.entry(p).or_insert_with(Conversation::new);
                            if conv.name.is_empty() {
                                conv.name = name.clone();
                            }
                        }
                    }
                    push_cmd(&mut ctx.ops, seam::Cmd::Dial { addr: ma });
                }
                Err(reason) => {
                    eprintln!("{}", format!("地址无效: {reason}").red());
                    print_dial_template();
                }
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
