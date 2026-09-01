//! GUI 应用（P1 终端式 + 诊断）：滚动文本区 + 输入框驱动嵌入式聊天控制台。
//! 附带：函数耗时组件（timing）+ 双文件运行日志（logging，缓存根/gui_logs/<时间戳>/）。
//! P2 起将替换为进程内原生界面（LineReader/UiOut 抽象 + 登录表单/气泡/弹窗）。

pub mod console;
pub mod fonts;
pub mod input_guard;
pub mod logging;
pub mod timing;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use console::{Console, UiOut};
use input_guard::InputGuard;
use logging::{Level, LogStore};
use tokio::sync::mpsc;

/// 日志面板视图切换
#[derive(Clone, Copy, PartialEq, Eq)]
enum LogView {
    Runtime,
    Interact,
}

/// 子进程界面状态：由输出特征行推断（精确整行匹配——聊天内容都带
/// `[对方] `/`[我 -> ` 前缀，正文同款文本不会误触发）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChildState {
    /// 主菜单（含启动初始态）
    Menu,
    /// 登录流程（[角色登录] 之后：身份/资料/密码输入）
    Login,
    /// 已进入聊天循环（打印"发现模式: "之后）
    Chat,
}

impl ChildState {
    fn label(self) -> &'static str {
        match self {
            ChildState::Menu => "主菜单",
            ChildState::Login => "登录中",
            ChildState::Chat => "聊天中",
        }
    }
}

/// 按输出行推进状态机：主菜单 →（选 4）登录 →（登录成功）聊天 →（/q）回主菜单
fn next_state(current: ChildState, line: &str) -> ChildState {
    if line == "=== 主菜单 ===" {
        ChildState::Menu
    } else if line == "[角色登录]" {
        ChildState::Login
    } else if line.starts_with("发现模式: ") {
        ChildState::Chat
    } else {
        current
    }
}

pub struct GuiApp {
    /// 滚动区文本行（子进程输出 + 本机状态）
    lines: Vec<String>,
    /// 输入框内容
    input: String,
    /// 命令输入框内容（/list 等；与文本框分离）
    cmd_input: String,
    /// 子进程 → UI 事件
    rx: mpsc::UnboundedReceiver<UiOut>,
    /// 控制台句柄（None = 启动失败）
    console: Option<Console>,
    /// 首帧标记：自动聚焦输入框
    first_frame: bool,
    /// 耗时统计
    stats: timing::TimingStats,
    /// 软件运行日志
    runtime: Arc<LogStore>,
    /// 用户交互输入输出日志
    interact: Arc<LogStore>,
    /// 输入发送时刻（算输入→响应往返）
    input_sent_at: Option<Instant>,
    /// 每帧 drain 出的日志（面板渲染缓冲）
    pending_runtime: Vec<logging::Entry>,
    pending_interact: Vec<logging::Entry>,
    /// 日志面板状态
    show_log: bool,
    log_view: LogView,
    log_follow: bool,
    log_level: Level,
    /// 命令输入框的检查&修改规则链（拦终端逃逸穿透 + /sendStrings）
    cmd_guard: InputGuard,
    /// 文本框的检查&修改规则链（当前为空，纯文本语义；接口保留供未来加长度/敏感词等检查）
    text_guard: InputGuard,
    /// 子进程界面状态（决定文本框启停与命令框提示）
    child_state: ChildState,
}

