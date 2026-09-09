# UI 规划与待办（egui/eframe GUI）

> **状态：待办（未开始）**——为现有 CLI 应用新增图形界面。
> 技术选型：**egui/eframe**（纯 Rust 即时模式，无 Node/WebView 依赖，Windows/Linux/WSLg 直接编译运行）。
> 已定决策：**保留 CLI + 新增 GUI bin**（`p2p_rust_app_gui`）；**渐进式**（先终端式再原生）；**仅 Windows 验证**（Linux 保证编译通过即可）。

---

## 总原则

L1/L2/L3 三层、信号注册表、群/文件/信任逻辑**零改动**。GUI 只做两件事：换输入源（`stdin` → UI 通道）、换输出渲染（`println!` → UI 事件）。

```
现状：chat.rs::run_node() 双路 select!（chat.rs:1692）
  输入腿  line = stdin.next_line()    （命令/消息）
  事件腿  event = ev_rx.recv()        （连接/信号/gossip）
  输出    println!/eprintln!           （colored ANSI）
  交互确认 内嵌 await stdin（登录 / TOFU 指纹 / /backup 密码 / 未信任发送 / 文件接收 y/n）

GUI：tokio 后台线程跑核心，UI 主线程每帧 drain 事件通道
  核心→UI  mpsc::UnboundedSender<UiOut>（Text/Ask/Status）
  UI→核心  mpsc::UnboundedSender<UiIn> （输入行）
  后端 send 后 ctx.request_repaint()
```

---

## P0 — 骨架（✅ 已完成）

- [x] **Cargo.toml**：加 `[[bin]] p2p_rust_app_gui`（path `source/main_gui.rs`）+ `default-run = "p2p_rust_app"`（保住 `cargo run` 走 CLI）+ `eframe 0.36.1`（默认 wgpu 渲染器）
- [x] **`source/main_gui.rs`**：入口 `eframe::run_native`（1200x800）
- [x] **`source/ui/mod.rs`**：`GuiApp` 实现 eframe 0.36 新 `App` trait（`logic()` drain 通道 + `ui()` 渲染）；滚动区 + 输入框
- [x] **`source/ui/fonts.rs`**：CJK 字体回退链（Windows `msyh.ttc`→`simhei.ttf`；Linux Noto CJK/文泉驿；macOS PingFang），`Arc<FontData>` 注入家族末尾 fallback
- [x] **异步集成**：后台 std 线程 + `tokio::sync::mpsc::unbounded_channel`（send 同步、`try_recv` 每帧 drain）+ `ctx.request_repaint()` 演示
- [x] 验收：`cargo build --bin p2p_rust_app_gui` 通过；启动 6 秒存活未崩溃；`cargo run` 仍 CLI

> **实现要点（2026-08-30）**：
> - eframe 0.36 `App` trait 主方法改为 `fn ui(&mut self, ui: &mut egui::Ui, frame)`（原 `update` 移除），另有可选 `logic(&mut self, ctx, frame)`（隐藏时也调用、不能画 UI）——drain 通道放 `logic`，渲染放 `ui`，`CentralPanel::show(ui, …)` 直接收 `&mut Ui`
> - `egui` 需为**直接依赖**（eframe 不再 re-export）
> - `std::sync::mpsc::unbounded_channel`/`UnboundedReceiver` 在 **Rust 1.97 已被移除**（`channel`/`sync_channel` 保留）——改用 tokio 的 unbounded 通道（send 为同步方法，无需运行时）
> - `FontData::from_owned` 返回 `FontData`，插入 `font_data` map 需包 `Arc`

## P1 — 终端式 GUI（✅ 已完成）

> 方案：**嵌入式控制台**——复用 e2e 既有模式（tests/common/mod.rs 的 `Node::spawn` = spawn CLI + 管道驱动，已 battle-tested）。

