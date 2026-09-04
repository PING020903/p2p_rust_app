//! GUI 应用（P2.0 进程内引擎）：滚动区 + 输入区直驱聊天核心。
//! 引擎线程（单线程 current_thread runtime）跑 run_node(LineSource::Channel)——
//! 聊天核心与 GUI 同进程：文本框直发引擎（无 /sendStrings 协议）、输出经 sink 通道进滚动区。
//! 诊断组件与双文件日志保留；纯 CLI 模式（--cli/管道）与 GUI 共用同一份核心代码。

pub mod fonts;
pub mod input_guard;
pub mod logging;
pub mod timing;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::chat;
use crate::p2p::identity::LoginOutcome;
use crate::p2p::identity_service::{InputMsg, LineSource};
use crate::p2p_app::chat::gui::login;
use crate::sink;
use input_guard::InputGuard;
use logging::{Level, LogStore};
use tokio::sync::mpsc;

/// 日志面板视图切换
#[derive(Clone, Copy, PartialEq, Eq)]
enum LogView {
    Runtime,
    Interact,
}

/// 中区时间线条目：系统文本行与结构化聊天气泡混排（单列表保时序）
enum TimelineItem {
    Line(String),
    Chat {
        msg: crate::uievent::ChatMessage,
        /// 到达时刻（HH:MM，本地时区）
        at: String,
    },
}

impl TimelineItem {
    fn line(s: impl Into<String>) -> Self {
        TimelineItem::Line(s.into())
    }

    fn chat(msg: crate::uievent::ChatMessage) -> Self {
        TimelineItem::Chat {
            msg,
            at: chrono::Local::now().format("%H:%M").to_string(),
        }
    }
}

/// 引擎界面状态：由引擎输出特征行推断（精确整行匹配——聊天内容都带
/// `[对方] `/`[我 -> ` 前缀，正文同款文本不会误触发）。
/// GUI 模式无主菜单：进程内引擎直接进入登录流程。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChildState {
    /// 登录流程（[角色登录] 之后：身份/资料/密码输入）
    Login,
    /// 已进入聊天循环（打印"发现模式: "之后）
    Chat,
}

impl ChildState {
    fn label(self) -> &'static str {
        match self {
            ChildState::Login => "登录",
            ChildState::Chat => "聊天中",
        }
    }
}

/// 按输出行推进状态机：登录 →（登录成功）聊天
fn next_state(current: ChildState, line: &str) -> ChildState {
    if line == "[角色登录]" {
        ChildState::Login
    } else if line.starts_with("发现模式: ") {
        ChildState::Chat
    } else {
        current
    }
}

pub struct GuiApp {
    /// 中区时间线（系统行 + 聊天气泡，单列表保时序）
    timeline: Vec<TimelineItem>,
    /// 输入框内容
    input: String,
    /// 命令输入框内容（/list 等；与文本框分离）
    cmd_input: String,
    /// UI → 引擎输入通道（引擎启动后有效；文本框 ChatText / 命令框 Line）
    ui_tx: Option<mpsc::UnboundedSender<InputMsg>>,
    /// 引擎 → UI 输出通道（TextSink 捕获的引擎输出；Line/Event 统一枚举保序）
    out_rx: Option<mpsc::UnboundedReceiver<crate::uievent::EngineOut>>,
    /// 引擎任务结束标记（聊天退出 → GUI 联动关闭）
    engine_done: Arc<AtomicBool>,
    /// 登录页状态机（引擎未启动阶段的全窗口卡片）
    login: login::LoginState,
    /// 左栏侧栏快照（联系人 + 群；引擎侧推送，登录期为 None）
    sidebar: Option<crate::uievent::SidebarState>,
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
    /// 引擎界面状态（决定文本框启停与命令框提示）
    child_state: ChildState,
}