impl GuiApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let runtime = Arc::new(LogStore::new(5000));
        let interact = Arc::new(LogStore::new(5000));
        let mut lines = Vec::new();

        // CJK 字体：系统优先 → 内置兜底（结果写入运行日志，无头环境可凭日志验证）
        let font = fonts::install(&cc.egui_ctx);
        let font_note = font.source.clone();

        // 缓存根/gui_logs/<时间戳>/ 建目录 + 开两份日志
        let log_dir = cache_root().join("gui_logs").join(logging::now_folder_ts());
        let mut log_note = "日志落盘失败".to_string();
        if std::fs::create_dir_all(&log_dir).is_ok() {
            match runtime.enable_file(&log_dir.join("runtime.log")) {
                Ok(()) => {
                    let _ = interact.enable_file(&log_dir.join("interact.log"));
                    log_note = log_dir.display().to_string();
                }
                Err(e) => lines.push(format!("日志目录写入失败: {e}")),
            }
        }
        runtime.log(Level::Info, "gui", format!("GUI 启动，日志目录: {log_note}"));
        runtime.log(Level::Info, "gui", format!("缓存根: {}", cache_root().display()));
        runtime.log(Level::Info, "gui", format!("CJK 字体: {font_note}"));

        let (console, rx) = match console::spawn(cc.egui_ctx.clone(), Some(interact.clone())) {
            Ok(h) => {
                lines.push("已启动聊天控制台（CLI 管道模式）".to_string());
                (Some(h.console), h.out_rx)
            }
            Err(e) => {
                runtime.log(Level::Error, "gui", format!("控制台启动失败: {e}"));
                lines.push(format!("启动控制台失败: {e}"));
                let (tx, rx) = mpsc::unbounded_channel();
                drop(tx);
                (None, rx)
            }
        };

        GuiApp {
            lines,
            input: String::new(),
            cmd_input: String::new(),
            rx,
            console,
            first_frame: true,
            stats: timing::TimingStats::default(),
            runtime,
            interact,
            input_sent_at: None,
            pending_runtime: Vec::new(),
            pending_interact: Vec::new(),
            show_log: true,
            log_view: LogView::Runtime,
            log_follow: true,
            log_level: Level::Debug,
            cmd_guard: InputGuard::command_box(),
            text_guard: InputGuard::text_box(),
            child_state: ChildState::Menu,
        }
    }
}