- [x] **`source/ui/console.rs`**：spawn `p2p_rust_app.exe`（`current_exe()` 同目录兄弟 bin），stdout/stderr 双线程管道 → `UiOut::Text`，子进程退出 `try_wait` 非阻塞轮询；窗口关闭 `Drop` 杀子进程
- [x] **输入框**：`egui::TextEdit`；管道下 CLI `interactive=false` → 密码行读取**不回显**；`TextEdit.password` 留待 P2 表单
- [x] **滚动文本区**：无换行部分输出（提示符）即时上屏；`stick_to_bottom` 自动滚底
- [x] 验收：GUI 存活、spawn 出 CLI 子进程、关闭后子进程被回收；`cargo build` 全绿

> **P1 行为说明**：
> - 用户输入**不做本地回显**（密码防泄漏；聊天发送由 CLI 自打 `[我 -> 名]` 回显）
> - 管道模式下 CLI 的 TOFU 指纹确认 / 文件接收 / 未信任发送确认走**自动放行**语义——P2 原生弹窗再接管
> - `colored` 在管道下自动关闭 ANSI，无需剥色码

## P1.5 — 诊断组件（✅ 已完成）

> 目的：量化"子进程输出 → 屏幕可见"延迟 + 自动输出日志，方便分析对比。

- [x] **耗时组件 `source/ui/timing.rs`**：`Sample`（count/last/max/avg）+ 线程安全 `TimingStats` + 手动 `Timer` + 作用域 `ScopeTimer`（Drop guard）；零依赖、可复用（P2 抽 lib 后上移共享）
- [x] **运行日志组件 `source/ui/logging.rs`**：线程安全环形缓冲 `LogStore` + 可选文件落盘；`Level` 分级；**chrono 本地时区**时间戳（目录名与行内时间戳均为真实本地时间）
- [x] **双文件日志**：缓存根 `~/.p2p_rust_app/gui_logs/<YYYYMMDD-HHMMSS>/`（`P2P_ID_CACHE_DIR` 可覆盖根），每次运行一个时间戳文件夹：
  - `runtime.log` 软件运行日志：启停 / 控制台成败 / 子进程退出 / 管线延迟
  - `interact.log` 用户交互输入输出：子进程每行输出 + 用户每次输入，均带时间戳（密码阶段暂按用户要求原样记录）
- [x] **自动打点**：console reader 每行写 `[child]` 到 interact；`send_line` 写 `[user]`；`logic()` 记录 `pipeline.drain` / `roundtrip.input->resp`；`ScopeTimer` 包 `frame.logic`/`frame.ui`
- [x] **日志面板**：顶栏「日志」开关 → 右侧 `Panel::right`，radio 切换 运行/交互，级别过滤（ComboBox）+ 级别着色 + 跟随/暂停 + 清空
- [x] **状态行**：`管线 drain last/max · 输入→响应 last/max · frame.ui last/max`
- [x] 验证：`gui_logs/<ts>/` 双文件生成、内容正确；civil 单测 2 项通过

## P1.6 — GUI 治理与生命周期（✅ 已完成）

> 针对运行反馈的四个"功能不完整/逻辑漏洞"：

- [x] **日志本地时间**：`logging.rs` 改用 `chrono`（`Local::now()` / `DateTime::from(SystemTime).with_timezone(&Local)`），目录名与行内时间戳均为真实本地时区
- [x] **输入检查&修改层 `source/ui/input_guard.rs`**：`Rule` trait + `InputGuard` 规则链（**检查&修改接口，可扩展**，拦截返回 `(规则名, 理由)` 便于日志）：
  - `BlockTerminalEscape`：拦截 `cmd/`、`ps/`、`sh/` —— GUI 禁止穿透操作命令行（CLI 保留）
  - `CollapseNewlines`：多行折叠为一行（修"换行没发出去"拆行 bug）
  - 单测 4 项（拦截/放行/改写/改写后再拦截）
