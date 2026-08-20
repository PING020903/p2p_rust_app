use colored::Colorize;
use futures::StreamExt;
use libp2p::{
    identity::Keypair, mdns, multiaddr::Protocol, noise, ping,
    request_response::{self, ProtocolSupport},
    swarm::behaviour::toggle::Toggle,
    swarm::SwarmEvent,
    tcp, yamux, Multiaddr, PeerId, StreamProtocol, Swarm, SwarmBuilder,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error;
use std::io::{IsTerminal, Write};
use std::net::Ipv6Addr;
use std::time::{Duration, Instant};
use tokio::io::AsyncBufReadExt;

use crate::cmd_tree::{CmdError, CmdTree, ROOT};
use crate::p2p::mdns_stealth::StealthMdns;
use crate::p2p::{
    decrypt_mnemonic, fingerprint_of, generate_mnemonic, keypair_from_mnemonic,
    load_discovery_mode, load_keystores, probe_duplicate_id, probe_window,
    save_discovery_mode, save_keystore, valid_password, ContactBook, DiscoveryMode,
    IdentityInfo, LoginOutcome,
};

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

/// 新身份助记词抄写确认词数
const MNEMONIC_CONFIRM_WORDS: usize = 3;

/// mDNS 行为开关：Advertise 模式启用 libp2p-mdns；隐身/关闭模式用 Toggle 关闭
#[derive(libp2p::swarm::NetworkBehaviour)]
struct NodeBehaviour {
    mdns: Toggle<mdns::tokio::Behaviour>,
    ping: ping::Behaviour,
    chat: request_response::cbor::Behaviour<ChatRequest, ChatResponse>,
}

enum ChatAction {
    None,
    Quit,
    Dial(Multiaddr),
    Chat(String),
    List,
    Backup,
    Trust(String),
    Discover(DiscoveryMode),
}

struct ChatCtx {
    action: ChatAction,
}

/// 一个 1v1 会话：与某 peer 的聊天上下文（连接可多路共存）。
/// Phase 2 群聊将扩展该结构（群成员/群名）
struct Conversation {
    name: String,             // 对方角色名（Hello 更新；未知为空）
    greeted: bool,            // 是否已发过 Hello（重连后重置，避免漏问候）
    bye: bool,                // 对方已主动退出（不再心跳/重连）
    pending_dial: bool,       // /chat 后尚无地址，等待 mDNS 发现自动拨号
    last_rx: Option<Instant>, // 本会话最近收到消息/心跳响应时间（超时判离线）
}

impl Conversation {
    fn new() -> Self {
        Conversation {
            name: String::new(),
            greeted: false,
            bye: false,
            pending_dial: false,
            last_rx: None,
        }
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
    let backup = tree.register(ROOT, "backup", |ctx, _| ctx.action = ChatAction::Backup);
    tree.set_help(backup, "重新查看本身份助记词（需输入密码）");
    let trust = tree.register(ROOT, "trust", |ctx, args| {
        if args.is_empty() {
            eprintln!(
                "{}",
                "用法: /trust <角色名 或 节点ID>（加 ! 前缀取消信任）".yellow()
            );
            return;
        }
        ctx.action = ChatAction::Trust(args.join(" "));
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
        ctx.action = ChatAction::Discover(mode);
    });
    tree.set_help(discover, "设置 mDNS 发现模式（下次进入聊天生效）");
    tree
}

/// 将 peer 加入重连队列（不在当前重连中、且未在队列里才入队）
fn enqueue_reconnect(
    peer: PeerId,
    queue: &mut VecDeque<PeerId>,
    reconnect_peer: &Option<PeerId>,
) {
    if *reconnect_peer != Some(peer) && !queue.contains(&peer) {
        queue.push_back(peer);
    }
}

/// 重连驱动：依次处理重连队列，逐个尝试目标 peer 的已知地址。
/// 当前无在途目标时从队列取队首；地址耗尽则报失败、清地址、继续下一目标
fn dial_next_reconnect(
    swarm: &mut Swarm<NodeBehaviour>,
    dialing: &mut HashSet<PeerId>,
    reconnect_peer: &mut Option<PeerId>,
    pending: &mut Vec<Multiaddr>,
    known_addrs: &mut HashMap<PeerId, Vec<Multiaddr>>,
    queue: &mut VecDeque<PeerId>,
) {
    loop {
        if reconnect_peer.is_none() {
            let p = match queue.pop_front() {
                Some(p) => p,
                None => return,
            };
            *reconnect_peer = Some(p);
            *pending = known_addrs.get(&p).cloned().unwrap_or_default();
        }
        let target = reconnect_peer.as_ref().unwrap().clone();
        while let Some(ma) = pending.pop() {
            if swarm.dial(ma).is_ok() {
                dialing.insert(target);
                return;
            }
        }
        known_addrs.remove(&target);
        *reconnect_peer = None;
        eprintln!(
            "{}",
            format!("重连失败: {target} 的已知地址均无法连接，对方可能已退出").yellow()
        );
    }
}

/// 节点被发现（mDNS 广播 或 隐身监听）的公共处理：登记地址 + 待接呼叫自动拨号
fn on_peer_discovered(
    found_id: PeerId,
    addr: Multiaddr,
    local: &PeerId,
    swarm: &mut Swarm<NodeBehaviour>,
    known_addrs: &mut HashMap<PeerId, Vec<Multiaddr>>,
    conversations: &HashMap<PeerId, Conversation>,
    reconnect_peer: &mut Option<PeerId>,
    reconnect_pending: &mut Vec<Multiaddr>,
    dialing: &mut HashSet<PeerId>,
    conn_count: &HashMap<PeerId, u32>,
    queue: &mut VecDeque<PeerId>,
) {
    if found_id == *local {
        return;
    }
    println!("{}", format!("mDNS 发现节点: {found_id}").cyan());
    let recorded = known_addrs.entry(found_id).or_default();
    if !recorded.contains(&addr) {
        recorded.push(addr.clone());
    }
    let pending_dial = conversations
        .get(&found_id)
        .map(|c| c.pending_dial)
        .unwrap_or(false);
    if pending_dial
        && *reconnect_peer != Some(found_id)
        && !conn_count.contains_key(&found_id)
    {
        println!("{}", format!("发现待接呼叫节点，拨号 {found_id}").cyan());
        enqueue_reconnect(found_id, queue, reconnect_peer);
        dial_next_reconnect(
            swarm,
            dialing,
            reconnect_peer,
            reconnect_pending,
            known_addrs,
            queue,
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

/// 输入行迭代器（stdin 被管道接管时逐行读取）
type StdinLines = tokio::io::Lines<tokio::io::BufReader<tokio::io::Stdin>>;

async fn read_line(stdin: &mut StdinLines, prompt: &str) -> Result<String, Box<dyn Error>> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    match stdin.next_line().await? {
        Some(l) => Ok(l),
        None => Err("输入结束".into()),
    }
}

/// 读取密码：交互终端不回显（rpassword）；管道环境（测试/脚本）退回行读取
async fn read_secret(
    stdin: &mut StdinLines,
    interactive: bool,
    prompt: &str,
) -> Result<String, Box<dyn Error>> {
    if interactive {
        Ok(rpassword::prompt_password(prompt)?)
    } else {
        read_line(stdin, prompt).await
    }
}

/// 交互收集资料信息（姓名/生日/性别）
async fn prompt_profile(stdin: &mut StdinLines) -> Result<IdentityInfo, Box<dyn Error>> {
    let name = loop {
        let raw = read_line(stdin, "姓名: ").await?;
        let name = raw.trim().to_string();
        if name.is_empty() || name.len() > 64 {
            eprintln!("{}", "姓名不能为空且不超过 64 字节".yellow());
        } else {
            break name;
        }
    };
    let birthday = loop {
        let raw = read_line(stdin, "生日 (YYYY-MM-DD): ").await?;
        match normalize_birthday(&raw) {
            Ok(b) => break b,
            Err(reason) => eprintln!("{}", reason.yellow()),
        }
    };
    let gender = loop {
        let raw = read_line(stdin, "性别 (男/M 女/F 保密/O): ").await?;
        match normalize_gender(&raw) {
            Ok(g) => break g,
            Err(reason) => eprintln!("{}", reason.yellow()),
        }
    };
    Ok(IdentityInfo {
        name,
        birthday,
        gender,
    })
}

/// 交互收集并校验密码
async fn prompt_password(
    stdin: &mut StdinLines,
    interactive: bool,
) -> Result<String, Box<dyn Error>> {
    loop {
        let pwd = read_secret(stdin, interactive, "密码: ").await?;
        if valid_password(&pwd) {
            return Ok(pwd);
        }
        eprintln!("{}", "密码须为 8~128 字节".yellow());
    }
}

/// 展示助记词与安全提示
fn print_mnemonic_guide(phrase: &str) {
    println!("{}", "=".repeat(60).yellow());
    println!(
        "{}",
        "你的身份助记词（12 词，唯一备份；丢失即永久丢失身份，泄露即身份被窃取）:".yellow()
    );
    println!("{}", phrase.red());
    println!("{}", "=".repeat(60).yellow());
}

/// 登录流程：新身份生成 / 助记词恢复 / 缓存 keystore 解锁。
/// 新身份与恢复都会自动加密保存 keystore；同 ID 冲突由调用方在探测后处理。
async fn login_flow(
    stdin: &mut StdinLines,
    interactive: bool,
) -> Result<LoginOutcome, Box<dyn Error>> {
    loop {
        let cached = load_keystores();
        println!("{}", "[角色登录]".green());
        if cached.is_empty() {
            println!("{}", "暂无本地身份".dimmed());
        } else {
            println!("缓存身份:");
            for (i, (ks, info)) in cached.iter().enumerate() {
                println!("  {}. {}  ({})", i + 1, info.name, ks.peer_id);
            }
        }
        println!("  0. 新身份登录");
        println!("  r. 从助记词恢复");
        let input = read_line(stdin, "请选择: ").await?;
        let input = input.trim();

        if input == "0" {
            // 新身份：生成助记词，展示一次并要求抄写确认
            let info = prompt_profile(stdin).await?;
            let phrase = loop {
                let phrase = match generate_mnemonic() {
                    Ok(p) => p,
                    Err(reason) => {
                        eprintln!("{}", reason.red());
                        continue;
                    }
                };
                print_mnemonic_guide(&phrase);
                let confirm = read_line(
                    stdin,
                    &format!("请抄下助记词，输入前 {MNEMONIC_CONFIRM_WORDS} 个词确认: "),
                )
                .await?;
                let first: Vec<&str> = phrase
                    .split_whitespace()
                    .take(MNEMONIC_CONFIRM_WORDS)
                    .collect();
                let got: Vec<&str> = confirm.split_whitespace().collect();
                if got.len() >= MNEMONIC_CONFIRM_WORDS
                    && got[..MNEMONIC_CONFIRM_WORDS] == first[..]
                {
                    break phrase;
                }
                eprintln!("{}", "确认词不匹配，请重新抄写".yellow());
            };
            let password = prompt_password(stdin, interactive).await?;
            let keypair = keypair_from_mnemonic(&phrase)?;
            let peer_id = keypair.public().to_peer_id();
            save_keystore(&info, &peer_id, &phrase, &password)?;
            return Ok(LoginOutcome { keypair, info });
        } else if input == "r" {
            // 从助记词恢复身份（跨设备迁移 / 备份恢复）
            let phrase = read_line(stdin, "助记词（12 个英文词，空格分隔）: ").await?;
            let keypair = match keypair_from_mnemonic(&phrase) {
                Ok(kp) => kp,
                Err(reason) => {
                    eprintln!("{}", reason.red());
                    continue;
                }
            };
            let info = prompt_profile(stdin).await?;
            let password = prompt_password(stdin, interactive).await?;
            let peer_id = keypair.public().to_peer_id();
            save_keystore(&info, &peer_id, &phrase, &password)?;
            return Ok(LoginOutcome { keypair, info });
        } else if let Ok(n) = input.parse::<usize>() {
            if n >= 1 && n <= cached.len() {
                // 缓存解锁：密码错误最多重试 3 次
                let (ks, info) = &cached[n - 1];
                for _ in 0..3 {
                    let password = read_secret(stdin, interactive, "密码: ").await?;
                    if !valid_password(&password) {
                        eprintln!("{}", "密码须为 8~128 字节".yellow());
                        continue;
                    }
                    match decrypt_mnemonic(
                        &password,
                        &ks.salt,
                        &ks.nonce,
                        &ks.enc,
                        ks.kdf_m,
                        ks.kdf_t,
                        ks.kdf_p,
                    ) {
                        Ok(phrase) => match keypair_from_mnemonic(&phrase) {
                            Ok(kp) if kp.public().to_peer_id().to_string() == ks.peer_id => {
                                return Ok(LoginOutcome {
                                    keypair: kp,
                                    info: IdentityInfo {
                                        name: info.name.clone(),
                                        birthday: info.birthday.clone(),
                                        gender: info.gender,
                                    },
                                });
                            }
                            Ok(_) => {
                                eprintln!("{}", "keystore 与派生身份不符，数据可能损坏".red());
                            }
                            Err(reason) => {
                                eprintln!("{}", reason.red());
                            }
                        },
                        Err(reason) => {
                            eprintln!("{}", reason.red());
                        }
                    }
                }
                eprintln!("{}", "连续多次密码错误，返回选择菜单".yellow());
            } else {
                eprintln!("{}", "序号无效，请重新选择".yellow());
            }
        } else {
            eprintln!("{}", "无效选择，请输入序号、0 或 r".yellow());
        }
    }
}


fn build_swarm(keypair: Keypair, mode: DiscoveryMode) -> Result<Swarm<NodeBehaviour>, Box<dyn Error>> {
    let swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(|key| {
            let peer_id = key.public().to_peer_id();
            let mdns = match mode {
                DiscoveryMode::AdvertiseAndDiscover => {
                    Some(mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id)?)
                }
                DiscoveryMode::DiscoverOnly | DiscoveryMode::Off => None,
            };
            Ok(NodeBehaviour {
                mdns: Toggle::from(mdns),
                ping: ping::Behaviour::default(),
                chat: request_response::cbor::Behaviour::new(
                    [(StreamProtocol::new("/chat/4.0.0"), ProtocolSupport::Full)],
                    request_response::Config::default(),
                ),
            })
        })?
        .build();
    Ok(swarm)
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

    // 登录 + 影子探测；ID 冲突时退回重新选择身份
    let (mut swarm, my_name, discovery_mode) = loop {
        let outcome = login_flow(&mut stdin, interactive).await?;
        let real_id = outcome.keypair.public().to_peer_id();
        println!(
            "{}",
            format!("登录成功: {} (节点ID {real_id})", outcome.info.name).green()
        );
        match probe_duplicate_id(real_id, probe_window()).await? {
            Some(addr) => {
                eprintln!(
                    "{}",
                    format!("该角色 ID 已在线（发现于 {addr}），同一 ID 不能同时上线").red()
                );
                eprintln!(
                    "{}",
                    "请改用其他身份，或先关闭占用该 ID 的设备后重试".yellow()
                );
            }
            None => {
                let mode = load_discovery_mode(&real_id);
                break (build_swarm(outcome.keypair, mode)?, outcome.info.name, mode);
            }
        }
    };
    println!(
        "{}",
        format!("发现模式: {}", discovery_mode.name()).dimmed()
    );

    // TOFU 联系人簿（首次接触记录指纹）
    let mut contacts = ContactBook::load(swarm.local_peer_id());

    // 隐身模式：只收不发的 mDNS 监听任务，经通道向主循环上报发现
    let mut stealth_rx: Option<tokio::sync::mpsc::Receiver<(PeerId, Multiaddr)>> = None;
    if discovery_mode == DiscoveryMode::DiscoverOnly {
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        stealth_rx = Some(rx);
        tokio::spawn(async move {
            let mut listener = match StealthMdns::new() {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("{}", format!("隐身监听启动失败: {e}").yellow());
                    return;
                }
            };
            loop {
                match listener.next_discovery().await {
                    Some((pid, addr)) => {
                        if tx.send((pid, addr)).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        });
    }

    let listen_addr: Multiaddr = "/ip4/0.0.0.0/tcp/0".parse()?;
    swarm.listen_on(listen_addr)?;
    let mut v6_listen_issued = false;

    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    let mut conversations: HashMap<PeerId, Conversation> = HashMap::new();
    let mut focused: Option<PeerId> = None;
    let mut conn_count: HashMap<PeerId, u32> = HashMap::new();
    let mut dialing: HashSet<PeerId> = HashSet::new();
    let mut known_addrs: HashMap<PeerId, Vec<Multiaddr>> = HashMap::new();
    let mut reconnect_peer: Option<PeerId> = None;
    let mut reconnect_pending: Vec<Multiaddr> = Vec::new();
    let mut reconnect_queue: VecDeque<PeerId> = VecDeque::new();
    let mut user_dials: HashSet<PeerId> = HashSet::new();
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
                            let peers: Vec<PeerId> = conversations
                                .iter()
                                .filter(|(p, c)| conn_count.contains_key(p) && !c.bye)
                                .map(|(p, _)| *p)
                                .collect();
                            for p in peers {
                                let req_id = swarm.behaviour_mut().chat.send_request(
                                    &p,
                                    ChatRequest(ChatPayload::Control(Control::Bye)),
                                );
                                println!(
                                    "{}",
                                    format!("正在通知对方下线: {p}...").dimmed()
                                );
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
                            let resolved = conversations
                                .iter()
                                .find(|(_, c)| c.name == target)
                                .map(|(p, _)| *p)
                                .or_else(|| target.parse::<PeerId>().ok());
                            match resolved {
                                Some(p) => {
                                    if conn_count.contains_key(&p) {
                                        // 已连接：仅切换焦点
                                        focused = Some(p);
                                        let name = conversations
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
                                        conversations
                                            .entry(p)
                                            .or_insert_with(Conversation::new);
                                        focused = Some(p);
                                        let name = conversations[&p].name.clone();
                                        if name.is_empty() {
                                            conversations.get_mut(&p).unwrap().name =
                                                target.to_string();
                                        }
                                        match known_addrs.get(&p) {
                                            Some(addrs) if !addrs.is_empty() => {
                                                println!(
                                                    "{}",
                                                    format!("正在连接 {target}...").cyan()
                                                );
                                                conversations.get_mut(&p).unwrap().pending_dial =
                                                    false;
                                                enqueue_reconnect(
                                                    p,
                                                    &mut reconnect_queue,
                                                    &reconnect_peer,
                                                );
                                                dial_next_reconnect(
                                                    &mut swarm,
                                                    &mut dialing,
                                                    &mut reconnect_peer,
                                                    &mut reconnect_pending,
                                                    &mut known_addrs,
                                                    &mut reconnect_queue,
                                                );
                                            }
                                            _ => {
                                                conversations.get_mut(&p).unwrap().pending_dial =
                                                    true;
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
                                    let who = conversations
                                        .get(p)
                                        .map(|c| c.name.as_str())
                                        .unwrap_or("未知");
                                    let state = if focused == Some(*p) {
                                        "当前会话"
                                    } else if conn_count.contains_key(p) {
                                        "已连接"
                                    } else {
                                        "离线"
                                    };
                                    let trust_badge = if contacts.verified(&id_str) {
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
                        }
                        ChatAction::Backup => {
                            let my_id = swarm.local_peer_id().to_string();
                            let stored = load_keystores();
                            if let Some((ks, _)) =
                                stored.iter().find(|(k, _)| k.peer_id == my_id)
                            {
                                println!("{}", "请输入密码以解锁本身份".yellow());
                                let password =
                                    read_secret(&mut stdin, interactive, "密码: ").await?;
                                match decrypt_mnemonic(
                                    &password,
                                    &ks.salt,
                                    &ks.nonce,
                                    &ks.enc,
                                    ks.kdf_m,
                                    ks.kdf_t,
                                    ks.kdf_p,
                                ) {
                                    Ok(phrase) => {
                                        print_mnemonic_guide(&phrase);
                                        println!(
                                            "{}",
                                            "助记词是唯一备份，请妥善保管".dimmed()
                                        );
                                    }
                                    Err(reason) => eprintln!("{}", reason.red()),
                                }
                            } else {
                                eprintln!(
                                    "{}",
                                    "未找到本身份的 keystore（身份未在本机加密保存过）".yellow()
                                );
                            }
                        }
                        ChatAction::Trust(target) => {
                            let (untrust, target) = match target.strip_prefix('!') {
                                Some(stripped) => (true, stripped.to_string()),
                                None => (false, target),
                            };
                            let resolved = conversations
                                .iter()
                                .find(|(_, c)| c.name == target)
                                .map(|(p, _)| *p)
                                .or_else(|| target.parse::<PeerId>().ok());
                            match resolved {
                                Some(p) => {
                                    let name = conversations
                                        .get(&p)
                                        .map(|c| c.name.clone())
                                        .unwrap_or_else(|| "未知".to_string());
                                    contacts.ensure_contact(&p, &name, !untrust);
                                    if untrust {
                                        println!(
                                            "{}",
                                            format!("已取消信任: {name}").yellow()
                                        );
                                    } else {
                                        println!("{}", format!("已信任: {name}").green());
                                    }
                                }
                                None => eprintln!(
                                    "{}",
                                    "未知节点，无法标记信任（用 /list 查看）".yellow()
                                ),
                            }
                        }
                        ChatAction::Discover(mode) => {
                            match save_discovery_mode(swarm.local_peer_id(), mode) {
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
                        }
                        ChatAction::None => {}
                    }
                    continue;
                }
                match focused {
                    Some(p) => {
                        if !conn_count.contains_key(&p) {
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
                            swarm.behaviour_mut().chat.send_request(
                                &p,
                                ChatRequest(ChatPayload::Text(line.to_string())),
                            );
                            println!("{}", format!("[我 -> {who}] {line}").green());
                        }
                    }
                    None => eprintln!(
                        "{}",
                        "尚未选择会话，无法发送（先 /chat <角色>）".yellow()
                    ),
                }
            }
            _ = heartbeat.tick() => {
                if let Some(p) = focused {
                    if let Some(conv) = conversations.get_mut(&p) {
                        if !conv.bye {
                            if conv.last_rx.is_some_and(|t| t.elapsed() > HEARTBEAT_TIMEOUT) {
                                println!(
                                    "{}",
                                    format!(
                                        "心跳超时（超过 {} 秒无响应），判定对方离线",
                                        HEARTBEAT_TIMEOUT.as_secs()
                                    )
                                    .yellow()
                                );
                                conv.last_rx = None;
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
            }
            discovered = async {
                match stealth_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some((found_id, addr)) = discovered {
                    let local = swarm.local_peer_id().clone();
                    on_peer_discovered(
                        found_id,
                        addr,
                        &local,
                        &mut swarm,
                        &mut known_addrs,
                        &conversations,
                        &mut reconnect_peer,
                        &mut reconnect_pending,
                        &mut dialing,
                        &conn_count,
                        &mut reconnect_queue,
                    );
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
                            dial_next_reconnect(
                                &mut swarm,
                                &mut dialing,
                                &mut reconnect_peer,
                                &mut reconnect_pending,
                                &mut known_addrs,
                                &mut reconnect_queue,
                            );
                        }
                        *conn_count.entry(peer_id).or_insert(0) += 1;
                        let conv = conversations
                            .entry(peer_id)
                            .or_insert_with(Conversation::new);
                        conv.pending_dial = false;
                        conv.last_rx = Some(Instant::now());
                        if focused.is_none() {
                            focused = Some(peer_id);
                            if !conv.name.is_empty() {
                                println!(
                                    "{}",
                                    format!(
                                        "已切换到会话: {}（{peer_id}）",
                                        conv.name
                                    )
                                    .green()
                                );
                            }
                        }
                        println!("{}", format!("已连接对端: {peer_id}").green());
                        if !conv.greeted {
                            swarm.behaviour_mut().chat.send_request(
                                &peer_id,
                                ChatRequest(ChatPayload::Control(Control::Hello(
                                    my_name.clone(),
                                ))),
                            );
                            conv.greeted = true;
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
                            if let Some(conv) = conversations.get_mut(&peer_id) {
                                conv.greeted = false;
                                conv.last_rx = None;
                            }
                            conn_count.remove(&peer_id);
                            if focused == Some(peer_id) {
                                focused = None;
                                eprintln!(
                                    "{}",
                                    format!(
                                        "当前会话已断开（{peer_id}），用 /chat 重新选择"
                                    )
                                    .yellow()
                                );
                            }
                            let bye = conversations
                                .get(&peer_id)
                                .map(|c| c.bye)
                                .unwrap_or(false);
                            if bye {
                                known_addrs.remove(&peer_id);
                                println!("{}", "对方已正常退出，不进行重连".dimmed());
                            } else if let Some(addrs) = known_addrs.get(&peer_id) {
                                if !addrs.is_empty() {
                                    println!(
                                        "{}",
                                        format!("尝试重连 {peer_id}...").cyan()
                                    );
                                    enqueue_reconnect(
                                        peer_id,
                                        &mut reconnect_queue,
                                        &reconnect_peer,
                                    );
                                    dial_next_reconnect(
                                        &mut swarm,
                                        &mut dialing,
                                        &mut reconnect_peer,
                                        &mut reconnect_pending,
                                        &mut known_addrs,
                                        &mut reconnect_queue,
                                    );
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
                                    &mut reconnect_queue,
                                );
                            } else if focused.is_none() {
                                if let Some(addrs) = known_addrs.get(&p) {
                                    if !addrs.is_empty() {
                                        println!(
                                            "{}",
                                            format!("拨号失败，尝试 {p} 的其他已知地址...").cyan()
                                        );
                                        enqueue_reconnect(p, &mut reconnect_queue, &reconnect_peer);
                                        dial_next_reconnect(
                                            &mut swarm,
                                            &mut dialing,
                                            &mut reconnect_peer,
                                            &mut reconnect_pending,
                                            &mut known_addrs,
                                            &mut reconnect_queue,
                                        );
                                    }
                                }
                            }
                        } else {
                            eprintln!("{}", format!("拨号失败: {error}").red());
                        }
                    }
                    SwarmEvent::Behaviour(NodeBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                        let local = swarm.local_peer_id().clone();
                        for (found_id, addr) in list {
                            on_peer_discovered(
                                found_id,
                                addr,
                                &local,
                                &mut swarm,
                                &mut known_addrs,
                                &conversations,
                                &mut reconnect_peer,
                                &mut reconnect_pending,
                                &mut dialing,
                                &conn_count,
                                &mut reconnect_queue,
                            );
                        }
                    }
                    SwarmEvent::Behaviour(NodeBehaviourEvent::Chat(request_response::Event::Message { peer: from, message, .. })) => {
                        match message {
                            request_response::Message::Request { request, channel, .. } => {
                                let conv = conversations
                                    .entry(from)
                                    .or_insert_with(Conversation::new);
                                conv.last_rx = Some(Instant::now());
                                match request.0 {
                                    ChatPayload::Text(text) => {
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
                                            conv.name = name.clone();
                                            println!(
                                                "{}",
                                                format!("对方已上线: {name}").green()
                                            );
                                            let pid = from.to_string();
                                            if contacts.get(&pid).is_none() {
                                                // 首次接触：TOFU 指纹核对。
                                                // 交互终端须人工确认；管道环境（脚本/自动化）
                                                // 无法交互，采用 SSH accept-new 语义自动信任
                                                if interactive {
                                                    println!(
                                                        "{}",
                                                        "首次连接，请核对对方身份指纹:".yellow()
                                                    );
                                                    println!(
                                                        "  指纹: {}",
                                                        fingerprint_of(&from).dimmed()
                                                    );
                                                    println!("  节点ID: {pid}");
                                                    let ans = read_line(
                                                        &mut stdin,
                                                        "是否信任该节点（记录为联系人）? (y/n): ",
                                                    )
                                                    .await?;
                                                    let trusted =
                                                        ans.trim().eq_ignore_ascii_case("y");
                                                    contacts.ensure_contact(
                                                        &from,
                                                        &name,
                                                        trusted,
                                                    );
                                                    if trusted {
                                                        println!(
                                                            "{}",
                                                            format!("已记录并信任: {name}").green()
                                                        );
                                                    } else {
                                                        println!(
                                                            "{}",
                                                            format!("已记录但未信任: {name}").yellow()
                                                        );
                                                    }
                                                } else {
                                                    contacts.ensure_contact(&from, &name, true);
                                                }
                                            } else {
                                                contacts.ensure_contact(&from, &name, false);
                                            }
                                        }
                                        Control::Bye => {
                                            println!("{}", "对方已正常退出".yellow());
                                            conv.bye = true;
                                        }
                                    },
                                }
                                let _ = swarm.behaviour_mut().chat.send_response(channel, ChatResponse(true));
                            }
                            request_response::Message::Response { .. } => {
                                let conv = conversations
                                    .entry(from)
                                    .or_insert_with(Conversation::new);
                                conv.last_rx = Some(Instant::now());
                            }
                        }
                    }
                    SwarmEvent::Behaviour(NodeBehaviourEvent::Chat(request_response::Event::OutboundFailure { peer: p, error, .. })) => {
                        let bye = conversations.get(&p).map(|c| c.bye).unwrap_or(false);
                        if bye || focused != Some(p) {
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

    #[test]
    fn birthday_normalization() {
        assert_eq!(normalize_birthday("1990-1-1").unwrap(), "1990-01-01");
        assert_eq!(normalize_birthday(" 2000-12-05 ").unwrap(), "2000-12-05");
        assert!(normalize_birthday("1990/1/1").is_err());
        assert!(normalize_birthday("1899-01-01").is_err());
        assert!(normalize_birthday("1990-13-01").is_err());
        assert!(normalize_birthday("1990-01-32").is_err());
    }

    #[test]
    fn gender_normalization() {
        assert_eq!(normalize_gender("男").unwrap(), 'M');
        assert_eq!(normalize_gender("m").unwrap(), 'M');
        assert_eq!(normalize_gender("女").unwrap(), 'F');
        assert_eq!(normalize_gender("保密").unwrap(), 'O');
        assert!(normalize_gender("x").is_err());
    }
}