impl eframe::App for GuiApp {
    /// 每帧（含窗口隐藏时）的逻辑回调：drain 子进程输出 + 轮询退出 + 计时（不能画 UI）。
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let _t = timing::ScopeTimer::start("frame.logic", &self.stats);
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                UiOut::Text { line, received_at } => {
                    self.stats
                        .record("pipeline.drain", received_at.elapsed());
                    if let Some(t0) = self.input_sent_at.take() {
                        self.stats.record("roundtrip.input->resp", t0.elapsed());
                    }
                    self.child_state = next_state(self.child_state, &line);
                    self.lines.push(line);
                }
            }
            ctx.request_repaint();
        }
        if let Some(c) = &mut self.console {
            if let Some(code) = c.poll_exit() {
                // 生命周期联动：CLI 结束 → GUI 一并关闭
                let level = if code == Some(0) { Level::Info } else { Level::Warn };
                self.runtime
                    .log(level, "console", format!("子进程已退出，code={code:?}，GUI 即将关闭"));
                self.lines.push(format!("[控制台进程已退出，code={code:?}]"));
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                ctx.request_repaint();
            }
        }
        self.runtime.drain(&mut self.pending_runtime);
        self.interact.drain(&mut self.pending_interact);
        cap(&mut self.pending_runtime, 5000);
        cap(&mut self.pending_interact, 5000);

        // 空闲时低频轮询：子进程退出后无新输出触发重绘，logic() 不会被调用，
        // 需靠定时 repaint 保证 poll_exit 及时检测（否则 GUI 不会联动关闭）。
        if self.console.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
    }

    /// 根 UI 回调：给定的 `ui` 无外边距/背景，用面板分区布局。
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let mut frame_timer = timing::Timer::start();

        // 日志侧栏（可开关）
        if self.show_log {
            egui::Panel::right("log")
                .default_size(380.0)
                .resizable(true)
                .show(ui, |ui| self.log_panel(ui));
        }

        // 底部输入面板先声明 → 先占位，CentralPanel 只拿剩余高度（ScrollArea 不会挤掉输入行）
        egui::Panel::bottom("input").show(ui, |ui| {
            let in_chat = self.child_state == ChildState::Chat;
            ui.add_space(4.0);

            // 命令输入行（与文本框分离；guard 拦截穿透命令与 /sendStrings；提示随状态变化）
            ui.horizontal(|ui| {
                ui.label("命令");
                let hint = match self.child_state {
                    ChildState::Menu => "菜单选择：4 进入 P2P 聊天；q 退出",
                    ChildState::Login => "登录输入：序号 / 资料 / 密码…",
                    ChildState::Chat => "/list、/chat、/trust …（单行命令）",
                };
                let edit = egui::TextEdit::singleline(&mut self.cmd_input)
                    .hint_text(hint)
                    .desired_width(ui.available_width() - 260.0)
                    .font(egui::TextStyle::Monospace);
                let resp = ui.add(edit);
                if self.first_frame && !in_chat {
                    resp.request_focus();
                }
                let run = ui.button("执行");
                let enter = resp.lost_focus()
                    && ui.ctx().input(|i| i.key_pressed(egui::Key::Enter));
                if (run.clicked() || enter) && !self.cmd_input.trim().is_empty() {
                    let text = self.cmd_input.trim().to_string();
                    self.cmd_input.clear();
                    match self.cmd_guard.process(text) {
                        Ok(final_text) => {
                            self.interact
                                .log(Level::Info, "user", format!("/cmd: {final_text}"));
                            if let Some(c) = &mut self.console {
                                c.send_line(&final_text);
                            } else {
                                self.lines.push("[控制台未启动]".to_string());
                            }
                        }
                        Err((rule, reason)) => {
                            self.lines.push(format!("[已拦截] {reason}"));
                            self.runtime
                                .log(Level::Warn, "guard", format!("拦截命令[{rule}]: {reason}"));
                        }
                    }
                }
                // 快捷命令（固定白名单，直接透传；仅聊天态有意义——主菜单下它们不是有效选择）
                for cmd in ["/list", "/q"] {
                    if ui.add_enabled(in_chat, egui::Button::new(cmd)).clicked() {
                        self.interact
                            .log(Level::Info, "user", format!("/cmd: {cmd}"));
                        if let Some(c) = &mut self.console {
                            c.send_line(cmd);
                        }
                    }
                }
            });

            // 文本框 = 纯文本：包成 /sendStrings <N> 协议发送（内容零变换，换行/引号/以 / 开头均原样）。
            // 非聊天态禁用——登录/菜单阶段 CLI 在等单行响应，文本框协议行会造成读取错位。
            ui.horizontal(|ui| {
                let edit = egui::TextEdit::multiline(&mut self.input)
                    .hint_text(if in_chat {
                        "输入消息，回车发送；Shift+回车换行（可粘贴多行文章）"
                    } else {
                        "登录/菜单操作请用上方命令框"
                    })
                    .desired_width(ui.available_width() - 88.0)
                    .desired_rows(3);
                let resp = ui.add_enabled(in_chat, edit);
                if self.first_frame && in_chat {
                    resp.request_focus();
                }
                let send = ui.add_enabled(in_chat, egui::Button::new("发送"));
                // 多行下回车不触发 lost_focus，直接按键判断：回车发送（Shift+回车换行）
                let send_now = in_chat
                    && (send.clicked()
                        || ui
                            .ctx()
                            .input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift));
                if send_now && !self.input.trim().is_empty() {
                    let text = self.input.trim().to_string();
                    self.input.clear();
                    // 文本框检查层（当前为空规则，接口保留）；通过后包成 sendStrings 协议发送
                    match self.text_guard.process(text) {
                        Ok(text) => {
                            self.interact
                                .log(Level::Info, "user", format!("> {text}"));
                            self.input_sent_at = Some(Instant::now());
                            match &mut self.console {
                                Some(c) => c.send_multiline(&text),
                                None => self.lines.push("[控制台未启动]".to_string()),
                            }
                        }
                        Err((rule, reason)) => {
                            self.lines.push(format!("[已拦截] {reason}"));
                            self.runtime
                                .log(Level::Warn, "guard", format!("拦截文本[{rule}]: {reason}"));
                        }
                    }
                }
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("P2P 聊天 GUI（终端式）");
                ui.toggle_value(&mut self.show_log, "日志");
            });
            ui.horizontal(|ui| {
                let state_color = match self.child_state {
                    ChildState::Chat => egui::Color32::from_rgb(120, 200, 120),
                    _ => egui::Color32::from_rgb(230, 180, 0),
                };
                ui.colored_label(
                    state_color,
                    format!("状态: {}", self.child_state.label()),
                );
                ui.separator();
                ui.weak(self.status_line());
            });
            ui.separator();

            // 滚动输出区（最新自动滚底）
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    if self.lines.is_empty() {
                        ui.weak("（暂无输出）");
                    } else {
                        for line in &self.lines {
                            ui.label(line);
                        }
                    }
                });
        });

        frame_timer.stop_and_record("frame.ui", &self.stats);
        self.first_frame = false;
    }
}