- [x] **GUI 启动器分离**：`main_gui.rs` 无 `P2P_GUI_CHILD` → `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` spawn 自己后立即退出（**原命令行释放**）；带标记才进 GUI；spawn 失败回退前台；`windows_subsystem="windows"` **无条件**（双击/启动器均无黑窗闪显）；Unix 不分离
- [x] **CLI 子进程禁窗**：`console.rs` spawn CLI 加 `CREATE_NO_WINDOW`——分离后 GUI 无控制台，若不禁窗 Windows 会给 console 子进程新建常驻黑窗（stdio 仍管道，行为不变）
- [x] **生命周期联动**：CLI 子进程退出（`poll_exit`）→ 记 Info(0)/Warn(非零) → `ViewportCommand::Close` 关 GUI；**空闲低频轮询**（`request_repaint_after(500ms)`）解决"子进程退出后无新输出触发重绘 → 退出检测永不执行"的漏洞；双向绑定（GUI 关→杀 CLI 已有，CLI 退→GUI 关新增）
- [x] 验证：启动器退出+分离 GUI 存活+子 CLI 在跑+**无伴随黑窗**（窗口列表仅见 "P2P 聊天 GUI"）+杀 CLI 后 GUI 联动关闭；日志目录为本地时间；`cargo run --bin p2p_rust_app_gui` 现在**立即返回**（GUI 独立进程运行，行为已写入 README/指南）


## P1.7 — 多行发送 + 文本/命令分离（✅ 已完成）

> 诉求：文本框粘贴多行文章（含空行/引号/以 `/` 开头）作为**一条消息**原样发送，不被拆散、不误解析。
> 协议调研后定为**行数声明**（netstring 思路）：流式管道里"任意内容 verbatim + 引号定界"互斥（无法定位流中最后一个引号），
> 长度前缀是业界对"任意负载"的标准解。

- [x] **协议**：`/sendStrings <N>` + 恰好 N 行原文——内容**零解析、零转义**（空行、`/` 开头行、引号原样）；CLI 按行数精确收集拼接发送；EOF 未收满 → 报错丢弃；N=0/非法 → 用法提示
- [x] **CLI**：抽 `send_focused_text` 共享发送函数（群/1v1/信任门控/回显，普通消息与多行共用，行为等价 e2e 守护）；主循环 stdin 收集态**优先于 trim/空行跳过**（空行保留、`/` 开头不解析）；`parse_line_count`/`collect_multiline` + 单测 3 项；帮助同步
- [x] **GUI 文本框 = 纯文本**：`send_multiline` 包 `/sendStrings <N>` 发送，`/list` 等以 `/` 开头的内容作为聊天文本发出（不再触发命令）；interact 日志记原文
- [x] **独立命令输入行**（文本框上方）：guard 拦 `cmd/`/`ps/`/`sh/` 穿透 + `/sendStrings` 多行入口，其余透传 CLI
- [x] `input_guard` 按输入源分派（命令框两条规则 / 文本框预留空规则链）；单测更新
- [x] **e2e +1 场景**：空行与 `/` 开头行原样、引号原样（`p2p_chat.rs` suite 注册）；逻辑 e2e 全量回归通过
- [x] 冒烟：启动器分离/无黑窗/生命周期联动不回归

> 已知边界：CLI 终端手输 `/sendStrings` 需自数行数（GUI 自动计数）；`\r\n` 经管道被行读取器剥成 `\n`（渲染无差，P2 进程内方案字节保真）。


## P1.8 — 状态感知 + 快捷命令（✅ 已完成）

> 针对双客户端实测发现的 UX 问题：文本框在登录/主菜单阶段协议错位、用户习惯在文本框输命令。
> 纯 GUI 侧改动，零 CLI 变更；按"先逻辑后 UI"原则在逻辑批次完成后实施。

- [x] **子进程状态机**：`ChildState{Menu,Login,Chat}`，`logic()` drain 输出行时按特征行推进
  （`=== 主菜单 ===` → Menu / `[角色登录]` → Login / `发现模式: ` 前缀 → Chat）——**精确整行匹配**，
  聊天内容带 `[对方]`/`[我 -> ` 前缀不会误触发；单测 2 项（生命周期转移矩阵 / 内容同款文本不触发）
- [x] **文本框按状态启停**：仅 `Chat` 态启用（sendStrings 路径不变）；`Menu/Login` 态
  `add_enabled(false)` + placeholder"登录/菜单操作请用上方命令框"——**根治登录阶段协议错位误用**；
  发送逻辑（含回车判定）整体加 `in_chat` 守卫
