# 更新日志

本项目所有重要变更都记录在此文件中。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

### 新增

- **P2.3 子步 a+b：GUI 原生操作化（切会话/信任脱离命令文本）**：
  - **输入协议结构化**：新建 `source/lineio.rs`（输入 I/O 基础设施，与 sink/uievent 对称）——
    `LineSource`/`InputMsg`/`prompt*` 自 p2p/identity_service.rs 迁出（p2p 只是使用方）；
    `InputMsg` 增 `Control(Control)` 变体（p2p 零引用，仅 chat.rs 应用编排层消费；
    CLI/e2e 永不产出）
  - **Control 枚举**：FocusPeer{peer, name}（复刻 /chat：已连接仅聚焦/未连接建会话+拨号或待 mDNS）、
    FocusGroup（复刻 /group 聚焦+拨群成员）、Trust{peer, trusted}（复刻 /trust：trust()+
    清 send_confirmed+在线发 TrustConfirm/Revoke 信号）；`describe()` 供交互日志
  - **GUI 侧栏**：点击联系人/群 → 结构化动作（**peer_id 直传**，名字歧义/注入根除）；
    行尾信任按钮（信任/取消信任，指纹信息行照打时间线——核对弹窗留弹窗批）；
    interact.log 记动作语义（"点击: 切换会话: 名"），不再记 /cmd
  - **消重**：ChatCtx 构造（4 处）与 ops 消费（2 处）提取 make_chat_ctx/consume_ops 共用
  - next_raw_line 防御：提示符阶段收到 Control 不作答继续等待
- **P2.2 子步 e：引擎主动唤醒（纯事件驱动，零轮询）**：
  - sink 基础设施加通知回调：`install(tx, notify)`——GUI 传 `ctx.request_repaint` 闭包
    （egui 类型封在闭包内不穿透签名），四出口（line/err/raw/event）send 成功后即唤醒 UI
  - 消息/侧栏快照显示延迟 500ms→~0；突发输出合并为一帧（request_repaint 置标记不排队）
  - 引擎退出也推送：线程闭包持 ctx 副本，`done.store(true)` **先于** `request_repaint`
    （顺序防竞态：保证唤醒帧必能看到退出标记 → GUI 联动关闭）；
    runtime 构建失败路径同步补唤醒（此前靠轮询兜底）
  - 删除 logic() 的 500ms 空闲轮询——空闲时主线程阻塞在事件队列（零 CPU），
    用户输入/缩放由 OS 事件天然触发
- **P2.2 子步 b1+b2：气泡布局修正（手工测量 + 手绘）**：
  - 症状：我侧（RTL）气泡撑满锚定列且不贴右缘，对侧（LTR）正常——egui Frame 自动尺寸
    与 RTL 布局交互不对称，自动尺寸路径不可靠
  - 修复：render_bubble 重写为方向无关的确定性布局——`painter.layout/layout_no_wrap`
    先排版测量（正文按列宽−内边距换行，galley 缓存兜底，缩放窗口即时重排），
    气泡尺寸 = max(头宽, 文宽) + 内边距，`allocate_exact_size` 精确放置 + `rect_filled`
    画圆角 + galley 手绘；两侧同一代码路径，仅锚定方向不同
  - 反应式重绘模型澄清入文档：egui 事件驱动（无事不画），非全速刷新
- **P2.2 子步 c：左栏联系人 + 群列表**：
  - 侧栏快照事件 `UiEvent::Sidebar(SidebarState{contacts, groups})`——推送点：命令处理后 +
    每个传输事件后（联系人/信任/连接/焦点变化全覆盖）；CLI 无事件通道 no-op
  - L2 只读快照 API：`ContactBook::all()` + `IdentityService::contact_entries()`（按名排序）
  - GUI 左栏（`Panel::left`）：联系人 = 在线圆点 ●/○ + 名字（焦点加粗）+ 信任徽标
    [互信]绿/[我信任]黄/[未信任]灰 + 悬浮显示节点ID；群列表 = 名字 + 成员数（焦点加粗）；
    点击即发 `/chat <名>` / `/group <名>`（复用 CLI 命令语义，无新增引擎输入协议）
- **P2.2 子步 b：中区气泡时间线**：
  - GuiApp `lines: Vec<String>` → `timeline: Vec<TimelineItem>`——系统行与聊天气泡混排单列表保时序
  - 气泡渲染（egui Frame 圆角）：对侧左对齐/我侧右对齐；头部小字（名字、群前缀 `[群名] 名`、
    未信任 ⚠ 标记、到达时刻 HH:MM）；配色 我侧绿/对侧深灰蓝/未信任黄；正文自动换行（上限 72% 宽）
  - 聊天消息仍按 to_cli_line 落 interact.log（日志与界面解耦）
- **P2.2 子步 a：消息结构化通道**（GUI 气泡化的管线铺垫）：
  - `source/uievent.rs`：ChatMessage（from/outgoing/focused/group/untrusted）+ UiEvent + EngineOut
  - sink 单通道统一 `EngineOut{Line, Event}`——文本行与结构化事件 FIFO 保序（气泡与系统提示不打乱时序）
  - `p2p_app/chat/display.rs` 显示路由：1v1 收/发、群收/发四个漏斗统一入口——
    CLI 分支逐字节保持历史文本（群焦点 `[{谁}]`、群非焦点 `[{群名}] [{谁}]` 双前缀、
    1v1 焦点 `[对方]`、未信任 `[未信任]` 标记），GUI 分支发结构化事件
  - GUI 子步 a 兜底：Chat 事件按 to_cli_line 文本渲染（视觉不变，子步 b 起改气泡）
  - e2e 抓漏：群消息 CLI 分支初版丢失群名前缀——已恢复语义并补 to_cli_line 对应分支
