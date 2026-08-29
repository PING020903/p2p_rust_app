# 工程架构（P2P 功能）

> 基于 0.21.0 代码。三层架构 **L3 业务 → L2 传输适配+身份服务 → L1 传输层**，
> 严格分层：L3 不接触 L1 的 `Frame`/`control`，编译期由 `pub(crate)` 强制隔离。

---

## 一、分层 + 模块总览

```
┌─────────────────────────────────────────────────────────────────────────────┐
│ L3 业务应用层（source/）                                                     │
│  ┌─────────────────────────┐  ┌──────────────────────────────┐              │
│  │ chat.rs 聊天应用         │  │ file_transfer.rs 文件传输     │             │
│  │ · 指令树 CmdTree+handler │  │ · FileTransferState          │             │
│  │ · ops 队列(AsyncOp)      │  │ · 事件驱动推送 drive_sender  │              │
│  │ · SignalRegistry 注册    │  │ · 逐块 CRC32 + 8 个 file.* tag│             │
│  │ · 群/会话/信任 UI/门禁   │  │                              │              │
│  └───────────┬─────────────┘  └───────────────┬──────────────┘              │
│              │ 注册 handler / register_untrusted                            │
├──────────────┼──────────────────────────────────────────────────────────────┤
│ L2 传输适配 + 身份服务层（source/p2p/）                                       │
│  ┌────────────────────────────────────────────────────────────────────────┐  │
│  │ seam.rs（传输适配层）                                                   │  │
│  │ · seam::Cmd / seam::Event（tag+payload，无 Frame）                      │  │
│  │ · SignalRegistry<C>（GAT 泛化，查表分发 + 按 tag 未互信钩子）            │   │
│  │ · spawn_transport()：适配任务(Cmd→P2pCommand 组装帧 / P2pEvent→Event 拆帧)│ │
│  └───────┬───────────────────────────────────────────────┬──────────────┘    │
│          │                                              │                    │
│  ┌───────┴────────────┐                     ┌───────────┴────────────┐       │
│  │ identity_service.rs │                    │ discovery.rs / settings.rs│    │
│  │ · TextTag/is_l2_signal│                  │ 发现模式 / per-identity 配置│   │
│  │ · effective_trusted   │                  └────────────────────────┘       │
│  │ · hello/bye/trust 信号│                  ┌────────────────────────┐       │
│  │ · TOFU 首连           │                  │ identity.rs            │       │
│  └───────────┬──────────┘                   │ keystore/助记词/影子探测│      │
│  ┌───────────┴──────────┐                   └────────────────────────┘      │
│  │ contacts.rs          │                   ┌────────────────────────┐      │
│  │ 联系人簿 verified+   │                    │ mdns_stealth.rs        │      │
│  │ their_trust(互信)    │                   │ 隐身 mDNS 监听          │      │
│  └──────────────────────┘                   └────────────────────────┘      │
├──────────────┼──────────────────────────────────────────────────────────────┤
│ L1 传输层（source/p2p/node.rs，类型全 pub(crate) 内部化）                     │
│  · Frame{control,text,binary} / Control / P2pCommand / P2pEvent / P2pNode   │
│  · Swarm: request_response(/frame/1.0.0) 1v1 + gossipsub 群消息              │
│  · 心跳/超时/重连/发现/mDNS/隐身 listener 登记                                │
└──────────────┼──────────────────────────────────────────────────────────────┘
```

## 二、运行时任务与数据流

