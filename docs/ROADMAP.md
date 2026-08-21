# 路线图（Roadmap）

P2P 传输层产品化方向规划。以"身份验证 + 连接"为根基，聊天只是第一个协议。

## 根基：身份验证与连接（已完成，后续一切复用）

| 层 | 机制 | 解决的问题 |
|---|---|---|
| 密钥 | BIP39 助记词 → Ed25519 → PeerId = 公钥哈希（自证身份） | 身份无法伪造 |
| 连接 | Noise 握手双向校验公钥，拨号带 `/p2p/PeerId` | 连接对象不可被 MITM 顶替 |
| 信任 | TOFU 联系人簿：首接指纹确认 + `/trust` 持久化 | 把 peer_id 与"真人"绑定 |

## 已落地里程碑

- **0.4.0** 身份缓存 + 影子探测防同 ID 双在线
- **0.5.0** 身份模型革命：随机种子 + BIP39 助记词，密码只护本地 keystore（Argon2id + ChaCha20-Poly1305），`/backup` 与 `r` 恢复
- **0.6.0** SSH 式 TOFU 指纹核对 + 联系人簿 + `/trust`
- **0.7.0** mDNS 发现模式（advertise/stealth/off），自实现隐身监听器
- **0.8.0** 通用 p2p 模块抽离（`source/p2p/`：identity/contacts/discovery/mdns_stealth），聊天变薄
- **0.14.0** 三层架构：L1 `P2pNode` 传输任务（命令/事件通道，永不阻塞于应用交互）+
  L2 `IdentityService`（登录/TOFU/信任/Hello-Bye）+ L3 业务（会话/群/焦点/列表）；
  心跳归 L1 且对全部已连接非 bye peer；根治 TOFU 卡住导致的心跳冻结
- **0.15.0** 群成员一致性加固（单写者收口）：群主不在线禁止退群、群主退群一步顺位转移、
  幽灵/重复成员去重兜底；CRDT 方向见 `docs/GROUP_CRDT_ROADMAP.md`
- **0.16.0** 1v1 信任管理完善（取消信任修复、联系人名解析、指纹复核、会话徽标、未信任发消息确认）+
  群主可见（/list 群聊行）+ 群主转移自愈 + 邀请重发

## 后续规划

### Phase B：通用传输 API（`P2pNode` + `P2pEvent`）✅ 0.14.0
- `source/p2p/node.rs`：`P2pNode` 收拢 build_swarm、连接建立/关闭、地址簿、重连、发现、心跳
- `P2pCommand`（Dial/DialPeer/Send/MarkBye/Subscribe/Unsubscribe/Publish/Shutdown）+
  `P2pEvent`（PeerConnected/PeerDisconnected/PeerDiscovered/Message/Gossip/SendFailure）
  双通道解耦 `run_node` 的 select 循环
- 传输任务独立运行，事件用无界通道——应用卡在 TOFU/密码交互时心跳照常
- 聊天作为第一个消费方，行为不变（e2e 守护）

### Phase C：协议与传输解耦
- **消息帧信封**（✅ 0.12.0 核心）：`Frame{control/text/binary}` 三通道——
  `control` 心跳/传输控制、`text` 节点间短消息（Hello/Bye）、`binary` 用户内容负载
  （`AppPayload` 自描述，cbor）；协议 `/chat/7.0.0`
- **心跳（保活）进传输层**（✅ 0.14.0）：L1 对全部已连接非 bye peer 保活，超时判离线；
  上线/离线通知（`PeerConnected/PeerDisconnected`）与名字握手（Hello/Bye 归 L2）分属传输/身份层
- **handler 注册表**（后续）：把三通道的硬编码 match 改为"按变体标签注册处理器 +
  按标签分发"，新信号/新业务（文件等）= 注册一个 handler 不动核心

### 文件传输（`/file/1.0.0`）
- 复用身份验证 + 联系人簿：发送前校验接收方是**已验证联系人**（"发给谁"由传输层保证）
- 分块 / 进度 / 校验和（哈希）；文件数据将作为 `AppPayload` 变体走 `Frame.binary`
- UX 改进（后期）：二维码互扫、联系人选择器、邀请码，降低首接指纹确认的繁琐
### 多点通讯

- **传输层已是多点**：libp2p 同时持有多条认证连接，`send(peer, frame)` 按 peer 寻址；
  当前 1v1 是应用层 `active: Option<PeerId>` 的"塌缩"
- **并发多会话**（✅ 0.9.0）：`active` → `HashMap<PeerId, Conversation>` + `focused`；
  `/chat` 切换焦点、非焦点来信带名、重连队列、/q 全会话 Bye
- **群聊**（✅ 0.10.0）：gossipsub（`MessageAuthenticity::Signed`）+ 本地群注册表
  （成员须已验证联系人）+ 1v1 邀请入群 + 成员名单同步；`/group` 命令、`/chat/5.0.0`
- **群成员一致性**（✅ 0.11.0）：**群主为中心**的单一权威模型——仅群主可邀请、
  成员退群通知群主划去、群主不能踢人；每次变更版本化后向最新名单所有成员 1v1 扇出
  （`/chat/6.0.0`）。已知局限：成员若与群主不直连会漏收名单更新（可后续加重连快照补偿）
- **常驻接收/连接分离**（✅ 0.13.0）：`/group resident on|off` 防通讯风暴——常驻群
  成员上线自动 mesh、普通群聚焦时按需连接；连接与进入 1v1 会话解耦
- **群聊后续**：群消息历史持久化；重连时向群主请求名单快照（补偿漏收）

### 公网（M6）
- relay 中继（circuit v2）+ dcutr UDP 打洞
- 身份/TOFU/加密已就绪，可平滑上公网；发现层从 mDNS 换成 relay/rendezvous

### 其他
- 修复 stdin 预读冲突（主菜单 `std::io::stdin()` vs 聊天 `tokio::io::stdin()`，
  管道一次性喂完整输入时跨会话重进聊天丢行；TTY 与 e2e 增量写入不受影响）
- 联系人黑名单 / 群组管理 / 消息历史持久化
