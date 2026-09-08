//! chat 命令树：/dial /chat /list /trust /group /send /backup /q 等全部注册（CLI 文本层）。
//! 框架在 crate 根 cmd_tree.rs（全局组件）；本模块是 chat 域的注册内容——
//! 对应固件惯例：CommandParse/（框架）与 userTasks_cmds.c（域内注册）分离。

use colored::Colorize;
use libp2p::multiaddr::Protocol;
use libp2p::PeerId;
use rand::{rngs::OsRng, RngCore};

use crate::cmd_tree::{CmdTree, ROOT};
use crate::p2p::identity_service::TextTag;
use crate::p2p::seam;
use crate::p2p::{save_discovery_mode, save_download_dir, DiscoveryMode};
use crate::p2p_app::chat::ctx::{peer_name, push_cmd, AsyncOp, ChatCtx, Conversation};
use crate::p2p_app::chat::dial::{parse_dial_addr, print_dial_template};
use crate::p2p_app::chat::group::{
    dedup_members, dial_group_members, fanout_member_list, group_owner_label, group_topic,
    next_creator, save_groups, Group,
};
use crate::p2p_app::chat::payloads::{
    GroupInvitePayload, GroupLeavePayload, GroupOwnerTransferPayload, TAG_GROUP_INVITE,
    TAG_GROUP_LEAVE, TAG_GROUP_OWNER_TRANSFER,
};
use crate::p2p_app::chat::sidebar::trust_badge;
/// 终端逃逸：`cmd/<命令>` 走 cmd.exe，`ps/<命令>` 走 PowerShell，`sh/<命令>` 走 POSIX sh。
/// stdout/stderr 继承到真实终端（cls 可真清屏），input 置 null 不与应用抢输入。
pub(crate) async fn run_terminal_escape(program: &str, args: &[&str], rest: &str) {
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

pub(crate) fn build_tree<'a>() -> CmdTree<ChatCtx<'a>> {
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
                if let Err(e) = crate::p2p_app::file_transfer::start_send(
                    ctx.file,
                    &mut ctx.ops,
                    peer,
                    &path,
                    &crate::p2p_app::chat::ctx::peer_name(&peer, ctx.conversations, ctx.identity),
                ) {
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
