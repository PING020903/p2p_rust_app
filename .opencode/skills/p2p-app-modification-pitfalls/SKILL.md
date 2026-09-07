---
description: 使用时机：修改 source/p2p_app/chat/session.rs、source/ui/、source/p2p_app/file_transfer/、做交互确认（TOFU/文件/备份/未信任）、新增 Ask 卡片、调试 GUI 阻塞/冻结、用 PowerShell 改代码、写 GUI 冒烟脚本、或任何"逻辑改动连带 GUI 也要改"的场景。内容为本项目实战踩坑教训：两段式确认协议、egui 陷阱、PowerShell 禁令、冒烟脚本纪律、层级规则。涉及以上任一场景时必须先读本技能再动手。
---

# P2P 应用层修改陷阱与纪律

本项目（p2p_rust_app）GUI/应用层迭代的实战教训沉淀。六节按"工具 → 模式 → 架构 → 验证 → 提交 → 边界"组织。

---

## 1. 结构性代码修改：工具纪律

### 禁令：PowerShell here-string / .Replace() 做代码修改

三坑实证（每次都浪费 ≥1 轮调试）：
- `` `n `` 在双引号字符串中被字面量写入文件（而非换行）→ 代码行断裂
- 引号嵌套（`\"` / `""`）解析崩溃 → 脚本本身跑不起来
- `.Replace()` 的 anchor 含 `\r\n` 但目标文件是 `\n` → 匹配失败却打印"已插入"（假成功）

**结构性修改（多行插入/移动/重写函数）→ 用 Python 脚本或 edit 工具，绝不碰 PowerShell 字符串操作。**

### 行号切片脚本纪律

用 Python 脚本按行号切分大文件时（如 chat.rs 拆 12 模块）：
- 断言（`assert lines[N].startswith(...)`）**跑通后立即执行切分**，不隔轮（文件可能被其它操作改变）
- 脚本用完即删或**单独提交**（不留一次性工具污染仓库）
- 切分后先 `cargo build` 再继续下一步

### 编译错误 → 先看 cargo check

不手写括号扫描器。rustc 的 `unclosed delimiter` + `might not be properly closed` + 缩进提示已足够定位。
**陷阱案例**：手写括号计数器把生命周期 `'_` 误判为字符字面量开引号，吞掉后续闭引号 → 假阳性"花括号平衡"误导两轮。
**正确姿势**：`cargo check 2>&1` 读全量输出；`e0425/e0308/e0004` 编号直接 grep 文档。

---

## 2. egui 交互确认：两段式协议

### 核心：禁止 handler 内 `.await` 读用户答案

单任务 `tokio::select!` 中，任一分支 handler 在 `.await` 挂起 = **整个循环冻结**（对端消息排队不显示）。
日志实证：未互信发送确认等待期间，双方各自"已发送"，双向门禁各自丢弃，互不知情。

**两段式**：
```
phase 1：提示/发卡 + 登记待决状态（pending_confirm/pending_offers）→ 立即返回
phase 2：答案到达（GUI=ConfirmAnswer 通道 / CLI=确认子窗口退出码）→ 二次进入执行
```

```rust
// phase 1（事件臂/命令臂）：发卡 + 登记 → 立即返回（不阻塞）
crate::sink::ask(AskRequest {
    id: crate::uievent::next_ask_id(),
    kind: AskKind::FileReceive { from, filename: name.clone(), size: p.size },
    secret: false,
});
// 不读行！返回后 select 继续转——对端消息实时显示
```

```rust
// phase 2（select 新臂）：答案到达 → 执行第二阶段
answer = confirm_rx.recv(), if !confirm_rx.is_closed() => {
    match answer {
        Some(ConfirmAnswer::Tofu { peer, trusted }) => {
            identity.complete_tofu(&peer, &name, trusted); // 落账
            // 补跑钩子 + 信任重报信号
        }
        // ...
    }
}
```

**确认子窗口（CLI Interactive）**：
```rust
// CREATE_NEW_CONSOLE 拉起自身 --confirm-tofu 子模式；退出码编码答案（0=信任）
tokio::process::Command::new(exe)
    .args(["--confirm-tofu", &name, &fingerprint, &peer.to_string()])
    .creation_flags(CREATE_NEW_CONSOLE)
    .status().await
// 密码类：子进程 rpassword 不回显读入 → 写结果临时文件 → 父进程读后删
```

