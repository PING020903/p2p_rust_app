use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const CYCLES_GRACEFUL: usize = 15;
const CYCLES_KILL: usize = 5;
const WAIT: Duration = Duration::from_secs(20);

struct Creds {
    name: String,
    birthday: String,
    gender: String,
    password: String,
}

/// 从 tests/users.txt 读取 user1/user2 的凭据。
/// 文件格式：`userN-name / userN-age / userN-sex / userN-password` 键值行，
/// age 值允许带 "(YYYY-MM-DD)" 格式提示，解析时剥离。
fn load_creds() -> (Creds, Creds) {
    let path = format!("{}/tests/users.txt", env!("CARGO_MANIFEST_DIR"));
    let content = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("读取 {path} 失败: {e}（请复制 tests/users.template.txt 为 tests/users.txt 并填写）")
    });
    let mut fields: HashMap<String, String> = HashMap::new();
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
    (cred("user1"), cred("user2"))
}

/// 每场景独立的身份缓存临时目录（保证登录菜单行为确定）
fn scenario_cache_dir(scenario: &str) -> String {
    let dir = std::env::temp_dir()
        .join(format!("p2p_e2e_cache_{}", std::process::id()))
        .join(scenario);
    std::fs::create_dir_all(&dir).expect("创建测试缓存目录失败");
    dir.to_string_lossy().into_owned()
}

struct Node {
    child: Child,
    lines: mpsc::Receiver<String>,
}

