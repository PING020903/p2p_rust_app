---
name: p2p-libp2p-tokio
description: 本项目 libp2p 0.56 / tokio 1.53 用法参考。Use when editing or generating code in this project that touches libp2p or tokio — SwarmBuilder, #[derive(NetworkBehaviour)], Toggle, mDNS, tokio::select!, or module (mod) references. Covers the constraints and gotchas we hit (Option fields not supported, Toggle<T> pattern, multicast dnsaddr TXT parsing, stdin read-ahead conflict).
---

# libp2p + tokio 用法（本项目）

依赖版本：libp2p 0.56.0（libp2p-swarm 0.47）、tokio 1.53、bip39 2.2、chacha20poly1305 0.10、argon2 0.5、socket2 0.5。

## 1. 模块与 `mod` 引用

- `source/main.rs` 顶层一次性声明各模块：`mod chat; mod cmd_tree; mod mdns_stealth;` 等
- `chat.rs` 内引用同 crate 模块：`use crate::mdns_stealth::StealthMdns;`（crate 根 = `main.rs`）
- `mdns_stealth.rs` 是独立文件模块，不是 `chat.rs` 的子模块；不要重复 `mod` 声明
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
  `source/mdns_stealth.rs` 绑 `224.0.0.251:5353`，解析 `dnsaddr=` TXT 记录还原 (PeerId, Multiaddr)。
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
