---
name: p2p-libp2p-tokio
description: 本项目 libp2p 0.56 / tokio 1.53 用法参考。Use when editing or generating code in this project that touches libp2p or tokio — SwarmBuilder, #[derive(NetworkBehaviour)], Toggle, mDNS, tokio::select!, or module (mod) references. Covers the constraints and gotchas we hit (Option fields not supported, Toggle<T> pattern, multicast dnsaddr TXT parsing, stdin read-ahead conflict).
---

# libp2p + tokio 用法（本项目）

依赖版本：libp2p 0.56.0（libp2p-swarm 0.47）、tokio 1.53、bip39 2.2、chacha20poly1305 0.10、argon2 0.5、socket2 0.5。

## 1. 模块与 `mod` 引用

- `source/main.rs` 顶层一次性声明各模块：`mod chat; mod cmd_tree; mod p2p;` 等
- **分层（0.21 起严格 L3→L2→L1）**：
  - L1 = `source/p2p/node.rs`：`Frame`/`Control`/`P2pCommand`/`P2pEvent`/`P2pNode` 均 **`pub(crate)` 内部化**，
    只有 `seam.rs`（L2）能引用；线缆帧组装/拆解只在这层
  - L2 = `source/p2p/seam.rs`（传输适配层）：对 L3 暴露 `seam::Cmd`（`Send{peer,tag,payload}` 等）与
    `seam::Event`（`Signal{from,tag,payload}` 等），`spawn_transport()` 建节点 + 适配任务翻译两向通道
  - **L3（chat.rs / file_transfer.rs）禁止 import `p2p::node`**：只经 `seam::Cmd`/`seam::Event` 与 L1 通信，
    发送一律 `seam::Cmd::Send { peer, tag, payload }`（帧组装由 seam 收口）
- `source/p2p/` 是**通用传输层**（身份/联系人/发现/隐身监听/传输适配），`chat.rs` 是消费方：
  - `p2p/mod.rs` 声明 `pub mod identity; pub mod contacts; pub mod discovery; pub mod mdns_stealth; pub mod seam;`
  - `chat.rs` 引用：`use crate::p2p::seam::{self, Event, ...};`（**不引 node**）
- 通用层内跨文件引用：`super::identity::cache_dir`（如 contacts/discovery 用身份缓存目录）
- libp2p 的类型有时需全路径：`libp2p::swarm::behaviour::toggle::Toggle`（不在 `libp2p::swarm::*` 顶层 re-export）

## 2. Swarm 构建模式

```rust
SwarmBuilder::with_existing_identity(keypair)
    .with_tokio()
    .with_tcp(tcp::Config::default(), noise::Config::new, yamux::Config::default)?
    .with_behaviour(|key| {
        let peer_id = key.public().to_peer_id();
        // ... 构造 NodeBehaviour
    })?
    .build()
```

- 传输栈固定为 TCP + Noise（加密+身份认证）+ Yamux（多路复用）
- **身份认证**：Noise 握手会校验对端公钥 → 拨号地址必须带 `/p2p/<PeerId>` 段，
  且 `PeerId` 与对方公钥绑定（PeerId = 公钥哈希）。这是防中间人攻击的根：
  mDNS 投毒只能让连接失败，无法冒充
- 确定性身份：`Keypair::ed25519_from_bytes(32 字节种子)`（见 p2p-identity-keystore）

## 3. `#[derive(NetworkBehaviour)]` 约束（禁踩的坑）

组合行为字段：

```rust
#[derive(libp2p::swarm::NetworkBehaviour)]
struct NodeBehaviour {
    mdns: Toggle<mdns::tokio::Behaviour>,   // 不能用 Option!
    ping: ping::Behaviour,
    chat: request_response::cbor::Behaviour<ChatRequest, ChatResponse>,
}
```

- ❌ **不支持 `Option<T>` 字段**：报 `Option<Behaviour<T>>: NetworkBehaviour` 不满足
- ❌ **不支持 enum derive**：报 `Cannot derive NetworkBehaviour on enums`
- ✅ **用 `Toggle<T>`**：`libp2p::swarm::behaviour::toggle::Toggle`；
  `Toggle::from(Some(b)/None)`；`ToSwarm = T::ToSwarm`，
  事件变体**不**多套一层，仍是 `NodeBehaviourEvent::Mdns(mdns::Event)`
- 组合行为事件匹配写法：
  ```rust
  SwarmEvent::Behaviour(NodeBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => { ... }
  ```
- 被 Toggle 关闭的行为不发任何事件（`ToSwarm = Infallible`）

## 4. mDNS

- libp2p-mdns **无"只收不发"开关**：它周期性发送组播 PTR 查询，且因 multicast loop 会
  收到自己的查询并**组播响应**（自查自答）；对别人查询的响应也走组播
