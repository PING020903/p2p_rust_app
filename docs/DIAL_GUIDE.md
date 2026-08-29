# `/dial` 使用与原理（及"为什么有些场景发现不到"）

> 本文档说明 `/dial` 命令如何工作、地址格式、取地址途径，以及为什么在
> **mDNS 发现不到**的场景下 `/dial` 依然能连通（重点：Windows ↔ WSL 同机互连）。

---

## 一、地址格式（multiaddr）

要连对方，需要一条**完整的监听地址**，形如：

```
/ip4/<IPv4地址>/tcp/<端口>/p2p/<节点ID>
/ip6/<IPv6地址>/tcp/<端口>/p2p/<节点ID>
```

| 段 | 含义 | 示例 |
|---|---|---|
| `/ip4/<IP>` / `/ip6/<IP>` | 对方监听的 IP（局域网或全局 IPv6） | `/ip4/192.168.1.46` |
| `/tcp/<端口>` | 对方监听的 TCP 端口（0-65535） | `/tcp/51322` |
| `/p2p/<节点ID>` | 对方身份（`12D3KooW` 开头），Noise 握手校验 | `/p2p/12D3KooW...` |

> 完整地址会由应用启动时打印（`监听地址:` 行），或用 `/listen` 随时重打；
> 输入 `/dial` 直接回车可查看地址格式模板。

## 二、`/dial` 做了什么（调用链）

```
/dial <地址>
  → chat.rs parse_dial_addr：校验格式（IP 合法 / 端口范围 / /p2p/ 节点ID）
  → 登记进 registered（peer → 地址列表，供 /list 与重连使用）
  → seam::Cmd::Dial → L1 swarm.dial(addr)
  → TCP 连接 → Noise 握手（校验对端公钥 == /p2p/<节点ID>）→ Yamux 复用
  → 连接建立 → 应用侧发 hello、显示"已连接对端"
```

- **拨号即直达**：`/dial` 拿的是**完整可路由地址**，不需要任何"发现"过程——对方 IP 能通、端口能进、节点ID 对得上，就连上。
- **重连**：登记过的地址会存进地址簿，断线后 L1 自动按已知地址逐个重拨（`重连`逻辑）。

## 三、怎么拿到对方的地址

| 途径 | 适用 | 说明 |
|---|---|---|
| 启动打印 `监听地址:` | 任何场景 | 每个监听地址一行，含 `/p2p/<节点ID>` |
| `/listen` | 随时重打 | IPv6 前缀/端口变化后重新获取 |
| `mDNS 发现` + `/list` | 同一局域网 | 自动登记，但见下方"发现不到的常见场景" |

## 四、为什么有些场景 mDNS 发现不到，但 `/dial` 能连

mDNS 依赖**组播广播**，而 `/dial` 依赖**明确地址**——两者网络前提不同。以下场景 mDNS 失效但 `/dial` 正常：

### 1. Windows ↔ WSL 同机互连（最常见）

- **现象**：Windows 版应用与 WSL 里的 Linux 版应用互相 `/list` 看不到对方。
- **根因**：WSL2 虚拟网络命名空间**不透传 mDNS 组播**（`224.0.0.251:5353`）。即便 `.wslconfig` 配了 `networkingMode=mirrored`（WSL 共享主机 IP），宿主与 WSL 虚拟机之间的组播投递仍被虚拟化边界隔离——应用侧 socket/组成员/防火墙全正确也无效，属 WSL2 限制。
- **解决**：mirrored 模式下两应用**同 IP、不同端口**，手动互贴地址 `/dial` 即连：
  ```
  Windows 端：/ip4/192.168.1.46/tcp/<win端口>/p2p/<win_id>
  WSL 端：    /ip4/192.168.1.46/tcp/<wsl端口>/p2p/<wsl_id>
  ```
  各自用 `/listen` 拿到自己的地址发给对方，互相 `/dial`。

### 2. 同一主机多个实例

- **现象**：同机跑两个实例，mDNS 可能互相干扰/不发现（测试套件因此用 `P2P_DISCOVERY=off` + 手动拨号）。
- **根因**：同一 IP 上多个 mDNS responder 竞争组播套接字/成员。
- **解决**：手动 `/dial`（用对方监听地址，同 IP 不同端口）。

### 3. 发现模式 stealth / off

- `/discover stealth` 只收不发、`/discover off` 彻底关 mDNS——对方自然发现不到你。
- `/dial` 不受影响（直达地址），随时可用。