**禁止事项**：
- Ask 等待期间禁用聊天输入（防文本被当作答案吞掉）
- 不得在确认 handler 内做 `next_raw_line().await`（冻结根源——曾导致 GUI 收文件时 chat 冻结、CLI 打字被当 y/n 吞）

### egui Label 不可点击

egui `Label` 默认 `Sense::hover()`——`resp.clicked()` 永远 false（点击事件从未触发的根因）。

```rust
// 可点击 Label 必须显式声明 sense + 手型光标
ui.add(
    egui::Label::new(name_text)
        .selectable(false)
        .sense(egui::Sense::click()),          // ← 必须显式
).on_hover_cursor(egui::CursorIcon::PointingHand);
```

### egui RTL Frame 自动尺寸不对称

`Frame::group` / `Frame::default` 在 `Layout::right_to_left` 下撑满锚定列（LTR 正常）——自动尺寸与 RTL 交互不对称。
**解法**：放弃 Frame 自动尺寸，改手工 galley 测量 + `allocate_exact_size` + `painter` 手绘（见 render_bubble）。

### 冒烟脚本 UIA 三层问题与对策

| 问题 | 对策 |
|---|---|
| 鼠标坐标点击 egui 不可靠 | **UIA Invoke**（accesskit 按钮支持语义点击） |
| UIA ValuePattern 对 egui 无效 + 中文 IME 拦截键入 | **剪贴板 Ctrl+V 粘贴**（`pyperclip.copy` + `pyautogui.hotkey("ctrl","v")`） |
| UIA 树多 Edit 歧义（accesskit 残留 + 新卡片） | 逐个 Edit 点击粘贴（真实卡片最后获焦点生效），随后单次解锁 |

---

## 3. 层级纪律

```
p2p/           协议核心：无渲染、无交互流程、无 GUI 形状类型
p2p_app/       应用层：交互流程（确认卡片/登录向导）、按应用×模式分 cli/gui
ui/            跨应用渲染件（GuiApp 壳、fonts）
```

- GUI 事件类型（AskKind/ConfirmAnswer 等）放 crate 根 `uievent.rs`/`lineio.rs`——**不进 p2p/**
- 交互流程永不进 p2p/（协议核心无菜单/表单/确认编排）
- L2 两段化（TOFU/Backup 拆 begin/complete）的 pending 状态存 IdentityService 字段或 session——**不新建 GUI 专用类型**

**纯 GUI 两段式**（信任/取消信任确认卡片）：不走 Ask 协议（引擎无感知）——按钮先出卡片（`pending_trust` 状态），确认后才发 `Control::Trust`。

---

## 4. 冒烟脚本纪律（tests/gui_smoke.py）

### UIA 操作

```python
def click(el):
    try: el.invoke()            # UIA Invoke 优先（鼠标坐标对 egui 不可靠）
    except Exception: el.click_input()

# 密码：剪贴板粘贴（UIA ValuePattern 对 egui 无效 + 中文 IME 拦截逐字符键入）
edit.click_input(); time.sleep(0.3)
pyperclip.copy(password); pyautogui.hotkey("ctrl", "v")
```

### 脚本头部必须

```python
pyautogui.FAILSAFE = False  # UIA 元素坐标可能落屏幕角落
# 收尾 taskkill /IM 防残留（残留实例干扰后续 e2e——mDNS/端口）
```

### 断言走日志不走截图

```python
# interact.log（用户动作） / runtime.log（ui trace）按内容断言
wait_log_contains(log, "已信任: {name}")           # 引擎执行结果
wait_log_contains(runtime_log, "event=trust_card") # ui trace
```

**每交互点改动 → 冒烟同步加断言**（不留手测盲区）。

---

## 5. 提交纪律

- 修 bug 单独提交；清理（脚本删除等）单独提交——不混
- 每步 e2e 全量门禁（Auto 管道语义逐字节不变是底线）
- 冒烟 FAIL → 先查残留进程（`Get-Process p2p_rust_app | Stop-Process -Force`）再重跑——残留实例干扰 mDNS/端口曾致 e2e 连挂 3 场景

---

## 6. 已知边界（显式记录不遗忘）

- CLI 确认提示期间输入 = 答案（终端串行固有；提示已引导 y/n）
- TOFU 拒绝后重复 hello 不重复弹卡（升级走侧栏信任按钮两段式）
- 非 Windows 确认子窗口退化为阻塞式（known boundary）
- 确认子窗口密码经结果临时文件回传（父读后删；v1 简化）