- **P2.1 GUI 登录页 + 应用层结构（p2p_app/）**：
  - **应用层落地**：`source/p2p_app/`（按应用细分，应用内分 cli/gui）——聊天应用 `chat/{cli,gui}`、
    文件传输占位；CLI 文本登录流程自 identity_service.rs 迁入 `chat/cli/login.rs`（文案逐字节不变）；
    架构纪律入 UI_PLAN：p2p/ 无渲染无交互流程，交互编排永不进协议核心
  - **登录页**（`p2p_app/chat/gui/login.rs`）：全窗口登录卡片——缓存身份列表 / masked 密码解锁
    （错误内联重试）/ 新身份向导（资料 → 助记词复制展示 → 抄写前 3 词确认 → 密码二次确认）/
    助记词恢复；登录期引擎未启动（无输出噪声、关窗即退），成功后带凭据启动引擎
  - **p2p 领域 API**：`IdentityService::login_pre(LoginOutcome)`（既有凭据建会话 + 影子探测，
    冲突类型化 `LoginError::IdInUse`）、`normalize_birthday/gender` 开放、
    `LineSource::prompt/prompt_secret`（带提示符读行/密码，rpassword 语义保留）
  - **安全**：GUI 密码全程只在表单内流转（masked），不进命令框、不落 interact.log；
    engine_done 生命周期简化（登录期无引擎线程）
  - 单测 68（+登录编排：资料/密码校验、抄写确认、解锁错误与成功路径）+ e2e 全量回归
  - **登录纯逻辑共享内核**（`p2p_app/chat/login_common.rs`）：姓名/生日/密码校验、抄写确认
    （统一大小写不敏感，并修复空输入空真漏洞——恢复长度守卫）、缓存解锁、保存路径收敛为
    单一来源；CLI/GUI 各保留原生 UX（重试策略/提示文案/二次确认），规则语义不再漂移
- **P2.0 前提重构完成（GUI 进程内引擎）**：
  - **单 exe 双模式分发**（M2）：无参→GUI（分离进程拉起窗口+引擎线程）、`--cli`/管道→纯 CLI
  - **LineSource 输入抽象**：`Stdin`（CLI/e2e 逐行）+ `Channel`（GUI 消息）；`InputMsg{Line, ChatText}`——
    GUI 文本框纯聊天文本绕过命令解析直发焦点（多行原样，/sendStrings 协议 GUI 路径退役）
  - **TextSink 输出事件化**：crate 级宏遮蔽（println!/eprintln!/print! → 线程局部 sink）——
    聊天路径 166 处输出点零改造自动路由；未装 sink 线程回落 std（CLI 逐字节不变）
  - **引擎线程化**：GUI 内 `current_thread` runtime 跑 run_node（进程内聊天核心），
    退役 console.rs 子进程桥与 /sendStrings GUI 路径；生命周期：引擎任务结束→GUI 联动关闭
  - 交互语义按输入源判定（Stdin 终端=交互 / 管道与 GUI 通道=管道语义）；登录状态机化列入 P2.1
- **GUI 骨架（P0，`p2p_rust_app_gui`）**：egui/eframe 0.36（默认 wgpu 渲染器）+ 独立 bin；
  `App::ui/logic` 新 trait 集成后台 `tokio::sync::mpsc` 通道 + `request_repaint`；CJK 字体运行时
  回退链（Windows msyh/simhei、Linux Noto CJK、macOS PingFang）；`default-run` 保住 `cargo run` 走 CLI；
  滚动文本区 + 输入框占位。详见 `docs/UI_PLAN.md`
- **GUI 终端式控制台（P1）**：`source/ui/console.rs` spawn CLI 子进程，stdout/stderr 双管道 →
  滚动文本区（提示符即时上屏），输入框 → stdin（无本地回显，密码防泄漏）；`try_wait` 轮询退出、
  窗口关闭 `Drop` 杀子进程；核心零改动（管道模式复用 e2e 驱动模式）
- **GUI 诊断组件（P1.5）**：
  - `source/ui/timing.rs` 耗时组件：`Sample`/`TimingStats`/`Timer`/`ScopeTimer`（零依赖，可复用）
  - `source/ui/logging.rs` 运行日志组件：线程安全 `LogStore` + 文件落盘 + 零依赖 UTC 民用历时间戳
  - **双文件日志**：`~/.p2p_rust_app/gui_logs/<YYYYMMDD-HHMMSS>/` 下 `runtime.log`（软件运行日志）+
    `interact.log`（用户交互输入输出，带时间戳；密码暂原样记录），每次运行一个时间戳文件夹便于对比
  - **自动打点**：`pipeline.drain` / `roundtrip.input->resp` / `frame.logic` / `frame.ui` 延迟统计；
    GUI 日志面板（运行/交互切换 + 级别过滤着色 + 跟随/清空）+ 状态行实时延迟
- **GUI 治理与生命周期修复**：
  - **日志本地时间**：改用 `chrono` 取本地时区（替换零依赖 UTC 民用历），目录名与行内时间戳均为真实本地时间
  - **输入检查&修改层** `source/ui/input_guard.rs`：`Rule` 接口 + `InputGuard` 规则链（可扩展）；
    GUI 默认两条规则——`BlockTerminalEscape` 拦截 `cmd/`/`ps/`/`sh/` 穿透命令（CLI 保留）、
    `CollapseNewlines` 多行折叠（修多行消息被逐行拆分的 bug）；拦截时滚动区提示 + 日志 Warn
  - **GUI 启动器分离**：`p2p_rust_app_gui` 无 `P2P_GUI_CHILD` 时以 `DETACHED_PROCESS` 重新
    spawn 自己后立即退出，原命令行即释放；带标记才进 GUI；spawn 失败回退前台运行；
    `windows_subsystem="windows"` 无条件启用（诊断全走 gui_logs，双击/启动器均无黑窗）
  - **CLI 子进程禁窗**：GUI spawn CLI 时加 `CREATE_NO_WINDOW`——分离后的 GUI 无控制台，
    若不禁窗 Windows 会给 console 子进程新建常驻黑窗（stdio 仍为管道，行为不变）
  - **生命周期联动**：CLI 子进程退出 → GUI 自动 `ViewportCommand::Close` 关闭（双向绑定：
    GUI 关→杀 CLI 已有；CLI 退→GUI 关新增）；退出日志级别按 code（0=Info / 非零=Warn）；
    **空闲低频轮询**（`request_repaint_after(500ms)`）保证子进程退出无新输出时也能被及时检测
