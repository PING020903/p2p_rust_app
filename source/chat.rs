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
use crate::p2p::{cache_dir, load_discovery_mode, save_discovery_mode, DiscoveryMode};
use crate::p2p::identity_service::{IdentityService, StdinLines};
use crate::p2p::node::{Control, Frame, NodeMsg, P2pCommand, P2pEvent, P2pNode, BYE_HANDSHAKE_TIMEOUT};

/// binary 字段：用户内容负载（应用层自描述，cbor 序列化后放入 binary）
#[derive(Debug, Clone, Serialize, Deserialize)]
enum AppPayload {
    Text(String), // 用户聊天文本
    GroupInvite {
        group_id: String,
        group_name: String,
        version: u64,
        members: Vec<String>,
    },
    GroupLeave { group_id: String },
    GroupMemberList {
        group_id: String,
        version: u64,
        members: Vec<String>,
    },
    /// 群主退群时一步顺位转移：携带新群主 + 移除群主后的名单（版本门控整体替换）
    GroupOwnerTransfer {
        group_id: String,
        new_creator: String,
        version: u64,
        members: Vec<String>,
    },
}

/// 命令 handler 产出的异步动作：同步逻辑跑在指令树 handler 里，真正需要 `.await`
/// 的 I/O（发命令给传输任务 / 读密码）排进 `ChatCtx.ops`，由主循环统一消费。
/// 这本质是"同步生产者 → 异步消费者"的 ring buffer 解耦。
enum AsyncOp {
    Cmd(P2pCommand),
    Backup,
}

/// 命令上下文：一次性持有全部可变状态，供指令树 handler 直接读写。
/// 每次解析一行命令前临时构造（借用随本次处理结束释放），quit 置位表示请求退出。
struct ChatCtx<'a> {
    identity: &'a mut IdentityService,
    cmd_tx: &'a tokio::sync::mpsc::Sender<P2pCommand>,
    stdin: &'a mut StdinLines,
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
}