impl GuiApp {
    /// 延迟状态行（配合日志面板时间线对照分析）
    fn status_line(&self) -> String {
        let f = |label: &'static str| -> String {
            match self.stats.get(label) {
                Some(x) => format!("last={} max={}", ms(x.last), ms(x.max)),
                None => "-".to_string(),
            }
        };
        format!(
            "管线 drain {} · 输入→响应 {} · frame.ui {}",
            f("pipeline.drain"),
            f("roundtrip.input->resp"),
            f("frame.ui"),
        )
    }

    /// 右侧日志面板
    fn log_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.radio_value(&mut self.log_view, LogView::Runtime, "运行日志");
            ui.radio_value(&mut self.log_view, LogView::Interact, "交互日志");
        });
        ui.horizontal(|ui| {
            if ui.button("清空").clicked() {
                match self.log_view {
                    LogView::Runtime => {
                        self.runtime.clear();
                        self.pending_runtime.clear();
                    }
                    LogView::Interact => {
                        self.interact.clear();
                        self.pending_interact.clear();
                    }
                }
            }
            ui.checkbox(&mut self.log_follow, "跟随");
            egui::ComboBox::from_id_salt("log_level")
                .selected_text(self.log_level.as_str())
                .show_ui(ui, |ui| {
                    for lv in [Level::Trace, Level::Debug, Level::Info, Level::Warn, Level::Error]
                    {
                        ui.selectable_value(&mut self.log_level, lv, lv.as_str());
                    }
                });
        });
        ui.separator();

        let src = match self.log_view {
            LogView::Runtime => &self.pending_runtime,
            LogView::Interact => &self.pending_interact,
        };
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(self.log_follow)
            .show(ui, |ui| {
                let mut shown = 0usize;
                for e in src.iter().filter(|e| e.level >= self.log_level) {
                    let color = match e.level {
                        Level::Trace => egui::Color32::GRAY,
                        Level::Debug => ui.visuals().weak_text_color(),
                        Level::Info => ui.visuals().text_color(),
                        Level::Warn => egui::Color32::from_rgb(230, 180, 0),
                        Level::Error => egui::Color32::from_rgb(230, 60, 60),
                    };
                    ui.colored_label(
                        color,
                        format!("{} [{}] {}", logging::fmt_ts(e.ts), e.tag, e.msg),
                    );
                    shown += 1;
                }
                if shown == 0 {
                    ui.weak("（无日志）");
                }
            });
    }
}

impl Drop for GuiApp {
    /// 窗口关闭时杀掉子进程
    fn drop(&mut self) {
        if let Some(c) = &mut self.console {
            c.shutdown();
        }
    }
}

/// 缓存根目录：复用身份缓存根（P2P_ID_CACHE_DIR 可覆盖，默认 ~/.p2p_rust_app）
fn cache_root() -> PathBuf {
    if let Ok(dir) = std::env::var("P2P_ID_CACHE_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".p2p_rust_app")
}

/// 时长显示：毫秒（1 位小数）
fn ms(d: std::time::Duration) -> String {
    format!("{:.1}ms", d.as_secs_f64() * 1000.0)
}

/// 面板缓冲上限：超出丢弃最旧
fn cap(v: &mut Vec<logging::Entry>, max: usize) {
    if v.len() > max {
        let over = v.len() - max;
        v.drain(0..over);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_transitions_follow_marker_lines() {
        // 完整生命周期：主菜单 →（选 4）登录 →（登录成功）聊天 →（/q）回主菜单
        assert_eq!(
            next_state(ChildState::Menu, "[角色登录]"),
            ChildState::Login
        );
        assert_eq!(
            next_state(ChildState::Login, "发现模式: advertise（广播+发现）"),
            ChildState::Chat
        );
        assert_eq!(
            next_state(ChildState::Chat, "=== 主菜单 ==="),
            ChildState::Menu
        );
        // 主菜单标记在主菜单态：保持
        assert_eq!(
            next_state(ChildState::Menu, "=== 主菜单 ==="),
            ChildState::Menu
        );
    }

    #[test]
    fn state_ignores_chat_content_with_marker_text() {
        // 聊天内容都带 [对方]/[我 -> 前缀，精确整行匹配不会误触发
        assert_eq!(
            next_state(ChildState::Chat, "[对方] === 主菜单 ==="),
            ChildState::Chat
        );
        assert_eq!(
            next_state(ChildState::Chat, "[我 -> WJP] [角色登录]"),
            ChildState::Chat
        );
        assert_eq!(
            next_state(ChildState::Chat, "[对方] 发现模式: xxx"),
            ChildState::Chat
        );
        // 普通行保持原状态
        assert_eq!(next_state(ChildState::Chat, "hello"), ChildState::Chat);
        assert_eq!(next_state(ChildState::Login, "88888888"), ChildState::Login);
    }
}
