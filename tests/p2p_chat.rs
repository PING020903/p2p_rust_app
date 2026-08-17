use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

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

#[test]
fn two_nodes_chat_over_tcp() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");

    println!("=== 启动节点 A ===");
    let mut a = Node::spawn(bin);
    a.wait_for("=== 主菜单 ===", Duration::from_secs(10));
    a.send("4");
    let listen_line = a.wait_for("监听地址: /ip4/127.0.0.1", Duration::from_secs(20));
    let addr = listen_line
        .split("监听地址: ")
        .nth(1)
        .unwrap()
        .trim()
        .to_string();
    println!("=== 节点 A 地址: {addr} ===");

    println!("=== 启动节点 B ===");
    let mut b = Node::spawn(bin);
    b.wait_for("=== 主菜单 ===", Duration::from_secs(10));
    b.send("4");
    b.wait_for("监听地址:", Duration::from_secs(20));
    b.send(&format!("/dial {addr}"));

    a.wait_for("已连接对端", Duration::from_secs(20));
    b.wait_for("已连接对端", Duration::from_secs(20));

    println!("=== B -> A 发消息 ===");
    b.send("你好，我是节点B");
    a.wait_for("[对方] 你好，我是节点B", Duration::from_secs(20));

    println!("=== A -> B 回消息 ===");
    a.send("收到，我是节点A");
    b.wait_for("[对方] 收到，我是节点A", Duration::from_secs(20));

    a.kill();
    b.kill();
}
