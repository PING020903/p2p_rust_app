use colored::Colorize;
use futures::StreamExt;
use libp2p::{
    identity::Keypair, mdns, multiaddr::Protocol, noise, ping,
    request_response::{self, ProtocolSupport},
    swarm::SwarmEvent,
    tcp, yamux, Multiaddr, PeerId, StreamProtocol, Swarm, SwarmBuilder,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::io::{IsTerminal, Write};
use std::net::Ipv6Addr;
use std::time::{Duration, Instant};
use tokio::io::AsyncBufReadExt;

use crate::cmd_tree::{CmdError, CmdTree, ROOT};

/// 控制面：协议信令，静默处理，不作为聊天内容显示
#[derive(Debug, Clone, Serialize, Deserialize)]
enum Control {
    Heartbeat,
    Hello(String),
    Bye,
}

/// 数据面（Text/Binary）+ 控制面（Control）
#[derive(Debug, Clone, Serialize, Deserialize)]
enum ChatPayload {
    Text(String),
    Binary { name: String, data: Vec<u8> },
    Control(Control),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatRequest(ChatPayload);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatResponse(bool);

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(15);
const BYE_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// Argon2id 派生参数（v1 常量：参数是角色 ID 的一部分，改动将导致所有 ID 变更）
const ARGON2_M_KIB: u32 = 19456;
const ARGON2_T: u32 = 2;
const ARGON2_P: u32 = 1;

#[derive(libp2p::swarm::NetworkBehaviour)]
struct NodeBehaviour {
    mdns: mdns::tokio::Behaviour,
    ping: ping::Behaviour,
    chat: request_response::cbor::Behaviour<ChatRequest, ChatResponse>,
}

enum ChatAction {
    None,
    Quit,
    Dial(Multiaddr),
    Chat(String),
    List,
}

struct ChatCtx {
    action: ChatAction,
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

fn build_tree() -> CmdTree<ChatCtx> {
    let mut tree: CmdTree<ChatCtx> = CmdTree::new();
    let dial = tree.register(ROOT, "dial", |ctx, args| {
        if args.is_empty() {
            print_dial_template();
            return;
        }
        let raw = args.join(" ");
        match parse_dial_addr(&raw) {
            Ok(ma) => ctx.action = ChatAction::Dial(ma),
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
        ctx.action = ChatAction::Chat(args.join(" "));
    });
    tree.set_help(chat, "按完整角色名或完整节点ID发起 1v1 聊天");
    let list = tree.register(ROOT, "list", |ctx, _| ctx.action = ChatAction::List);
    tree.set_help(list, "列出已登记节点与状态");
    let quit = tree.register(ROOT, "quit", |ctx, _| ctx.action = ChatAction::Quit);
    tree.set_help(quit, "退出聊天");
    let q = tree.register(ROOT, "q", |ctx, _| ctx.action = ChatAction::Quit);
    tree.set_help(q, "退出聊天");
    tree
}

fn dial_next_reconnect(
    swarm: &mut Swarm<NodeBehaviour>,
    dialing: &mut HashSet<PeerId>,
    reconnect_peer: &mut Option<PeerId>,
    pending: &mut Vec<Multiaddr>,
    known_addrs: &mut HashMap<PeerId, Vec<Multiaddr>>,
) {
    while let Some(ma) = pending.pop() {
        if swarm.dial(ma).is_ok() {
            if let Some(p) = reconnect_peer {
                dialing.insert(*p);
            }
            return;
        }
    }
    if let Some(p) = reconnect_peer.take() {
        known_addrs.remove(&p);
        eprintln!(
            "{}",
            "重连失败: 已知地址均无法连接，对方可能已退出".yellow()
        );
    }
}

fn normalize_birthday(raw: &str) -> Result<String, String> {
    let parts: Vec<&str> = raw.trim().split('-').collect();
    if parts.len() != 3 {
        return Err("生日格式应为 YYYY-MM-DD，如 1990-01-01".into());
    }
    let (y, m, d): (u32, u32, u32) = (
        parts[0]
            .parse()
            .map_err(|_| "年份应为数字".to_string())?,
        parts[1]
            .parse()
            .map_err(|_| "月份应为数字".to_string())?,
        parts[2]
            .parse()
            .map_err(|_| "日期应为数字".to_string())?,
    );
    if !(1900..=2100).contains(&y) {
        return Err(format!("年份 {y} 超出范围 1900-2100"));
    }
    if !(1..=12).contains(&m) {
        return Err(format!("月份 {m} 超出范围 1-12"));
    }
    if !(1..=31).contains(&d) {
        return Err(format!("日期 {d} 超出范围 1-31"));
    }
    Ok(format!("{y:04}-{m:02}-{d:02}"))
}

fn normalize_gender(raw: &str) -> Result<char, String> {
    match raw.trim() {
        "男" | "M" | "m" => Ok('M'),
        "女" | "F" | "f" => Ok('F'),
        "保密" | "O" | "o" => Ok('O'),
        other => Err(format!("性别须为 男/M、女/F 或 保密/O，当前: {other}")),
    }
}

/// 角色身份派生：salt = SHA-256(姓名|生日|性别)，seed = Argon2id(密码, salt)，
/// 再由 32 字节种子确定性生成 Ed25519 密钥对。同样的信息在任何机器登录都得到同一 ID。
fn derive_identity(
    name: &str,
    birthday: &str,
    gender: char,
    password: &str,
) -> Result<Keypair, String> {
    let salt: [u8; 32] =
        Sha256::digest(format!("{name}|{birthday}|{gender}").as_bytes()).into();
    let params = argon2::Params::new(ARGON2_M_KIB, ARGON2_T, ARGON2_P, Some(32))
        .map_err(|e| format!("Argon2 参数错误: {e}"))?;
    let argon2 = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut seed = [0u8; 32];
    argon2
        .hash_password_into(password.as_bytes(), &salt, &mut seed)
        .map_err(|e| format!("密钥派生失败: {e}"))?;
    Keypair::ed25519_from_bytes(seed).map_err(|e| format!("生成密钥失败: {e}"))
}

/// 角色登录：收集四项信息并派生身份。交互终端下密码不回显（rpassword）；
/// stdin 被管道接管时（测试/脚本）退回普通行读取。输入不合法循环重问。
async fn login(
    stdin: &mut tokio::io::Lines<tokio::io::BufReader<tokio::io::Stdin>>,
    interactive: bool,
) -> Result<(Keypair, String), Box<dyn Error>> {
    loop {
        print!("姓名: ");
        std::io::stdout().flush()?;
        let name = match stdin.next_line().await? {
            Some(l) => l.trim().to_string(),
            None => return Err("输入结束，无法登录".into()),
        };
        if name.is_empty() || name.len() > 64 {
            eprintln!("{}", "姓名不能为空且不超过 64 字节".yellow());
            continue;
        }
        print!("生日 (YYYY-MM-DD): ");
        std::io::stdout().flush()?;
        let birthday_raw = match stdin.next_line().await? {
            Some(l) => l,
            None => return Err("输入结束，无法登录".into()),
        };
        let birthday = match normalize_birthday(&birthday_raw) {
            Ok(b) => b,
            Err(reason) => {
                eprintln!("{}", reason.yellow());
                continue;
            }
        };
        print!("性别 (男/M 女/F 保密/O): ");
        std::io::stdout().flush()?;
        let gender_raw = match stdin.next_line().await? {
            Some(l) => l,
            None => return Err("输入结束，无法登录".into()),
        };
        let gender = match normalize_gender(&gender_raw) {
            Ok(g) => g,
            Err(reason) => {
                eprintln!("{}", reason.yellow());
                continue;
            }
        };
        let password = if interactive {
            rpassword::prompt_password("密码: ")?
        } else {
            print!("密码: ");
            std::io::stdout().flush()?;
            match stdin.next_line().await? {
                Some(l) => l,
                None => return Err("输入结束，无法登录".into()),
            }
        };
        if password.is_empty() || password.len() > 128 {
            eprintln!("{}", "密码不能为空且不超过 128 字节".yellow());
            continue;
        }
        println!("{}", "正在派生角色身份...".dimmed());
        match derive_identity(&name, &birthday, gender, &password) {
            Ok(kp) => return Ok((kp, name)),
            Err(reason) => {
                eprintln!("{}", reason.red());
                continue;
            }
        }
    }
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

    println!("{}", "[角色登录]".green());
    let (keypair, my_name) = login(&mut stdin, interactive).await?;
    let local_id = keypair.public().to_peer_id();
    println!(
        "{}",
        format!("登录成功: {my_name} (节点ID {local_id})").green()
    );

    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(|key| {
            let peer_id = key.public().to_peer_id();
            Ok(NodeBehaviour {
                mdns: mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id)?,
                ping: ping::Behaviour::default(),
                chat: request_response::cbor::Behaviour::new(
                    [(StreamProtocol::new("/chat/4.0.0"), ProtocolSupport::Full)],
                    request_response::Config::default(),
                ),
            })
        })?
        .build();

    let listen_addr: Multiaddr = "/ip4/0.0.0.0/tcp/0".parse()?;
    swarm.listen_on(listen_addr)?;
    let mut v6_listen_issued = false;

    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    let mut active: Option<PeerId> = None;
    let mut conn_count: HashMap<PeerId, u32> = HashMap::new();
    let mut names: HashMap<PeerId, String> = HashMap::new();
    let mut dialing: HashSet<PeerId> = HashSet::new();
    let mut known_addrs: HashMap<PeerId, Vec<Multiaddr>> = HashMap::new();
    let mut reconnect_peer: Option<PeerId> = None;
    let mut reconnect_pending: Vec<Multiaddr> = Vec::new();
    let mut last_rx: Option<Instant> = None;
    let mut bye_peers: HashSet<PeerId> = HashSet::new();
    let mut greeted: HashSet<PeerId> = HashSet::new();
    let mut user_dials: HashSet<PeerId> = HashSet::new();
    let mut pending_chat: Option<PeerId> = None;
    let mut ctx = ChatCtx {
        action: ChatAction::None,
    };
    let mut tree = build_tree();

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
                    ctx.action = ChatAction::None;
                    if let Err(CmdError::NotFound) = tree.parse(cmd, &mut ctx) {
                        eprintln!("{}", format!("未知命令: {cmd}").yellow());
                    }
                    match ctx.action {
                        ChatAction::Quit => {
                            if let Some(p) = active {
                                let req_id = swarm.behaviour_mut().chat.send_request(
                                    &p,
                                    ChatRequest(ChatPayload::Control(Control::Bye)),
                                );
                                println!("{}", "正在通知对方下线...".dimmed());
                                let deadline =
                                    tokio::time::Instant::now() + BYE_HANDSHAKE_TIMEOUT;
                                loop {
                                    tokio::select! {
                                        _ = tokio::time::sleep_until(deadline) => break,
                                        event = swarm.select_next_some() => {
                                            let done = match event {
                                                SwarmEvent::Behaviour(NodeBehaviourEvent::Chat(
                                                    request_response::Event::Message {
                                                        message:
                                                            request_response::Message::Response {
                                                                request_id,
                                                                ..
                                                            },
                                                        ..
                                                    },
                                                )) => request_id == req_id,
                                                SwarmEvent::Behaviour(NodeBehaviourEvent::Chat(
                                                    request_response::Event::OutboundFailure {
                                                        request_id,
                                                        ..
                                                    },
                                                )) => request_id == req_id,
                                                SwarmEvent::Behaviour(NodeBehaviourEvent::Chat(
                                                    request_response::Event::Message {
                                                        message:
                                                            request_response::Message::Request {
                                                                channel,
                                                                ..
                                                            },
                                                        ..
                                                    },
                                                )) => {
                                                    let _ = swarm
                                                        .behaviour_mut()
                                                        .chat
                                                        .send_response(channel, ChatResponse(true));
                                                    false
                                                }
                                                _ => false,
                                            };
                                            if done {
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                            break;
                        }
                        ChatAction::Dial(ma) => {
                            let target = ma.iter().find_map(|p| match p {
                                Protocol::P2p(pid) => Some(pid),
                                _ => None,
                            });
                            if let Some(p) = target {
                                let recorded = known_addrs.entry(p).or_default();
                                if !recorded.contains(&ma) {
                                    recorded.push(ma.clone());
                                }
                            }
                            match swarm.dial(ma) {
                                Ok(()) => {
                                    if let Some(p) = target {
                                        dialing.insert(p);
                                        user_dials.insert(p);
                                    }
                                }
                                Err(e) => {
                                    eprintln!("{}", format!("拨号失败: {e}").red());
                                }
                            }
                        }
                        ChatAction::Chat(target) => {
                            let resolved = names
                                .iter()
                                .find(|(_, n)| n.as_str() == target)
                                .map(|(p, _)| *p)
                                .or_else(|| target.parse::<PeerId>().ok());
                            match resolved {
                                Some(p) => {
                                    pending_chat = Some(p);
                                    active = Some(p);
                                    match known_addrs.get(&p) {
                                        Some(addrs) if !addrs.is_empty() => {
                                            println!(
                                                "{}",
                                                format!("正在连接 {target}...").cyan()
                                            );
                                            reconnect_peer = Some(p);
                                            reconnect_pending = addrs.clone();
                                            dial_next_reconnect(
                                                &mut swarm,
                                                &mut dialing,
                                                &mut reconnect_peer,
                                                &mut reconnect_pending,
                                                &mut known_addrs,
                                            );
                                        }
                                        _ => println!(
                                            "{}",
                                            "该节点暂无已知地址，等待 mDNS 发现，发现后自动连接".cyan()
                                        ),
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
                        }
                        ChatAction::List => {
                            if known_addrs.is_empty() {
                                println!(
                                    "{}",
                                    "暂无已登记节点（等待 mDNS 发现或用 /dial 直连）".dimmed()
                                );
                            } else {
                                println!("{}", "=== 已登记节点 ===".cyan());
                                let mut entries: Vec<(String, &PeerId, usize)> = known_addrs
                                    .iter()
                                    .map(|(p, addrs)| (p.to_string(), p, addrs.len()))
                                    .collect();
                                entries.sort();
                                for (id_str, p, addr_n) in entries {
                                    let who = names
                                        .get(p)
                                        .map(String::as_str)
                                        .unwrap_or("未知");
                                    let state = if active == Some(*p) {
                                        "当前聊天"
                                    } else if conn_count.contains_key(p) {
                                        "已连接"
                                    } else {
                                        "离线"
                                    };
                                    println!("  {who}  {id_str}  [{state}]  地址数 {addr_n}");
                                }
                            }
                        }
                        ChatAction::None => {}
                    }
                    continue;
                }
                match active {
                    Some(p) => {
                        swarm.behaviour_mut().chat.send_request(
                            &p,
                            ChatRequest(ChatPayload::Text(line.to_string())),
                        );
                        println!("{}", format!("[我] {line}").green());
                    }
                    None => eprintln!("{}", "尚未连接对端，无法发送".yellow()),
                }
            }
            _ = heartbeat.tick() => {
                if let Some(p) = active {
                    if !bye_peers.contains(&p) {
                        if last_rx.is_some_and(|t| t.elapsed() > HEARTBEAT_TIMEOUT) {
                            println!(
                                "{}",
                                format!(
                                    "心跳超时（超过 {} 秒无响应），判定对方离线",
                                    HEARTBEAT_TIMEOUT.as_secs()
                                )
                                .yellow()
                            );
                            last_rx = None;
                            let _ = swarm.disconnect_peer_id(p);
                            continue;
                        }
                        swarm.behaviour_mut().chat.send_request(
                            &p,
                            ChatRequest(ChatPayload::Control(Control::Heartbeat)),
                        );
                    }
                }
            }
            event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::NewListenAddr { address, .. } => {
                        println!("{}", format!("监听地址: {address}/p2p/{}", swarm.local_peer_id()).green());
                        if !v6_listen_issued
                            && address.iter().any(|p| matches!(p, Protocol::Ip4(_)))
                        {
                            v6_listen_issued = true;
                            let port = address.iter().find_map(|p| match p {
                                Protocol::Tcp(port) => Some(port),
                                _ => None,
                            });
                            let mut v6_addr = Multiaddr::empty();
                            v6_addr.push(Protocol::Ip6(Ipv6Addr::UNSPECIFIED));
                            v6_addr.push(Protocol::Tcp(port.unwrap_or(0)));
                            if let Err(e) = swarm.listen_on(v6_addr) {
                                eprintln!(
                                    "{}",
                                    format!("ip6 复用 ip4 端口监听失败({e})，改用随机端口").yellow()
                                );
                                if let Ok(fallback) = "/ip6/::/tcp/0".parse::<Multiaddr>() {
                                    let _ = swarm.listen_on(fallback);
                                }
                            }
                        }
                    }
                    SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                        dialing.remove(&peer_id);
                        if reconnect_peer == Some(peer_id) {
                            reconnect_peer = None;
                            reconnect_pending.clear();
                        }
                        if pending_chat == Some(peer_id) {
                            pending_chat = None;
                        }
                        *conn_count.entry(peer_id).or_insert(0) += 1;
                        if active.is_none() {
                            active = Some(peer_id);
                        }
                        last_rx = Some(Instant::now());
                        println!("{}", format!("已连接对端: {peer_id}").green());
                        if greeted.insert(peer_id) {
                            swarm.behaviour_mut().chat.send_request(
                                &peer_id,
                                ChatRequest(ChatPayload::Control(Control::Hello(
                                    my_name.clone(),
                                ))),
                            );
                        }
                    }
                    SwarmEvent::ConnectionClosed {
                        peer_id,
                        num_established,
                        cause,
                        ..
                    } => {
                        let cause_text = match &cause {
                            Some(c) => format!("，原因: {c}"),
                            None => String::new(),
                        };
                        println!(
                            "{}",
                            format!("连接已关闭: {peer_id}（剩余连接 {num_established}{cause_text}）")
                                .yellow()
                        );
                        if num_established == 0 {
                            dialing.remove(&peer_id);
                            greeted.remove(&peer_id);
                            conn_count.remove(&peer_id);
                            if active == Some(peer_id) {
                                active = None;
                                last_rx = None;
                                if bye_peers.contains(&peer_id) {
                                    known_addrs.remove(&peer_id);
                                    println!("{}", "对方已正常退出，不进行重连".dimmed());
                                } else if let Some(addrs) = known_addrs.get(&peer_id) {
                                    if !addrs.is_empty() {
                                        println!(
                                            "{}",
                                            format!("尝试重连 {peer_id}...").cyan()
                                        );
                                        reconnect_peer = Some(peer_id);
                                        reconnect_pending = addrs.clone();
                                        dial_next_reconnect(
                                            &mut swarm,
                                            &mut dialing,
                                            &mut reconnect_peer,
                                            &mut reconnect_pending,
                                            &mut known_addrs,
                                        );
                                    }
                                }
                            }
                        } else {
                            conn_count.insert(peer_id, num_established);
                        }
                    }
                    SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                        if let Some(p) = peer_id {
                            if user_dials.remove(&p) {
                                eprintln!("{}", format!("拨号 {p} 失败: {error}").red());
                            } else {
                                eprintln!(
                                    "{}",
                                    format!("拨号 {p} 失败（自动恢复中）: {error}").dimmed()
                                );
                            }
                            dialing.remove(&p);
                            if reconnect_peer == Some(p) {
                                dial_next_reconnect(
                                    &mut swarm,
                                    &mut dialing,
                                    &mut reconnect_peer,
                                    &mut reconnect_pending,
                                    &mut known_addrs,
                                );
                            } else if active.is_none() {
                                if let Some(addrs) = known_addrs.get(&p) {
                                    if !addrs.is_empty() {
                                        println!(
                                            "{}",
                                            format!("拨号失败，尝试 {p} 的其他已知地址...").cyan()
                                        );
                                        reconnect_peer = Some(p);
                                        reconnect_pending = addrs.clone();
                                        dial_next_reconnect(
                                            &mut swarm,
                                            &mut dialing,
                                            &mut reconnect_peer,
                                            &mut reconnect_pending,
                                            &mut known_addrs,
                                        );
                                    }
                                }
                            }
                        } else {
                            eprintln!("{}", format!("拨号失败: {error}").red());
                        }
                    }
                    SwarmEvent::Behaviour(NodeBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                        for (found_id, addr) in list {
                            if found_id == *swarm.local_peer_id() {
                                continue;
                            }
                            println!("{}", format!("mDNS 发现节点: {found_id}").cyan());
                            let recorded = known_addrs.entry(found_id).or_default();
                            if !recorded.contains(&addr) {
                                recorded.push(addr.clone());
                            }
                            if pending_chat == Some(found_id)
                                && reconnect_peer != Some(found_id)
                                && !conn_count.contains_key(&found_id)
                            {
                                println!(
                                    "{}",
                                    format!("发现待接呼叫节点，拨号 {found_id}").cyan()
                                );
                                reconnect_peer = Some(found_id);
                                reconnect_pending = vec![addr];
                                dial_next_reconnect(
                                    &mut swarm,
                                    &mut dialing,
                                    &mut reconnect_peer,
                                    &mut reconnect_pending,
                                    &mut known_addrs,
                                );
                            }
                        }
                    }
                    SwarmEvent::Behaviour(NodeBehaviourEvent::Chat(request_response::Event::Message { peer: from, message, .. })) => {
                        match message {
                            request_response::Message::Request { request, channel, .. } => {
                                last_rx = Some(Instant::now());
                                match request.0 {
                                    ChatPayload::Text(text) => {
                                        println!("{}", format!("[对方] {text}").bright_cyan());
                                    }
                                    ChatPayload::Binary { name, data } => {
                                        println!(
                                            "{}",
                                            format!(
                                                "[对方] 收到二进制数据 '{name}'（{} 字节），文件功能未实现",
                                                data.len()
                                            )
                                            .dimmed()
                                        );
                                    }
                                    ChatPayload::Control(ctrl) => match ctrl {
                                        Control::Heartbeat => {}
                                        Control::Hello(name) => {
                                            names.insert(from, name.clone());
                                            println!(
                                                "{}",
                                                format!("对方已上线: {name}").green()
                                            );
                                        }
                                        Control::Bye => {
                                            println!("{}", "对方已正常退出".yellow());
                                            bye_peers.insert(from);
                                        }
                                    },
                                }
                                let _ = swarm.behaviour_mut().chat.send_response(channel, ChatResponse(true));
                            }
                            request_response::Message::Response { .. } => {
                                last_rx = Some(Instant::now());
                            }
                        }
                    }
                    SwarmEvent::Behaviour(NodeBehaviourEvent::Chat(request_response::Event::OutboundFailure { peer: p, error, .. })) => {
                        if bye_peers.contains(&p) || active != Some(p) {
                            eprintln!(
                                "{}",
                                format!("发送到 {p} 失败（对方正在退出或已离线）: {error}").dimmed()
                            );
                        } else {
                            eprintln!("{}", format!("发送到 {p} 失败: {error}").red());
                        }
                    }
                    _ => {}
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
}