- **多行发送（`/sendStrings <N>`，文本/命令分离）**：
  - **协议**：`/sendStrings <N>` + 恰好 N 行原文——**内容零解析、零转义**（空行、以 `/` 开头、含引号的行原样保留）；CLI 按行数精确收集后拼接发送，未收满（EOF）报错丢弃
  - **CLI**：`send_focused_text` 共享发送函数（群 gossipsub / 1v1 信任门控 / cbor / 回显，普通消息与多行共用）；主循环 stdin 收集态优先（不 trim、空行保留）；`/sendStrings` 用法/行数校验；帮助同步
  - **GUI 文本框 = 纯文本语义**：内容包成 `/sendStrings <N>` 发送（`send_multiline`），`/list` 等以 `/` 开头的内容作为聊天文本发出；新增**独立命令输入行**（guard 拦 `cmd/`/`ps/`/`sh/` 穿透 + `/sendStrings` 多行入口，其余透传）
  - `input_guard` 按输入源分派（命令框两条规则 / 文本框预留空规则链）
  - 测试：单测 +3（行数解析/多行收集 verbatim/单行）、e2e +1 场景（空行与 `/` 开头行原样、引号原样）
- **GUI 状态感知 + 快捷命令（P1.8）**：
  - **子进程状态机** `ChildState{Menu,Login,Chat}`：按输出特征行精确推进（主菜单/角色登录/发现模式），
    聊天内容带前缀不误触发；单测 +2
  - **文本框按状态启停**：仅聊天态启用；登录/主菜单态禁用 + placeholder 提示用命令框
    （根治登录阶段文本框协议错位误用）；首帧焦点按状态引导
  - **命令框动态提示**（菜单选择/登录输入/命令）+ **快捷命令按钮** `/list`、`/q`（聊天态启用）
  - **状态行**：`状态: 聊天中/登录中/主菜单` 着色显示 + 延迟统计
- **双平台构建就绪（P1.9）**：
  - **xtask 归档双 bin**：CLI + GUI 都进 `target/{profile}/bin/<os>-<arch>/`（Windows/WSL 各自构建，产物按系统目录汇集）
  - **Linux 分离启动对齐**：`process_group(0)` + stdio null（Linux 下 GUI 也不占终端）；`windows_subsystem` 加 `cfg(windows)` 门控
  - **WSLg 实测通过**：Linux 全量构建、分离启动、子进程拉起、日志落盘全部验证；运行库备注
    （WSL 发行版 cargo 需换 rustup stable、补 `libxkbcommon-x11-0`）
  - **CLI 告警清零**：未用导入删除、`cQ` 更名、cmd_tree C 版对齐接口显式豁免、
    `effective_trusted` 复用 `their_trust`（消除死包装）
- **主菜单「5. 清除会话日志」**：清理 `gui_logs/<时间戳>/` 运行日志碎片（每次 GUI 运行产生一个
  目录）——统计目录数/体积 → `y/n` 确认 → **保留最近 1 次**删除其余（GUI 运行中触发自动豁免当前
  会话目录，避开文件锁）；单个目录删除失败跳过不中断；只清日志，身份/联系人/群/设置一律不动；
  核心函数按目录名字典序判新旧（不依赖 mtime）+ tempdir 单测 ×4
- **内置 CJK 字体兜底（P1.10）**：`assets/fonts/NotoSansCJKsc-Regular.otf`（Noto Sans CJK SC，
  OFL 授权，15.7MB，`include_bytes!` 编译进二进制）——系统候选全部落空时自动启用，
  **任何环境开箱即显中文**（WSL 最小安装裸机实测通过）；`fonts::install` 返回加载来源写入
  runtime.log（无头环境凭日志验证）；`P2P_FONT_FORCE_EMBEDDED=1` 强制内置测试开关；单测 +4
- **xtask 构建工具**：`cargo xtask build [--release]` —— 构建后自动把可执行文件按 `<os>-<arch>`
  归档到 `target/{debug|release}/bin/<os>-<arch>/`（`std::env::consts` 自动检测，Windows 为
  `windows-x86_64`，Linux 为 `linux-x86_64`；跨平台同一套命令，见 `.cargo/config.toml` 的 alias）
- `docs/新手测试指南.md` 增补 WSL 章节：mirrored 网络配置 + 「同机拨号地址速查表」
  （同机 WSL↔Windows 必须用 `127.0.0.1`，局域网 IP 会被 WSL 本地接管导致 Connection refused）
- README 全面同步至 0.21 状态

### 修复

- `seam.rs` `SignalHandler` 类型别名去掉 `: SignalCtx` bound，关联类型完全限定
  （`<C as SignalCtx>::Ctx<'ctx>`）——消除 `type_alias_bounds` 告警
- **"对方已正常退出，不进行重连"重复打印**：同一 `ConnectionClosed` 事件被 L1（node.rs）与
  L3（chat.rs Disconnected 分支）各打印一次——删除 L1 侧打印（分层职责：L1 只发事件，
  下线提示归 L3）；测试断言均只依赖 Bye 到达时的短版提示，不受影响

## [0.21.0] - 2026-08-29

### 变更（严格 L3→L2→L1 分层）

- **新增 seam 传输适配层** `source/p2p/seam.rs`：L3 只见 `seam::Cmd`/`seam::Event`
  （tag+payload），不接触 L1 的 `Frame`/`control`；适配任务做 Cmd→帧组装、
  P2pEvent→Event 拆帧（滤 control 心跳）双向翻译
- **L1 类型 `pub(crate)` 内部化**：`Frame/P2pCommand/P2pEvent/P2pNode` 编译期强制隔离，
  L3 不可触碰
- **hello/bye/trust 统一为 L2 内化 `TextTag`**（门禁收口）：`is_l2_signal` 白名单，
  L3 不直接处理存在信号
- **未互信信号钩子**：`SignalRegistry` 下沉 seam 并 GAT 泛化上下文；按 tag 注册
  `register_untrusted`，未注册 tag 默认空函数 = 丢弃（未互信业务信号默认丢弃）
