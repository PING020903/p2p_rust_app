//! P2P 聊天：单 exe 双模式。
//! - 无参数启动（双击 / `cargo run`）→ **GUI**：以分离进程拉起自身（`P2P_GUI_CHILD` 标记），
//!   窗口 + 引擎线程（P2.0 起聊天核心进程内运行，不再 spawn CLI 子进程）
//! - `--cli` 参数 / 管道输入（非终端）→ **纯 CLI**：主菜单模式（终端/e2e/脚本）
//!
//! 控制台子系统（不设 windows_subsystem）：交互式 `--cli` 拥有原生终端体验；
//! GUI 模式以 `DETACHED_PROCESS` 分离 spawn 自身，窗口无黑窗伴随，
//! 仅双击启动时启动器控制台有 <1s 闪现（已知项）。

// 输出重定向宏：crate 级遮蔽 std 宏，路由到线程局部 sink（source/sink.rs）。
// 未安装 sink 的线程（CLI 模式、主线程）回落 ::std 宏——行为与原来逐字节一致；
// GUI 引擎线程 install 通道后输出进滚动区。必须先于 mod 声明（文本作用域）。
macro_rules! println {
    ($($arg:tt)*) => { $crate::sink::line(::std::format!($($arg)*)) };
}
macro_rules! eprintln {
    ($($arg:tt)*) => { $crate::sink::err(::std::format!($($arg)*)) };
}
macro_rules! print {
    ($($arg:tt)*) => { $crate::sink::raw(::std::format!($($arg)*)) };
}

mod calculator;
mod cmd_tree;
mod color_print;
mod file_transfer;
mod lineio;
mod p2p;
mod p2p_app;
mod sink;
mod student;
mod ui;
mod uievent;

// Color 提升到 crate 根：debug_print! 宏展开引用 `$crate::Color`
use color_print::Color;

/// GUI 子进程标记：分离 spawn 的副本带此环境变量，分发时直接进 GUI
pub(crate) const CHILD_ENV: &str = "P2P_GUI_CHILD";

use std::io::IsTerminal;

fn main() {
    // 1. GUI 子进程标记 → 窗口 + 引擎线程
    if std::env::var(CHILD_ENV).is_ok() {
        if let Err(e) = run_gui() {
            eprintln!("GUI 错误: {e}");
            std::process::exit(1);
        }
        return;
    }
    // 2. 管道输入（非终端）→ 纯 CLI（e2e / 脚本喂入）
    // 3. 显式 --cli 参数 → 纯 CLI
    if !std::io::stdin().is_terminal() || std::env::args().any(|a| a == "--cli") {
        run_cli();
        return;
    }
    // 4. 交互终端无参数 → 分离 spawn GUI 后退出，释放终端
    match spawn_detached() {
        Ok(()) => return,
        Err(e) => eprintln!("分离启动失败（{e}），回退前台 GUI…"),
    }
    if let Err(e) = run_gui() {
        eprintln!("GUI 错误: {e}");
        std::process::exit(1);
    }
}

/// GUI 模式：egui 窗口 + ui 应用（诊断/日志/输入区见 source/ui）
fn run_gui() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("P2P 聊天 GUI")
            .with_inner_size([1200.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "p2p_rust_app_gui",
        options,
        Box::new(|cc| Ok(Box::new(ui::GuiApp::new(cc)))),
    )
}

/// 以分离进程重新启动自身跑 GUI：
/// - Windows：`DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`（无控制台伴随）
/// - Unix：`process_group(0)` + stdio null（Ctrl+C 不传播、不占终端）
/// spawn 失败由调用方回退前台运行。
#[cfg(windows)]
fn spawn_detached() -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    let exe = std::env::current_exe().map_err(|e| format!("定位当前 exe 失败: {e}"))?;
    std::process::Command::new(&exe)
        .env(CHILD_ENV, "1")
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .map_err(|e| format!("spawn {} 失败: {e}", exe.display()))?;
    Ok(())
}

