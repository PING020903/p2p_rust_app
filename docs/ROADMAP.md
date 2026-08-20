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
- **0.8.0（进行中）** 通用 p2p 模块抽离（`source/p2p/`：identity/contacts/discovery/mdns_stealth），聊天变薄

## 后续规划

### Phase B：通用传输 API（`P2pNode` + `P2pEvent`）
- 把 `build_swarm`、连接建立/关闭、地址簿、重连、`on_peer_discovered` 收进 `P2pNode`
- 事件通道解耦 `run_node` 的 select 循环（stdin / 心跳 / 事件流）
- 聊天成为第一个消费方，行为不变（e2e 守护）

### Phase C：协议与传输解耦
- 消息改**通用 Frame 信封**：`Frame { protocol, payload }`，聊天/文件传输各自编解码，新协议不改传输层
- 心跳（保活）进传输层，对全部已连接 peer；上线/离线通知进传输层
  （`PeerConnected/PeerDisconnected`），名字握手留协议层

### 文件传输（`/file/1.0.0`）
- 复用身份验证 + 联系人簿：发送前校验接收方是**已验证联系人**（"发给谁"由传输层保证）
- 分块 / 进度 / 校验和（哈希）；`ChatPayload::Binary` 变体已预留
- UX 改进（后期）：二维码互扫、联系人选择器、邀请码，降低首接指纹确认的繁琐
### 多点通讯

- **传输层已是多点**：libp2p 同时持有多条认证连接，`send(peer, frame)` 按 peer 寻址；
  当前 1v1 是应用层 `active: Option<PeerId>` 的"塌缩"
- **并发多会话**（✅ 0.9.0）：`active` → `HashMap<PeerId, Conversation>` + `focused`；
  `/chat` 切换焦点、非焦点来信带名、重连队列、/q 全会话 Bye
- **群聊**（N→M，下一轮）：gossipsub（`MessageAuthenticity::Signed`）+ 本地群注册表
  （成员须已验证联系人）+ 1v1 邀请入群；/group 命令、`/chat/5.0.0`

### 公网（M6）
- relay 中继（circuit v2）+ dcutr UDP 打洞
- 身份/TOFU/加密已就绪，可平滑上公网；发现层从 mDNS 换成 relay/rendezvous

### 其他
- 修复 stdin 预读冲突（主菜单 `std::io::stdin()` vs 聊天 `tokio::io::stdin()`，
  管道一次性喂完整输入时跨会话重进聊天丢行；TTY 与 e2e 增量写入不受影响）
- 联系人黑名单 / 群组管理 / 消息历史持久化
