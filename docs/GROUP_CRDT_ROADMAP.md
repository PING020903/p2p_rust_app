# 群成员名单 CRDT 路线图（未来版本方向）

> 本文档记录"群成员名单去中心化"的未来演进方向（OR-Set CRDT）。
> 当前（0.15.0）仍是**群主为中心的单写者模型**：仅群主可加人、群主不在线禁止退群、
> 群主退群顺位转移。CRDT 是把名单从"单点权威"升级为"多写者收敛"的学术方案。

## 动机（要解决的痛点）

| 痛点 | 现状（0.15.0） | CRDT 后 |
|---|---|---|
| 群主离线，成员退群 | 禁止退群（单写者一致性优先） | ✅ 成员本地 remove + 传播即收敛 |
| 群主离线，加人 | 禁止（仅群主可加） | 视策略：仍可仅群主（策略 A）或任何成员（策略 B） |
| 退群者被群主 stale 名单"复活"（幽灵） | 靠"群主不在线禁退 + 去重兜底"防 | ✅ add-wins 语义天然无幽灵 |
| 成员退了又重进 | 需群主重新邀请，有歧义 | ✅ 新 add uid 进，旧 tombstone 不挡 |
| 并发加人/退群 | version 门控可能冲突 | ✅ CRDT 合并收敛 |

## 模型

- **选型**：add-wins OR-Set（成员集合）
- **加人（add）**：策略 A（仅群主，单写者，uid 由群主单调生成）→ 可演进策略 B
  （任何已验证成员，uid 用 `(inviter, seq)` 保证唯一）
- **退群（remove）**：仅本人自退（保留"群主不能踢人"）；remove = tombstone 自己的 add uid
- **有效成员** = add_set 中 uid 不在 tombstone 的 peer；**合并** = add_set 并集 + tombstone 并集（幂等）

## 存储（含迁移）

- [ ] `Group.members: Vec<String>` → `add_set + tombstone`（结构破坏性变更）
  ```rust
  struct Group {
      id: String, name: String,
      add_set: HashMap<String, Vec<AddUid>>,   // peer → 每个 add 的 uid
      tombstone: HashSet<u64>,                 // 被删除的 add uid
      creator: String,        // 仅决定"谁可邀请"，不再做名单权威
      resident: bool,
      version: u64,           // 降级为"快照新鲜度"
  }
  struct AddUid { uid: u64, added_by: String }
  ```
- [ ] 群文件加 `format=2`（现为裸 `Vec<Group>` 数组，需 `{format, groups}` 包装或字段判定）
- [ ] 迁移器：旧 `members` 逐个分配 uid——**uid 须确定性全网一致**（如 `fnv(group_id + peer_id)`），
      否则不同节点对同一成员算不同 uid → remove 对不上
- [ ] 新 add 的 uid 用计数器/随机，须与迁移 uid 空间不碰撞
- [ ] tombstone 单调增长不回收（可做周期性压缩，但压缩引入短暂幽灵窗口，需权衡）
- [ ] 持久化敏感性：tombstone 丢失会复活幽灵——比单写者模型更依赖完整性

## 协议

- [ ] `/chat/7.0.0` → `/chat/8.0.0`（成员名单语义结构变化，跨版本不互通）
- [ ] `AppPayload` 新增变体（cbor 末尾追加，向后兼容旧节点忽略）：
  - `GroupAdd { group_id, add: Vec<(peer, uid, added_by)> }`
  - `GroupRemove { group_id, remove: Vec<uid> }`
  - `GroupSnapshot { group_id, add_set, tombstone }`（入群/重连拉全量）
- [ ] 传播通道仍走 1v1 request-response（可靠送达；gossip 只做聊天文本，小群转发放大无确认）
- [ ] `version` 从"权威门控"降级为"快照新鲜度"（判断新于本地）

## 转移/邀请权（CRDT 之外仍须保留）

- CRDT 只解决**名单收敛**，不解决"谁可邀请"的信任策略
- [ ] 保留群主转移机制（0.15.0 的 `/group leave` 顺位转移）作为邀请权底座
- [ ] 策略 A→B 演进时：邀请由"群主"扩为"任何已验证成员"，信任由邀请者背书

## 里程碑拆解

- [ ] **M1**：存储迁移（`format=2` + 确定性 uid 迁移器）+ 单测
- [ ] **M2**：协议变体 `GroupAdd/GroupRemove/GroupSnapshot` + 合并器（并集、幂等）
- [ ] **M3**：命令接入（`/group add` 走 add op、`/group leave` 走 remove op）
- [ ] **M4**：e2e 多节点并发加/退收敛测试 + 离线收敛测试 + 迁移后数据一致性
- [ ] **M5**（可选）：策略 B（任何成员可邀请）；tombstone 压缩

## 参考

- Shapiro 等《A comprehensive study of CRDTs》(2011)：OR-Set / add-wins 语义
- Bieniusa 等 (2012)：add-wins 偏置消除歧义
- Birman 虚拟同步（ISIS/Horus）：一致性群视图（备选，对 LAN 小群过重）
- Raft / PBFT：领导者选举（单主模型的容错补丁方向，与 CRDT 是两条路）
