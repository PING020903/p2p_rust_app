//! P1 嵌入式控制台：spawn CLI 二进制（p2p_rust_app），管道接 stdout/stderr → UI，
//! UI 输入框 → 子进程 stdin。复用 e2e 的「spawn + 管道驱动」模式（tests/common/mod.rs）。
//!
//! 管道模式下 CLI 的 `interactive=false`：密码经行读取不回显（无 rpassword 控制台依赖）；
//! TOFU/文件接收等确认走「管道自动放行」语义——P1 阶段可接受，P2 原生界面再做弹窗确认。

use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use egui::Context;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::ui::logging::{Level, LogStore};

/// 后端 → UI 事件（P2 扩展为 Ask/Status 等）
pub enum UiOut {
    /// 一行文本（colored 在管道下自动关闭 ANSI，无需剥码）；`received_at` 供延迟统计
    Text {
        line: String,
        received_at: Instant,
    },
}

/// 控制台句柄：写子进程 stdin + 轮询退出
pub struct Console {
    stdin: ChildStdin,
    child: Child,
    reported_exit: bool,
}

impl Console {
    /// 向子进程 stdin 发送一行（含换行）
    pub fn send_line(&mut self, line: &str) {
        let _ = writeln!(self.stdin, "{line}");
        let _ = self.stdin.flush();
    }

    /// 发送多行文本：`/sendStrings <N>` + N 行原文（内容零变换，换行/引号/以 `/` 开头均原样）。
    /// N = `text.lines().count()`；CLI 侧按行数精确收集后拼接发送。
    pub fn send_multiline(&mut self, text: &str) {
        let n = text.lines().count();
        let out = format!("/sendStrings {n}\n{text}\n");
        // 防御：内容内部/尾部换行不影响行数协议（CLI 只按 N 行取，多余空行被主循环跳过）
        let _ = self.stdin.write_all(out.as_bytes());
        let _ = self.stdin.flush();
    }

    /// 非阻塞查询子进程是否退出；返回 Some(退出码) 仅在首次检测到退出时
    pub fn poll_exit(&mut self) -> Option<Option<i32>> {
        if self.reported_exit {
            return None;
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.reported_exit = true;
                Some(status.code())
            }
            _ => None,
        }
    }

    /// 关闭：杀掉子进程并回收（窗口关闭时调用）
    pub fn shutdown(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// spawn 结果：事件接收端 + 控制台句柄
pub struct ConsoleHandle {
    pub out_rx: UnboundedReceiver<UiOut>,
    pub console: Console,
}

/// 启动 CLI 子进程（当前 exe 同目录的兄弟二进制）。
/// `interact`：可选交互日志存储，子进程每行输出会写入（带时间戳，供分析对比）。
pub fn spawn(ctx: Context, interact: Option<Arc<LogStore>>) -> Result<ConsoleHandle, String> {
    let bin = locate_cli_binary()?;
    let mut cmd = Command::new(&bin);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // 分离后的 GUI 自身无控制台：若不给 CLI 子进程禁窗，Windows 会为 console 子进程
    // 新建一个常驻黑窗。stdio 本就是管道，禁窗不影响 is_terminal 判断与管道读写。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("启动 {} 失败: {e}", bin.display()))?;
    let stdout = child.stdout.take().ok_or("无法接管子进程 stdout")?;
    let stderr = child.stderr.take().ok_or("无法接管子进程 stderr")?;
    let stdin = child.stdin.take().ok_or("无法接管子进程 stdin")?;

    let (tx, out_rx) = unbounded_channel();
    spawn_reader(stdout, tx.clone(), ctx.clone(), interact.clone());
    spawn_reader(stderr, tx, ctx, interact);

    Ok(ConsoleHandle {
        out_rx,
        console: Console {
            stdin,
            child,
            reported_exit: false,
        },
    })
}

/// 定位 CLI 二进制：p2p_rust_app_gui.exe 的兄弟 p2p_rust_app.exe（同 target/debug 或 release 目录）
fn locate_cli_binary() -> Result<std::path::PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("无法定位当前 exe: {e}"))?;
    let dir = exe.parent().ok_or("无法确定 exe 所在目录")?;
    let name = if cfg!(windows) {
        "p2p_rust_app.exe"
    } else {
        "p2p_rust_app"
    };
    let bin = dir.join(name);
    if bin.exists() {
        Ok(bin)
    } else {
        Err(format!("未找到 CLI 二进制: {}", bin.display()))
    }
}

/// 管道读取线程：按行拆包送 UI；无换行的部分输出（提示符）即时上屏。
/// 每行同时写入交互日志（interact.log，带时间戳）。
fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
    tx: UnboundedSender<UiOut>,
    ctx: Context,
    interact: Option<Arc<LogStore>>,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut pending = Vec::<u8>::new();
        let emit = |line: String, tx: &UnboundedSender<UiOut>, interact: &Option<Arc<LogStore>>| {
            if line.trim().is_empty() {
                return;
            }
            let received_at = Instant::now();
            if let Some(log) = interact {
                log.log(Level::Info, "child", &line);
            }
            let _ = tx.send(UiOut::Text { line, received_at });
        };
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    pending.extend_from_slice(&buf[..n]);
                    let mut start = 0;
                    while let Some(pos) = pending[start..].iter().position(|&b| b == b'\n') {
                        let line =
                            String::from_utf8_lossy(&pending[start..start + pos]).into_owned();
                        emit(line, &tx, &interact);
                        start += pos + 1;
                    }
                    pending.drain(..start);
                    if !pending.is_empty() {
                        let line = String::from_utf8_lossy(&pending).into_owned();
                        emit(line, &tx, &interact);
                        pending.clear();
                    }
                    ctx.request_repaint();
                }
                Err(_) => break,
            }
        }
        if !pending.is_empty() {
            let line = String::from_utf8_lossy(&pending).into_owned();
            emit(line, &tx, &interact);
        }
    });
}