impl GuiApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let runtime = Arc::new(LogStore::new(5000));
        let interact = Arc::new(LogStore::new(5000));
        let mut lines: Vec<TimelineItem> = Vec::new();

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
                Err(e) => lines.push(TimelineItem::line(format!("日志目录写入失败: {e}"))),
            }
        }
        runtime.log(Level::Info, "gui", format!("GUI 启动，日志目录: {log_note}"));
        runtime.log(Level::Info, "gui", format!("缓存根: {}", cache_root().display()));
        runtime.log(Level::Info, "gui", format!("CJK 字体: {font_note}"));

        // 引擎在登录成功后启动（GUI 登录表单产出凭据 → run_engine(Some(outcome))）
        let engine_done = Arc::new(AtomicBool::new(false));

        GuiApp {
            timeline: lines,
            input: String::new(),
            cmd_input: String::new(),
            ui_tx: None,
            out_rx: None,
            engine_done,
            login: login::LoginState::Menu {
                identities: login::load_cached(),
                error: None,
            },
            sidebar: None,
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
            child_state: ChildState::Login,
        }
    }

    /// 启动聊天引擎线程：GUI 登录表单凭据（Some）直建会话；
    /// 单线程 current_thread runtime，线程局部 sink 在本线程生效。
    /// `egui_ctx` 供引擎主动唤醒 UI：输出 send 后 notify、退出前置位 done 再唤醒
    /// （纯事件驱动，无轮询；顺序关键——先置位再唤醒，保证唤醒帧必能看到退出标记）。
    fn start_engine(&mut self, pre: Option<LoginOutcome>, egui_ctx: egui::Context) {
        let (out_tx, out_rx) = mpsc::unbounded_channel::<crate::uievent::EngineOut>();
        let (ui_tx, ui_rx) = mpsc::unbounded_channel::<InputMsg>();
        let done = self.engine_done.clone();
        let engine_log = self.runtime.clone();
        let spawn_result = std::thread::Builder::new()
            .name("chat-engine".into())
            .spawn(move || {
                // 唤醒回调：sink 每次 send 后调用（egui 类型封在闭包内，不穿透 sink 签名）
                let notify = {
                    let ctx = egui_ctx.clone();
                    Box::new(move || ctx.request_repaint()) as Box<dyn Fn() + Send>
                };
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        engine_log.log(
                            Level::Error,
                            "engine",
                            format!("引擎 runtime 构建失败: {e}"),
                        );
                        done.store(true, Ordering::Release);
                        egui_ctx.request_repaint();
                        return;
                    }
                };
                sink::install(out_tx, Some(notify));
                engine_log.log(Level::Info, "engine", "引擎线程启动（进程内聊天核心）");
                if let Err(e) = rt.block_on(chat::run_engine(LineSource::Channel(ui_rx), pre)) {
                    engine_log.log(Level::Error, "engine", format!("引擎错误: {e}"));
                }
                engine_log.log(Level::Info, "engine", "引擎已退出");
                sink::uninstall();
                // 先置位再唤醒：这一帧必能看到退出标记 → GUI 联动关闭
                done.store(true, Ordering::Release);
                egui_ctx.request_repaint();
            });
        match spawn_result {
            Ok(_) => {
                self.ui_tx = Some(ui_tx);
                self.out_rx = Some(out_rx);
                self.child_state = ChildState::Chat;
                self.timeline
                    .push(TimelineItem::line("聊天引擎已启动（进程内模式）"));
            }
            Err(e) => {
                self.runtime
                    .log(Level::Error, "gui", format!("引擎线程启动失败: {e}"));
                self.timeline
                    .push(TimelineItem::line(format!("引擎线程启动失败: {e}")));
            }
        }
    }
    /// 发送输入到引擎（通道未就绪时降级为时间线提示）
    fn send_input(&mut self, msg: InputMsg) {
        match &self.ui_tx {
            Some(tx) => {
                if let Err(e) = tx.send(msg) {
                    self.timeline
                        .push(TimelineItem::line(format!("[引擎输入通道关闭: {e}]")));
                }
            }
            None => self.timeline.push(TimelineItem::line("[引擎未运行]")),
        }
    }
}

