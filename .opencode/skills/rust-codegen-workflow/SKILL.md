---
description: 使用时机：写代码/改代码/新增函数/修编译错误后、大范围重构拆阶段、提交前、或任何"逻辑改动连带 GUI 也要改"的场景。内容为本项目代码生成的验证链纪律：cargo check 先行（秒级语法/类型反馈）→ cargo build（需要运行时才做）→ cargo test → 冒烟 → 提交。以及工具选择（PowerShell 禁令）、小步迭代、编译器诊断优先。涉及以上任一场景时必须先读本技能再动手。
---

# Rust 代码生成工作流纪律

本项目（p2p_rust_app）代码生成的验证链与工具纪律。
**核心：`cargo check` 先行（秒级反馈），`cargo build` 只在需要运行二进制时才做。**

---

## 1. 验证链（每次代码改动的固定顺序）

```
┌─ 1. cargo check 2>&1     语法/类型（秒级，不产出二进制）
│     └─ 零 error 零 warning → 通过
│     └─ 有错 → 修 → 再 check（循环，不跳步）
├─ 2. cargo build          需要运行/冒烟时才做
├─ 3. cargo test --bin p2p_rust_app            单测（秒级）
├─ 4. 先杀残留进程（防 mDNS/端口干扰——曾连挂 3 场景）
│      cargo test --test p2p_chat -- --test-threads=1   e2e（分钟级）
├─ 5. 冒烟（python tests/gui_smoke.py）   GUI 交互链路
└─ 6. git commit           全绿后
```

### check / build 分工

| 命令 | 用途 | 成本 |
|---|---|---|
| `cargo check` | 语法/类型检查，不链接不产出二进制 | **秒级** |
| `cargo build` | 产出可执行文件，仅在需要运行/冒烟时 | 分钟级 |
| `cargo test --bin` | 单测（纯逻辑，秒级） | 秒级 |
| `cargo test --test p2p_chat -- --test-threads=1` | e2e 全量 | ~160s |

- `cargo check` 是**写代码过程中的循环验证器**——每改一处逻辑就 check，绿了再改下一处
- `cargo build` 只在**需要运行二进制**（冒烟/手测/交付）时执行
- 反面案例：本轮多轮在 check 能秒报的错误上花了 build 级等待

---

## 2. 编译器诊断优先

### 读全量输出

```bash
cargo check 2>&1 | Select-String -Pattern "^error|^warning|Finished"
```
error 编号（E0308/E0425/E0004/E0277/E0599）直接 grep 文档——不猜不手写扫描器。

### 陷阱案例

- **手写括号计数器把生命周期 `'_` 误判为字符字面量**（吞掉后续闭引号）→ 假阳性"花括号平衡"误导两轮 → rustc 一秒定位
- **`String::new(),` 多逗号** → 编译器报"未闭合分隔符"（错误位置偏移，真错是多逗号）——修掉逗号后仍需再查
- 编译器 hint（"consider importing""expected X found Y"）**直接采纳**，不自行推断

### 编译器报错位置有时偏移

`String::new(),` 多逗号导致编译器报"未闭合分隔符"且位置偏移到函数尾部——修掉真错（多余逗号）后其余"错误"消失。**当编译器报错位置与实际代码不符时，先修最简单的语法错误再重查。**

---

## 3. 结构性代码修改：工具纪律

### 禁令：PowerShell here-string / .Replace() 做代码修改

三坑实证（每次浪费 ≥1 轮）：
- `` `n `` 双引号字符串中被字面量写入文件（而非换行）→ 代码行断裂
- 引号嵌套（`\"` / `""`）解析崩溃 → 脚本本身跑不起来
- `.Replace()` anchor 含 `\r\n` 但目标文件是 `\n` → 匹配失败却打印"已插入"（假成功）

**结构性修改（多行插入/移动/重写函数）→ 用 Python 脚本或 edit 工具，绝不碰 PowerShell 字符串操作。**

### 行号切片脚本纪律

用 Python 脚本按行号切分大文件时：
- 断言（`assert lines[N].startswith(...)`）**跑通后立即执行切分**，不隔轮
- 脚本用完即删或**单独提交**（不留一次性工具污染仓库）
- 切分后先 `cargo build` 再继续

---

## 4. 小步纪律

- 每个逻辑改动 → check → 绿了再改下一处
- 大范围重构 → 拆阶段，每阶段独立门禁+提交（P2.5 chat.rs 2483 行拆 12 模块 = 3 commits，每步 e2e 兜底）
- 修 bug 单独提交；清理（脚本删除等）单独提交

### 冒烟前必须杀残留进程

残留 GUI 实例占 mDNS/端口 → e2e 连挂 3 场景（教训）：
```powershell
Get-Process p2p_rust_app -ErrorAction SilentlyContinue | Stop-Process -Force
```

### 冒烟 FAIL → 先读日志再盲跑

日志在 `$env:TEMP\p2p_smoke_*\gui_logs\<ts>\interact.log` + `runtime.log`。盲跑浪费 5 分钟/轮。

---

## 5. egui / GUI 修改补充（详见 p2p-app-modification-pitfalls 技能）

- egui `Label` 默认 `Sense::hover` 不可点击 → 可点击必须显式 `Sense::click` + `PointingHand`
- egui RTL Frame 自动尺寸与 LTR 不对称（撑满锚定列）→ 气泡类自绘改手工 galley 测量
- PowerShell 编辑引入的 `` `n `` 字面量污染在 ctx.rs / ui/mod.rs 各出现一次——每次 PS 编辑后检查文件

---

## 6. 本轮修正实例索引（git log 可查）

| 修正 | 教训归属 |
|---|---|
| GUI 备份密码被当聊天发送 | Control 臂缺挂起登记（冒烟日志抓到） |
| "我的地址"缺 /p2p/ 尾巴 | 提交后用户实测发现 |
| 单方面信任"消息仍已发送"误导文案 | 用户截图+日志实证 |
| Signal 臂整段误删（AddRange 类型转换） | cargo check unclosed delimiter |
| ctx.rs 参数行 `` `n `` 字面量断裂 | cargo check E0425 |
