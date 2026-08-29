# P2P_RUST_APP

这是一个 Rust 的 P2P 学习项目：从菜单式命令行工具起步，逐步演进到基于 libp2p 的 P2P 实时通讯。

如果你也在学 Rust 或 P2P，欢迎参考、交流。

> **初心与愿景**：本项目服务于一个更宏大的去中心化平台构想，初心与蓝图见 [wish/](wish/) 目录下的文档。

> **声明**：本项目使用 [opencode](https://opencode.ai) 搭配 AI API 生成。

---

## 小白食用指南

### 第一步：安装 Rust

到 [rustup.rs](https://rustup.rs) 下载 rustup 并安装（Windows 上需先装 Visual Studio C++ 生成工具），装完确认：

```bash
rustc --version
cargo --version
```

### 第二步：编译与运行

```bash
cargo run
```

首次编译需要几分钟（libp2p 依赖较多），之后秒开。启动后进入主菜单：

```
=== 主菜单 ===
  1. 计算器
  2. 学生信息管理
  3. 彩色打印演示
  4. P2P 聊天
  q. 退出
```

任何模组里输入 `help` 可查看该模组的命令列表。完整联机测试步骤见 [docs/新手测试指南.md](docs/新手测试指南.md)。

### 第三步：逐个模组体验

**1. 计算器** — 直接输入表达式回车即算（支持 `+ - * /`，先乘除后加减），如 `1+2*3`。输入 `q` 退出。

**2. 学生信息管理** — 命令：`add`（交互式录入）/ `list` / `find` / `status` / `quit`。

**3. 彩色打印演示** — 展示分级日志着色。

**4. P2P 聊天** — 本项目的主角，玩法见下节。

### 第四步：P2P 聊天怎么玩

需要两个终端（同一台机器开两个窗口即可，有条件最好用局域网内两台机器）：

1. 两个终端分别 `cargo run`，都选 `4`，进入**身份登录**菜单：
   - 新用户：选 `0` 新身份登录，填资料（姓名/生日/性别）后系统生成**12 词助记词**并展示一次，须抄下并回输前 3 词确认；再设密码（8~128 字节）。助记词是**唯一备份**：丢失即永久丢失身份，泄露即身份被窃取
   - 已有身份（同机再次登录）：选缓存身份序号，**只输密码**解锁（密码不回显）
   - 换机器 / 备份恢复：选 `r`，输入助记词后重新设置本机密码
   - 同一助记词 → 同一个角色 ID：跨重启、跨机器节点 ID 都不变
2. 登录后 mDNS 自动登记局域网内的节点（只登记、不自动连接），用 `/list` 查看已登记节点与状态
3. 想和谁聊，输入 `/chat <对方完整角色名 或 完整节点ID>`（对方角色名未知时，先从 `/list` 复制其完整节点 ID）
   - 也可用 `/dial <对方完整监听地址>` 直连；输入 `/dial` 直接回车可查看地址格式模板
4. 连上后直接打字就是聊天消息（发给**当前焦点会话**）；`/help` 查看命令；`/quit` 退出（会向所有已连接会话发送下线通知）
5. **多会话（微信式 1v1）**：可与多个联系人同时保持连接，`/chat <角色>` 随时切换焦点
   - 焦点会话来信显示 `[对方]`，非焦点会话来信显示 `[对方名]`；发送回显 `[我 -> 名]`
   - `/list` 状态列 `[当前会话]` 标记当前焦点；焦点断开会提示重新选择
6. **对称信任**：互信 = 我信任你 **且** 你信任我，**任一方不信任，整条链路就不互信**
   - 首次连接时核对对方身份指纹（SSH 式短指纹 + 节点 ID），确认后记录为联系人
   - `/trust <名>` 标记信任、`/trust !<名>` 取消信任（取消后双向消息互被丢弃，重新 `/trust` 恢复）
   - `/list` 显示信任状态：`[互信]` / `[我信任/对方未确认]` / `[未信任]`
   - **未互信的业务信号默认被丢弃**（L2 未互信钩子，可按 tag 定制）
7. **文件传输**：`/send <角色|节点ID> <路径>`（须**互信**）——事件驱动分块（1 MiB）推送，逐块 CRC32 校验，接收方落盘到下载目录并校验
   - 下载目录默认用户 `Downloads`，`/download-dir <路径>` 可改（持久化）；`/send` 传大文件也安全（停等 ack，天然背压）
8. **发现模式**（隐私）：`/discover advertise|stealth|off` 切换并持久化，下次进入聊天生效
   - `advertise`（默认）广播自己并发现他人；`stealth`（隐身）**只收不发**——能发现别人，但局域网内别人看不到你的在线状态/身份；`off` 彻底关闭 mDNS，仅用 `/dial` 手动直连
9. **群聊**（微信式）：`/group new <群名>` 建群 → `/group add <群名> <角色>` 拉人（须已验证联系人，对方收到邀请自动入群）→ `/group <群名>` 聚焦后输入即发群里
   - 群消息经 gossipsub 签名分发：焦点群显示 `[成员名]`，非焦点群显示 `[群名] [成员名]`
   - **群成员以群主为中心**：仅群主可邀请；成员退群（`/group leave <群名>`）通知群主划去；群主退群一步顺位转移
   - **常驻接收**（防通讯风暴）：`/group resident <群名> on|off` —— 常驻群成员上线自动连接维持接收
10. **IPv6 直连**：启动时打印 `全局IPv6直连地址`（跨城市可分享，对方 `/dial` 即连，需路由器放行端口；**已实现，跨城市真实双端实测待验证**）；`/listen` 随时重打
11. **终端逃逸**：`cmd/<命令>`（cmd）、`ps/<命令>`（PowerShell）、`sh/<命令>`（POSIX sh）绕过应用直控当前终端（如 `cmd/cls` 或 `sh/clear` 清屏）
12. `/backup` 可随时重新查看本身份助记词（需输入密码）

> 身份安全模型：身份 = 随机 128 bit 种子派生（BIP39 助记词），密码只用于加密本地 keystore，
> 不再从姓名/生日/性别派生。知道你的个人信息也无法冒充你；密码可以随时更换。
> 节点识别采用 TOFU：首次接触记录指纹，之后凭指纹识别，防身份切换/冒充。
>
> 三层架构：L3 业务（chat/file）→ L2（seam 传输适配 + 身份/信任服务）→ L1 传输层（node），
> 严格分层：L3 只见 tag+payload，不接触 L1 的 Frame/control（见 [docs/PROJECT_ARCHITECTURE.md](docs/PROJECT_ARCHITECTURE.md)）。
> 跨局域网方案（IPv6 直连优先 + 国内 relay 兜底）：[docs/CROSS_LAN_CONNECTIVITY.md](docs/CROSS_LAN_CONNECTIVITY.md)。

### 运行测试

**逻辑测试（默认，功能正确性）**：

```bash
cargo test --bin p2p_rust_app                     # 单元测试
cargo test --test p2p_chat -- --test-threads=1    # 逻辑 e2e（串行：同机 mDNS 会跨测试干扰）
```

**稳定性测试（显式声明，重复上下线/掉线/异常）**：

```bash
cargo test --test p2p_chat_stability -- --ignored --test-threads=1
```

e2e 的登录凭据从 `tests/users.txt` 读取（首次运行前复制 `tests/users.template.txt` 创建，该文件不入库）。

---

## 开发状态与进度

### 模组总览

| 模组 | 状态 | 说明 |
|------|------|------|
| 计算器 | ✅ 完成 | 四则运算，先乘除后加减 |
| 学生信息管理 | ✅ 完成 | 增/查/列表/状态 |
| 彩色打印 | ✅ 完成 | 分级日志着色 + 调试宏 |
| 指令树组件 `cmd_tree` | ✅ 完成 | 由 C 版 simpleCmd cmdTree v2.0 移植：树形路由、最深命中、双引号分词、多实例、help、dataHandler |
| 通用 P2P 层 `source/p2p/` | ✅ 完成 | 身份/联系人/发现/隐身监听/传输适配（seam）/设置，与聊天协议解耦，多应用复用 |
| P2P 聊天（libp2p） | ✅ 局域网可用 | 三层架构（L3→L2→L1）；TCP + Noise + Yamux；mDNS 发现 + 按需 1v1 多会话；群聊（gossipsub）；心跳/重连；对称信任（互信门禁）；IPv6 直连 |
| 文件传输 | ✅ 完成 | `/send` 事件驱动分块推送 + 逐块 CRC32 + 落盘校验；下载目录可配置（默认用户 Downloads） |
| 公网 P2P | 🕐 规划 | 方案已定：IPv6 直连优先 + 国内可达 relay 兜底（见 CROSS_LAN_CONNECTIVITY.md），尚未实现 |

### P2P 聊天里程碑

| 里程碑 | 内容 | 状态 |
|--------|------|------|
| M0-M4 | 依赖/最小节点/拨号发现/request-response/交互循环 | ✅ |
| M5 | 健壮性（心跳包活 + 超时判离线 + Hello/Bye + 自动重连） | ✅ |
| M6 | 跨局域网（IPv6 直连优先 + relay 兜底；IPv4 打洞跳过） | 🕐 规划 |
| M7 | 多用户角色身份与按需 1v1 | ✅ 助记词派生身份 + Hello 花名册 + /list + /chat |
| 0.14 | 三层架构：L1 传输任务 + L2 身份服务 + L3 业务；根治应用阻塞导致的心跳冻结 | ✅ |
| 0.15 | 群成员一致性加固（群主不在线禁止退群 + 顺位转移 + 去重） | ✅ |
| 0.16 | 1v1 信任管理完善 + 群主可见 + 转移自愈 + 邀请重发 | ✅ |
| 0.17-0.18 | 语义注册表 + 帧分发内核化（Frame.text 统一字符串标签，查表分发） | ✅ |
| 0.19 | 文件传输（事件驱动 /send + 逐块 CRC32）+ 可配置下载目录 | ✅ |
| 0.20 | 对称信任（L2 互信门控，任一方取消整链断）+ cmd//ps/ 终端逃逸 + 测试拆分 | ✅ |
| 0.21 | 严格 L3→L2→L1 分层（seam 适配层）+ L2 内化 TextTag + 未互信钩子 | ✅ |
| 远景 | 嵌入式主机配对 + 固件传输升级 | 🕐 构想 |

### 测试现状

- **单元测试 42 项**：指令树 / 拨号地址解析 / 身份与 keystore / 资料归一 / 指纹与联系人簿（含对称互信判定）/ mDNS 解析与发现模式 / seam 注册表路由（正常 vs 未互信）/ TextTag 往返 / 文件传输（sanitize + CRC32 向量）
- **逻辑 e2e（tests/p2p_chat.rs，9 场景 + 2 standalone）**：基础聊天 / 按角色名呼叫 / 隐身发现模式 / 三节点 1v1 多会话 / 三节点群聊 / 信任管理+联系人名解析 / **对称信任**（互信→单方取消双向丢弃→恢复）/ **未互信钩子边界**（A 注册显示、B 未注册丢弃）/ 文件传输 / IPv6 自连
- **稳定性 e2e（tests/p2p_chat_stability.rs，6 场景，`--ignored` 显式）**：主动上下线循环 ×15 / kill 掉线循环 ×5 / 身份缓存回环（错密码校验）/ 同 ID 冲突拒绝 / 应用阻塞心跳仍存活 / 群主离线退群被拒 + 顺位转移
- e2e 登录凭据从 `tests/users.txt` 读取（首次运行前复制 `tests/users.template.txt` 创建，该文件不入库；user1/2/3 名字互不相同）；测试身份用 BIP39 官方测试向量助记词（代码内常量，仅测试用）

---

## 项目结构

```
source/
  main.rs         主菜单（CmdTree 驱动）
  calculator.rs   计算器
  student.rs      学生信息管理
  color_print.rs  彩色打印与调试宏
  cmd_tree.rs     指令树组件（多实例、help、dataHandler）
  chat.rs         L3 聊天应用（消费方：指令树/会话/群/信任 UI + SignalRegistry 注册）
  file_transfer.rs L3 文件传输应用（file.* tag，事件驱动分块）
  p2p/            通用 P2P 层（L1 + L2）
    identity.rs     L2 助记词↔Ed25519、keystore 加解密、影子探测
    contacts.rs     L2 TOFU 联系人簿 + 对称互信（verified + their_trust）
    identity_service.rs L2 身份服务（登录/TOFU/信任判定 + TextTag 内化信号）
    discovery.rs     L2 发现模式（advertise/stealth/off）
    mdns_stealth.rs  L2 隐身模式最小 mDNS 监听器
    seam.rs          L2 传输适配层（Cmd/Event + SignalRegistry + spawn_transport 适配任务）
    settings.rs      L2 per-identity 设置（download_dir 等）
    node.rs          L1 传输层（Frame/Control/P2pCommand/P2pEvent/P2pNode，pub(crate) 内部化）
tests/
  common/mod.rs     e2e 共享脚手架
  p2p_chat.rs       逻辑 e2e（8 场景 + 2 standalone）
  p2p_chat_stability.rs 稳定性 e2e（#[ignore]）
docs/
  PROJECT_ARCHITECTURE.md  工程架构（分层 + 数据流）
  CROSS_LAN_CONNECTIVITY.md 跨局域网联通方案
  新手测试指南.md          小白联机测试手册
  ROADMAP.md               演进路线图
  GROUP_CRDT_ROADMAP.md    群成员名单去中心化方向
xtask/
  构建/发布辅助工具（cargo xtask build）
wish/             初心与愿景文档（去中心化平台蓝图）
```

## 技术栈

| 依赖 | 用途 |
|------|------|
| libp2p | P2P 网络栈（TCP/Noise/Yamux/mDNS/request-response/gossipsub） |
| tokio | 异步运行时 |
| serde + cbor | 消息序列化 |
| bip39 | 身份助记词（BIP39 标准） |
| chacha20poly1305 | keystore 身份加密（AEAD） |
| argon2 | keystore 密码 KDF |
| crc32fast | 文件分块校验（CRC32） |
| socket2 | 隐身模式 mDNS 组播监听（UDP） |
| fnv | 指令树子节点查找（FNV-1a hash） |
| colored | 终端着色 |

## 许可证

[MIT](LICENSE)