```
┌─ L3 应用任务 ───────────────────────────────────────────────────────────┐
│  stdin 行 → 指令树 handler（同步）                                       │
│     │ push_cmd / ops.push_back（AsyncOp::Cmd(seam::Cmd)）               │
│     ▼                                                                   │
│  ops 队列 drain（主循环 async 消费）                                     │
│     │ cmd_tx.send(seam::Cmd).await                                      │
└─────┼───────────────────────────────────────────────────────────────────┘
      │ Sender<seam::Cmd>            UnboundedReceiver<seam::Event>（ev_rx）
┌─────┼───────────────────────────────▲─────────────────────────────────┐
│ L2 适配任务（seam::spawn_transport） │                                 │
│  to_l1: Cmd → P2pCommand（组装 Frame）                                 │
│  to_l2: P2pEvent → Event（拆 Frame，滤 control 心跳）                  │
└─────┼───────────────────────────────┴─────────────────────────────────┘
      │ Sender<P2pCommand>            UnboundedReceiver<P2pEvent>
┌─────┼───────────────────────────────▲─────────────────────────────────┐
│ L1 传输任务（node.rs run 循环）      │                                 │
│  · apply_command → swarm（dial/send/subscribe/publish）               │
│  · 心跳 = Frame{control:Heartbeat}（L1 独占）                          │
│  · swarm 事件 → P2pEvent                                              │
└─────┴───────────────────────────────┴─────────────────────────────────┘
      ▼ 线缆
  request_response /frame/1.0.0（1v1）/ gossipsub（群）
```

## 三、信号与分发（L3 门禁 + 注册表）

```
Event::Signal{from, tag, payload} 到达主循环
   │
   ├─ 门禁（唯一收口）: is_l2_signal(tag) 或 effective_trusted(from)?
   │     ├─ 是 → registry.dispatch(tag, from, payload) → 该 tag 的 handler
   │     │        （hello/bye/trust.* → L2 内化；chat.*/file.* → L3 业务）
   │     └─ 否 → registry.handle_untrusted(tag, ...)
   │              ├─ 该 tag 注册了 register_untrusted → 执行钩子
   │              └─ 未注册 → 空函数 = 丢弃
   └─ 事件由 seam 拆好（tag+payload），L3 全程不碰 Frame/control
```

**帧模型**：`Frame{ control, text, binary }`——`text`=协议语义标签（字符串），`binary`=该标签的 cbor 负载。
L1 对 text/binary 不解释只透传；`control`（心跳）由 L1 独占。

**信号清单**：
- L2 内化（`TextTag`，`is_l2_signal` 白名单）：`hello` / `bye` / `trust.confirm` / `trust.revoke`
- L3 chat 业务：`chat.text` / `chat.group_invite` / `chat.group_leave` / `chat.group_member_list` / `chat.group_owner_transfer`
- L3 文件：`file.offer/accept/reject/chunk/ack/finish/complete/abort`

## 四、信任（对称互信）

```
effective_trusted(peer) = verified(我信他) && their_trust(他信我)   ← contacts.rs
  · /trust    → 发 TextTag::TrustConfirm；/trust ! → TrustRevoke（重置 send_confirmed）
  · hello 处理后就地重报信任态（重连自愈）；对方离线静默跳过
  · TextTag{hello,bye,trust.confirm,trust.revoke} = L2 内化信号，is_l2_signal 白名单
  · 任一方取消信任 → 整条链路不互信；未互信业务信号走未互信钩子（默认丢弃）
```

## 五、关键设计点

1. **严格 L3→L2→L1**：L3 只见 `seam::Cmd/Event`（tag+payload），L1 的 `Frame/P2pCommand/P2pEvent` 全 `pub(crate)`，编译期强制隔离
2. **适配任务**：seam 起一个 task 做两向翻译（Cmd→帧组装 / 事件→拆帧滤 control）
3. **查表分发**：`SignalRegistry`（seam，经 `SignalCtx` GAT 泛化上下文）HashMap 无 match；按 tag 未互信钩子（默认空=丢弃）
4. **ring buffer**：同步命令 handler → `AsyncOp` 队列 → 主循环 async 消费
5. **L1 独占 control**：心跳只在 node.rs，L3 无感
6. **跨局域网（规划）**：IPv6 直连优先 + 国内可达 relay 兜底，见 `docs/CROSS_LAN_CONNECTIVITY.md`

## 相关文档

- `docs/ROADMAP.md` — 演进路线图
- `docs/CROSS_LAN_CONNECTIVITY.md` — 跨局域网联通方案
- `docs/GROUP_CRDT_ROADMAP.md` — 群成员名单去中心化方向
- `.opencode/skills/p2p-libp2p-tokio/SKILL.md` — libp2p/tokio 用法与分层规则