impl eframe::App for GuiApp {
    /// 每帧（含窗口隐藏时）的逻辑回调：drain 引擎输出 + 轮询引擎退出 + 计时（不能画 UI）。
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let _t = timing::ScopeTimer::start("frame.logic", &self.stats);
        if let Some(out_rx) = &mut self.out_rx {
            while let Ok(out) = out_rx.try_recv() {
                if let Some(t0) = self.input_sent_at.take() {
                    self.stats.record("roundtrip.input->resp", t0.elapsed());
                }
                match out {
                    crate::uievent::EngineOut::Line(line) => {
                        self.child_state = next_state(self.child_state, &line);
                        self.interact.log(Level::Info, "engine", &line);
                        self.timeline.push(TimelineItem::line(line));
                    }
                    // 气泡：结构化消息进时间线（日志仍按 CLI 文本形态落盘）
                    crate::uievent::EngineOut::Event(crate::uievent::UiEvent::Chat(m)) => {
                        self.interact.log(Level::Info, "engine", m.to_cli_line());
                        self.timeline.push(TimelineItem::chat(m));
                    }
                    // 侧栏快照：整体替换（推送点：命令处理后 + 每个传输事件后）
                    crate::uievent::EngineOut::Event(crate::uievent::UiEvent::Sidebar(s)) => {
                        self.sidebar = Some(s);
                    }
                }
                ctx.request_repaint();
            }
        }
        // 生命周期联动：引擎任务结束（聊天退出）→ GUI 一并关闭
        if self.engine_done.swap(false, Ordering::AcqRel) {
            self.runtime
                .log(Level::Info, "engine", "引擎已退出，GUI 即将关闭");
            self.timeline.push(TimelineItem::line("[引擎已退出]"));
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            ctx.request_repaint();
        }
        self.runtime.drain(&mut self.pending_runtime);
        self.interact.drain(&mut self.pending_interact);
        cap(&mut self.pending_runtime, 5000);
        cap(&mut self.pending_interact, 5000);

        // 纯事件驱动，无轮询：引擎输出/退出均由引擎线程主动 request_repaint 唤醒；
        // 空闲时主线程阻塞在事件队列上（零 CPU），用户输入/缩放由 OS 事件天然触发
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

        // 登录页（引擎未启动）：全窗口卡片，无底部输入面板；成功后带凭据启动引擎
        if self.child_state == ChildState::Login {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("P2P 聊天");
                    ui.toggle_value(&mut self.show_log, "日志");
                });
                if let Some(outcome) = login::view(&mut self.login, ui) {
                    self.interact.log(Level::Info, "user", "登录成功（GUI 表单）");
                    self.runtime
                        .log(Level::Info, "gui", "GUI 登录完成，启动聊天引擎");
                    self.start_engine(Some(outcome), ui.ctx().clone());
                }
            });
            self.first_frame = false;
            return;
        }

        // 左栏：联系人（信任徽标/在线/焦点）+ 群列表；点击发 /chat|/group 命令（复用 CLI 语义）
        let mut sidebar_cmd: Option<String> = None;
        if let Some(sb) = &self.sidebar {
            egui::Panel::left("sidebar")
                .default_size(210.0)
                .resizable(true)
                .show(ui, |ui| {
                    sidebar_cmd = render_sidebar(ui, sb);
                });
        }
        if let Some(cmd) = sidebar_cmd {
            self.interact.log(Level::Info, "user", format!("/cmd: {cmd}"));
            self.input_sent_at = Some(Instant::now());
            self.send_input(InputMsg::Line(cmd));
        }

        // 底部输入面板先声明 → 先占位，CentralPanel 只拿剩余高度（ScrollArea 不会挤掉输入行）
        egui::Panel::bottom("input").show(ui, |ui| {
            let in_chat = self.child_state == ChildState::Chat;
            ui.add_space(4.0);

            // 命令输入行（与文本框分离；guard 拦截穿透命令；提示随状态变化）
            ui.horizontal(|ui| {
                ui.label("命令");
                let hint = match self.child_state {
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
                            self.input_sent_at = Some(Instant::now());
                            self.send_input(InputMsg::Line(final_text));
                        }
                        Err((rule, reason)) => {
                            self.timeline
                                .push(TimelineItem::line(format!("[已拦截] {reason}")));
                            self.runtime
                                .log(Level::Warn, "guard", format!("拦截命令[{rule}]: {reason}"));
                        }
                    }
                }
                // 快捷命令（固定白名单，直接透传；仅聊天态有意义）
                for cmd in ["/list", "/q"] {
                    if ui.add_enabled(in_chat, egui::Button::new(cmd)).clicked() {
                        self.interact
                            .log(Level::Info, "user", format!("/cmd: {cmd}"));
                        self.input_sent_at = Some(Instant::now());
                        self.send_input(InputMsg::Line(cmd.to_string()));
                    }
                }
            });

            // 文本框 = 纯聊天文本：ChatText 直进引擎（多行原样、/ 开头也不解析为命令、无协议包装）
            ui.horizontal(|ui| {
                let edit = egui::TextEdit::multiline(&mut self.input)
                    .hint_text(if in_chat {
                        "输入消息，回车发送；Shift+回车换行（可粘贴多行文章）"
                    } else {
                        "登录操作请用上方命令框"
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
                    // 文本框检查层（当前为空规则，接口保留）；通过后以 ChatText 直进引擎
                    match self.text_guard.process(text) {
                        Ok(text) => {
                            self.interact
                                .log(Level::Info, "user", format!("> {text}"));
                            self.input_sent_at = Some(Instant::now());
                            self.send_input(InputMsg::ChatText(text));
                        }
                        Err((rule, reason)) => {
                            self.timeline
                                .push(TimelineItem::line(format!("[已拦截] {reason}")));
                            self.runtime
                                .log(Level::Warn, "guard", format!("拦截文本[{rule}]: {reason}"));
                        }
                    }
                }
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("P2P 聊天 GUI");
                ui.toggle_value(&mut self.show_log, "日志");
            });
            ui.horizontal(|ui| {
                let state_color = match self.child_state {
                    ChildState::Chat => egui::Color32::from_rgb(120, 200, 120),
                    ChildState::Login => egui::Color32::from_rgb(230, 180, 0),
                };
                ui.colored_label(
                    state_color,
                    format!("状态: {}", self.child_state.label()),
                );
                ui.separator();
                ui.weak(self.status_line());
            });
            ui.separator();

            // 滚动输出区（最新自动滚底）：系统行 + 聊天气泡混排
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    if self.timeline.is_empty() {
                        ui.weak("（暂无输出）");
                    } else {
                        for item in &self.timeline {
                            match item {
                                TimelineItem::Line(s) => {
                                    ui.label(s);
                                }
                                TimelineItem::Chat { msg, at } => {
                                    render_bubble(ui, msg, at);
                                }
                            }
                        }
                    }
                });
        });

        frame_timer.stop_and_record("frame.ui", &self.stats);
        self.first_frame = false;
    }
}

