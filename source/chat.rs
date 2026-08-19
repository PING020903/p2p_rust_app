use bip39::Mnemonic;
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Key, Nonce,
};
use colored::Colorize;
use futures::StreamExt;
use libp2p::{
    identity::Keypair, mdns, multiaddr::Protocol, noise, ping,
    request_response::{self, ProtocolSupport},
    swarm::SwarmEvent,
    tcp, yamux, Multiaddr, PeerId, StreamProtocol, Swarm, SwarmBuilder,
};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::io::{IsTerminal, Write};
use std::net::Ipv6Addr;
use std::path::PathBuf;
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

/// Argon2id KDF 参数（仅用于本地 keystore 加密；逐文件记录，可独立升级，不再绑定身份）
const ARGON2_M_KIB: u32 = 19456;
const ARGON2_T: u32 = 2;
const ARGON2_P: u32 = 1;

/// 新身份助记词词数（12 词 = 128 bit 熵）与抄写确认词数
const MNEMONIC_WORD_COUNT: usize = 12;
const MNEMONIC_CONFIRM_WORDS: usize = 3;

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
    Backup,
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
    let backup = tree.register(ROOT, "backup", |ctx, _| ctx.action = ChatAction::Backup);
    tree.set_help(backup, "重新查看本身份助记词（需输入密码）");
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

/// 助记词 → 32 字节 Ed25519 种子（BIP39 PBKDF2 输出取前 32 字节）
fn seed_from_mnemonic(phrase: &str) -> Result<[u8; 32], String> {
    let mnemonic = Mnemonic::parse(phrase.trim())
        .map_err(|_| "助记词无效（须为 12 个标准英文词，空格分隔）".to_string())?;
    let seed = mnemonic.to_seed("");
    let mut out = [0u8; 32];
    out.copy_from_slice(&seed[..32]);
    Ok(out)
}

fn keypair_from_seed(seed: [u8; 32]) -> Result<Keypair, String> {
    Keypair::ed25519_from_bytes(seed).map_err(|e| format!("生成密钥失败: {e}"))
}

/// 助记词 → 确定性 Ed25519 密钥对（同一助记词在任何机器派生同一身份）
fn keypair_from_mnemonic(phrase: &str) -> Result<Keypair, String> {
    keypair_from_seed(seed_from_mnemonic(phrase)?)
}

/// 生成新身份助记词（12 词 = 128 bit 熵）
fn generate_mnemonic() -> Result<String, String> {
    Mnemonic::generate_in_with(&mut OsRng, bip39::Language::English, MNEMONIC_WORD_COUNT)
        .map(|m| m.to_string())
        .map_err(|e| format!("生成助记词失败: {e}"))
}

/// 由密码派生 keystore 加密密钥（Argon2id）
fn kdf_key(password: &str, salt: &[u8], m: u32, t: u32, p: u32) -> Result<[u8; 32], String> {
    let params = argon2::Params::new(m, t, p, Some(32)).map_err(|e| format!("Argon2 参数错误: {e}"))?;
    let argon2 = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut key = [0u8; 32];
    argon2
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| format!("密钥派生失败: {e}"))?;
    Ok(key)
}

/// 加密助记词 → (密文, salt, nonce)。Argon2id 派生密钥 + ChaCha20-Poly1305 认证加密
fn encrypt_mnemonic(
    mnemonic: &str,
    password: &str,
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>), String> {
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    let key = kdf_key(password, &salt, ARGON2_M_KIB, ARGON2_T, ARGON2_P)?;
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), mnemonic.as_bytes())
        .map_err(|e| format!("加密失败: {e}"))?;
    Ok((ciphertext, salt.to_vec(), nonce.to_vec()))
}

/// 解密助记词；密码错误返回"密码错误"，密文被篡改也会失败（AEAD 完整性）
fn decrypt_mnemonic(
    password: &str,
    salt: &[u8],
    nonce: &[u8],
    enc: &[u8],
    m: u32,
    t: u32,
    p: u32,
) -> Result<String, String> {
    let key = kdf_key(password, salt, m, t, p)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let plain = cipher
        .decrypt(Nonce::from_slice(nonce), enc)
        .map_err(|_| "密码错误".to_string())?;
    String::from_utf8(plain).map_err(|_| "keystore 数据损坏".to_string())
}