- 广告内容：TXT 记录携带 `dnsaddr=<multiaddr 含 /p2p/PeerId>`（见 libp2p-mdns `MdnsPeer::new`）
- **隐身模式**（只收不发）= `Toggle` 关闭 libp2p-mdns 行为 + 自实现监听器：
  `source/p2p/mdns_stealth.rs` 绑 `224.0.0.251:5353`，解析 `dnsaddr=` TXT 记录还原 (PeerId, Multiaddr)。
  仅实现 IPv4。靠对端周期自查自答在约一个查询间隔内发现它
- 发现模式枚举 `DiscoveryMode { AdvertiseAndDiscover, DiscoverOnly, Off }`，
  `Toggle` 只在 advertise 时启用 mdns

## 5. tokio 事件循环

主循环 `tokio::select!` 多分支：

```rust
tokio::select! {
    line = stdin.next_line() => { ... }
    _ = heartbeat.tick() => { ... }
    event = swarm.select_next_some() => { ... }
    discovered = async {
        match stealth_rx.as_mut() {
            Some(rx) => rx.recv().await,
            None => std::future::pending().await,   // 无监听器时不触发此分支
        }
    } => { ... }
}
```

- 可选分支写法：`Option<Receiver>` + `std::future::pending().await` 占位
- 后台任务：`tokio::spawn(async move { ... })` + `mpsc::channel` 上报
- 交互输入：stdin 为管道时（测试）密码走普通行读取，交互终端用 rpassword

## 6. ⚠️ 已知坑

- **stdin 预读冲突**：主菜单 `main.rs` 用 `std::io::stdin()`，聊天 `chat.rs` 用
  `tokio::io::stdin()`（内部 BufReader 会预读一大块）。**管道一次性喂完整输入时**，
  退出聊天再进（跨会话）会丢后续行；TTY 与 e2e 增量写入不受影响。
  改动 main.rs / chat.rs 输入层前先确认此行为
- 影子探测 `probe_duplicate_id` 用一次性随机身份（libp2p-mdns 会过滤与本机身份同
  PeerId 的发现，故需借影子身份）——仅在 `AdvertiseAndDiscover` 时有效

## 7. 信号格式规范（可扩展约定，新增信号照此格式）

**`Frame` 三通道**：`control`（L1 心跳等传输控制）/ `text`（协议语义**标签**）/
`binary`（该标签的 cbor 负载）。L1 对 text/binary 内容不解释，只透传。

**注册与分发**：`SignalRegistry<C>`（在 `seam.rs`，L2，经 `SignalCtx` GAT 泛化上下文，本项目 `C=AppCtx<'static>`）维护
`tag → async handler` 表（HashMap 查表，**无业务 match**），收到 `Event::Signal` 查表分发。
构造负载帧用 `seam::Cmd::Send { peer, tag, payload }`（帧组装由 seam 收口）。

**L2 内化信号（hello/bye/trust）**：`identity_service.rs` 的 `TextTag` 枚举
（`Hello`/`Bye`/`TrustConfirm`/`TrustRevoke`），经 `from_str`/`as_str` 与线缆字符串互转。
**仅 L2 认识，L3 业务不触碰**；门禁白名单 `is_l2_signal(tag)` 判内化信号。

**现有标签**：
- L2 内化（TextTag）：`hello`（binary=cbor(名字)）、`bye`、`trust.confirm`/`trust.revoke`
  （binary=cbor(名字)；互信 = 我信他 且 他信我）
- L3 chat 业务：`chat.text` / `chat.group_invite` / `chat.group_leave` /
  `chat.group_member_list` / `chat.group_owner_transfer`
- L3 文件传输：`file.offer` / `file.accept` / `file.reject` / `file.chunk` / `file.ack` /
  `file.finish` / `file.complete` / `file.abort`

**新增信号的套路**：① 定义 `const TAG_X: &str` ② 定义 cbor 负载 `struct`（serde）
③ 在 `SignalRegistry` 注册 `registry.register(TAG_X, |ctx, from, payload| Box::pin(handler))`
④ L2 语义放 `identity_service.rs`，L3 业务放各自模块（如 `file_transfer.rs`）。
协议版本号只在改动既有标签语义时 bump。

**L2 门禁（唯一收口，chat.rs 分发入口）**：`is_l2_signal(tag)` 为真（内化信号）一律放行；
业务信号（chat.*/file.*）须 `effective_trusted`，否则走该 tag 的**未互信钩子**
`SignalRegistry::register_untrusted(tag, handler)`（L2 API，async，与应用 handler 同签名；
**不注册 = 空函数 = 丢弃**——payload 无人引用，自然回收）。测试专用：`P2P_E2E_UNTRUSTED_HOOK=1`
让 chat 注册 `chat.text` 未互信显示钩子（`[未信任]` 标记），用于验证"未互信处理是每端本地策略"。
**对称信任红线**：`effective_trusted = is_verified && their_trust`；`/trust` 发 confirm、
`/trust !` 发 revoke 并重置会话 `send_confirmed`（对方离线静默跳过）；hello 处理后重报当前
信任态（重连自愈）。群消息（gossipsub `P2pEvent::Gossip`）不走 frame.text 分发，不受此门禁。
