//! 双节点 e2e 共享脚手架：凭据读取、节点启动/喂输入/等输出、固定身份助记词、登录流程。
//! 供 `p2p_chat.rs`（逻辑测试）与 `p2p_chat_stability.rs`（稳定性测试）两个测试二进制复用。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

pub const CYCLES_GRACEFUL: usize = 15;
pub const CYCLES_KILL: usize = 5;
pub const WAIT: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct Creds {
    pub name: String,
    pub birthday: String,
    pub gender: String,
    pub password: String,
}

/// 从 tests/users.txt 读取 user1/user2 的凭据。
/// 文件格式：`userN-name / userN-age / userN-sex / userN-password` 键值行，
/// age 值允许带 "(YYYY-MM-DD)" 格式提示，解析时剥离。
pub fn load_creds() -> (Creds, Creds) {
    let all = load_creds_n(2);
    (all[0].clone(), all[1].clone())
}

/// user1/user2/user3（3 号用于三节点多会话/群聊场景，要求名字互不相同）
pub fn load_creds3() -> (Creds, Creds, Creds) {
    let all = load_creds_n(3);
    (all[0].clone(), all[1].clone(), all[2].clone())
}

/// user1..user4（4 号用于四节点群主转移场景）
pub fn load_creds4() -> (Creds, Creds, Creds, Creds) {
    let all = load_creds_n(4);
    (
        all[0].clone(),
        all[1].clone(),
        all[2].clone(),
        all[3].clone(),
    )
}

pub fn load_creds_n(n: usize) -> Vec<Creds> {
    let path = format!("{}/tests/users.txt", env!("CARGO_MANIFEST_DIR"));
    let content = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("读取 {path} 失败: {e}（请复制 tests/users.template.txt 为 tests/users.txt 并填写）")
    });
    let mut fields: HashMap<String, String> = HashMap::new();
    // 防御 UTF-8 BOM（\u{feff}）：部分编辑器/写盘会带 BOM，污染首行键名
    let content = content.trim_start_matches('\u{feff}');
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            fields.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    let take = |user: &str, field: &str| -> String {
        fields
            .get(&format!("{user}-{field}"))
            .unwrap_or_else(|| panic!("users.txt 缺少 {user}-{field}"))
            .clone()
    };
    let strip_hint = |v: String| v.split('(').next().unwrap_or("").trim().to_string();
    let cred = |user: &str| Creds {
        name: take(user, "name"),
        birthday: strip_hint(take(user, "age")),
        gender: take(user, "sex"),
        password: take(user, "password"),
    };
    (1..=n).map(|i| cred(&format!("user{i}"))).collect()
}

/// 每场景独立的身份缓存临时目录（保证登录菜单行为确定）
pub fn scenario_cache_dir(scenario: &str) -> String {
    let dir = std::env::temp_dir()
        .join(format!("p2p_e2e_cache_{}", std::process::id()))
        .join(scenario);
    std::fs::create_dir_all(&dir).expect("创建测试缓存目录失败");
    dir.to_string_lossy().into_owned()
}

pub struct Node {
    pub child: Child,
    pub lines: mpsc::Receiver<String>,
}

impl Node {
    pub fn spawn(bin: &str, cache_dir: &str) -> Self {
        Self::spawn_with(bin, cache_dir, "advertise")
    }

    pub fn spawn_with(bin: &str, cache_dir: &str, discovery: &str) -> Self {
        Self::spawn_with_env(bin, cache_dir, discovery, &[])
    }

