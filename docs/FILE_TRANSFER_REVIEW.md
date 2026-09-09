# 文件传输实现评估（FILE_TRANSFER_REVIEW）

> 评估对象：`p2p_app/file_transfer/mod.rs`（564 行，P2.6 步 4-2 自 `source/file_transfer.rs` 纯搬入）。
> 评估方式：全文通读 + 与 chat 应用层（payloads/handlers/ctx）同构对照。
> 评估时点：P2.6 步 3（UntrustedSend Ask + 接收拦截提示）之后。

## 一、实现概览

```
发送侧（事件驱动推送，无后台任务）                    接收侧
/start → start_send 登记发送态 + offer ──────►  on_file_offer（三态确认）
              ▲ accept                                    │ y：建 .{name}.part.{id} 临时文件
              │                                           ▼ 回 file.accept
        on_file_ack ──── drive_sender 读下一块 ◄── on_file_chunk（CRC32 校验→写盘→ack）
              │                                           │ finish：flush→改名→complete
        on_file_finish ──────────────────────────► on_file_finish（改名→complete）
abort：双侧状态清理（发送态移除/临时文件删除）
```

- 语义标签 ×8：file.offer/accept/reject/chunk/ack/finish/complete/abort（SignalRegistry 注册，session.rs）
- 状态：`FileTransferState{ senders: HashMap<u64,_>, receivers: HashMap<(PeerId,u64),_>, next_id, downloads_dir }`
- 下载目录解析链：`P2P_DOWNLOAD_DIR` env → per-identity 设置（settings::load_download_dir）→ `~/Downloads` → `./downloads`

## 二、优点（保持不动）

1. **事件驱动推送**：accept/ack 触发下一块，无后台任务——状态全在 AppCtx，与"L1 永不阻塞于应用"的并发模型契合
2. **逐块 CRC32 校验**：写盘前校验，失败即 abort + 临时文件清理 + 通知对端，错误路径完整
3. **路径安全**：`sanitize_file_name` 只取 basename 拒绝穿越（有单测）；临时文件 `.{name}.part.{id}` 隐藏式写盘
4. **错误路径全覆盖**：目录创建/文件创建/写盘/读文件失败各有 reject/abort + 状态清理，无泄漏路径
5. **同 peer 并发多文件**：receivers 按 (peer, file_id) 键控、senders 按 file_id 全局唯一，支持交错传输
6. **验证完备**：sanitize 穿越/CRC32 标准向量单测；e2e `standalone_file_transfer` 场景全链路

## 三、风险与改进清单（按优先级）

### H1. 完整性校验与注释不符（正确性/一致性）
- **现象**：`ReceivingFile` 注释宣称"完成时校验 sha256 并改名"，但 `on_file_finish` 只做 flush→rename→complete(ok:true)，**无 sha256 校验**——`FileCompletePayload.ok` 仅在改名失败时为 false
- **影响**：端到端完整性仅靠逐块 CRC32（非加密校验；传输层有 Noise 加密 + TCP 校验，实际错包风险很低，但协议自洽性与文档真实性受损）
- **建议**：`FileOfferPayload` 增 `sha256: [u8;32]`（发送侧 start_send 预计算），on_file_finish 改名后算 sha256 比对，不符→删文件+complete(ok:false)。协议字段新增向后兼容（serde default 跳过旧字段？新加字段旧端反序列化忽略——需两端同步升级，同版本部署场景可接受）
- **成本**：中（协议字段 + 两处计算）

### H2. offer 无超时（健壮性）
- **现象**：offer 发出后发送方**无限等待** accept（用户不点卡片/对端挂起 → 双方状态悬挂）；接收方 Ask 卡片同样无限等待
- **影响**：状态表悬挂条目累积（`senders`/`receivers` 不清理）；无泄漏但久置
- **建议**：发送侧 offer 超时（如 60s 无 accept → abort + 清理）。实现需定时器（tokio::time 在 select 或惰性检查），与"事件驱动无后台任务"设计有张力——可用"下次任何 file 事件到达时惰性检查过期"或单独 timeout 分支。**成本**：中；**收益**：UX（失败可见）
- **顺带**：Ask 卡片本身不设超时（同 CLI TOFU 行为，关窗即终止）——保持

### M1. GUI 发送入口缺失（UX）
- **现象**：/send 仅命令框可用；GUI 用户无法发起文件发送
- **建议**：气泡/工具栏"发送文件"按钮 → `rfd`（原生文件选择器 crate）选路径 → `Control::SendFile{peer, path}` → start_send——依赖设置页/系统消息区基建已就绪，成本小
- **关联**：P3"命令按钮化"清单项