- **终端逃逸新增 `sh/` 前缀**（`sh -c`，POSIX/Linux），`/help` 与欢迎行同步
- 新增 `docs/PROJECT_ARCHITECTURE.md`（分层 + 数据流 + 信号分发）

### 测试

- 新增 e2e：未互信钩子边界（`P2P_E2E_UNTRUSTED_HOOK` 注入，A 注册显示 `[未信任]` /
  B 未注册丢弃，互信恢复双向正常）；`spawn_with_env` 辅助

## [0.20.0] - 2026-08-29

### 变更

- **对称信任**（L2 互信门控）：`effective_trusted = 我信任 且 对方信任`，任一方
  `/trust !` 取消 → 整条链路不互信，业务信号经未互信钩子（默认丢弃）；
  `/list` 信任徽标 `[互信] / [我信任/对方未确认] / [未信任]`
- **终端逃逸**：`cmd/<命令>`（cmd）、`ps/<命令>`（PowerShell）绕过应用直控当前终端
- **测试拆分**：逻辑测试默认运行；稳定性测试（上下线循环/kill 掉线/阻塞心跳）移至
  `tests/p2p_chat_stability.rs` 标 `--ignored` 显式声明
- 信号格式规范入库

### 测试

- 新增 e2e：对称信任（互信→单方取消双向丢弃→恢复）、`standalone_ipv6_connect`
  （同机 IPv6 自连：::1 回环 + 全局地址双路径）

## [0.19.0] - 2026-08-29

### 新增

- **文件传输**（`/send <角色|节点ID> <路径>`，须互信）：事件驱动分块（1 MiB）推送 +
  逐块 CRC32 校验 + 接收方落盘校验；停等 ack 天然背压
- **可配置下载目录**：默认用户 Downloads，`/download-dir <路径>` 修改并持久化
  （settings 文件）；`P2P_DOWNLOAD_DIR` 环境变量优先
- **IPv6 直连地址 UX**：启动打印 `全局IPv6直连地址`（可分享给跨城市伙伴 `/dial` 即连，
  需路由器放行端口）；`/listen` 随时重打（oneshot 查询监听地址）；本机双全局地址合并为
  一条标题 + 多条地址
- `docs/CROSS_LAN_CONNECTIVITY.md`（IPv6 直连优先 + 国内可达 relay 兜底方案）、
  `docs/新手测试指南.md`（小白联机测试手册）

## [0.18.0] - 2026-08-22

### 变更（帧分发内核化：L3 不再 match frame.text）

- **`Frame.text` 改为 `Option<String>` 统一标签**：删除 `NodeMsg` 枚举；hello 帧
  `text="hello"` + `binary=cbor(名字)`，bye 帧 `text="bye"`；chat 业务 `text="chat.*"`。
  L1 纯透传不解释；协议 id `/frame/1.0.0`（同机升级）
- **`SignalRegistry` 升级为 async 分发**：`register(tag, async_handler)` +
  `dispatch(tag, from, payload)`（await handler）；事件分支**无 match**，收到帧查表分发，
  未注册 tag 报"未处理语义"
- **L2 映射存在语义 + L3 钩子分析"谁上线"**：`"hello"`/`"bye"` 分发条目调 L2
  `handle_peer_hello/handle_peer_bye`（TOFU/联系人簿，默认行为），L3 通过钩子解析负载
  （名字）判断谁上线/下线并反应（会话名/打印/MarkBye）；L3 不再直接处理原始帧
- **handler 为 async**：可直接 await（TOFU 读输入 / 发命令），AppCtx 扩展 `stdin`/
  `interactive`/`cmd_tx`；事件侧不再用 ops 队列（命令侧 ChatCtx 的 ops 保留）

### 测试

- 37 单测 + 12 e2e 全绿——Hello/Bye 钩子路径与 chat 业务经 async 分发后行为不变

## [0.17.0] - 2026-08-22

### 变更（语义注册表：L1 通用语义通道落地）

- **`NodeMsg` 增加 `Custom(String)` 变体**：text 通道成为"可注册协议语义"通道——
  基础语义 `Hello`/`Bye` 由 L2 存在层处理；L3 应用注册自定义语义标签（tag）+ handler，
  按 tag 分发，`binary` 承载该标签的负载
- **新增 `SignalRegistry`（L3 应用层）**：`register(tag, handler)` + `dispatch(tag, from, payload)`；
  事件分支收到 `text=Custom(tag)` 时查表分发，handler 同步逻辑 + `ops` 队列排异步动作（同 ring buffer 解耦）
- **chat 业务迁入注册表**：`AppPayload` 枚举拆为 5 个注册 tag + 各自负载结构——
  `chat.text` / `chat.group_invite` / `chat.group_leave` / `chat.group_member_list` /
  `chat.group_owner_transfer`；发送侧用 `text=Custom(tag)`，接收侧注册 handler
- **协议版本**：`/chat/7.0.0` → `/chat/8.0.0`（NodeMsg 变体变化，同机升级）
- 文件传输/固件升级等新应用 = 注册自己的 tag + handler，不动核心（为嵌入式固件升级打基础）

### 测试

- 全量 36 单测 + 12 e2e 场景全绿——chat 迁移到注册表后行为不变
  （12 场景覆盖 5 个注册 handler：文本/邀请/退群/名单/转移）

## [0.16.0] - 2026-08-22

### 变更（1v1 信任管理 + 群主可见 + 转移自愈）

- **修复取消信任**：`/trust !名` 之前是空操作（`ensure_contact` 对已存在条目只 OR 置真）。
  新增 `ContactBook::set_verified` 显式置位，`/trust !名` 真正取消——`/list` 徽标变未信任、
  群加人被"尚未验证"门控拒绝
- **联系人名解析**：`ChatCtx::resolve` 三级（会话名 → 联系人名 `contact_by_name` → 节点ID），
  `/trust`、`/chat`、`/group add` 均能按已记录联系人名解析（重启后无会话也能用）
- **D2 会话信任徽标**：`/chat` 聚焦与自动聚焦提示显示 `[已信任]`/`[未信任]`
- **D3 未信任首次发消息确认**：交互终端给未信任联系人首次发消息弹 `(y/n)` 确认
  （`Conversation.send_confirmed` 记录）；管道/e2e 自动放行
