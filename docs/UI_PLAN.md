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


## P2 — 原生界面（"微信式"）

### 前提重构
- [ ] 抽 **`source/lib.rs`**（`pub mod chat/...`），GUI bin 依赖 lib（为进程内调用铺路）
- [ ] 引入 **`LineReader` trait**（`async fn next_line`）：`StdinLines`（CLI/e2e 保留）+ `ChannelReader`（GUI）；签名跨 identity_service/chat/file_transfer 机械替换
- [ ] 输出改 **`UiOut` 事件 + `TextSink`**；交互确认（登录/TOFU/backup/未信任发送/文件接收）改为「Ask → 答」通道
- [ ] P1 子进程方案退役，同一份逻辑进进程内跑
- [ ] e2e 全量回归（42 单测 + 9+2 e2e，走 CLI `StdinLines` 不受影响）

### 界面
- [ ] **登录页**：缓存身份列表 / 新身份表单（资料 + 助记词确认）/ 助记词恢复 / 密码（password 模式）
- [ ] **左栏**：联系人（含信任徽标 `[互信]/[我信任/对方未确认]/[未信任]`）+ 群列表 + 发现节点
- [ ] **中区**：会话气泡（焦点/非焦点带名）、群消息（`[群名] [成员名]`）
- [ ] **底部**：输入框 + `/` 命令快捷入口
- [ ] **弹窗**：TOFU 指纹确认、`/backup` 助记词、未信任发送确认、文件接收、下载目录选择
- [ ] **状态栏**：发现模式、下载目录、本机节点 ID

## P3 — 打磨
- [ ] 命令按钮化（`/trust`、`/group`、`/send` 原生文件选择器）
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