- [x] **命令框动态提示**：Menu →"菜单选择：4 进入 P2P 聊天；q 退出"/ Login →"登录输入：序号/资料/密码…"/ Chat → 命令提示
- [x] **快捷命令按钮**：命令框旁 `/list`、`/q`（固定白名单直接透传；仅聊天态启用——主菜单下它们不是有效选择）
- [x] **状态行**：`状态: 聊天中/登录中/主菜单`（聊天绿/其他黄）+ 延迟统计
- [x] **首帧焦点**：聊天态聚焦文本框，否则聚焦命令框（引导先登录）
- [x] 验证：构建全绿 + 单测 6 项 + 冒烟（启动器/无黑窗/生命周期联动不回归）

## P1.9 — 双平台构建就绪（✅ 已完成）

> 目标：一套代码 Windows/Linux 双平台运行与产物统一收集。

- [x] **xtask 归档双 bin**：`cargo xtask build` 同时归档 `p2p_rust_app` + `p2p_rust_app_gui` 到
  `target/{profile}/bin/<os>-<arch>/`（Windows 与 WSL/Linux 各自构建，产物按系统目录自然汇集）
- [x] **Linux 分离启动对齐**：`cfg(unix)` 用 `process_group(0)` + stdio null 分离 spawn，
  与 Windows 启动器体验一致（Linux 下 GUI 也不占终端）；`windows_subsystem` 属性加 `cfg(windows)` 门控
- [x] **WSLg 实测通过**：Linux 全量构建（egui 0.36 全家 + wgpu）✓；分离启动（启动器立即退出、
  后台 GUI 存活）✓；子进程 CLI 正常拉起（interact.log 200ms 内收到主菜单）✓；日志双文件落盘 ✓
- [x] **WSL 环境备注**：发行版 cargo 1.93 不满足 egui 0.36 的 rustc≥1.95 要求 → 改用
  rustup stable（1.98）；需补运行库 `libxkbcommon-x11-0`（winit X11 后端 dlopen 依赖）
- [x] CLI 9 个历史告警清零（未用导入/`cQ` 命名；cmd_tree C 版对齐接口显式豁免；
  `effective_trusted` 复用 `their_trust` 消除死包装）

## P1.10 — 内置 CJK 字体兜底（✅ 已完成）

> 问题：WSL 最小安装无任何 CJK 字库（实测仅 DejaVu/Ubuntu），egui 中文渲染为方块——
> fonts.rs 的系统字体回退链全部落空。

- [x] **内置兜底字体**：`assets/fonts/NotoSansCJKsc-Regular.otf`（Noto Sans CJK SC，OFL 授权，15.7MB，
  `include_bytes!` 编译进二进制）——系统候选全部落空时自动启用，**任何环境开箱即显中文**
- [x] **加载结果可观测**：`fonts::install` 返回 `LoadedFont`（来源描述），`new()` 写入 runtime.log
  （`CJK 字体: 系统字体: …` / `CJK 字体: 内置兜底: …`）——无头环境凭日志即可验证
- [x] **`P2P_FONT_FORCE_EMBEDDED=1`**：跳过系统探测强制内置（测试开关）
- [x] **优先级验证（WSL 实测）**：强制内置模式 runtime.log 记录 `内置兜底: NotoSansCJKsc-Regular` ✅；
  用户装 fonts-noto-cjk 后系统字体优先路径已先行人工验证 ✅
- [x] 单测 +4（候选探测跳过不存在路径 / 命中存在路径 / 内置字体魔数与体积 / 空候选返回 None）
- [x] 文档同步：README、xtask/README 的 WSL 字体要求从"必装"降级为"可选增强（内置兜底已覆盖）"

## P2 — 原生界面（"微信式"）

### 前提重构（✅ P2.0 已完成——M2 单 exe 路线）

> 设计演化：原计划"抽 lib + LineReader trait + TextSink 逐点改造"；M2 单 exe 分发确认后简化——
> **不抽 lib**（单 crate root 天然互见，双 bin 问题消失）、输入用 **`LineSource` 枚举**（Stdin/Channel 双实现场景固定，避免 dyn trait）、
> 输出用 **crate 级宏遮蔽 + 线程局部 sink**（166 处输出点零改造自动路由，未装 sink 线程回落 std 逐字节一致）。