### M2. 接收进度与完成通知（UX）
- **现象**：接收侧无进度显示（发送侧有每 8 块打印）；完成仅一行绿字（GUI 在时间线）
- **建议**：接收侧按块出进度（同发送侧节流），完成出系统消息卡片（"文件已保存: 路径 [打开目录]"）——低优先

### M3. FileReceive Ask（安全闭环，本报告落位）
- **现象**：Auto/Ask 模式下 offer **自动接受**（`else { true }`）——GUI 用户文件被静默保存（安全缺口：任意联系人可推文件落盘）
- **建议（首批实施）**：on_file_offer 三态补 Ask 分支——发 `AskKind::FileReceive{from, filename, size}` 系统消息卡片 + `next_raw_line` 读行（y=接收/n=拒绝）；Auto 保持自动接受（e2e 不变）
- **成本**：小（模式与 TOFU 卡片完全对称；GUI 卡片 arm 同套机制）

### L1. 注释清理
- `on_file_finish` 尾部 `let _ = file;`（保持句柄至 rename 后关闭的惯用法）补一行注释
- `ReceivingFile` 注释的 sha256 宣称随 H1 决策修正

### L2. 下载目录设置入口（✅ 已实施——P2.6 步 5 设置页）
- /download-dir 命令 + GUI 设置页（rfd pick_folder）；两者均**立即生效**（settings 落账 + file_state 运行时更新）；接收确认开关 /auto-receive 同批实施

## 四、实施批次建议

| 批次 | 内容 | 理由 |
|---|---|---|
| 首批（本步） | **M3 FileReceive Ask**（+GUI 卡片） | 堵最后一个静默自动化安全点；实现成本最小 |
| 次批 | **H1 sha256**（协议字段+校验）+ L1 注释修正 | 协议自洽；两端同版本部署可接受 |
| 再次 | H2 offer 超时、M1 发送按钮（原生选择器）、M2 进度卡片 | UX 增强，可并入 P3 |
| 暂缓 | L2 设置入口（随 P2.6 步 5 设置页） | 已在计划内 |

## 五、结论

实现质量整体**良好**：事件驱动模型干净、错误路径完整、路径安全有防、测试覆盖关键点。主要缺口是**文档与实现不符的 sha256**（H1）与 **GUI 静默自动接收**（M3，本报告后立即实施）。无阻断性缺陷，迁移后可放心在此基础上迭代。

## 六、后记（FileReceive 两段化实施后，2026-09-08）

- **M3 已实施并升级**：on_file_offer 拆两段——phase1 提示/卡片/登记 `AppCtx.file_pending`（**不再内联 await**），phase2 `complete_file_receive` 落账（接受/拒绝路径整体迁出）；CLI Interactive 拉起 `--confirm-file` 子窗口、Ask 卡片 Line 回程、Auto 自动接受（e2e 语义零变化）
- **H2 已实施并双侧补齐**：发送侧 offer 60s 超时（原有）；接收侧确认 60s 超时自动 reject（新增，并入定时臂 deadline=min(offer 过期,pending 过期)，防单槽 pending 被永久占位）
- **H1 仍未实施**（逐块 CRC32 够用，sha256 端到端校验待定）；L2 随步 5 设置页
- **新边界**：pending 被占时新 offer 忙拒（"正在等待其他确认"）；`/backup` 命令路径仍直接覆盖 pending_confirm（信号侧已忙拒，命令侧未拦截——已知边界）
- **顺带修复**：CLI 确认子窗口分发缺失缺陷（spawn 侧 `--confirm-tofu/--confirm-secret` 从未被 main() 解析——此前 Interactive TOFU 恒自动信任+误拉无关 GUI 窗口）

## 七、后记 2（GUI 文件传输实施后，2026-09-08）

- **M1 已实施**：GUI 发送入口——底部命令行区「发送文件」按钮（rfd 0.17 原生对话框；启用条件=1v1 互信焦点，群/未信任置灰+tooltip）→ `Control::SendFile{peer,path}` 结构化直传 → control.rs 复刻 /send（信任复查+start_send）；Linux 侧 XDG Portal→zenity 兜底，取消/无 portal 返回 None 提示行
- **M2 已实施**：结构化进度事件——`UiEvent::FileTransfer(FileTransferView)` + display.rs `file_transfer` 漏斗（CLI 文案逐字节保持/GUI 事件不进滚动区）；接收侧节流进度新增（每 8 块/尾块）；GUI 传输卡片（时间线占位一次 + ProgressBar 实时进度 + 终态：已保存路径+「打开所在目录」/ 失败原因）；CLI 文本零变化（e2e 逐字节）
- **顺带**：WSL cargo check 抓出并修复既有 Linux 编译破坏（ed43c6f 起 spawn_secret_window 调用点未 cfg 门）；双平台 check 零告警
- **遗留**：H1 sha256（待定）；下载目录/自动接收开关 → P2.6 步 5 设置页；SHA-256 撤销维持
