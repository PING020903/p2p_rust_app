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

## P0 — 骨架（✅/⬜ 待办）

- [ ] **Cargo.toml**：加 `[[bin]] p2p_rust_app_gui`（path `source/main_gui.rs`）+ `eframe`（最新稳定，实现时锁定版本）
- [ ] **`source/main_gui.rs`**：入口 `eframe::run_native`
- [ ] **`source/ui/mod.rs`**：`GuiApp: eframe::App`，`update()` 每帧 drain 事件通道
- [ ] **`source/ui/fonts.rs`**：CJK 字体加载回退链（Windows `msyh.ttc`→`simhei.ttf`；Linux `Noto Sans CJK`；macOS `PingFang`），注入 `FontDefinitions` 并设为 fallback
- [ ] **异步集成**：tokio 后台线程 + 双通道 + `egui::Context` repaint
- [ ] 验收：能开窗口、中文渲染正常

## P1 — 终端式 GUI（快速全功能可用）

> 方案：**嵌入式控制台**——复用 e2e 既有模式（tests/common/mod.rs 的 `Node::spawn` = spawn CLI + 管道驱动，已 battle-tested）。

- [ ] **`source/ui/console.rs`**：spawn `p2p_rust_app.exe`（`current_exe()` 找同目录兄弟 bin），管道接 stdout/stderr → 滚动文本区，输入框 → stdin
- [ ] **输入框**：`egui::TextEdit`；`interactive = stdin().is_terminal()` 自动 false → 管道模式，**密码不回显**（GUI 密码输入用 `TextEdit.password(true)`）
- [ ] **滚动文本区**：最新输出自动滚底，保留历史滚动查看
- [ ] 验收：窗口内完成「登录 → /list → /chat → 互信 → 收发消息 → /send 文件」全流程，核心零改动

> 备选（若不用子进程）：进程内 + `LineReader` trait + 全局输出钩子——但 println 遍布 chat/identity_service/file_transfer，机械改动量大、还需剥 ANSI 色码，性价比低于子进程方案。

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
