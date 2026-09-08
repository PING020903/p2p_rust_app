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
    // 1.5 确认子窗口子模式：会话层拉起的独立控制台确认子进程（见 session.rs spawn_*_window）。
    //     必须先于 is_terminal 判定——子进程有自己的终端，否则会落入规则 4 误拉起 GUI。
    //     答案以退出码（0=确认）/结果文件回传父进程后立即退出，不进入任何主流程。
    let cargs: Vec<String> = std::env::args().skip(1).collect();
    match cargs.first().map(|s| s.as_str()) {
        Some(m @ ("--confirm-tofu" | "--confirm-secret" | "--confirm-file")) => {
            // 确认子进程：spawn 侧以 Stdio::null() 隔离了 stdio（防继承主窗口控制台句柄
            // 扣住主窗口键盘输入）——先把 std 句柄接回自己的新控制台，再关 QuickEdit
            #[cfg(windows)]
            attach_console_stdio();
            #[cfg(windows)]
            disable_quick_edit();
            match m {
                "--confirm-tofu" => confirm_tofu_entry(&cargs[1..]),
                "--confirm-secret" => confirm_secret_entry(&cargs[1..]),
                _ => confirm_file_entry(&cargs[1..]),
            }
        }
        _ => {}
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

/// 禁用当前控制台的快速编辑模式（QuickEdit）。根因（实测）：确认子窗口抢焦点 →
/// 用户点击主窗口聚焦时拖选文本 → conhost 进入选择模式，该控制台输入/输出整体冻结。
/// 清 QUICK_EDIT 位 + 置 EXTENDED_FLAGS（变更生效前提）；非控制台/失败返回 false（静默）。
#[cfg(windows)]
pub(crate) fn disable_quick_edit() -> bool {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(n_std_handle: u32) -> isize;
        fn GetConsoleMode(h_console: isize, lp_mode: *mut u32) -> i32;
        fn SetConsoleMode(h_console: isize, dw_mode: u32) -> i32;
    }
    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const ENABLE_QUICK_EDIT_MODE: u32 = 0x0040;
    const ENABLE_EXTENDED_FLAGS: u32 = 0x0080;
    unsafe {
        let h = GetStdHandle(STD_INPUT_HANDLE);
        let mut mode = 0u32;
        GetConsoleMode(h, &mut mode) != 0
            && SetConsoleMode(h, (mode & !ENABLE_QUICK_EDIT_MODE) | ENABLE_EXTENDED_FLAGS) != 0
    }
}

/// 确认子进程 stdio 自挂控制台：spawn 侧以 `Stdio::null()` 隔离父子 stdio（实测继承
/// 主窗口控制台句柄会扣住主窗口键盘输入，子窗口退出才涌出），本函数把 std 句柄接回
/// 子进程自己的新控制台——C 等价 `freopen("CONIN$","r",stdin); freopen("CONOUT$","w",stdout)`。
/// 判定：有真实控制台（GetConsoleWindow≠0）且 stdin 不是控制台（被 null）才接回；
/// 管道/脚本（无控制台）与真终端直敲（stdin 已是控制台）均不触发——行为不变。
#[cfg(windows)]
fn attach_console_stdio() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(n_std_handle: u32) -> isize;
        fn SetStdHandle(n_std_handle: u32, h_handle: isize) -> i32;
        fn GetConsoleMode(h_console: isize, lp_mode: *mut u32) -> i32;
        fn GetConsoleWindow() -> isize;
        fn CreateFileW(
            lp_filename: *const u16,
            dw_desired_access: u32,
            dw_share_mode: u32,
            lp_security_attributes: *const core::ffi::c_void,
            dw_creation_disposition: u32,
            dw_flags_and_attributes: u32,
            h_template_file: isize,
        ) -> isize;
    }
    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const OPEN_EXISTING: u32 = 3;
    const INVALID_HANDLE_VALUE: isize = -1;
    unsafe {
        if GetConsoleWindow() == 0 || GetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), &mut 0) != 0 {
            return;
        }
        let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
        let conin = CreateFileW(
            wide("CONIN$").as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            core::ptr::null(),
            OPEN_EXISTING,
            0,
            0,
        );
        if conin != INVALID_HANDLE_VALUE {
            SetStdHandle(STD_INPUT_HANDLE, conin);
        }
        let conout = CreateFileW(
            wide("CONOUT$").as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            core::ptr::null(),
            OPEN_EXISTING,
            0,
            0,
        );
        if conout != INVALID_HANDLE_VALUE {
            SetStdHandle(STD_OUTPUT_HANDLE, conout);
            SetStdHandle(STD_ERROR_HANDLE, conout);
        }
    }
}

/// 确认子窗口入口①：`--confirm-tofu <name> <fingerprint> <peer>`（会话层 spawn_tofu_window 拉起）。
/// 打印 TOFU 指纹确认卡片 → y/n 读行；退出码 0=信任 / 1=拒绝或读行失败 / 2=参数错误。
fn confirm_tofu_entry(args: &[String]) {
    use colored::Colorize;
    use std::io::{self, Write};

    let (name, fingerprint, peer) = match args {
        [n, f, p] => (n.as_str(), f.as_str(), p.as_str()),
        _ => {
            eprintln!("用法: --confirm-tofu <name> <fingerprint> <peer>");
            std::process::exit(2);
        }
    };
    println!("{}", "=== 新联系人信任确认（TOFU）===".cyan());
    println!("  名称: {name}");
    println!("  节点: {peer}");
    println!("{}", format!("  指纹: {fingerprint}").yellow());
    println!("{}", "请与对方当面核对指纹一致后再选择信任。".dimmed());
    println!("{}", "（本窗口仅用于本次确认；聊天请切回主窗口）".dimmed());
    print!("信任该联系人？(y/n): ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        std::process::exit(1);
    }
    // 防御：剥 BOM（脚本/管道喂入可能带 U+FEFF 前缀）+ 首尾空白
    if line.trim_start_matches('\u{feff}').trim().eq_ignore_ascii_case("y") {
        std::process::exit(0);
    }
    std::process::exit(1);
}