- [x] **单 exe 双模式分发**（M2 子步 0）：`main()` 四规则——`P2P_GUI_CHILD=1`→GUI / 管道→CLI / `--cli`→CLI / 交互无参→分离拉起 GUI 后释放终端；退役 `p2p_rust_app_gui` bin（单 crate root 后 lib 问题消失）
- [x] **LineSource 输入抽象**（子步 1）：`LineSource{Stdin, Channel}` + `InputMsg{Line, ChatText}`——Stdin 逐行产出 Line（CLI 语义不变）；Channel 透传 GUI 消息（ChatText 绕过命令解析）；`next_raw_line` 供登录/确认交互；签名替换 identity_service 9 处 + chat.rs 字段
- [x] **TextSink 输出事件化**（子步 2）：`source/sink.rs` 线程局部输出端 + main.rs crate 级宏遮蔽（println!/eprintln!/print! → sink::line/err/raw）——**聊天路径 166 处输出点零改造自动路由**；未装 sink 线程回落 std（CLI 逐字节一致）
- [x] **GUI 引擎线程化**（子步 3）：`ui/mod.rs` 引擎线程（`current_thread` runtime + `sink::install` + `run_engine(LineSource::Channel)`）——**退役 console.rs 子进程桥与 /sendStrings GUI 路径**（文本 ChatText 直进引擎，多行原样）；生命周期：引擎任务结束（/q）→ GUI 联动关闭；GUI 关 → 进程退出引擎随之结束
- [x] 交互语义按输入源判定：Stdin 终端=交互（rpassword/y 确认）；Stdin 管道与 GUI 通道=管道语义（自动放行，提示走滚动区）
- [x] e2e 全量回归（CLI 路径行为不变——管道路径零改动）+ 单测 61 + GUI 冒烟（单进程无子进程、引擎日志、登录流直出）
- 已知边界：GUI 模式无主菜单（引擎直入登录，`/q` 退出聊天即关闭应用）；node.rs L1 传输噪声行暂不进滚动区（P2.2 收口）

### 界面
- [x] **登录页**（✅ P2.1）：全窗口登录卡片（登录期引擎未启动、底部输入面板隐藏）——
  缓存身份列表（按钮 + 节点ID 摘要）/ 缓存解锁（masked 密码框，错误内联可重试）/
  新身份向导（资料表单 → 助记词展示可复制 → 抄写前 3 词确认 → 密码二次确认）/ 助记词恢复（12 词校验 → 资料 → 密码）；
  登录成功后带凭据启动引擎（`run_engine(Some(outcome))`）切聊天布局
  ——架构：视图+状态机在 `p2p_app/chat/gui/login.rs`，编排只调 p2p 领域 API；
  密码全程只在表单内流转（masked），不进命令框、不落 interact.log（CLI 命令框明文问题随之消除）
- [x] **左栏**：联系人（信任徽标/在线点/信任按钮）+ 已发现节点（未握手分段）+ 添加联系人表单 + 我的地址（点击复制）+ 群列表（✅ P2.2c/P2.3b/c）
- [x] **中区**：会话气泡（手工测量手绘，对侧左/我侧右，未信任黄）+ 系统行混排时间线；消息结构化（`ChatMessage` 事件经 sink 单通道保序，CLI 文本格式与 GUI 渲染分流）（✅ P2.2a/b/b2）
- [x] **底部**：输入框（ChatText 直发）+ 命令框（guard 拦穿透）+ `/list` `/q` 快捷按钮（登录期隐藏）（✅ P2.1/P2.2）
- [ ] **弹窗**：TOFU 指纹确认、`/backup` 助记词、未信任发送确认、文件接收均已由 **Ask 系统消息卡片**实现（✅ P2.6）；~~下载目录选择~~ ✅ 步 5 设置页（rfd pick_folder）
- [ ] **状态栏**：发现模式、下载目录、本机节点 ID（部分能力已被设置页承载）