- **D4 指纹复核**：`/trust <名>` 信任前展示 `节点ID` + `指纹`，供人工比对（允许重名）
- **群主可见**：`/list` 与 `/group list` 群聊行显示 `群主 {昵称} ({peerID})`
- **群主转移自愈**：`GroupOwnerTransfer` 接收门控放宽——只要 `from` 是群成员、
  `new_creator` 在名单内、版本更高即整体替换（不再要求 == 当前 creator）；
  漏收中间转移的节点收到任一后续转移即自愈到最新群主，不再永久错位导致"无法退群"
- **邀请重发**：`/group add` 目标已在名单中时仍重发 `GroupInvite`（不 bump 版本），
  对方 cache 被意外清理时重新入群+订阅

### 测试

- 新增 e2e 场景 12：取消信任→徽标+加人门控、重新信任显示指纹、重启后按联系人名 /trust /chat
- 单测 36 个（新增 `set_verified`/`find_by_name`；`contact_by_name` 并入既有测试）

## [0.15.0] - 2026-08-22

### 变更（群成员一致性加固：单写者模型收口）

- **群主不在线禁止退群**（单写者一致性优先）：普通成员 `/group leave` 前校验群主是否
  在线，不在线则拒绝并提示——把名单维护收敛为"仅群主写"，从根源防止名单发散/幽灵成员
- **群主退群一步顺位转移**：群主 `/group leave` 时自动把群主职位转移给名单中**下一位**
  成员（members 数组群主之后第一个；群主在末尾则回卷取第一个非群主；仅自己则解散），
  携带新名单 1v1 扇出 `GroupOwnerTransfer`；群不再因群主离开而冻结，新群主立即可加人
- **幽灵/重复成员防御**：`load_groups` 加载时、接收 `GroupInvite`/`GroupMemberList`/
  `GroupOwnerTransfer` 时、群主处理退群后均做保序去重（`dedup_members`）
- `AppPayload` 新增 `GroupOwnerTransfer` 变体（cbor 末尾，向后兼容）；协议仍为 `/chat/7.0.0`
  （CRDT 版本才需 bump `/chat/8.0.0`）

### 测试

- 新增 e2e 场景 11（四节点）：群主离线退群被拒 + 群主退群顺位转移给下一位 + 新群主加人成功
- 单测 36 个（新增 `dedup_members`/`next_creator`）
- 凭据支持 user1..user4（users.txt/template 补 4 号）；users.txt 解析防御 UTF-8 BOM

### 路线图

- 新增 `docs/GROUP_CRDT_ROADMAP.md`：群成员名单 OR-Set CRDT 演进方向（去单点权威、
  退群去中心化、add-wins 防幽灵），未来版本据此推进

## [0.14.0] - 2026-08-22

### 变更（三层架构落地）

- **L1 传输层抽象**：新增 `source/p2p/node.rs`，`P2pNode` 持有 swarm 与全部连接/地址簿/
  重连/发现/心跳状态，在**独立 tokio 任务**中运行（`P2pNode::run`），经 `P2pCommand`
  （Dial/DialPeer/Send/MarkBye/Subscribe/Unsubscribe/Publish/Shutdown）与 `P2pEvent`
  （PeerConnected/PeerDisconnected/PeerDiscovered/Message/Gossip/SendFailure）与应用层通信
- **根治网络冻结**：应用层卡在 TOFU 指纹确认、`/backup` 密码等交互 await 时，传输任务
  仍独立维持心跳与收发——事件通道用无界 mpsc，传输任务永不因应用阻塞
- **心跳归 L1**：对全部已连接非 bye 的 peer 保活（原只护聚焦会话）；超时判离线并断开
- **bye 策略命令化**：收到 Bye 后应用发 `MarkBye(peer)` → L1 停止心跳、断开后不再自动重连
- **发现决策归 L3**：L1 只登记地址并上报 `PeerDiscovered`；待接呼叫/常驻群成员是否拨号
  由应用决策，经 `DialPeer` 命令执行
- **L2 身份基础服务**：新增 `source/p2p/identity_service.rs`，`IdentityService` 收拢
  登录（含影子探测）+ 联系人簿（TOFU）+ 信任判定 + Hello/Bye 存在处理，供 L3 与未来
  多协议复用；聊天协议无关
- gossip 经 L1 通用 `Subscribe/Publish/Gossip` 透传，topic 为不透明字符串，L1 不解释
- **命令逻辑收进指令树**：去掉 `ChatAction` 枚举 + run_node 巨型 match；每个命令的完整逻辑
  注册为 `CmdTree` 同步 handler，需要 `.await` 的动作（发命令/读密码）经
  `ChatCtx.ops`（`VecDeque`，同步生产者 → 异步消费者）排队，主循环统一消费

### 测试

- 新增 e2e 场景 10：应用卡在 `/backup` 密码交互（模拟 TOFU 阻塞）17 秒 > 心跳超时 15s，
  传输任务心跳仍存活，解锁后连接照常收发——三层架构核心验收点
- 单测 32 个（新增 `IdentityService` 信任判定测试）；e2e 10 场景串行

## [0.13.1] - 2026-08-22

### 变更

- `group` 子命令装入指令树：`/group new/add/resident/leave/list/<群名>` 注册为嵌套路由节点
  （指令树最深命中分词），`ChatAction::Group(String)` 的手写 `split_whitespace` 解析改为
  类型化变体（`GroupNew/GroupAdd/GroupResident/GroupLeave/GroupList/GroupFocus`）
  —— 纯重构，无行为变更

## [0.13.0] - 2026-08-22

### 新增

- 群"常驻接收"配置（防通讯风暴）：`/group resident <群名> on|off`
  - **常驻群**：成员上线自动拨号维持 gossipsub mesh，始终实时接收
  - **普通群**（默认）：不自动拨号，`/group <名>` 聚焦时按需连接，聚焦时才收发
  - per-node 本地偏好（`Group.resident`，默认 false），不随名单传播
