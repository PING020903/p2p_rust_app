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

pub struct GuiApp {
    /// 滚动区文本行（子进程输出 + 本机状态）
    lines: Vec<String>,
    /// 输入框内容
    input: String,
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
    /// 输入检查&修改规则链
    guard: InputGuard,
}

impl GuiApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        fonts::install(&cc.egui_ctx);
        let runtime = Arc::new(LogStore::new(5000));
        let interact = Arc::new(LogStore::new(5000));
        let mut lines = Vec::new();

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
            guard: InputGuard::new(),
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
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let edit = egui::TextEdit::multiline(&mut self.input)
                    .hint_text("输入命令 / 消息，回车发送；Shift+回车换行")
                    .desired_width(ui.available_width() - 88.0)
                    .desired_rows(3);
                let resp = ui.add(edit);
                if self.first_frame {
                    resp.request_focus();
                }
                let send = ui.button("发送");
                // 多行下回车不触发 lost_focus，直接按键判断：回车发送（Shift+回车换行）
                let send_now = send.clicked()
                    || ui.ctx().input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
                if send_now && !self.input.trim().is_empty() {
                    let text = self.input.trim().to_string();
                    self.input.clear();
                    let len = text.len();
                    // 输入检查&修改层：拦截穿透命令、折叠多行换行
                    match self.guard.process(text) {
                        Ok(final_text) => {
                            // 交互日志：记录用户输入（密码阶段暂按用户要求原样记录）
                            self.interact
                                .log(Level::Info, "user", format!("> {final_text}"));
                            self.input_sent_at = Some(Instant::now());
                            match &mut self.console {
                                Some(c) => c.send_line(&final_text),
                                None => self.lines.push("[控制台未启动]".to_string()),
                            }
                        }
                        Err((rule, reason)) => {
                            self.lines.push(format!("[已拦截] {reason}"));
                            self.runtime.log(
                                Level::Warn,
                                "guard",
                                format!("拦截输入[{rule}]: {reason}（原文 {len}B）"),
                            );
                            self.interact
                                .log(Level::Warn, "user", format!("[拦截] {reason}"));
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
            ui.weak(self.status_line());
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