### P2.5 应用层结构（chat.rs 2483 行 → p2p_app/chat/ 12 模块，✅ 纯搬家）

```
p2p_app/chat/
├── payloads.rs   载荷×5 + TAG×5        ├── commands.rs  build_tree 命令树（CLI 文本层）
├── group.rs      群域逻辑+持久化        ├── session.rs   run/run_engine/run_node 主循环
├── dial.rs       拨号地址解析+模板      ├── ctx.rs       ChatCtx/AppCtx/Conversation/AsyncOp
├── control.rs    handle_control        ├── handlers.rs  SignalCtx + 语义 handlers
├── sidebar.rs    侧栏快照/徽标/监听打印 ├── display.rs   显示路由（CLI/GUI 分流）
└── cli/gui/login_common（登录域）
```
- 框架 cmd_tree.rs 留 crate 根（全局组件）；build_tree 随域——对应固件惯例 CommandParse/ 与 userTasks_cmds.c 分离
- 三步纯搬家（叶子→中间层→大块），每步 e2e 全量门禁，CLI 行为逐字节不变

### P2.6 — Ask 确认协议与两段式（✅ 已完成）

交互确认统一两跳状态机：**phase1 登记 pending → 立即返回（禁 handler 内 await 答案——单任务 select 冻结根源）→ phase2 答案到达执行**。

- [x] Ask/Answer 基建：`sink::ask(AskRequest)` + `AskKind`×4（TofuConfirm/BackupPassword/UntrustedSend/FileReceive）系统消息卡片；GUI 卡片按钮答案经 `InputMsg::Line` 回程（✅ 步 1-3）
- [x] L2 两段化：`on_peer_hello_begin/complete_tofu`、`backup_begin/backup_complete`；`HelloOutcome{Done,PendingTofu}`/`BackupProgress`（✅ 步 4）
- [x] CLI 确认子窗口：`--confirm-tofu`（指纹卡片，退出码 0/1）/`--confirm-secret`（rpassword，结果文件）/`--confirm-file`（文件接收 y/n）三个子模式入口（✅ 步 4；**分发缺失缺陷修复**——spawn 侧早已存在但 main() 从未解析，此前 Interactive TOFU 实际恒自动信任+误拉 GUI）
- [x] FileReceive 两段化（✅ 步 4-2 后续）：`on_file_offer` phase1（提示/卡片/登记 `AppCtx.file_pending`）+ `complete_file_receive` phase2；**CLI 接收 offer 不再阻塞 chat**（等待 y/n 期间聊天收发畅通）；confirm 臂（file_id 匹配防迟到答案）+ input 待决路由双路驱动
- [x] 接收确认 60s 超时：并入定时臂 `deadline=min(offer 过期,pending 过期)`，未答自动 reject 清槽（防单槽被永久占位）；忙拒：pending 被占时新 offer 直接 reject
- 已知边界：非 Windows Interactive 退化主窗口 pending 路由（TOFU 退化自动拒绝）；`/backup`/TOFU 登记仍直接覆盖 pending_confirm（信号侧 offer 已忙拒，命令侧未拦截）；CLI Interactive 的 D3 未互信 y/n 保留内联 await（终端串行固有）；AppCtx.input 字段随内联 await 消失退役
- 答案面分流（✅ 实测修正）：`PendingConfirm.via_window` 区分答案面——CLI 确认子窗口作答（true，confirm 通道回传，**主窗口行照常流转**，挂起期间聊天双向畅通）vs 主窗口行作答（false：非 Windows 退化/Auto e2e，保留串行劫持）；焦点抢占为 CREATE_NEW_CONSOLE 固有行为，子窗口/主窗口文案缓解
- QuickEdit 禁用（✅ 实测修正）：CLI Interactive 启动即关控制台快速编辑——子窗口抢焦点后用户点击拖选会冻结 conhost 输入（无回显/读不到输入/消息不上屏，Enter 清选中恢复）；kernel32 extern 零依赖实现，仅 Interactive（e2e 零影响）
- spawn stdio 隔离（✅ 实测修正）：确认子窗口 spawn 必带 `Stdio::null()` 三件套 + 子入口 `attach_console_stdio` 自挂 CONIN$/CONOUT$——裸 inherit 时子进程继承主窗口控制台句柄，存活期间主窗口键盘输入被扣、子窗口退出才涌出（trace 实证 7.6s 空窗）；三层修复链（输入路由劫持→QuickEdit 冻结→spawn inherit）详见 pitfalls SKILL 5.5；子窗口类功能 Auto e2e 不覆盖，须 Interactive 手测
- AskAnswer 答案专道（✅ 实测修正）：GUI 卡片答案 `InputMsg::AskAnswer{text}` 类型层分流——原走 Line 与命令框同类型，引擎待决路由把命令行当答案，历史对策=挂起期间锁死整个输入区（文本框无辜连坐）；现 pending Ask 期间输入区**保持可用**（命令框/文本框/快捷按钮/发送文件按钮），Line 劫持收窄 `mode!=Ask`（stdin 场景保留：非 Windows 退化/Auto e2e 密码行，e2e 实证）；UntrustedSend 两段化补完（原文随待决暂存，y 后 send_confirmed 置位重发）——**GUI 侧内联 await 清零**；迟到答案丢弃+提示
- 设置页（✅ P2.6 步 5）：中央区标题行「设置」toggle 切换设置面板（数据源 `UiEvent::Settings` 快照，登录后+变更后重推）——下载目录（rfd pick_folder 更改，**立即生效**：settings 落账+file_state 运行时更新）/ 文件接收前确认开关（settings 新键 confirm_file_receive 默认开；关=GUI 收到文件自动接收不弹卡片；CLI 对称命令 /auto-receive <on|off>；仅影响 GUI/Ask）/ 发现模式下拉（走 /discover 命令行复用命令树，下次进入聊天生效）；/download-dir 同步立即生效；清缓存（联系人/日志）后续单批