- 聚焦群聊即连成员：`/group <名>` 时拨号群成员（常驻补连、普通按需）
- `/group list` 与 `/list` 群聊区段显示 `[常驻]` 标记
- 分离"连接"与"进入会话"：群成员上线/聚焦即可连，不再被迫先进 1v1 会话

### 修复

- 连接建立自动聚焦抢群焦点：仅当**无任何焦点**（1v1 与群皆空）时才自动聚焦首个连接

## [0.12.1] - 2026-08-22

### 修复

- `/list` 已验证联系人显示"未知"：名字解析改用 `peer_name`（会话名 → 联系人名 → 未知），
  已信任但不在当前 1v1 会话的联系人正确显示名字
- `/list` 群聊区段缺"名单版本"：补上与 `/group list` 一致的 `名单版本 N`

## [0.12.0] - 2026-08-20

### 变更

- **消息帧重构：`ChatPayload` → `Frame{control/text/binary}` 三通道**
  （协议 `/chat/6.0.0` → `/chat/7.0.0`）
  - `control`：传输层控制指令（心跳 `Heartbeat`）
  - `text`：节点间短消息（`NodeMsg::Hello` 上线 / `NodeMsg::Bye` 下线），**非用户内容**
  - `binary`：用户内容负载（`AppPayload`：`Text` 聊天文本 + 群管理
    `GroupInvite/GroupLeave/GroupMemberList`），应用层自描述（cbor 序列化）
  - 删除原 `Binary{name,data}` 占位（从未使用）
- 接收端改为**通道路由**：`control` → `text` → `binary` 依次分发
  （硬编码 match；handler 注册表化留作后续迭代）
- 群文本仍走 gossipsub（`GroupPayload`），不进 1v1 帧
- 新增 `serde_cbor` 依赖；e2e 场景 2 待接呼叫等待放宽至 40s（抗 mDNS 时序抖动）

## [0.11.0] - 2026-08-20

### 变更

- **群成员一致性模型：群主为中心（单一权威 + 版本化 + 1v1 扇出）**
  - `Group` 增加 `creator`（群主）与 `version`（每次成员变更 +1）
  - 成员表唯一权威是群主本地；各成员本地表为群主发布的缓存
  - **仅群主可邀请**：`/group add` 校验 `creator==self`，非群主报"仅群主可邀请新成员"
  - **群主不能踢人**：不提供移除命令，名单只会因成员主动退群而缩小
  - **加人**：群主 `version++` → 邀请携带当前版本+全量名单（入群即一致）→
    向其余成员 1v1 扇出 `GroupMemberList`
  - **退群**：成员 `/group leave` → 1v1 通知群主 `GroupLeave` → 本地删群+退订；
    群主校验成员身份 → 移除、`version++` → 向剩余成员 1v1 扇出
  - **名单更新**：`version > 本地` 才整体替换（防乱序/重复），并提示"成员名单已更新（版本 N）"
  - 移除原 gossipsub `Members` 载荷；协议 `/chat/5.0.0` → `/chat/6.0.0`
- 群主计入成员表（此前建群 `members=[]`，群主自己不在名单里）
- e2e 场景 9 强化：断言 B 收到加人后的名单更新（3 人/版本 2）、C 退群后 A 处理
  并扇出更新（2 人/版本 3）、C 本地群已删

## [0.10.0] - 2026-08-20

### 新增

- **群聊（gossipsub，微信式群）**：
  - 群消息经 libp2p gossipsub 分发，`MessageAuthenticity::Signed` 签名保证来源真实可验
  - 本地群注册表 `groups_<节点ID>.json`（id/name/members）；**成员必须是已验证联系人**
  - `/group new <群名>` 建群（随机群 ID）并订阅 topic；`/group list` 列群；
    `/group <群名>` 聚焦（此后输入直接发群里）
  - `/group add <群名> <角色|节点ID>` 加人：经 1v1 发送**入群邀请**，接收方自动建群记录并订阅；
    加人后向全群发布**最新成员名单**（本地注册表同步）
  - 群消息显示：焦点群 `[成员名]`，非焦点群 `[群名] [成员名]`；群消息带**发送者自报名**
    （签名保证来源真实，名字为展示元数据）
- 协议升级 `/chat/4.0.0` → `/chat/5.0.0`（新增 `GroupInvite` 载荷）
- e2e 新增场景 9：三节点群聊（建群 / 邀请入群 / 群消息扇出 / 非焦点带群名）

### 变更

- `NodeBehaviour` 增加 `gossipsub` 行为（Signed 签名身份）；`ChatPayload` 增加 `GroupInvite`

## [0.9.0] - 2026-08-20

### 新增

- **1v1 多会话**：同时与多个联系人保持连接，`/chat <角色>` 切换焦点
  （已连接会话只切焦点不重拨；未连接则建会话并拨号/待接）
- **焦点指示**：`/chat` 切换横幅（`已切换到会话: 名（节点ID）`）、发送回显
  `[我 -> 名]`、`/list` 状态列 `[当前会话]`、焦点断开提示
- **来信消歧**：焦点会话显示 `[对方]`，非焦点会话显示 `[对方名]`（多会话分清来源）
- **重连队列**：意外断线 / 拨号失败 / 待接呼叫统一入队，多个会话断线逐个自动重连
- `/q` 退出时向**所有**已连接且未退出的会话逐个发送 Bye 通知
- 心跳按会话独立记录 last_rx，切换焦点即时发现该会话是否已超时离线
- e2e 新增场景 8：三节点 1v1 多会话（并发连接、焦点切换、非焦点来信带名、
  /list 双会话 + 焦点标记）；新增 `MNEMONIC_USER3` 与 `users.txt` user3 凭据
  （三个测试身份名字互不相同）

### 变更

- 会话模型重构：`active / names / last_rx / bye_peers / greeted / pending_chat`
  并入 `HashMap<PeerId, Conversation> + focused`
- `/list` 状态列文案 "当前聊天" → "当前会话"

## [0.8.0] - 2026-08-20

### 变更

