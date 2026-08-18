use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const CYCLES_GRACEFUL: usize = 15;
const CYCLES_KILL: usize = 5;
const WAIT: Duration = Duration::from_secs(20);

struct Node {
    child: Child,
    lines: mpsc::Receiver<String>,
}

impl Node {
    fn spawn(bin: &str) -> Self {
        let mut child = Command::new(bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("启动节点失败");
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
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

/// 启动节点并进入聊天，返回 127.0.0.1 监听地址行（含 /p2p/ 节点ID）
fn spawn_into_chat(bin: &str) -> (Node, String) {
    let mut node = Node::spawn(bin);
    node.wait_for("=== 主菜单 ===", Duration::from_secs(10));
    let listen = enter_chat(&mut node);
    (node, listen)
}

/// 在已有节点上进入（或重新进入）聊天
fn enter_chat(node: &mut Node) -> String {
    node.send("4");
    node.wait_for("监听地址: /ip4/127.0.0.1", Duration::from_secs(20))
}

/// 场景1：基础聊天——连接、Hello、双向收发、12 秒静默保活、Bye 优雅退出
fn basic_chat_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    println!("=== 场景1: 基础聊天 ===");

    println!("=== 启动节点 A ===");
    let (mut a, a_listen) = spawn_into_chat(bin);
    let a_addr = listen_addr(&a_listen);
    let a_id = parse_peer_id(&a_listen);
    println!("=== 节点 A 地址: {a_addr} ===");

    println!("=== 启动节点 B ===");
    let (mut b, b_listen) = spawn_into_chat(bin);
    let b_id = parse_peer_id(&b_listen);
    b.send(&format!("/dial {a_addr}"));

    a.wait_for(&format!("已连接对端: {b_id}"), WAIT);
    b.wait_for(&format!("已连接对端: {a_id}"), WAIT);

    println!("=== 上线通知（Hello）===");
    a.wait_for("对方已上线", WAIT);
    b.wait_for("对方已上线", WAIT);

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

/// 场景2：B 主动下线/上线循环，每轮发送 ≤64 字节随机消息
fn graceful_offline_online_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    println!("=== 场景2: 主动上下线循环 x{CYCLES_GRACEFUL} ===");

    let (mut a, a_listen) = spawn_into_chat(bin);
    let a_addr = listen_addr(&a_listen);
    let a_id = parse_peer_id(&a_listen);

    let mut b = Node::spawn(bin);
    b.wait_for("=== 主菜单 ===", Duration::from_secs(10));

    for i in 0..CYCLES_GRACEFUL {
        println!("=== 第 {} 轮: 主动上线 ===", i + 1);
        let b_listen = enter_chat(&mut b);
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

/// 场景3：kill 进程模拟掉线（无 Bye），隔一段时间后重新上线
fn kill_offline_online_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    println!("=== 场景3: kill 掉线循环 x{CYCLES_KILL} ===");

    let (mut a, a_listen) = spawn_into_chat(bin);
    let a_addr = listen_addr(&a_listen);
    let a_id = parse_peer_id(&a_listen);

    let mut b = Node::spawn(bin);
    b.wait_for("=== 主菜单 ===", Duration::from_secs(10));

    for i in 0..CYCLES_KILL {
        println!("=== 第 {} 轮: 上线 ===", i + 1);
        let b_listen = enter_chat(&mut b);
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
        if i + 1 < CYCLES_KILL {
            b = Node::spawn(bin);
            b.wait_for("=== 主菜单 ===", Duration::from_secs(10));
        }
    }

    a.kill();
}

#[test]
fn p2p_chat_e2e_suite() {
    // 三场景串行：若拆成并行 #[test]，同机 mDNS 会跨测试互相发现导致连错对象
    basic_chat_scenario();
    graceful_offline_online_scenario();
    kill_offline_online_scenario();
}