/// 输入行迭代器（stdin 被管道接管时逐行读取）
type StdinLines = tokio::io::Lines<tokio::io::BufReader<tokio::io::Stdin>>;

/// 本地 keystore：明文头只含公开资料与 KDF/密文参数，
/// 私密部分（身份助记词）用密码派生的密钥加密存放
struct Keystore {
    peer_id: String,
    salt: Vec<u8>,
    nonce: Vec<u8>,
    enc: Vec<u8>,
    kdf_m: u32,
    kdf_t: u32,
    kdf_p: u32,
}

/// 资料信息（姓名/生日/性别）：绑定在密钥上的元数据，不再参与身份派生
struct IdentityInfo {
    name: String,
    birthday: String,
    gender: char,
}

/// 登录结果
struct LoginOutcome {
    keypair: Keypair,
    info: IdentityInfo,
}

fn cache_dir() -> Result<PathBuf, String> {
    if let Ok(dir) = std::env::var("P2P_ID_CACHE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .ok_or_else(|| "无法确定用户主目录".to_string())?;
    Ok(PathBuf::from(home).join(".p2p_rust_app"))
}

/// 读取目录下全部 keystore（明文头 + 密文参数），损坏条目跳过
fn load_keystores() -> Vec<(Keystore, IdentityInfo)> {
    let dir = match cache_dir() {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut list = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("key") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let mut field: HashMap<String, String> = HashMap::new();
        for line in content.lines() {
            if let Some((k, v)) = line.split_once('=') {
                field.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        let need = |k: &str| field.get(k).cloned();
        let (Some(name), Some(birthday), Some(gender), Some(peer_id), Some(salt), Some(nonce), Some(enc)) =
            (
                need("name"),
                need("birthday"),
                need("gender"),
                need("peer_id"),
                need("salt"),
                need("nonce"),
                need("enc"),
            )
        else {
            continue;
        };
        let (Some(salt_b), Some(nonce_b), Some(enc_b)) =
            (hex::decode(&salt).ok(), hex::decode(&nonce).ok(), hex::decode(&enc).ok())
        else {
            continue;
        };
        let Some(gender_c) = gender.chars().next() else {
            continue;
        };
        let kdf_m = field
            .get("kdf_m")
            .and_then(|v| v.parse().ok())
            .unwrap_or(ARGON2_M_KIB);
        let kdf_t = field
            .get("kdf_t")
            .and_then(|v| v.parse().ok())
            .unwrap_or(ARGON2_T);
        let kdf_p = field
            .get("kdf_p")
            .and_then(|v| v.parse().ok())
            .unwrap_or(ARGON2_P);
        list.push((
            Keystore {
                peer_id,
                salt: salt_b,
                nonce: nonce_b,
                enc: enc_b,
                kdf_m,
                kdf_t,
                kdf_p,
            },
            IdentityInfo {
                name,
                birthday,
                gender: gender_c,
            },
        ));
    }
    list.sort_by(|a, b| a.1.name.cmp(&b.1.name));
    list
}

/// 加密保存 keystore（自动落盘；明文头含公开资料，私密助记词加密）
fn save_keystore(
    info: &IdentityInfo,
    peer_id: &PeerId,
    mnemonic: &str,
    password: &str,
) -> Result<(), String> {
    let dir = cache_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建缓存目录失败: {e}"))?;
    let (ciphertext, salt, nonce) = encrypt_mnemonic(mnemonic, password)?;
    let path = dir.join(format!("{peer_id}.key"));
    let content = format!(
        "format=1\nname={}\nbirthday={}\ngender={}\npeer_id={peer_id}\n\
         kdf_m={ARGON2_M_KIB}\nkdf_t={ARGON2_T}\nkdf_p={ARGON2_P}\n\
         salt={}\nnonce={}\nenc={}\n",
        info.name,
        info.birthday,
        info.gender,
        hex::encode(&salt),
        hex::encode(&nonce),
        hex::encode(&ciphertext)
    );
    std::fs::write(&path, content).map_err(|e| format!("写入 keystore 失败: {e}"))?;
    println!(
        "{}",
        format!("身份已保存（加密）: {}", path.display()).dimmed()
    );
    Ok(())
}

fn valid_password(pwd: &str) -> bool {
    (8..=128).contains(&pwd.len())
}

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

/// 影子探测：用一次性随机身份（仅 mDNS，不监听不聊天）探测局域网内是否已存在
/// 与真实 ID 相同的节点。libp2p 的 mDNS 库会把与"本机身份"同 PeerId 的节点
/// 过滤掉（behaviour/iface/query.rs 的 filter），因此必须借影子身份绕过。
async fn probe_duplicate_id(
    real_id: PeerId,
    window: Duration,
) -> Result<Option<Multiaddr>, Box<dyn Error>> {
    let mut probe_swarm = SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(|key| {
            let peer_id = key.public().to_peer_id();
            mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id)
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
        })?
        .build();
    println!(
        "{}",
        format!(
            "正在探测局域网内是否存在同 ID 节点（{} 秒）...",
            window.as_secs()
        )
        .dimmed()
    );
    let deadline = tokio::time::Instant::now() + window;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return Ok(None),
            event = probe_swarm.select_next_some() => {
                if let SwarmEvent::Behaviour(mdns::Event::Discovered(list)) = event {
                    if let Some((_, addr)) = list.iter().find(|(p, _)| *p == real_id) {
                        return Ok(Some(addr.clone()));
                    }
                }
            }
        }
    }
}