- **抽离通用 P2P 通讯层** `source/p2p/`，聊天成为消费方（行为零变更，e2e 全绿守护）：
  - `identity.rs`：助记词↔Ed25519、keystore 加解密、影子探测（自 chat.rs 迁出，含单测）
  - `contacts.rs`：TOFU 联系人簿 + 指纹（迁出，含单测）
  - `discovery.rs`：发现模式 advertise/stealth/off（迁出，含单测）
  - `mdns_stealth.rs`：隐身 mDNS 监听器（自 `source/mdns_stealth.rs` 迁入）
- `chat.rs` 1904 → ~1300 行：仅保留聊天协议、cmd_tree、登录 UI、连接/事件循环
- 新增 `docs/ROADMAP.md`：规划传输 API（P2pNode/P2pEvent）、通用 Frame 信封、
  文件传输、多点通讯（多会话/群组）、公网 M6
- `.gitignore` 放行 `docs/`

## [0.7.0] - 2026-08-20

### 新增

- mDNS 发现模式（隐私最小化），`/discover advertise|stealth|off` 切换并持久化
  （per-identity `settings_<节点ID>.json`），下次进入聊天生效；登录时打印当前模式；
  测试可用 `P2P_DISCOVERY` 环境变量覆盖
  - advertise（默认）：广播自身 + 发现他人（原行为）
  - stealth 隐身：**只收不发**——自实现最小 mDNS 监听器（UDP 组播 224.0.0.251:5353，
    解析 libp2p-mdns 组播响应中的 `dnsaddr=` TXT 记录），能发现他人，
    但不对局域网广播本机在线状态/身份/地址
  - off 关闭：完全禁用 mDNS，仅 `/dial` 手动直连
- 单元测试新增 3 项：mDNS 报文 `dnsaddr=` TXT 解析、垃圾报文拒绝、发现模式解析
- e2e 新增场景 7：隐身发现——隐身节点经监听发现广播节点，而广播节点看不到隐身节点，
  手动 `/dial` 仍可直连

### 变更

- `NodeBehaviour` 的 mDNS 行为改用 libp2p `Toggle` 组合子按模式启停
- mDNS 发现处理抽为 `on_peer_discovered`，广播发现与隐身监听共用
- e2e 测试节点支持 `P2P_DISCOVERY` 启动环境（`Node::spawn_with`）

## [0.6.0] - 2026-08-20

### 新增

- SSH 式 TOFU（Trust On First Use）指纹核对：
  - 首次接触的节点触发身份指纹核对——展示 SSH 风格短指纹
    （PeerId 字节 SHA-256 前 16 字节，冒号分组十六进制）与完整节点 ID，
    人工确认后记录为联系人；同一节点再次连接自动识别，不重复询问
  - 管道/脚本环境（不可交互）自动采用 SSH accept-new 语义：首次使用即记录并信任
  - `/trust <角色名|节点ID>` 手动标记信任，`!` 前缀取消
- 本地联系人簿 `contacts_<我的节点ID>.json`（明文：节点ID/角色名/指纹/信任状态/首见与最近见时间；
  peer_id 与名字本就是公开元数据）；每身份独立文件，多身份互不串扰
- `/list` 增加信任状态徽标（已信任 / 未信任）
- 单元测试新增 2 项：指纹稳定且不同节点不同、联系人簿跨加载持久化

### 变更

- e2e 各场景在非交互模式下自动建立 TOFU 联系人，回归断言不变

## [0.5.0] - 2026-08-20

### 新增

- 身份模型升级（随机密钥 + BIP39 助记词）：
  - 身份 = 随机 128 bit 种子 → 12 词 BIP39 助记词 → Ed25519 确定性密钥；
    姓名/生日/性别降级为绑定在密钥上的**资料元数据**，不再参与身份派生
  - 新身份登录：生成助记词展示一次，须抄写并回输前 3 词确认后才继续
  - 助记词恢复（`r` 路径）：跨设备迁移 / 备份恢复，输入助记词即可还原身份
  - `/backup`：随时重新查看本身份助记词（需输入密码解锁）
- 本地 keystore：助记词用 `Argon2id(密码, 随机盐)` 派生密钥 +
  ChaCha20-Poly1305 认证加密落盘；明文头只含公开资料与 KDF/密文参数；
  密码错误由 AEAD 校验检出（密文被篡改同样失败）
- keystore 加密参数逐文件记录、可独立升级——旧"Argon2id 参数是身份一部分、
  调整即全员换 ID"的限制彻底解除
- 单元测试新增 6 项：助记词确定性、非法助记词拒绝、生成的助记词可逆、
  keystore 加解密回环、错密码拒绝、密码长度规则

### 变更

- 登录菜单重构：`[角色登录]` → 缓存身份列表 / `0` 新身份 / `r` 助记词恢复；
  新身份与恢复均自动加密保存 keystore，之后登录只输密码即可解锁
- 密码规则收紧为 8~128 字节（Argon2id 最低输入长度要求）
- e2e 全面适配新登录流程：以 BIP39 官方测试向量助记词作确定性测试身份、
  每节点独立缓存目录，新增"缓存解锁错密码校验"回归断言

### 移除

- 旧的"登录信息派生身份"（`derive_identity`：SHA-256 盐 + Argon2id 密码种子）。
  该模型下知道姓名/生日/性别+弱密码即可离线暴力破解冒充、无法换密/吊销、
  且 KDF 参数终身锁死，不适合产品方向

## [0.4.0] - 2026-08-20

### 新增

- 身份缓存：登录成功后可选在本机缓存身份（姓名/生日/性别/派生 PeerId，不存密码），
  下次进入聊天直接选号、只输密码即可解锁，免去重复输入基本信息；
  缓存目录默认 `~/.p2p_rust_app/`，可用 `P2P_ID_CACHE_DIR` 环境变量覆盖
- 影子探测防同 ID 双在线：登录后借一次性随机 mDNS 身份探测局域网内是否已存在
  同 ID 节点（libp2p mDNS 会过滤同本机身份节点，故需绕道），存在则拒绝登录；
  探测窗口可用 `P2P_ID_PROBE_SECS` 覆盖（默认 5 秒）
- e2e 新增 2 个场景：身份缓存回环（缓存登录 + 错密码校验）、同 ID 冲突拒绝

### 变更