    pub fn spawn_with_env(bin: &str, cache_dir: &str, discovery: &str, extra_env: &[(&str, &str)]) -> Self {
        let mut cmd = Command::new(bin);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("P2P_ID_CACHE_DIR", cache_dir)
            .env("P2P_ID_PROBE_SECS", "2")
            .env("P2P_DISCOVERY", discovery)
            .env("P2P_DOWNLOAD_DIR", format!("{cache_dir}/downloads"));
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("启动节点失败");
        let (tx, rx) = mpsc::channel();
        let forward = |mut stream: Box<dyn std::io::Read + Send>, tag: &'static str, tx: mpsc::Sender<String>| {
            thread::spawn(move || {
                for line in BufReader::new(&mut stream).lines() {
                    match line {
                        Ok(l) => {
                            if tx.send(format!("[{tag}] {l}")).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        };
        forward(
            Box::new(child.stdout.take().unwrap()),
            "out",
            tx.clone(),
        );
        forward(Box::new(child.stderr.take().unwrap()), "err", tx);
        Node { child, lines: rx }
    }

    pub fn send(&mut self, text: &str) {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(text.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    pub fn wait_for(&self, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::ZERO);
            match self.lines.recv_timeout(remaining) {
                Ok(line) => {
                    println!("  | {line}");
                    if line.contains(needle) {
                        return line;
                    }
                }
                Err(_) => panic!("等待 '{needle}' 超时"),
            }
        }
    }

    /// 等待可选出现：限时内出现返回 Some，否则 None（用于断言"不应出现"）
    pub fn wait_for_optional(&self, needle: &str, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::ZERO);
            match self.lines.recv_timeout(remaining) {
                Ok(line) => {
                    println!("  | {line}");
                    if line.contains(needle) {
                        return Some(line);
                    }
                }
                Err(_) => return None,
            }
        }
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn listen_addr(listen_line: &str) -> String {
    listen_line
        .split("监听地址: ")
        .nth(1)
        .expect("监听地址行格式不符")
        .trim()
        .to_string()
}

pub fn parse_peer_id(listen_line: &str) -> String {
    listen_line
        .split("/p2p/")
        .nth(1)
        .expect("监听地址行缺少 /p2p/ 段")
        .trim()
        .to_string()
}

/// 从监听地址行提取 TCP 端口（v4/v6 监听复用同一端口）
pub fn listen_port(listen_line: &str) -> String {
    listen_line
        .split("/tcp/")
        .nth(1)
        .and_then(|s| s.split('/').next())
        .expect("监听地址行缺少端口")
        .to_string()
}

/// LCG 伪随机字母数字串：长度 16~64，纯 ASCII（字符数==字节数），不引 rand 依赖
pub fn random_msg(seed: u64) -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut state = seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    let len = 16 + (state % 49) as usize;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            CHARSET[((state >> 33) as usize) % CHARSET.len()] as char
        })
        .collect()
}

/// e2e 固定身份助记词（BIP39 官方测试向量，同一助记词派生同一 PeerId，
/// 保证场景确定性；仅测试用，勿用于生产）
pub const MNEMONIC_USER1: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
pub const MNEMONIC_USER2: &str =
    "legal winner thank year wave sausage worth useful legal winner thank yellow";
pub const MNEMONIC_USER3: &str =
    "ozone drill grab fiber curtain grace pudding thank cruise elder eight picnic";
pub const MNEMONIC_USER4: &str =
    "letter advice cage absurd amount doctor acoustic avoid letter advice cage above";

/// 从助记词恢复身份登录（r 路径）：喂 r → 助记词 → 四项资料 → 密码
pub fn login_restore(node: &mut Node, creds: &Creds, mnemonic: &str) {
    node.send("4");
    node.send("r");
    node.send(mnemonic);
    node.send(&creds.name);
    node.send(&creds.birthday);
    node.send(&creds.gender);
    node.send(&creds.password);
    node.wait_for("登录成功: ", Duration::from_secs(30));
}

/// 缓存身份登录：进入聊天后选第一个缓存身份（每节点独立缓存目录，保证唯一）→ 只输密码
pub fn login_cached(node: &mut Node, creds: &Creds) {
    node.send("4");
    node.send("1");
    node.send(&creds.password);
    node.wait_for("登录成功: ", Duration::from_secs(30));
}

/// 启动节点并登录进入聊天，返回 127.0.0.1 监听地址行（含 /p2p/ 节点ID）
pub fn spawn_into_chat(bin: &str, cache_dir: &str, creds: &Creds, mnemonic: &str) -> (Node, String) {
    let mut node = Node::spawn(bin, cache_dir);
    node.wait_for("=== 主菜单 ===", Duration::from_secs(10));
    login_restore(&mut node, creds, mnemonic);
    let listen = node.wait_for("监听地址: /ip4/127.0.0.1", Duration::from_secs(20));
    (node, listen)
}

/// 在已有节点上重新进入聊天（缓存解锁）
pub fn enter_chat(node: &mut Node, creds: &Creds) -> String {
    login_cached(node, creds);
    node.wait_for("监听地址: /ip4/127.0.0.1", Duration::from_secs(20))
}

/// 等待双方互信信号就绪（对称信任：管道模式 hello 自动互信，等对端 trust.confirm 到达即生效）。
/// 发消息前必须调用，否则消息会在互信建立前被接收方丢弃。
pub fn wait_mutual_trust(a: &Node, a_name: &str, b: &Node, b_name: &str) {
    a.wait_for(&format!("对方已信任你: {b_name}"), WAIT);
    b.wait_for(&format!("对方已信任你: {a_name}"), WAIT);
}

/// 在节点输出流中等待第一条「全局 IPv6 监听地址」行（跳过 ::1 回环 / fe80 链路本地），
/// 超时返回 None。复用 recv_timeout 循环（不 panic，供"无全局 IPv6 则跳过"判定）。
pub fn wait_global_ipv6_listen(node: &Node, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        match node.lines.recv_timeout(remaining) {
            Ok(line) => {
                println!("  | {line}");
                let Some(addr) = line.split("监听地址: ").nth(1).map(|s| s.trim()) else {
                    continue;
                };
                let is_global_v6 = addr.starts_with("/ip6/")
                    && !addr.starts_with("/ip6/::1")
                    && !addr.starts_with("/ip6/fe80");
                if is_global_v6 {
                    return Some(line);
                }
            }
            Err(_) => return None,
        }
    }
}

/// 以 P2P_DISCOVERY=off 启动节点并恢复身份登录（同机多实例互测专用，防 mDNS 干扰）
pub fn spawn_chat_off(bin: &str, cache_dir: &str, creds: &Creds, mnemonic: &str) -> (Node, String) {
    let mut node = Node::spawn_with(bin, cache_dir, "off");
    node.wait_for("=== 主菜单 ===", Duration::from_secs(10));
    login_restore(&mut node, creds, mnemonic);
    let listen = node.wait_for("监听地址: /ip4/127.0.0.1", Duration::from_secs(20));
    (node, listen)
}