impl Node {
    fn spawn(bin: &str, cache_dir: &str) -> Self {
        let mut child = Command::new(bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("P2P_ID_CACHE_DIR", cache_dir)
            .env("P2P_ID_PROBE_SECS", "2")
            .spawn()
            .expect("启动节点失败");
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

    fn send(&mut self, text: &str) {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(text.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    fn wait_for(&self, needle: &str, timeout: Duration) -> String {
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

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn listen_addr(listen_line: &str) -> String {
    listen_line
        .split("监听地址: ")
        .nth(1)
        .expect("监听地址行格式不符")
        .trim()
        .to_string()
}

fn parse_peer_id(listen_line: &str) -> String {
    listen_line
        .split("/p2p/")
        .nth(1)
        .expect("监听地址行缺少 /p2p/ 段")
        .trim()
        .to_string()
}

/// LCG 伪随机字母数字串：长度 16~64，纯 ASCII（字符数==字节数），不引 rand 依赖
fn random_msg(seed: u64) -> String {
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
const MNEMONIC_USER1: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
const MNEMONIC_USER2: &str =
    "legal winner thank year wave sausage worth useful legal winner thank yellow";

/// 从助记词恢复身份登录（r 路径）：喂 r → 助记词 → 四项资料 → 密码
fn login_restore(node: &mut Node, creds: &Creds, mnemonic: &str) {
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
fn login_cached(node: &mut Node, creds: &Creds) {
    node.send("4");
    node.send("1");
    node.send(&creds.password);
    node.wait_for("登录成功: ", Duration::from_secs(30));
}

/// 启动节点并登录进入聊天，返回 127.0.0.1 监听地址行（含 /p2p/ 节点ID）
fn spawn_into_chat(bin: &str, cache_dir: &str, creds: &Creds, mnemonic: &str) -> (Node, String) {
    let mut node = Node::spawn(bin, cache_dir);
    node.wait_for("=== 主菜单 ===", Duration::from_secs(10));
    login_restore(&mut node, creds, mnemonic);
    let listen = node.wait_for("监听地址: /ip4/127.0.0.1", Duration::from_secs(20));
    (node, listen)
}

/// 在已有节点上重新进入聊天（缓存解锁）
fn enter_chat(node: &mut Node, creds: &Creds) -> String {
    login_cached(node, creds);
    node.wait_for("监听地址: /ip4/127.0.0.1", Duration::from_secs(20))
}

/// 场景1：基础聊天——登录、连接、带名字的 Hello、双向收发、12 秒静默保活、Bye 优雅退出
fn basic_chat_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    let cache_a = scenario_cache_dir("s1_a");
    let cache_b = scenario_cache_dir("s1_b");
    let (cred_a, cred_b) = load_creds();
    println!("=== 场景1: 基础聊天 ===");

    println!("=== 启动节点 A ===");
    let (mut a, a_listen) = spawn_into_chat(bin, &cache_a, &cred_a, MNEMONIC_USER1);
    let a_addr = listen_addr(&a_listen);
    let a_id = parse_peer_id(&a_listen);

    println!("=== 启动节点 B ===");
    let (mut b, b_listen) = spawn_into_chat(bin, &cache_b, &cred_b, MNEMONIC_USER2);
    let b_id = parse_peer_id(&b_listen);
    b.send(&format!("/dial {a_addr}"));

    a.wait_for(&format!("已连接对端: {b_id}"), WAIT);
    b.wait_for(&format!("已连接对端: {a_id}"), WAIT);

    println!("=== 上线通知（Hello 携带角色名）===");
    a.wait_for(&format!("对方已上线: {}", cred_b.name), WAIT);
    b.wait_for(&format!("对方已上线: {}", cred_a.name), WAIT);

    println!("=== B -> A 发消息 ===");
    b.send("你好，我是节点B");
    a.wait_for("[对方] 你好，我是节点B", WAIT);

    println!("=== A -> B 回消息 ===");
    a.send("收到，我是节点A");
    b.wait_for("[对方] 收到，我是节点A", WAIT);

    println!("=== 静默 12 秒（超过 10s 空闲超时），验证心跳保活 ===");
    thread::sleep(Duration::from_secs(12));
    a.send("心跳保活后的消息");
    b.wait_for("[对方] 心跳保活后的消息", WAIT);

    println!("=== B 优雅退出，验证 Bye 通知 ===");
    b.send("/q");
    a.wait_for("对方已正常退出", WAIT);

    a.kill();
    b.kill();
}

/// 场景2：B 退出后重新登录（同一凭据身份不变），A 立即按角色名呼叫（待接呼叫），
/// 并回归验证 /list 地址数不随上下线循环累积
fn chat_by_name_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    let cache_a = scenario_cache_dir("s2_a");
    let cache_b = scenario_cache_dir("s2_b");
    let (cred_a, cred_b) = load_creds();
    println!("=== 场景2: 按角色名呼叫 ===");

    let (mut a, a_listen) = spawn_into_chat(bin, &cache_a, &cred_a, MNEMONIC_USER1);
    let a_addr = listen_addr(&a_listen);
    let a_id = parse_peer_id(&a_listen);

    let (mut b, b_listen) = spawn_into_chat(bin, &cache_b, &cred_b, MNEMONIC_USER2);
    let b_id = parse_peer_id(&b_listen);
    b.send(&format!("/dial {a_addr}"));
    b.wait_for(&format!("已连接对端: {a_id}"), WAIT);
    a.wait_for(&format!("对方已上线: {}", cred_b.name), WAIT);

    println!("=== B 优雅退出后重新登录，A 立即按角色名呼叫（待接呼叫）===");
    b.send("/q");
    a.wait_for("对方已正常退出", WAIT);
    enter_chat(&mut b, &cred_b);

    a.send(&format!("/chat {}", cred_b.name));
    a.wait_for(&format!("已连接对端: {b_id}"), WAIT);
    b.wait_for(&format!("已连接对端: {a_id}"), WAIT);
    b.wait_for(&format!("对方已上线: {}", cred_a.name), WAIT);

    a.send("按名呼叫后的消息");
    b.wait_for("[对方] 按名呼叫后的消息", WAIT);

    println!("=== /list 地址簿回归：地址数不得累积膨胀 ===");
    a.send("/list");
    a.wait_for("=== 已登记节点 ===", WAIT);
    let list_line = a.wait_for(&b_id, WAIT);
    let addr_n: usize = list_line
        .split("地址数")
        .nth(1)
        .expect("/list 行缺少地址数")
        .trim()
        .parse()
        .expect("地址数不是数字");
    assert!(addr_n <= 4, "地址簿膨胀: 地址数 {addr_n} > 4");

    a.kill();
    b.kill();
}

/// 场景3：B 主动下线/上线循环，每轮发送 ≤64 字节随机消息
fn graceful_offline_online_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    let cache_a = scenario_cache_dir("s3_a");
    let cache_b = scenario_cache_dir("s3_b");
    let (cred_a, cred_b) = load_creds();
    println!("=== 场景3: 主动上下线循环 x{CYCLES_GRACEFUL} ===");

    let (mut a, a_listen) = spawn_into_chat(bin, &cache_a, &cred_a, MNEMONIC_USER1);
    let a_addr = listen_addr(&a_listen);
    let a_id = parse_peer_id(&a_listen);

    let mut b = Node::spawn(bin, &cache_b);
    b.wait_for("=== 主菜单 ===", Duration::from_secs(10));

    for i in 0..CYCLES_GRACEFUL {
        println!("=== 第 {} 轮: 主动上线 ===", i + 1);
        let b_listen = if i == 0 {
            login_restore(&mut b, &cred_b, MNEMONIC_USER2);
            b.wait_for("监听地址: /ip4/127.0.0.1", Duration::from_secs(20))
        } else {
            enter_chat(&mut b, &cred_b)
        };
        let b_id = parse_peer_id(&b_listen);
        b.send(&format!("/dial {a_addr}"));
        b.wait_for(&format!("已连接对端: {a_id}"), WAIT);
        a.wait_for(&format!("已连接对端: {b_id}"), WAIT);

        let msg = random_msg(i as u64 + 1);
        assert!(msg.len() <= 64, "消息长度须 ≤64 字节");
        println!("=== 第 {} 轮: 发送 {} 字节随机消息 ===", i + 1, msg.len());
        b.send(&msg);
        a.wait_for(&format!("[对方] {msg}"), WAIT);

        println!("=== 第 {} 轮: 主动下线（/q）===", i + 1);
        b.send("/q");
        a.wait_for("对方已正常退出", WAIT);
    }

    a.kill();
    b.kill();
}

/// 场景4：kill 进程模拟掉线（无 Bye），隔一段时间后重新上线
fn kill_offline_online_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    let cache_a = scenario_cache_dir("s4_a");
    let cache_b = scenario_cache_dir("s4_b");
    let (cred_a, cred_b) = load_creds();
    println!("=== 场景4: kill 掉线循环 x{CYCLES_KILL} ===");

    let (mut a, a_listen) = spawn_into_chat(bin, &cache_a, &cred_a, MNEMONIC_USER1);
    let a_addr = listen_addr(&a_listen);
    let a_id = parse_peer_id(&a_listen);

    for i in 0..CYCLES_KILL {
        println!("=== 第 {} 轮: 上线 ===", i + 1);
        let mut b = Node::spawn(bin, &cache_b);
        b.wait_for("=== 主菜单 ===", Duration::from_secs(10));
        login_restore(&mut b, &cred_b, MNEMONIC_USER2);
        let b_listen = b.wait_for("监听地址: /ip4/127.0.0.1", Duration::from_secs(20));
        let b_id = parse_peer_id(&b_listen);
        b.send(&format!("/dial {a_addr}"));
        b.wait_for(&format!("已连接对端: {a_id}"), WAIT);
        a.wait_for(&format!("已连接对端: {b_id}"), WAIT);

        let msg = random_msg(i as u64 + 101);
        assert!(msg.len() <= 64, "消息长度须 ≤64 字节");
        println!("=== 第 {} 轮: 发送 {} 字节随机消息 ===", i + 1, msg.len());
        b.send(&msg);
        a.wait_for(&format!("[对方] {msg}"), WAIT);

        println!("=== 第 {} 轮: kill 进程模拟掉线 ===", i + 1);
        b.kill();
        a.wait_for("连接已关闭", WAIT);

        println!("=== 隔 3 秒后重新上线 ===");
        thread::sleep(Duration::from_secs(3));
    }

    a.kill();
}

/// 场景5：身份缓存回环——助记词恢复登录（自动加密保存）→ 退出重进 → 选缓存身份 + 只输密码
/// （先故意输错验证密码校验）
fn cache_login_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    let cache = scenario_cache_dir("s5");
    let (cred_a, _cred_b) = load_creds();
    println!("=== 场景5: 身份缓存回环 ===");

    let mut a = Node::spawn(bin, &cache);
    a.wait_for("=== 主菜单 ===", Duration::from_secs(10));
    login_restore(&mut a, &cred_a, MNEMONIC_USER1);
    a.wait_for("监听地址: /ip4/127.0.0.1", WAIT);

    println!("=== 退出聊天后重新进入，走缓存登录 ===");
    a.send("/q");
    a.wait_for("=== 主菜单 ===", WAIT);
    a.send("4");
    a.wait_for("缓存身份:", WAIT);

    println!("=== 先输错密码，验证校验 ===");
    a.send("1");
    a.send("wrong-password");
    a.wait_for("密码错误", WAIT);

    println!("=== 输正确密码（免姓名/生日/性别/助记词）===");
    a.send(&cred_a.password);
    a.wait_for("登录成功: ", WAIT);
    a.wait_for("监听地址: /ip4/127.0.0.1", WAIT);

    a.kill();
}

/// 场景6：同 ID 冲突——两节点同一助记词，后者登录必须被拒绝
fn duplicate_id_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    let cache_a = scenario_cache_dir("s6_a");
    let cache_b = scenario_cache_dir("s6_b");
    let (cred_a, _cred_b) = load_creds();
    println!("=== 场景6: 同 ID 冲突拒绝 ===");

    let (mut a, _a_listen) = spawn_into_chat(bin, &cache_a, &cred_a, MNEMONIC_USER1);

    println!("=== B 用同一助记词登录，必须被拒绝 ===");
    let mut b = Node::spawn(bin, &cache_b);
    b.wait_for("=== 主菜单 ===", Duration::from_secs(10));
    login_restore(&mut b, &cred_a, MNEMONIC_USER1);
    b.wait_for("该角色 ID 已在线", Duration::from_secs(30));

    a.kill();
    b.kill();
}

#[test]
fn p2p_chat_e2e_suite() {
    // 六场景串行：若拆成并行 #[test]，同机 mDNS 会跨测试互相发现导致连错对象
    basic_chat_scenario();
    chat_by_name_scenario();
    graceful_offline_online_scenario();
    kill_offline_online_scenario();
    cache_login_scenario();
    duplicate_id_scenario();
}