- 登录流程重构为两级菜单（缓存身份选择 / 新身份登录），命令回退逻辑归一
- e2e 测试节点同时捕获 stdout 与 stderr，错误提示（eprintln）可被断言；
  每场景使用独立缓存临时目录，保证登录菜单行为确定

## [0.3.1] - 2026-08-19

### 修复

- 地址簿膨胀：Bye 退出的 peer 其已知地址立即清除；意外断连的 peer 在重连穷尽失败后
  清除地址。`/list` 的地址数不再随上下线循环累积（此前每循环 +3~4 条陈旧地址）
- 对方刚重新上线时立即 `/chat` 失败：新增**待接呼叫**——`/chat` 登记呼叫意图，
  mDNS 一旦发现目标的新地址即自动拨号，无需人工等待或重试

### 变更

- `/chat` 目标暂无地址时的提示改为"等待 mDNS 发现，发现后自动连接"
- e2e 场景2 改为 B 重进后 A **立即**呼叫（直接验证待接呼叫），并新增
  `/list` 地址数 ≤4 的回归断言

## [0.3.0] - 2026-08-19

### 新增

- 角色登录：进入聊天模组须输入 姓名 / 生日(YYYY-MM-DD) / 性别(M/F/O) / 密码
- 确定性角色身份：`salt = SHA-256(姓名|生日|性别)`，`seed = Argon2id(密码, salt)`，
  Ed25519 密钥对 → 稳定 PeerId；同样的信息在任何机器登录都得到同一角色 ID
- 密码终端隐藏输入（rpassword）；stdin 被管道接管时（测试/脚本）自动退回行读取
- 聊天协议 v4（`/chat/4.0.0`）：`Hello` 携带角色名，连接即互通身份建立花名册
- `/list`：已登记节点列表（完整节点 ID、角色名、当前聊天/已连接/离线状态）
- `/chat <完整角色名 或 完整节点ID>`：按需发起 1v1，精确匹配，无前缀/模糊匹配
- e2e 新增"按角色名呼叫"场景：退出重进身份不变，对端按名呼叫成功
- e2e 登录凭据外置至 `tests/users.txt`（git 忽略，不入库），提供
  `tests/users.template.txt` 虚构信息模板

### 变更

- mDNS 发现改为**只登记不自动拨号**，连接一律由 `/chat` 或 `/dial` 显式发起
- 当前聊天对象（active）粘滞：已有对象时新来电不抢占，连接归零才释放；
  心跳与超时判定只针对 active
- 登录输入校验归一：生日零填充为 YYYY-MM-DD，性别映射为 M/F/O，不合法循环重问
- mDNS 发现过滤自身 PeerId（同账号双开不再尝试自连）

### 安全说明

- Argon2id 参数（m=19456KiB, t=2, p=1）为 v1 常量：**参数是角色 ID 的一部分**，
  未来调整参数将导致所有角色 ID 变更
- 加密套件按嵌入式友好选型：Ed25519（身份）+ X25519/ChaCha20-Poly1305/SHA-256
  （Noise 传输层，libp2p 自带），为未来嵌入式主机配对与固件传输场景预留

## [0.2.0] - 2026-08-19

### 新增

- 聊天协议 v3（`/chat/3.0.0`）：消息封装拆分为数据面与控制面
  - 数据面：`Text` 文本、`Binary` 二进制文件（预留变体，收发通道未实现）
  - 控制面：`Heartbeat` 心跳、`Hello` 上线通知、`Bye` 主动下线通知
- 心跳包活：每 5 秒发送心跳；超过 15 秒无任何响应判定对方离线并主动断开
- 上线通知：连接建立即发 `Hello`，按 peer 分别问候
- 主动下线：`/quit` 时先发 `Bye` 并限时等待对方确认（退出握手）；
  收到 `Bye` 的节点不再对对方心跳与重连
- 拨号失败自动恢复：任一路径拨号失败后自动改用该 peer 的其他已知地址逐个重试
- 断线自动重连：最后一条连接关闭且非对方主动退出时，按已知地址重拨
- 连接关闭上报：关闭原因（cause）与剩余连接数
- e2e 测试新增 2 个场景：主动上下线循环 ×15、kill 进程掉线循环 ×5
  （每轮发送 ≤64 字节随机消息，掉线后隔 3 秒重新上线）
- e2e 基础场景：静默 12 秒心跳保活验证、优雅退出通知验证

### 变更

- 改用本机 MinGW64 GCC 链接（`.cargo/config.toml` 指定 `linker = "gcc"`）
- ip6 监听复用 ip4 实际端口，消除 mDNS 广播 ip4/ip6 端口交叉错配产生的无效拨号
- mDNS 自动拨号去重：每 peer 只拨一个地址，拨号中/已连接不重复拨
- e2e 连接断言改为 peer id 锚定，防止同机 mDNS 跨测试串扰误判
- 日志分级：手动拨号失败/突发中断 = 红，自动恢复过程 = 灰，重连最终放弃 = 黄

### 修复

- "假断连"：原来单槽 peer 状态在任一连接关闭时即清空；现以剩余连接数归零才判离线
- Hello 丢失：原来只在第一次连接发送，若先连上错误节点则真正的聊天对象收不到
  上线通知；现按 peer 记录问候状态
- 僵尸连接误重连：原 `voluntary_offline` 单标志被第一条连接消耗后，迟到的僵尸连接
  会触发对已退出节点的重连；现改为 `bye_peers` 集合按 peer 判定
- 退出握手期间不响应新进请求，导致对端心跳收到 cbor Eof 错误；现子循环内照常回包

## [0.1.0] - 2026-08-18

### 新增

- 菜单式命令行工具：计算器（四则运算，先乘除后加减）、学生信息管理、彩色打印演示
- 指令树组件 `cmd_tree`：由 C 版 simpleCmd cmdTree v2.0 移植——树形路由、最深命中、
  双引号分词、多实例、help、dataHandler
- libp2p P2P 聊天（协议 v1）：TCP + Noise 加密 + Yamux 多路复用；mDNS 局域网自动发现
  + 手动拨号；地址模板与分步诊断；1v1 实时互聊
- 18 项单元测试（指令树 8 + 地址诊断 10）+ 双节点端到端集成测试