### 4. 跨局域网（不同路由器之后）

- **现象**：两个不同局域网/城市的节点互相看不到。
- **根因**：mDNS 组播**不跨路由器**（路由器不转发组播广播）；且 NAT 后无公网入站地址。
- **解决**：跨局域网方案见 `docs/CROSS_LAN_CONNECTIVITY.md`（IPv6 直连优先 + relay 兜底）；临时可用 `/dial` 对方**全局 IPv6** 地址（需对方路由器放行端口）。

### 5. 入站被防火墙挡

- **现象**：`/dial` 拨号失败/超时，但地址与节点ID 无误。
- **根因**：被拨**一方的入站**被拦（Windows 防火墙 / 路由器 NAT 未放行端口 / WSL Hyper-V 防火墙），SYN 进不来。
- **解决**：放行被拨方的端口（Windows 防火墙允许应用入站；IPv6 需路由器放行；排障用 `Test-NetConnection <IP> -Port <端口>`）。

## 五、排障对照

| 现象 | 可能原因 | 处置 |
|---|---|---|
| `/dial` 报"地址无效" | 格式/端口/节点ID 不对 | 用对方 `/listen` 的完整行，别手敲 |
| 拨号超时/无响应 | 被拨方入站被防火墙挡；或地址是旧端口 | 被拨方放行端口；重 `/listen` 拿新地址；对照下方错误码 |
| mDNS 看不到对方 | WSL↔Windows / 同机多实例 / stealth·off / 跨局域网 | 用 `/dial` 直达；跨局域网见 CROSS_LAN 文档 |
| 连上但消息发不出 | 未互信（对称信任门禁） | 双方 `/trust` 建立互信 |

## 六、常见 socket 错误码（Windows / Linux）

> `os error NNNNN` 的 **NNNNN 是操作系统内核（Windows Winsock / Linux errno）的错误码**，
> libp2p 只是把它包装搬运上来（见"认清来源"）。查错按**码语义**，别记平台数字。

| 含义 | Windows (Winsock) | Linux (errno) | 常见触发 |
|---|---|---|---|
| 连接被拒 | 10061 (WSAECONNREFUSED) | 111 (ECONNREFUSED) | 对方端口没监听 / 防火墙 RST |
| 地址/端口占用 | 10048 (WSAEADDRINUSE) | 98 (EADDRINUSE) | 本地源端口复用冲突（同机多实例） |
| 网络不可达 | 10051 (WSAENETUNREACH) | 101 (ENETUNREACH) | 无路由（拨 fe80 / 无全局 IPv6） |
| 连接超时 | 10060 (WSAETIMEDOUT) | 110 (ETIMEDOUT) | 防火墙 drop SYN（无响应） |
| 连接被重置 | 10054 (WSAECONNRESET) | 104 (ECONNRESET) | 对方崩溃 / 防火墙 RST |
| 地址不可用 | 10049 (WSAEADDRNOTAVAIL) | 99 (EADDRNOTAVAIL) | 本地地址不存在 / 未分配 |
| 主机不可达 | 10065 (WSAEHOSTUNREACH) | 113 (EHOSTUNREACH) | 有路由但目标主机不可达 |
| 权限拒绝 | 10013 (WSAEACCES) | 13 (EACCES) | 绑定特权端口 / 防火墙拦截 |
| 资源耗尽 | 10055 (WSAENOBUFS) | 105 (ENOBUFS) | 缓冲区 / 句柄耗尽 |

**编码规则提示**：早期经典 errno（如 `EACCES`=13）Winsock 是 `10000+errno`（10013）；
网络相关码**分叉**（`ECONNREFUSED`：Windows 10061 vs Linux 111），不能简单加减换算。

**如何查码含义**：

| 平台 | 命令 |
|---|---|
| Windows | `net helpmsg 10061` |
| Linux | `errno 111` 或 `man errno`（/usr/include/errno.h） |
| 通用 | 搜 "WSAEADDRINUSE 10048" / "errno 98 EADDRINUSE" |

## 相关

- `docs/新手测试指南.md` — 小白联机测试手册（含 `/dial` 上手）
- `docs/CROSS_LAN_CONNECTIVITY.md` — 跨局域网联通方案
- `docs/PROJECT_ARCHITECTURE.md` — 三层架构（`/dial` 走 seam::Cmd::Dial → L1）