fn probe_window() -> Duration {
    std::env::var("P2P_ID_PROBE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(5))
}

fn build_swarm(keypair: Keypair) -> Result<Swarm<NodeBehaviour>, Box<dyn Error>> {
    let swarm = SwarmBuilder::with_existing_identity(keypair)
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
    let (mut swarm, my_name) = loop {
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
            None => break (build_swarm(outcome.keypair)?, outcome.info.name),
        }
    };

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

    /// BIP39 官方测试向量助记词（仅测试用）
    const MNEMONIC_A: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    const MNEMONIC_B: &str =
        "legal winner thank year wave sausage worth useful legal winner thank yellow";

    #[test]
    fn mnemonic_identity_deterministic() {
        let ka = keypair_from_mnemonic(MNEMONIC_A).unwrap();
        let ka2 = keypair_from_mnemonic(MNEMONIC_A).unwrap();
        let kb = keypair_from_mnemonic(MNEMONIC_B).unwrap();
        assert_eq!(ka.public().to_peer_id(), ka2.public().to_peer_id());
        assert_ne!(ka.public().to_peer_id(), kb.public().to_peer_id());
    }

    #[test]
    fn mnemonic_invalid_rejected() {
        assert!(seed_from_mnemonic("this is not a valid bip39 phrase").is_err());
        assert!(seed_from_mnemonic("").is_err());
        assert!(keypair_from_mnemonic(MNEMONIC_A).is_ok());
    }

    #[test]
    fn generated_mnemonic_is_valid() {
        let phrase = generate_mnemonic().unwrap();
        let words: Vec<&str> = phrase.split_whitespace().collect();
        assert_eq!(words.len(), MNEMONIC_WORD_COUNT);
        assert!(keypair_from_mnemonic(&phrase).is_ok());
    }

    #[test]
    fn keystore_encrypt_decrypt_roundtrip() {
        let (enc, salt, nonce) = encrypt_mnemonic(MNEMONIC_A, "password-123").unwrap();
        let out = decrypt_mnemonic(
            "password-123",
            &salt,
            &nonce,
            &enc,
            ARGON2_M_KIB,
            ARGON2_T,
            ARGON2_P,
        )
        .unwrap();
        assert_eq!(out, MNEMONIC_A);
    }

    #[test]
    fn keystore_wrong_password_rejected() {
        let (enc, salt, nonce) = encrypt_mnemonic(MNEMONIC_A, "password-123").unwrap();
        let err = decrypt_mnemonic(
            "wrong-password",
            &salt,
            &nonce,
            &enc,
            ARGON2_M_KIB,
            ARGON2_T,
            ARGON2_P,
        )
        .unwrap_err();
        assert_eq!(err, "密码错误");
    }

    #[test]
    fn password_length_rule() {
        assert!(valid_password("12345678"));
        assert!(valid_password(&"x".repeat(128)));
        assert!(!valid_password("1234567"));
        assert!(!valid_password(""));
        assert!(!valid_password(&"x".repeat(129)));
    }
}