### 应用层结构（P2.1 起生效，架构纪律）

```
source/
├── p2p/          协议核心（L1/L2）：无渲染、无交互流程、无 GUI 形状类型
│                 前端触碰核心只有两条合法通道——调用领域 API + LineSource/sink I/O
├── p2p_app/      应用层（L3+）：按应用细分（chat/file_transfer），应用内分 cli/gui
│                 └── 交互流程（登录菜单/表单、确认编排）永不进 p2p/
├── ui/           跨应用渲染件（GuiApp 壳、fonts、日志面板）
└── chat.rs       应用编排（主体后续专项迁入 p2p_app/chat/）
```

- P2.1 落地：CLI 文本登录流程自 identity_service.rs 迁入 `p2p_app/chat/cli/login.rs`（提示文案逐字节不变）；
  L2 只增领域 API：`IdentityService::login_pre(LoginOutcome)`（既有凭据建会话，冲突返回 `LoginError::IdInUse`）、
  `normalize_birthday/normalize_gender` 开放、`LineSource::prompt/prompt_secret`（带提示符读行/密码）

## P3 — 打磨
- [ ] 命令按钮化（`/trust`、`/group`；~~`/send` 原生文件选择器~~ ✅ P2.6：底部「发送文件」按钮 rfd + Control::SendFile + 传输进度卡片 ProgressBar/打开目录）
- [ ] 主题、窗口状态持久化（`eframe::App::save`）
- [ ] 可选：`/` 命令右键菜单 / 快捷键

---

## 风险与护栏

- P1 零核心改动，风险最低，当日可交付
- P2 重构面：`LineReader` 签名替换（约 15 处）+ `Ask` 通道（约 6 处交互点），42 单测 + 9+2 e2e 全程回归
- e2e 走 CLI（`StdinLines`）不受影响，GUI 走新实现，两条路并存

## 相关文档

- `docs/PROJECT_ARCHITECTURE.md` — 三层架构（L3→L2→L1），GUI 复用底层
- `docs/新手测试指南.md` — CLI 联机测试手册（GUI 完成前的测试途径）
- `.opencode/skills/p2p-libp2p-tokio/SKILL.md` — libp2p/tokio 用法约定