/// 侧栏视图：联系人（在线点 + 名字 + 信任徽标）与群列表；返回点击产生的命令
fn render_sidebar(
    ui: &mut egui::Ui,
    sb: &crate::uievent::SidebarState,
) -> Option<String> {
    let mut cmd = None;

    ui.heading("联系人");
    if sb.contacts.is_empty() {
        ui.weak("（暂无联系人）");
    }
    for c in &sb.contacts {
        ui.horizontal(|ui| {
            let (dot, dot_color) = if c.online {
                ("●", egui::Color32::from_rgb(90, 200, 120))
            } else {
                ("○", egui::Color32::from_rgb(110, 118, 130))
            };
            ui.label(egui::RichText::new(dot).color(dot_color));
            // 焦点会话加粗高亮；悬浮显示节点ID（重名时人工核对）
            let name_text = if c.focused {
                egui::RichText::new(&c.name).strong()
            } else {
                egui::RichText::new(&c.name)
            };
            let resp = ui.add(egui::Label::new(name_text).selectable(false));
            if resp.clicked() {
                cmd = Some(format!("/chat {}", c.name));
            }
            resp.on_hover_text(&c.peer_id);
            let (badge, color) = if c.effective_trusted {
                ("[互信]", egui::Color32::from_rgb(90, 200, 120))
            } else if c.i_trust {
                ("[我信任]", egui::Color32::from_rgb(230, 180, 0))
            } else {
                ("[未信任]", egui::Color32::from_rgb(110, 118, 130))
            };
            ui.label(egui::RichText::new(badge).small().color(color));
        });
    }

    ui.separator();
    ui.heading("群");
    if sb.groups.is_empty() {
        ui.weak("（暂无群）");
    }
    for g in &sb.groups {
        ui.horizontal(|ui| {
            let label = if g.focused {
                egui::RichText::new(&g.name).strong()
            } else {
                egui::RichText::new(&g.name)
            };
            if ui
                .add(egui::Label::new(label).selectable(false))
                .clicked()
            {
                cmd = Some(format!("/group {}", g.name));
            }
            ui.weak(format!("{}人", g.member_count));
        });
    }
    cmd
}