/// 确认子窗口入口②：`--confirm-secret <title> <result_file>`（会话层 spawn_secret_window 拉起）。
/// 交互终端 rpassword 不回显；管道/脚本退化普通读行（同 lineio::prompt_secret 退化语义）。
/// 密码写结果文件；退出码 0=已写入 / 1=取消或失败 / 2=参数错误。
fn confirm_secret_entry(args: &[String]) {
    use std::io::IsTerminal;

    let (title, result_file) = match args {
        [t, r] => (t.as_str(), r.as_str()),
        _ => {
            eprintln!("用法: --confirm-secret <title> <result_file>");
            std::process::exit(2);
        }
    };
    print!("{title}: ");
    let pw = if std::io::stdin().is_terminal() {
        match rpassword::prompt_password("") {
            Ok(p) => p,
            Err(_) => std::process::exit(1),
        }
    } else {
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            std::process::exit(1);
        }
        line.trim_start_matches('\u{feff}').trim().to_string()
    };
    if !pw.is_empty() && std::fs::write(result_file, &pw).is_ok() {
        std::process::exit(0);
    }
    std::process::exit(1);
}

/// 确认子窗口入口③：`--confirm-file <from> <file_id> <name> <size>`（会话层 spawn_file_window 拉起）。
/// 打印文件接收确认卡片 → y/n 读行；退出码 0=接收 / 1=拒绝或读行失败 / 2=参数错误。
fn confirm_file_entry(args: &[String]) {
    use colored::Colorize;
    use std::io::{self, Write};

    let (from, file_id, name, size) = match args {
        [f, i, n, s] => (f.as_str(), i.as_str(), n.as_str(), s.as_str()),
        _ => {
            eprintln!("用法: --confirm-file <from> <file_id> <name> <size>");
            std::process::exit(2);
        }
    };
    println!("{}", "=== 文件接收确认 ===".cyan());
    println!("  文件: {name}（{size} 字节）");
    println!("  来自: {from}");
    println!("  文件序号: {file_id}");
    println!("{}", "（本窗口仅用于本次确认；聊天请切回主窗口）".dimmed());
    print!("保存到下载目录？(y/n): ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        std::process::exit(1);
    }
    if line.trim_start_matches('\u{feff}').trim().eq_ignore_ascii_case("y") {
        std::process::exit(0);
    }
    std::process::exit(1);
}

/// 纯 CLI 模式主菜单：计算器 / 学生信息 / 彩色打印 / P2P 聊天 / 清除会话日志 / 清除联系人缓存
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
    let c6 = tree.register(ROOT, "6", |_, _| clear_contacts_menu());
    tree.set_help(c6, "清除联系人缓存（contacts_*.json，TOFU 信任态重置；不影响身份/群）");
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
        println!("  6. 清除联系人缓存");
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

/// 清除联系人缓存菜单入口：列出 contacts_*.json → 选序号/全清 → y/n 确认 → 删除。
/// 只删联系人簿（TOFU 信任态/指纹重置），身份 keystore/群缓存/会话日志一律不动。
fn clear_contacts_menu() {
    use colored::Colorize;
    use std::io::{self, Write};

    let files = crate::p2p::contacts::ContactBook::cache_files();
    if files.is_empty() {
        println!("{}", "缓存目录无联系人文件（已是干净状态）".dimmed());
        return;
    }
    println!("{}", "=== 清除联系人缓存 ===".cyan());
    println!("{}", format!("缓存根: {}", crate::p2p::cache_dir().unwrap_or_default().display()).dimmed());
    for (i, f) in files.iter().enumerate() {
        // 文件名内嵌节点ID：contacts_<peer_id>.json → 截短展示
        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        println!("  {}. {}", i + 1, name);
    }
    println!("  0. 全部清除");
    println!("  q. 取消");

    print!("选择: ");
    let _ = io::stdout().flush();
    let mut choice = String::new();
    if io::stdin().read_line(&mut choice).is_err() {
        return;
    }
    let choice = choice.trim();
    let targets: Vec<std::path::PathBuf> = match choice {
        "0" => files.clone(),
        "q" | "" => return,
        other => match other.parse::<usize>() {
            Ok(n) if (1..=files.len()).contains(&n) => vec![files[n - 1].clone()],
            _ => {
                println!("{}", "无效选择".yellow());
                return;
            }
        },
    };

    // 破坏性操作：二次确认
    print!("将删除 {} 个联系人缓存文件（TOFU 信任态重置，不可恢复），确认？(y/n): ", targets.len());
    let _ = io::stdout().flush();
    let mut confirm = String::new();
    if io::stdin().read_line(&mut confirm).is_err() {
        return;
    }
    if !confirm.trim().eq_ignore_ascii_case("y") {
        println!("{}", "已取消".dimmed());
        return;
    }
    for f in &targets {
        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        if crate::p2p::contacts::ContactBook::clear_cache_file(f) {
            println!("{}", format!("已删除: {name}").green());
        } else {
            eprintln!("{}", format!("删除失败: {name}").red());
        }
    }
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
