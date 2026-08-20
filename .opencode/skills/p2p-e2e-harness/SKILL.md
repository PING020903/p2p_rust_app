---
name: p2p-e2e-harness
description: 本项目双节点端到端测试脚手架用法。Use when editing, adding, or debugging tests/p2p_chat.rs or any end-to-end scenario — Node::spawn / spawn_with, login_restore / login_cached, wait_for / wait_for_optional, scenario cache dirs, deterministic test mnemonics. Covers the conventions that keep the serial e2e suite robust on one machine.
---

# 双节点 e2e 测试脚手架（本项目）

文件：`tests/p2p_chat.rs`。串行 7 场景（`p2p_chat_e2e_suite`）——**不要拆成并行 `#[test]`**，
同机 mDNS 会跨测试互相发现导致连错对象。

## 1. 节点启动与环境变量

```rust
Node::spawn(bin, cache_dir)                       // 默认 advertise 发现模式
Node::spawn_with(bin, cache_dir, "stealth")       // 指定发现模式
```

- 依赖 `env!("CARGO_BIN_EXE_p2p_rust_app")`
- spawn 自动设置：`P2P_ID_CACHE_DIR`（缓存目录）、`P2P_ID_PROBE_SECS=2`（加速影子探测）、
  `P2P_DISCOVERY`（发现模式，测试用覆盖）
- **stdout 与 stderr 双捕获**，行前缀 `[out]` / `[err]`——错误提示（eprintln）也能断言

## 2. 确定性测试身份

- BIP39 **官方测试向量**助记词常量（勿用于生产）：
  - `MNEMONIC_USER1 = "abandon abandon ... abandon about"`
  - `MNEMONIC_USER2 = "legal winner thank year wave sausage worth useful legal winner thank yellow"`
- 同一助记词 → 同一 PeerId（场景内跨重启/重进身份不变）
- 资料（姓名/生日/性别/密码）从 `tests/users.txt` 读取（git 忽略，模板 `users.template.txt`）

## 3. 登录驱动与断言

```rust
login_restore(&mut node, &creds, MNEMONIC_USER1)  // 走 "r" 恢复路径：r→助记词→资料→密码
login_cached(&mut node, &creds)                   // 走缓存：进入聊天→选 "1"→输密码
```

- `wait_for(needle, timeout)`：读到含 needle 的行返回，超时 panic
- `wait_for_optional(needle, timeout)`：返回 `Option<String>`，用于断言"不应出现"
  （如隐身节点不被对端发现）
- `spawn_into_chat(bin, cache_dir, creds, mnemonic)`：启动+登录+等 127.0.0.1 监听行

## 4. 场景约定

- **每节点独立缓存目录**：`scenario_cache_dir("s1_a")` / `"s1_b"` ——保证登录菜单
  只有一个缓存身份，选 "1" 无歧义；也隔离 keystore / 联系人簿 / settings
- 断言尽量用 peer_id 锚定（`parse_peer_id`），防 mDNS 串扰误判
- 场景末尾 `a.kill(); b.kill();` 清理进程
- mDNS 组播有延迟/抖动，发现类断言给足超时（40s），"不应出现"断言用 8s 观察窗口

## 5. 新增场景套路

1. `let cache_x = scenario_cache_dir("sN_a");` 每节点一个
2. `Node::spawn` / `spawn_with`（需要特殊发现模式时）→ 等主菜单 → `login_restore`
3. 交互操作（`/dial`、`/chat`、`/list`、`/q`、消息）→ `wait_for` 逐步断言
4. 在 `p2p_chat_e2e_suite` 末尾追加调用
5. 回归：改动 `source/chat.rs` 或 `source/p2p/` 后必须重跑整个 e2e（约 2.5 分钟）