#[cfg(not(windows))]
fn spawn_detached() -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    let exe = std::env::current_exe().map_err(|e| format!("定位当前 exe 失败: {e}"))?;
    std::process::Command::new(&exe)
        .env(CHILD_ENV, "1")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn {} 失败: {e}", exe.display()))?;
    Ok(())
}

struct MainCtx {
    quit: bool,
}

/// 纯 CLI 模式主菜单：计算器 / 学生信息 / 彩色打印 / P2P 聊天 / 清除会话日志
fn run_cli() {
    use cmd_tree::{CmdError, CmdTree, ROOT};
    use colored::Colorize;
    use std::io::{self, Write};

    let mut tree: CmdTree<MainCtx> = CmdTree::new();
    let c1 = tree.register(ROOT, "1", |_, _| calculator::run());
    tree.set_help(c1, "计算器");
    let c2 = tree.register(ROOT, "2", |_, _| student::run());
    tree.set_help(c2, "学生信息管理");
    let c3 = tree.register(ROOT, "3", |_, _| {
        simulate_code_execution();
        color_print::demo();
    });
    tree.set_help(c3, "彩色打印演示");
    let c4 = tree.register(ROOT, "4", |_, _| p2p_app::chat::session::run());
    tree.set_help(c4, "P2P 聊天");
    let c5 = tree.register(ROOT, "5", |_, _| clear_logs_menu());
    tree.set_help(c5, "清除会话日志（gui_logs/，保留最近 1 次；不影响身份/联系人/群）");
    let cq = tree.register(ROOT, "q", |ctx, _| ctx.quit = true);
    tree.set_help(cq, "退出");
    let c_q_upper = tree.register(ROOT, "Q", |ctx, _| ctx.quit = true);
    tree.set_help(c_q_upper, "退出");

    let mut ctx = MainCtx { quit: false };

    loop {
        println!("\n=== 主菜单 ===");
        println!("  1. 计算器");
        println!("  2. 学生信息管理");
        println!("  3. 彩色打印演示");
        println!("  4. P2P 聊天");
        println!("  5. 清除会话日志");
        println!("  q. 退出");
        print!("{}", "> ".green());
        io::stdout().flush().unwrap();

        let mut input = String::new();
        if io::stdin().read_line(&mut input).unwrap() == 0 {
            break;
        }
        let input = input.trim();
        if input.is_empty() {
            continue;
        }

        if let Err(CmdError::NotFound) = tree.parse(input, &mut ctx) {
            println!("{} 无效选择", "错误:".red());
        }
        if ctx.quit {
            println!("{}", "再见！".yellow());
            break;
        }
    }
}

/// 模拟代码执行过程
fn simulate_code_execution() {
    color_print::print_info("开始执行任务...");

    // 模拟步骤1
    crate::debug_print!("步骤1: 准备数据");
    color_print::print_success("数据准备完成");

    // 模拟步骤2
    crate::debug_print!("步骤2: 处理数据");
    color_print::print_warning("数据量较大，处理可能需要时间");

    let text = "这是一个错误信息";
    color_print::print_error(text);

    // 模拟步骤3
    crate::debug_print!("步骤3: 保存结果");
    color_print::print_success("任务执行完成！");
}

/// 清除会话日志菜单入口：统计 → 确认 → 保留最近 1 次删除其余。
/// 只清 `<cache_dir>/gui_logs/`（GUI 运行日志），身份/联系人/群/设置一律不动。
fn clear_logs_menu() {
    use colored::Colorize;
    use std::io::{self, Write};

    let root = match crate::p2p::cache_dir() {
        Ok(r) => r.join("gui_logs"),
        Err(e) => {
            eprintln!("{}", format!("无法定位缓存目录: {e}").red());
            return;
        }
    };
    if !root.is_dir() {
        println!("无日志可清理");
        return;
    }

    let mut dirs: Vec<std::path::PathBuf> = match std::fs::read_dir(&root) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect(),
        Err(e) => {
            eprintln!("{}", format!("读取日志目录失败: {e}").red());
            return;
        }
    };
    if dirs.len() <= 1 {
        println!("无日志可清理（仅最近 1 次运行）");
        return;
    }
    // 目录名为时间戳（YYYYMMDD-HHMMSS），字典序 = 时间序
    dirs.sort();
    let total: u64 = dirs.iter().map(|d| dir_size(d)).sum();
    println!(
        "共 {} 个日志目录，合计 {} KB（保留最近 1 次）",
        dirs.len(),
        total / 1024
    );

    print!("确认清除? (y/n): ");
    io::stdout().flush().unwrap();
    let mut ans = String::new();
    if io::stdin().read_line(&mut ans).unwrap_or(0) == 0 {
        return;
    }
    if !ans.trim().eq_ignore_ascii_case("y") {
        println!("{}", "已取消".dimmed());
        return;
    }

    let (removed, freed) = clear_gui_logs_in(&root, 1);
    println!(
        "{}",
        format!("已清理 {removed} 个目录，释放 {} KB", freed / 1024).green()
    );
}