/// 聊天气泡：对侧左对齐、我侧右对齐；头部小字（名字/群前缀 + 时刻），正文自动换行。
/// 配色：我侧绿、对侧深灰蓝、未信任黄（描边语义用文字 ⚠ 前缀）。
///
/// 布局（手工测量 + 手绘，方向无关的确定性尺寸）：
/// 1. 先用 galley 排版测量——正文按 `列宽 − 内边距` 换行（支持多行），头部单行；
/// 2. 气泡尺寸 = max(头宽, 文宽) + 内边距，与布局方向完全无关；
/// 3. `with_layout` 定锚定侧（我侧右/对侧左）→ `allocate_exact_size` 精确放置 →
///    painter 画圆角矩形 + 放置两个 galley；时间戳贴气泡旁。
/// （不用 Frame 自动尺寸：egui RTL 下 Frame 撑满锚定列与 LTR 行为不对称，见 b1 实测）
fn render_bubble(ui: &mut egui::Ui, msg: &crate::uievent::ChatMessage, at: &str) {
    let header = if msg.outgoing {
        format!("我 -> {}", msg.group.as_deref().unwrap_or(&msg.from))
    } else {
        let name = match &msg.group {
            Some(g) if !msg.focused => format!("[{g}] {}", msg.from),
            _ => msg.from.clone(),
        };
        if msg.untrusted {
            format!("⚠ 未信任 {name}")
        } else {
            name
        }
    };
    let (fill, text_color, head_color) = if msg.untrusted {
        (
            egui::Color32::from_rgb(64, 52, 16),
            egui::Color32::from_rgb(240, 210, 120),
            egui::Color32::from_rgb(230, 180, 0),
        )
    } else if msg.outgoing {
        (
            egui::Color32::from_rgb(22, 72, 54),
            egui::Color32::from_rgb(200, 245, 215),
            egui::Color32::from_rgb(140, 200, 165),
        )
    } else {
        (
            egui::Color32::from_rgb(42, 47, 58),
            egui::Color32::from_rgb(225, 230, 240),
            egui::Color32::from_rgb(150, 160, 180),
        )
    };

    // 字体随主题（Body/Small），颜色在排版时写入 galley
    let body_font = ui
        .style()
        .text_styles
        .get(&egui::TextStyle::Body)
        .cloned()
        .unwrap_or_else(|| egui::FontId::proportional(14.0));
    let small_font = ui
        .style()
        .text_styles
        .get(&egui::TextStyle::Small)
        .cloned()
        .unwrap_or_else(|| egui::FontId::proportional(10.0));

    // 排版测量（每帧执行、galley 缓存兜底；缩放窗口即重排）
    let col_w = ui.available_width() * 0.80;
    let (pad_x, pad_y, head_gap) = (8.0_f32, 5.0_f32, 3.0_f32);
    let wrap_w = (col_w - pad_x * 2.0).max(60.0);
    let painter = ui.painter().clone();
    let head_gal = painter.layout_no_wrap(header, small_font, head_color);
    let text_gal = painter.layout(msg.text.clone(), body_font, text_color, wrap_w);

    let bubble_w = head_gal.size().x.max(text_gal.size().x) + pad_x * 2.0;
    let bubble_h = head_gal.size().y + head_gap + text_gal.size().y + pad_y * 2.0;

    let layout_dir = if msg.outgoing {
        egui::Layout::right_to_left(egui::Align::TOP)
    } else {
        egui::Layout::left_to_right(egui::Align::TOP)
    };
    ui.with_layout(layout_dir, |ui| {
        let (rect, _resp) =
            ui.allocate_exact_size(egui::vec2(bubble_w, bubble_h), egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, egui::CornerRadius::same(10), fill);
        let head_h = head_gal.size().y;
        let origin = rect.min + egui::vec2(pad_x, pad_y);
        painter.galley(origin, head_gal, head_color);
        painter.galley(
            origin + egui::vec2(0.0, head_h + head_gap),
            text_gal,
            text_color,
        );
        ui.weak(at);
    });
    ui.add_space(2.0);
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
            "输入→响应 {} · frame.ui {}",
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
        // GUI 模式生命周期：直接进入登录 →（登录成功）聊天
        assert_eq!(
            next_state(ChildState::Login, "[角色登录]"),
            ChildState::Login
        );
        assert_eq!(
            next_state(ChildState::Login, "发现模式: advertise（广播+发现）"),
            ChildState::Chat
        );
    }

    #[test]
    fn state_ignores_chat_content_with_marker_text() {
        // 聊天内容都带 [对方]/[我 -> 前缀，精确整行匹配不会误触发
        assert_eq!(
            next_state(ChildState::Chat, "[对方] [角色登录]"),
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
