//! GUI 入口：p2p_rust_app_gui（egui/eframe）。
//! 双重角色：
//! 1. 启动器（无 `P2P_GUI_CHILD=1`）：以分离进程重新 spawn 自己后立即退出，释放原命令行
//! 2. 真正的 GUI（带 `P2P_GUI_CHILD=1`）：窗口 + 中文字体 + 后台通道 + 日志（见 source/ui）
//!
//! GUI 不挂控制台（无条件 windows_subsystem）：诊断已全部走 `gui_logs` 日志；
//! 否则双击启动时启动器/子进程会闪黑窗。Debug 构建的 eprintln 不再可见（可接受）。

#![windows_subsystem = "windows"]

mod ui;

/// 分离标记：子进程带此环境变量时直接进 GUI，否则作为启动器
const CHILD_ENV: &str = "P2P_GUI_CHILD";

fn main() -> eframe::Result {
    if !std::env::var(CHILD_ENV).is_ok() {
        // 启动器：spawn 分离的 GUI 进程后立即返回（原命令行即被释放）
        match spawn_detached() {
            Ok(()) => return Ok(()),
            Err(e) => {
                eprintln!("分离启动失败（{e}），回退为前台运行…");
            }
        }
    }

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

/// 以分离进程（无控制台）重新启动自己，带 `CHILD_ENV` 标记；返回即表示子进程已拉起。
/// Unix 下不分离（GUI 依赖显示服务，直接前台运行）。
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
    Ok(())
}