/// 核心清理（可测）：`dir` 下按目录名降序保留 `keep` 个最新，删除其余。
/// 返回 (成功删除的目录数, 释放字节数)；单个目录删除失败（如被占用）跳过留在磁盘，不重试。
fn clear_gui_logs_in(dir: &std::path::Path, keep: usize) -> (usize, u64) {
    use colored::Colorize;

    let mut subdirs: Vec<std::path::PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect(),
        Err(_) => return (0, 0),
    };
    subdirs.sort();
    let mut removed = 0usize;
    let mut freed = 0u64;
    while subdirs.len() > keep {
        let oldest = subdirs.remove(0);
        let size = dir_size(&oldest);
        match std::fs::remove_dir_all(&oldest) {
            Ok(()) => {
                removed += 1;
                freed += size;
            }
            Err(e) => {
                eprintln!(
                    "{}",
                    format!("跳过 {}（删除失败: {e}）", oldest.display()).yellow()
                );
            }
        }
    }
    (removed, freed)
}

/// 递归统计目录内文件总大小
fn dir_size(p: &std::path::Path) -> u64 {
    let mut total = 0u64;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.filter_map(|e| e.ok()) {
            let path = e.path();
            if path.is_dir() {
                total += dir_size(&path);
            } else if let Ok(m) = e.metadata() {
                total += m.len();
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在临时目录下造伪日志目录（各含 1 个指定大小的文件），返回根路径
    fn setup_fake_logs(tag: &str, names: &[&str], file_bytes: u64) -> std::path::PathBuf {
        use std::io::Write;
        let root = std::env::temp_dir().join(format!(
            "p2p_test_clearlogs_{}_{}",
            std::process::id(),
            tag
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for name in names {
            let d = root.join(name);
            std::fs::create_dir_all(&d).unwrap();
            let mut f = std::fs::File::create(d.join("interact.log")).unwrap();
            f.write_all(&vec![b'x'; file_bytes as usize]).unwrap();
        }
        root
    }

    #[test]
    fn clear_keeps_newest_only() {
        let root = setup_fake_logs(
            "keep1",
            &["20260901-000001", "20260901-000002", "20260901-000003"],
            100,
        );
        let (removed, freed) = clear_gui_logs_in(&root, 1);
        assert_eq!(removed, 2);
        assert_eq!(freed, 200);
        // 仅保留最新目录
        let left: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["20260901-000003"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn clear_noop_when_keep_covers_all() {
        let root = setup_fake_logs("noop", &["20260901-000001", "20260901-000002"], 50);
        let (removed, freed) = clear_gui_logs_in(&root, 5);
        assert_eq!((removed, freed), (0, 0));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn clear_empty_root() {
        let root = setup_fake_logs("empty", &[], 0);
        let (removed, freed) = clear_gui_logs_in(&root, 1);
        assert_eq!((removed, freed), (0, 0));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn clear_counts_bytes_of_removed_only() {
        let root = setup_fake_logs("bytes", &["a-000001", "a-000002"], 300);
        let (removed, freed) = clear_gui_logs_in(&root, 1);
        assert_eq!(removed, 1);
        assert_eq!(freed, 300); // 只统计被删目录的字节
        let _ = std::fs::remove_dir_all(&root);
    }
}