impl<'a> ChatCtx<'a> {
    /// 按名字/节点ID 解析目标 peer（先查 1v1 会话名，再按节点ID解析）
    fn resolve(&self, target: &str) -> Option<PeerId> {
        self.conversations
            .iter()
            .find(|(_, c)| c.name == target)
            .map(|(p, _)| *p)
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

/// 向命令队列排入"发命令"动作（字段级借用，可在 handler 持有其它字段借用时调用）
fn push_cmd(ops: &mut VecDeque<AsyncOp>, cmd: P2pCommand) {
    ops.push_back(AsyncOp::Cmd(cmd));
}

/// 一个 1v1 会话：与某 peer 的聊天上下文（连接可多路共存）
struct Conversation {
    name: String,       // 对方角色名（Hello 更新；未知为空）
    greeted: bool,      // 是否已发过 Hello（重连后重置，避免漏问候）
    bye: bool,          // 对方已主动退出（不再心跳/重连）
    pending_dial: bool, // /chat 后尚无地址，等待 mDNS 发现自动拨号
}

impl Conversation {
    fn new() -> Self {
        Conversation {
            name: String::new(),
            greeted: false,
            bye: false,
            pending_dial: false,
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
    let payload = serde_cbor::to_vec(&AppPayload::GroupMemberList {
        group_id: group_id.to_string(),
        version,
        members: members.to_vec(),
    })
    .unwrap_or_default();
    let frame = Frame {
        control: None,
        text: None,
        binary: Some(payload),
    };
    for p in targets {
        ops.push_back(AsyncOp::Cmd(P2pCommand::Send {
            peer: *p,
            frame: frame.clone(),
        }));
    }
}

/// 事件分支（主循环内、已有命令发送器）用的异步扇出
async fn fanout_member_list_async(
    cmd_tx: &tokio::sync::mpsc::Sender<P2pCommand>,
    group_id: &str,
    version: u64,
    members: &[String],
    targets: &[PeerId],
) {
    let payload = serde_cbor::to_vec(&AppPayload::GroupMemberList {
        group_id: group_id.to_string(),
        version,
        members: members.to_vec(),
    })
    .unwrap_or_default();
    let frame = Frame {
        control: None,
        text: None,
        binary: Some(payload),
    };
    for p in targets {
        let _ = cmd_tx
            .send(P2pCommand::Send {
                peer: *p,
                frame: frame.clone(),
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
            ops.push_back(AsyncOp::Cmd(P2pCommand::DialPeer(pid)));
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
                push_cmd(&mut ctx.ops, P2pCommand::Dial { addr: ma });
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
                    println!(
                        "{}",
                        format!(
                            "已切换到会话: {}（{p}）",
                            if name.is_empty() {
                                target.as_str()
                            } else {
                                name.as_str()
                            }
                        )
                        .green()
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
                            push_cmd(&mut ctx.ops, P2pCommand::DialPeer(p));
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
                let trust_badge = if ctx.identity.is_verified(p) {
                    "已信任".green()
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
                println!(
                    "  {}（{} 人，名单版本 {}，群ID {}）{resident}{focus}",
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
                P2pCommand::Send {
                    peer: p,
                    frame: Frame {
                        control: None,
                        text: Some(NodeMsg::Bye),
                        binary: None,
                    },
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
                P2pCommand::Send {
                    peer: p,
                    frame: Frame {
                        control: None,
                        text: Some(NodeMsg::Bye),
                        binary: None,
                    },
                },
            );
            println!("{}", format!("正在通知对方下线: {p}...").dimmed());
        }
        ctx.quit = true;
    });
    tree.set_help(q, "退出聊天");
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
                let name = ctx
                    .conversations
                    .get(&p)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| "未知".to_string());
                ctx.identity.trust(&p, &name, !untrust);
                if untrust {
                    println!("{}", format!("已取消信任: {name}").yellow());
                } else {
                    println!("{}", format!("已信任: {name}").green());
                }
            }
            None => eprintln!("{}", "未知节点，无法标记信任（用 /list 查看）".yellow()),
        }
    });
    tree.set_help(trust, "标记/取消信任联系人（! 前缀取消）");
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
                        P2pCommand::Subscribe {
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
                            println!("{}", format!("{target} 已在群 {group} 中").dimmed());
                        } else {
                            let name = peer_name(&p, ctx.conversations, ctx.identity);
                            ctx.groups.get_mut(&gid).unwrap().version += 1;
                            ctx.groups.get_mut(&gid).unwrap().members.push(p.to_string());
                            let _ = save_groups(ctx.identity.my_id(), &ctx.groups);
                            // 邀请新成员（携带当前版本 + 全量名单，入群即一致）
                            let g = &ctx.groups[&gid];
                            let invite = serde_cbor::to_vec(&AppPayload::GroupInvite {
                                group_id: g.id.clone(),
                                group_name: g.name.clone(),
                                version: g.version,
                                members: g.members.clone(),
                            })
                            .unwrap_or_default();
                            push_cmd(
                                &mut ctx.ops,
                                P2pCommand::Send {
                                    peer: p,
                                    frame: Frame {
                                        control: None,
                                        text: None,
                                        binary: Some(invite),
                                    },
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
                        let payload = serde_cbor::to_vec(&AppPayload::GroupOwnerTransfer {
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
                                P2pCommand::Send {
                                    peer: t,
                                    frame: Frame {
                                        control: None,
                                        text: None,
                                        binary: Some(payload.clone()),
                                    },
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
                            P2pCommand::Unsubscribe {
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
                            P2pCommand::Unsubscribe {
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
                    let leave = serde_cbor::to_vec(&AppPayload::GroupLeave {
                        group_id: gid.clone(),
                    })
                    .unwrap_or_default();
                    push_cmd(
                        &mut ctx.ops,
                        P2pCommand::Send {
                            peer: creator,
                            frame: Frame {
                                control: None,
                                text: None,
                                binary: Some(leave),
                            },
                        },
                    );
                    // 本地移除群记录并退订 topic
                    push_cmd(
                        &mut ctx.ops,
                        P2pCommand::Unsubscribe {
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
                println!(
                    "  {}（{} 人，名单版本 {}，群ID {}）{resident}{focus}",
                    g.name, n, g.version, g.id
                );
            }
        }
    });
    tree.set_help(g_list, "列群");
    tree
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
        if let Err(e) = run_node().await {
            eprintln!("{}", format!("节点运行错误: {e}").red());
        }
    });
}

async fn run_node() -> Result<(), Box<dyn Error>> {
    let interactive = std::io::stdin().is_terminal();
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin()).lines();

    // L2 身份基础：登录（含影子探测防同 ID 双在线）+ 联系人簿（TOFU）
    let mut identity = IdentityService::login(&mut stdin, interactive).await?;
    let discovery_mode = load_discovery_mode(identity.my_id());
    println!(
        "{}",
        format!("发现模式: {}", discovery_mode.name()).dimmed()
    );

    // L3 群注册表（登录后先读本地持久化）
    let mut groups: HashMap<String, Group> = load_groups(identity.my_id());
    let mut focused_group: Option<String> = None;

    // L1 传输任务：命令/事件双通道。事件用无界通道——传输任务永不因应用阻塞
    // （应用卡在 TOFU/密码等交互 await 时，心跳仍由传输任务独立维持）
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(32);
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
    let node = P2pNode::new(identity.keypair().clone(), discovery_mode)?;
    tokio::spawn(node.run(cmd_rx, ev_tx));

    // 订阅已保存群的 gossipsub topic
    for g in groups.values() {
        let _ = cmd_tx
            .send(P2pCommand::Subscribe {
                topic: group_topic(&g.id),
            })
            .await;
    }

    let mut conversations: HashMap<PeerId, Conversation> = HashMap::new();
    let mut focused: Option<PeerId> = None;
    let mut connected: HashSet<PeerId> = HashSet::new();
    let mut registered: HashMap<PeerId, Vec<Multiaddr>> = HashMap::new();

    println!(
        "{}",
        "命令以 / 开头（/help 查看详情，/list 查看节点，/chat <角色> 发起聊天），其余输入作为消息发送给当前聊天对象".dimmed()
    );

    loop {
        tokio::select! {
            line = stdin.next_line() => {
                let line = match line {
                    Ok(Some(l)) => l,
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("{}", format!("读取输入失败: {e}").red());
                        break;
                    }
                };
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Some(cmd) = line.strip_prefix('/') {
                    // 命令上下文：一次性借用全部状态，handler 同步改状态 + 排异步动作队列。
                    // 指令树每行重建（无状态 builder，开销可忽略）：其 `ChatCtx<'a>` 生命周期
                    // 随本次处理结束释放，借用不跨 select 迭代存活。
                    let mut ctx = ChatCtx {
                        identity: &mut identity,
                        cmd_tx: &cmd_tx,
                        stdin: &mut stdin,
                        interactive,
                        conversations: &mut conversations,
                        groups: &mut groups,
                        focused: &mut focused,
                        focused_group: &mut focused_group,
                        connected: &connected,
                        registered: &mut registered,
                        ops: VecDeque::new(),
                        quit: false,
                    };
                    let mut tree = build_tree();
                    if let Err(CmdError::NotFound) = tree.parse(cmd, &mut ctx) {
                        eprintln!("{}", format!("未知命令: {cmd}").yellow());
                    }
                    // 异步消费 handler 排队的动作（同步生产者 → 异步消费者）
                    while let Some(op) = ctx.ops.pop_front() {
                        match op {
                            AsyncOp::Cmd(c) => {
                                if let Err(e) = ctx.cmd_tx.send(c).await {
                                    eprintln!("{}", format!("命令发送失败: {e}").red());
                                }
                            }
                            AsyncOp::Backup => {
                                if let Err(e) =
                                    ctx.identity.backup(ctx.stdin, ctx.interactive).await
                                {
                                    eprintln!("{}", format!("备份失败: {e}").red());
                                }
                            }
                        }
                    }
                    if ctx.quit {
                        // 等 Bye 帧送达（传输任务独立处理），再关闭传输任务
                        tokio::time::sleep(BYE_HANDSHAKE_TIMEOUT).await;
                        let _ = ctx.cmd_tx.send(P2pCommand::Shutdown).await;
                        break;
                    }
                    continue;
                }
                // 非命令：作为消息发送给当前聊天对象
                if let Some(gid) = &focused_group {
                    // 群消息：gossipsub 发布到群 topic
                    let g = match groups.get(gid) {
                        Some(g) => g.clone(),
                        None => {
                            eprintln!("{}", "当前群不存在".yellow());
                            continue;
                        }
                    };
                    let payload = serde_json::to_vec(&GroupPayload::Text {
                        group_id: g.id.clone(),
                        text: line.to_string(),
                        sender: identity.my_name().to_string(),
                    })
                    .unwrap_or_default();
                    let _ = cmd_tx
                        .send(P2pCommand::Publish {
                            topic: group_topic(&g.id),
                            data: payload,
                        })
                        .await;
                    println!("{}", format!("[我 -> {}] {line}", g.name).green());
                } else {
                    match focused {
                        Some(p) => {
                            if !connected.contains(&p) {
                                eprintln!(
                                    "{}",
                                    "当前会话未连接，请用 /chat 重连".yellow()
                                );
                            } else {
                                let name = conversations
                                    .get(&p)
                                    .map(|c| c.name.clone())
                                    .unwrap_or_default();
                                let who = if name.is_empty() {
                                    p.to_string()
                                } else {
                                    name
                                };
                                let payload = serde_cbor::to_vec(&AppPayload::Text(
                                    line.to_string(),
                                ))
                                .unwrap_or_default();
                                let _ = cmd_tx
                                    .send(P2pCommand::Send {
                                        peer: p,
                                        frame: Frame {
                                            control: None,
                                            text: None,
                                            binary: Some(payload),
                                        },
                                    })
                                    .await;
                                println!("{}", format!("[我 -> {who}] {line}").green());
                            }
                        }
                        None => eprintln!(
                            "{}",
                            "尚未选择会话，无法发送（先 /chat <角色> 或 /group <群名>）".yellow()
                        ),
                    }
                }
            }
            event = ev_rx.recv() => {
                match event {
                    Some(ev) => {
                        match ev {
                            P2pEvent::PeerConnected(peer) => {
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
                                        println!(
                                            "{}",
                                            format!("已切换到会话: {}（{peer}）", name).green()
                                        );
                                    }
                                }
                                println!("{}", format!("已连接对端: {peer}").green());
                                let conv = conversations
                                    .entry(peer)
                                    .or_insert_with(Conversation::new);
                                if !conv.greeted {
                                    let _ = cmd_tx
                                        .send(P2pCommand::Send {
                                            peer,
                                            frame: Frame {
                                                control: None,
                                                text: Some(NodeMsg::Hello(
                                                    identity.my_name().to_string(),
                                                )),
                                                binary: None,
                                            },
                                        })
                                        .await;
                                    conv.greeted = true;
                                }
                            }
                            P2pEvent::PeerDisconnected { peer, bye } => {
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
                            P2pEvent::PeerDiscovered { peer, addr } => {
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
                                    let _ = cmd_tx.send(P2pCommand::DialPeer(peer)).await;
                                }
                            }
                            P2pEvent::Message { from, frame } => {
                                let conv = conversations
                                    .entry(from)
                                    .or_insert_with(Conversation::new);
                                // 通道路由：control（传输控制）→ text（节点信号）→ binary（用户内容）
                                if let Some(ctrl) = frame.control {
                                    match ctrl {
                                        Control::Heartbeat => {}
                                    }
                                } else if let Some(msg) = frame.text {
                                    match msg {
                                        NodeMsg::Hello(name) => {
                                            conv.name = name.clone();
                                            println!(
                                                "{}",
                                                format!("对方已上线: {name}").green()
                                            );
                                            identity
                                                .on_peer_hello(
                                                    &mut stdin,
                                                    interactive,
                                                    &from,
                                                    &name,
                                                )
                                                .await?;
                                        }
                                        NodeMsg::Bye => {
                                            let _ = identity.on_peer_bye(&from);
                                            println!("{}", "对方已正常退出".yellow());
                                            conv.bye = true;
                                            // L1 策略：标记 bye → 不再心跳、断开后不重连
                                            let _ = cmd_tx
                                                .send(P2pCommand::MarkBye(from))
                                                .await;
                                        }
                                    }
                                } else if let Some(bin) = frame.binary {
                                    let Ok(payload) =
                                        serde_cbor::from_slice::<AppPayload>(&bin)
                                    else {
                                        continue;
                                    };
                                    match payload {
                                        AppPayload::Text(text) => {
                                            if focused == Some(from) {
                                                println!(
                                                    "{}",
                                                    format!("[对方] {text}").bright_cyan()
                                                );
                                            } else {
                                                let who = if conv.name.is_empty() {
                                                    from.to_string()
                                                } else {
                                                    conv.name.clone()
                                                };
                                                println!(
                                                    "{}",
                                                    format!("[{who}] {text}").bright_cyan()
                                                );
                                            }
                                        }
                                        AppPayload::GroupInvite {
                                            group_id,
                                            group_name,
                                            version,
                                            members,
                                        } => {
                                            // 群主（邀请者 from）发来的邀请：携带当前版本 + 全量名单，入群即一致。
                                            // 名单先归一化去重（幽灵/重复防御）
                                            let mut members = members;
                                            dedup_members(&mut members);
                                            if !groups.contains_key(&group_id)
                                                || groups[&group_id].version < version
                                            {
                                                groups.insert(
                                                    group_id.clone(),
                                                    Group {
                                                        id: group_id.clone(),
                                                        name: group_name.clone(),
                                                        members: members.clone(),
                                                        version,
                                                        creator: from.to_string(),
                                                        resident: false, // 入群默认非常驻
                                                    },
                                                );
                                                let _ = save_groups(identity.my_id(), &groups);
                                                let _ = cmd_tx
                                                    .send(P2pCommand::Subscribe {
                                                        topic: group_topic(&group_id),
                                                    })
                                                    .await;
                                            }
                                            let sender = identity
                                                .contact_name(&from)
                                                .unwrap_or_else(|| from.to_string());
                                            println!(
                                                "{}",
                                                format!(
                                                    "被邀请加入群聊: {group_name}（邀请者 {sender}，成员 {} 人）",
                                                    members.len()
                                                )
                                                .green()
                                            );
                                        }
                                        AppPayload::GroupLeave { group_id } => {
                                            // 成员主动退群：校验发送者确为成员，移除并推进版本，向剩余成员扇出
                                            let is_member = groups
                                                .get(&group_id)
                                                .map(|g| {
                                                    g.members
                                                        .iter()
                                                        .any(|m| m == &from.to_string())
                                                })
                                                .unwrap_or(false);
                                            if !is_member {
                                                continue;
                                            }
                                            if let Some(g) = groups.get_mut(&group_id) {
                                                g.version += 1;
                                                g.members.retain(|m| m != &from.to_string());
                                                dedup_members(&mut g.members);
                                                let _ = save_groups(identity.my_id(), &groups);
                                                let name = identity
                                                    .contact_name(&from)
                                                    .unwrap_or_else(|| from.to_string());
                                                let g = &groups[&group_id];
                                                let my_id = identity.my_id().to_string();
                                                let remaining: Vec<PeerId> = g
                                                    .members
                                                    .iter()
                                                    .filter(|m| m.as_str() != &my_id)
                                                    .filter_map(|m| m.parse().ok())
                                                    .collect();
                                                fanout_member_list_async(
                                                    &cmd_tx,
                                                    &g.id,
                                                    g.version,
                                                    &g.members,
                                                    &remaining,
                                                )
                                                .await;
                                                println!(
                                                    "{}",
                                                    format!(
                                                        "成员 {name} 已退出群 {}（名单版本 {}）",
                                                        g.name, g.version
                                                    )
                                                    .yellow()
                                                );
                                            }
                                        }
                                        AppPayload::GroupMemberList {
                                            group_id,
                                            version,
                                            members,
                                        } => {
                                            // 群主 1v1 扇出名单：版本更高才整体替换（防乱序/重复）。
                                            // 名单先归一化去重（幽灵/重复防御）
                                            let mut members = members;
                                            dedup_members(&mut members);
                                            let newer = groups
                                                .get(&group_id)
                                                .map(|g| version > g.version)
                                                .unwrap_or(false);
                                            if newer {
                                                let gname = groups
                                                    .get(&group_id)
                                                    .map(|g| g.name.clone())
                                                    .unwrap_or_default();
                                                if let Some(g) = groups.get_mut(&group_id) {
                                                    g.version = version;
                                                    g.members = members.clone();
                                                }
                                                let _ = save_groups(identity.my_id(), &groups);
                                                println!(
                                                    "{}",
                                                    format!(
                                                        "群 {gname} 成员名单已更新（版本 {version}，{} 人）",
                                                        members.len()
                                                    )
                                                    .dimmed()
                                                );
                                            }
                                        }
                                        AppPayload::GroupOwnerTransfer {
                                            group_id,
                                            new_creator,
                                            version,
                                            members,
                                        } => {
                                            // 群主退群顺位转移：校验来源确为当前群主，版本更高才整体替换。
                                            // 名单先归一化去重（幽灵/重复防御）
                                            let mut members = members;
                                            dedup_members(&mut members);
                                            let is_creator = groups
                                                .get(&group_id)
                                                .map(|g| g.creator == from.to_string())
                                                .unwrap_or(false);
                                            let newer = groups
                                                .get(&group_id)
                                                .map(|g| version > g.version)
                                                .unwrap_or(false);
                                            if is_creator && newer {
                                                let was_creator_of = groups
                                                    .get(&group_id)
                                                    .map(|g| g.name.clone())
                                                    .unwrap_or_default();
                                                let new_is_me =
                                                    new_creator == identity.my_id().to_string();
                                                if let Some(g) = groups.get_mut(&group_id) {
                                                    g.version = version;
                                                    g.creator = new_creator.clone();
                                                    g.members = members.clone();
                                                }
                                                let _ = save_groups(identity.my_id(), &groups);
                                                if new_is_me {
                                                    println!(
                                                        "{}",
                                                        format!(
                                                            "群 {was_creator_of} 的群主已转移给你，你已成为群主（可 /group add 邀请）"
                                                        )
                                                        .green()
                                                    );
                                                } else {
                                                    let nc_name = identity
                                                        .contact_name(
                                                            &new_creator.parse().unwrap_or(
                                                                from,
                                                            ),
                                                        )
                                                        .unwrap_or_else(|| new_creator.clone());
                                                    println!(
                                                        "{}",
                                                        format!(
                                                            "群 {was_creator_of} 群主已顺位转移给 {nc_name}（名单版本 {}，{} 人）",
                                                            version,
                                                            members.len()
                                                        )
                                                        .dimmed()
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            P2pEvent::Gossip { source, data } => {
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
                                        if focused_group.as_deref() == Some(group_id.as_str()) {
                                            println!(
                                                "{}",
                                                format!("[{who}] {text}").bright_cyan()
                                            );
                                        } else {
                                            println!(
                                                "{}",
                                                format!("[{}] [{who}] {text}", g.name)
                                                    .bright_cyan()
                                            );
                                        }
                                    }
                                }
                            }
                            P2pEvent::SendFailure { peer, error } => {
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
}
