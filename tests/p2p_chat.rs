//! 逻辑测试（默认运行）：功能正确性场景——基础聊天 / 按名呼叫 / 多会话 / 群聊 / 发现模式 /
//! 信任管理 / 对称信任 / 文件传输 / IPv6 自连。稳定性场景见 `p2p_chat_stability.rs`。
//!
//! 运行：`cargo test --test p2p_chat -- --test-threads=1`
//! （串行必须：同机 mDNS 会跨测试互相发现，拆并行会连错对象）

mod common;
use common::*;
use std::thread;
use std::time::Duration;

/// 场景1：基础聊天——登录、连接、带名字的 Hello、双向收发、12 秒静默保活、Bye 优雅退出
fn basic_chat_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    let cache_a = scenario_cache_dir("s1_a");
    let cache_b = scenario_cache_dir("s1_b");
    let (cred_a, cred_b) = load_creds();
    println!("=== 场景1: 基础聊天 ===");

    println!("=== 启动节点 A ===");
    let (mut a, a_listen) = spawn_into_chat(bin, &cache_a, &cred_a, MNEMONIC_USER1);
    let a_addr = listen_addr(&a_listen);
    let a_id = parse_peer_id(&a_listen);

    println!("=== 启动节点 B ===");
    let (mut b, b_listen) = spawn_into_chat(bin, &cache_b, &cred_b, MNEMONIC_USER2);
    let b_id = parse_peer_id(&b_listen);
    b.send(&format!("/dial {a_addr}"));

    a.wait_for(&format!("已连接对端: {b_id}"), WAIT);
    b.wait_for(&format!("已连接对端: {a_id}"), WAIT);

    println!("=== 上线通知（Hello 携带角色名）===");
    a.wait_for(&format!("对方已上线: {}", cred_b.name), WAIT);
    b.wait_for(&format!("对方已上线: {}", cred_a.name), WAIT);
    wait_mutual_trust(&a, &cred_a.name, &b, &cred_b.name);

    println!("=== B -> A 发消息 ===");
    b.send("你好，我是节点B");
    a.wait_for("[对方] 你好，我是节点B", WAIT);

    println!("=== A -> B 回消息 ===");
    a.send("收到，我是节点A");
    b.wait_for("[对方] 收到，我是节点A", WAIT);

    println!("=== 静默 12 秒（超过 10s 空闲超时），验证心跳保活 ===");
    thread::sleep(Duration::from_secs(12));
    a.send("心跳保活后的消息");
    b.wait_for("[对方] 心跳保活后的消息", WAIT);

    println!("=== B 优雅退出，验证 Bye 通知 ===");
    b.send("/q");
    a.wait_for("对方已正常退出", WAIT);

    a.kill();
    b.kill();
}

/// 场景2：B 退出后重新登录（同一凭据身份不变），A 立即按角色名呼叫（待接呼叫），
/// 并回归验证 /list 地址数不随上下线循环累积
fn chat_by_name_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    let cache_a = scenario_cache_dir("s2_a");
    let cache_b = scenario_cache_dir("s2_b");
    let (cred_a, cred_b) = load_creds();
    println!("=== 场景2: 按角色名呼叫 ===");

    let (mut a, a_listen) = spawn_into_chat(bin, &cache_a, &cred_a, MNEMONIC_USER1);
    let a_addr = listen_addr(&a_listen);
    let a_id = parse_peer_id(&a_listen);

    let (mut b, b_listen) = spawn_into_chat(bin, &cache_b, &cred_b, MNEMONIC_USER2);
    let b_id = parse_peer_id(&b_listen);
    b.send(&format!("/dial {a_addr}"));
    b.wait_for(&format!("已连接对端: {a_id}"), WAIT);
    a.wait_for(&format!("对方已上线: {}", cred_b.name), WAIT);

    println!("=== B 优雅退出后重新登录，A 立即按角色名呼叫（待接呼叫）===");
    b.send("/q");
    a.wait_for("对方已正常退出", WAIT);
    enter_chat(&mut b, &cred_b);

    a.send(&format!("/chat {}", cred_b.name));
    // 待接呼叫依赖 A 的 mDNS 重新发现 B 的新地址，放宽超时抗时序抖动
    a.wait_for(&format!("已连接对端: {b_id}"), Duration::from_secs(40));
    b.wait_for(&format!("已连接对端: {a_id}"), WAIT);
    b.wait_for(&format!("对方已上线: {}", cred_a.name), WAIT);
    wait_mutual_trust(&a, &cred_a.name, &b, &cred_b.name);

    a.send("按名呼叫后的消息");
    b.wait_for("[对方] 按名呼叫后的消息", WAIT);

    println!("=== /list 地址簿回归：地址数不得累积膨胀 ===");
    a.send("/list");
    a.wait_for("=== 已登记节点 ===", WAIT);
    let list_line = a.wait_for(&b_id, WAIT);
    let addr_n: usize = list_line
        .split("地址数")
        .nth(1)
        .expect("/list 行缺少地址数")
        .trim()
        .parse()
        .expect("地址数不是数字");
    assert!(addr_n <= 4, "地址簿膨胀: 地址数 {addr_n} > 4");

    a.kill();
    b.kill();
}

/// 场景7：发现模式——隐身节点只收不发：能发现别人但自己不广播（对端看不到它）
fn discovery_mode_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    let cache_a = scenario_cache_dir("s7_a");
    let cache_b = scenario_cache_dir("s7_b");
    let (cred_a, cred_b) = load_creds();
    println!("=== 场景7: 隐身发现模式 ===");

    println!("=== 启动隐身节点 A ===");
    let mut a = Node::spawn_with(bin, &cache_a, "stealth");
    a.wait_for("=== 主菜单 ===", Duration::from_secs(10));
    login_restore(&mut a, &cred_a, MNEMONIC_USER1);
    let a_listen = a.wait_for("监听地址: /ip4/127.0.0.1", Duration::from_secs(20));
    let a_id = parse_peer_id(&a_listen);

    println!("=== 启动广播节点 B ===");
    let (mut b, b_listen) = spawn_into_chat(bin, &cache_b, &cred_b, MNEMONIC_USER2);
    let b_id = parse_peer_id(&b_listen);
    let b_addr = listen_addr(&b_listen);

    println!("=== 隐身节点 A 应通过监听发现 B（libp2p-mdns 周期性组播自查自答）===");
    a.send("/list");
    a.wait_for(&b_id, Duration::from_secs(40));

    println!("=== 广播节点 B 不应发现隐身节点 A（A 不广播）===");
    b.send("/list");
    let absent = b.wait_for_optional(&a_id, Duration::from_secs(8)).is_none();
    assert!(absent, "广播节点不应发现隐身节点 {a_id}");

    println!("=== A /dial B 手动直连仍可用 ===");
    a.send(&format!("/dial {b_addr}"));
    a.wait_for(&format!("已连接对端: {b_id}"), WAIT);
    b.wait_for(&format!("已连接对端: {a_id}"), WAIT);

    a.kill();
    b.kill();
}

/// 场景8：三节点 1v1 多会话——A 同时连 B、C，切换焦点收发，非焦点来信带名字
fn multi_session_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    let cache_a = scenario_cache_dir("s8_a");
    let cache_b = scenario_cache_dir("s8_b");
    let cache_c = scenario_cache_dir("s8_c");
    let (cred_a, cred_b, cred_c) = load_creds3();
    let b_name = cred_b.name.clone();
    let c_name = cred_c.name.clone();
    println!("=== 场景8: 三节点 1v1 多会话 ===");

    println!("=== 启动节点 A（user1）===");
    let (mut a, a_listen) = spawn_into_chat(bin, &cache_a, &cred_a, MNEMONIC_USER1);
    let a_addr = listen_addr(&a_listen);
    let a_id = parse_peer_id(&a_listen);

    println!("=== 启动节点 B（user2）===");
    let (mut b, b_listen) = spawn_into_chat(bin, &cache_b, &cred_b, MNEMONIC_USER2);
    let b_id = parse_peer_id(&b_listen);

    println!("=== 启动节点 C（user3）===");
    let (mut c, c_listen) = spawn_into_chat(bin, &cache_c, &cred_c, MNEMONIC_USER3);
    let c_id = parse_peer_id(&c_listen);

    println!("=== B、C 依次拨号 A：A 自动聚焦首个（B），C 不抢焦点 ===");
    b.send(&format!("/dial {a_addr}"));
    a.wait_for(&format!("已连接对端: {b_id}"), WAIT);
    b.wait_for(&format!("已连接对端: {a_id}"), WAIT);
    b.wait_for(&format!("对方已上线: {}", cred_a.name), WAIT);
    wait_mutual_trust(&a, &cred_a.name, &b, &cred_b.name);

    c.send(&format!("/dial {a_addr}"));
    a.wait_for(&format!("已连接对端: {c_id}"), WAIT);
    c.wait_for(&format!("已连接对端: {a_id}"), WAIT);
    a.wait_for(&format!("对方已上线: {c_name}"), WAIT);
    wait_mutual_trust(&a, &cred_a.name, &c, &cred_c.name);

    println!("=== A 聚焦 B：发消息 B 收 `[对方]` ===");
    a.send("hello B");
    b.wait_for("[对方] hello B", WAIT);

    println!("=== A /chat C 切焦点，发消息 C 收 `[对方]` ===");
    a.send(&format!("/chat {c_name}"));
    a.wait_for(&format!("已切换到会话: {c_name}"), WAIT);
    a.send("hello C");
    c.wait_for("[对方] hello C", WAIT);

    println!("=== B 来信（A 聚焦 C）：A 显示 `[B名] ...` ===");
    b.send("msg from B");
    a.wait_for(&format!("[{b_name}] msg from B"), WAIT);

    println!("=== C 来信（A 聚焦 C）：A 显示 `[对方] ...` ===");
    c.send("msg from C");
    a.wait_for("[对方] msg from C", WAIT);

    println!("=== /list 同时见 B、C 两会话 ===");
    // 显式登记 B/C 地址（/dial 会先记地址再拨，已连接则重复连接无害），
    // 避免 mDNS 时序抖动导致 /list 缺项
    let b_addr = listen_addr(&b_listen);
    let c_addr = listen_addr(&c_listen);
    a.send(&format!("/dial {c_addr}"));
    a.send(&format!("/dial {b_addr}"));
    a.send("/list");
    a.wait_for("=== 已登记节点 ===", WAIT);
    // /list 按节点ID排序，C（12D3KooWH…）排在 B（12D3KooWPCy…）前，按序断言
    a.wait_for(&c_id, WAIT);
    a.wait_for(&b_id, WAIT);

    a.kill();
    b.kill();
    c.kill();
}

/// 场景9：三节点群聊（gossipsub）——A 建群加 B、C，群消息扇出，非焦点群来信带群名
fn group_chat_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    let cache_a = scenario_cache_dir("s9_a");
    let cache_b = scenario_cache_dir("s9_b");
    let cache_c = scenario_cache_dir("s9_c");
    let (cred_a, cred_b, cred_c) = load_creds3();
    let b_name = cred_b.name.clone();
    let c_name = cred_c.name.clone();
    let group = "testgrp";
    println!("=== 场景9: 三节点群聊 ===");

    println!("=== 启动节点 A/B/C ===");
    let (mut a, a_listen) = spawn_into_chat(bin, &cache_a, &cred_a, MNEMONIC_USER1);
    let a_addr = listen_addr(&a_listen);
    let a_id = parse_peer_id(&a_listen);
    let (mut b, _b_listen) = spawn_into_chat(bin, &cache_b, &cred_b, MNEMONIC_USER2);
    let (mut c, _c_listen) = spawn_into_chat(bin, &cache_c, &cred_c, MNEMONIC_USER3);

    println!("=== B、C 拨号 A（建立 gossipsub 网格与邀请通道）===");
    b.send(&format!("/dial {a_addr}"));
    b.wait_for(&format!("已连接对端: {a_id}"), WAIT);
    c.send(&format!("/dial {a_addr}"));
    c.wait_for(&format!("已连接对端: {a_id}"), WAIT);

    println!("=== A 建群并加 B、C ===");
    a.send(&format!("/group new {group}"));
    a.wait_for(&format!("已创建并聚焦群聊: {group}"), WAIT);
    a.send(&format!("/group add {group} {b_name}"));
    a.wait_for(&format!("已将 {b_name} 加入群"), WAIT);
    b.wait_for(&format!("被邀请加入群聊: {group}"), WAIT);
    a.send(&format!("/group add {group} {c_name}"));
    a.wait_for(&format!("已将 {c_name} 加入群"), WAIT);
    c.wait_for(&format!("被邀请加入群聊: {group}"), WAIT);

    println!("=== B 收到群主 1v1 扇出的名单更新（成员 3 人，版本 2）===");
    b.wait_for(&format!("成员名单已更新（版本 2"), WAIT);
    b.send("/group list");
    b.wait_for(&format!("{group}（3 人，名单版本 2"), WAIT);

    // 等 gossipsub 网格形成（heartbeat ~1s），再发群消息
    thread::sleep(Duration::from_secs(2));

    println!("=== A 发群消息，B/C 都收到（非焦点 → 带群名）===");
    a.send("hello grp");
    b.wait_for(&format!("[{group}] [{}] hello grp", cred_a.name), Duration::from_secs(40));
    c.wait_for(&format!("[{group}] [{}] hello grp", cred_a.name), Duration::from_secs(40));

    println!("=== C 聚焦群聊并回消息，A/B 都收到 ===");
    c.send(&format!("/group {group}"));
    c.wait_for(&format!("已切换到群聊: {group}"), WAIT);
    c.send("hi from C");
    a.wait_for(&format!("[{c_name}] hi from C"), Duration::from_secs(40));
    b.wait_for(&format!("[{group}] [{c_name}] hi from C"), Duration::from_secs(40));

    println!("=== /group list 列出群（A：成员 3 人）===");
    a.send("/group list");
    a.wait_for(&format!("{group}（3 人，名单版本 2"), WAIT);

    println!("=== A 标记 testgrp 为常驻，/group list 显示 [常驻] ===");
    a.send(&format!("/group resident {group} on"));
    a.wait_for(&format!("群 {group} 已设为常驻"), WAIT);
    a.send("/group list");
    a.wait_for("[常驻]", WAIT);

    println!("=== C 退群：通知群主划去自己，A 处理并扇出更新名单 ===");
    c.send(&format!("/group leave {group}"));
    c.wait_for(&format!("已退出群聊 {group}"), WAIT);
    a.wait_for(&format!("成员 {c_name} 已退出群 {group}"), WAIT);

    println!("=== B 收到退群后的名单更新（成员 2 人，版本 3）===");
    b.wait_for(&format!("成员名单已更新（版本 3"), WAIT);
    b.send("/group list");
    b.wait_for(&format!("{group}（2 人，名单版本 3"), WAIT);

    println!("=== C 本地群已删除 ===");
    c.send("/group list");
    c.wait_for("暂无群聊", WAIT);

    a.kill();
    b.kill();
    c.kill();
}

/// 场景12：1v1 信任管理——取消信任真正生效（/list 徽标 + 群加人门控）、重新信任显示指纹复核（D4）、
/// 联系人名解析（重启后无会话仍能按联系人名 /trust /chat）
fn trust_management_and_contact_name_resolution_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    let cache_a = scenario_cache_dir("s12_a");
    let cache_b = scenario_cache_dir("s12_b");
    let (cred_a, cred_b) = load_creds();
    let b_name = cred_b.name.clone();
    let group = "grp1";
    println!("=== 场景12: 信任管理 + 联系人名解析 ===");

    println!("=== A/B 连接，A 建群 ===");
    let (mut a, a_listen) = spawn_into_chat(bin, &cache_a, &cred_a, MNEMONIC_USER1);
    let a_addr = listen_addr(&a_listen);
    let a_id = parse_peer_id(&a_listen);
    let (mut b, b_listen) = spawn_into_chat(bin, &cache_b, &cred_b, MNEMONIC_USER2);
    let b_id = parse_peer_id(&b_listen);
    b.send(&format!("/dial {a_addr}"));
    b.wait_for(&format!("已连接对端: {a_id}"), WAIT);
    a.wait_for(&format!("已连接对端: {b_id}"), WAIT);
    // 等 A 处理完 B 的 Hello（会话名 + 联系人记录就绪），否则 /trust 按名解析不到
    a.wait_for(&format!("对方已上线: {b_name}"), WAIT);
    a.send(&format!("/group new {group}"));
    a.wait_for(&format!("已创建并聚焦群聊: {group}"), WAIT);

    println!("=== A 取消信任 B → /list 徽标变未信任 + 群加人被门控拒绝 ===");
    a.send(&format!("/trust !{b_name}"));
    a.wait_for(&format!("已取消信任: {b_name}"), WAIT);
    a.send("/list");
    a.wait_for("=== 已登记节点 ===", WAIT);
    a.wait_for(&format!("{b_id}  [未信任]"), WAIT);
    a.send(&format!("/group add {group} {b_name}"));
    a.wait_for("尚未验证，请先 /trust", WAIT);

    println!("=== A 重新信任 B：显示指纹复核（D4）→ 加人成功 ===");
    a.send(&format!("/trust {b_name}"));
    a.wait_for("请核对对方身份", WAIT);
    a.wait_for("指纹: ", WAIT);
    a.wait_for(&format!("已信任: {b_name}"), WAIT);
    a.send(&format!("/group add {group} {b_name}"));
    a.wait_for(&format!("已将 {b_name} 加入群 {group}"), WAIT);
    b.wait_for(&format!("被邀请加入群聊: {group}"), WAIT);

    println!("=== 联系人名解析：A 重启后无会话，仍按联系人名 /trust /chat ===");
    a.send("/q");
    a.wait_for("=== 主菜单 ===", WAIT);
    enter_chat(&mut a, &cred_a);
    a.send(&format!("/trust {b_name}"));
    a.wait_for("请核对对方身份", WAIT);
    a.send(&format!("/chat {b_name}"));
    a.wait_for(&format!("已连接对端: {b_id}"), Duration::from_secs(40));

    a.kill();
    b.kill();
}

/// 场景14：对称信任——互信才能收发；任一方取消信任 → 双向丢弃（对称门控）；重新信任恢复。
/// 管道模式 hello 自动互信，故基线直接可收发。
fn symmetric_trust_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    let cache_a = scenario_cache_dir("st_a");
    let cache_b = scenario_cache_dir("st_b");
    let (cred_a, cred_b) = load_creds();
    let a_name = cred_a.name.clone();
    let b_name = cred_b.name.clone();
    println!("=== 场景14: 对称信任（互信收发 / 单方取消双向断 / 恢复）===");

    let (mut a, a_listen) = spawn_into_chat(bin, &cache_a, &cred_a, MNEMONIC_USER1);
    let a_addr = listen_addr(&a_listen);
    let a_id = parse_peer_id(&a_listen);
    let (mut b, b_listen) = spawn_into_chat(bin, &cache_b, &cred_b, MNEMONIC_USER2);
    let b_id = parse_peer_id(&b_listen);
    b.send(&format!("/dial {a_addr}"));
    b.wait_for(&format!("已连接对端: {a_id}"), WAIT);
    a.wait_for(&format!("已连接对端: {b_id}"), WAIT);

    println!("=== 基线：双方互信信号就绪后双向收发 ===");
    a.wait_for(&format!("对方已信任你: {b_name}"), WAIT);
    b.wait_for(&format!("对方已信任你: {a_name}"), WAIT);
    a.send("mutual ok");
    b.wait_for("[对方] mutual ok", WAIT);
    b.send("mutual back");
    a.wait_for("[对方] mutual back", WAIT);

    println!("=== B 取消信任 A：A 收到 revoke，双向消息应被丢弃 ===");
    b.send(&format!("/trust !{a_name}"));
    b.wait_for(&format!("已取消信任: {a_name}"), WAIT);
    a.wait_for(&format!("对方已取消信任: {b_name}"), WAIT);

    println!("=== A 发消息：B 应丢弃（不显示）===");
    a.send("should be dropped on B");
    let dropped_b = b
        .wait_for_optional("should be dropped on B", Duration::from_secs(5))
        .is_none();
    assert!(dropped_b, "B 不应收到 A 未互信的消息");

    println!("=== B 发消息：A 也应丢弃（对称）===");
    b.send("should be dropped on A");
    let dropped_a = a
        .wait_for_optional("should be dropped on A", Duration::from_secs(5))
        .is_none();
    assert!(dropped_a, "A 不应收到 B 未互信的消息");

    println!("=== 未互信时文件传输被拒（发送侧门控）===");
    a.send(&format!("/send {b_name} nonexistent.txt"));
    a.wait_for("尚未互信", WAIT);

    println!("=== B 重新信任 A：A 收到 confirm，双向收发恢复 ===");
    b.send(&format!("/trust {a_name}"));
    b.wait_for(&format!("已信任: {a_name}"), WAIT);
    a.wait_for(&format!("对方已信任你: {b_name}"), WAIT);
    a.send("restored after retrust");
    b.wait_for("[对方] restored after retrust", WAIT);

    a.kill();
    b.kill();
}

/// 场景13：文件传输——A 发送文件给已互信联系人 B，B 落盘并校验内容一致
fn file_transfer_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    let cache_a = scenario_cache_dir("s13_a");
    let cache_b = scenario_cache_dir("s13_b");
    let (cred_a, cred_b) = load_creds();
    let b_name = cred_b.name.clone();
    println!("=== 场景13: 文件传输 ===");

    // 生成 3MB 测试文件（伪随机字节）
    let src_name = "s13_file.bin";
    let src = std::env::temp_dir().join(src_name);
    let _ = std::fs::remove_file(&src);
    {
        let mut data = Vec::with_capacity(3 * 1024 * 1024);
        let mut seed: u64 = 0x1234_5678_9abc_def0;
        for _ in 0..(3 * 1024 * 1024) {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            data.push(((seed >> 33) & 0xff) as u8);
        }
        std::fs::write(&src, &data).expect("写测试文件失败");
    }
    // 接收方 B 的下载目录由 P2P_DOWNLOAD_DIR 指到 `{cache_b}/downloads`
    let final_path = format!("{cache_b}/downloads/{src_name}");
    let _ = std::fs::remove_file(&final_path);

    let (mut a, a_listen) = spawn_into_chat(bin, &cache_a, &cred_a, MNEMONIC_USER1);
    let a_addr = listen_addr(&a_listen);
    let a_id = parse_peer_id(&a_listen);
    let (mut b, b_listen) = spawn_into_chat(bin, &cache_b, &cred_b, MNEMONIC_USER2);
    let b_id = parse_peer_id(&b_listen);
    b.send(&format!("/dial {a_addr}"));
    b.wait_for(&format!("已连接对端: {a_id}"), WAIT);
    a.wait_for(&format!("已连接对端: {b_id}"), WAIT);
    a.wait_for(&format!("对方已上线: {b_name}"), WAIT);
    wait_mutual_trust(&a, &cred_a.name, &b, &cred_b.name);

    println!("=== A 发送文件给 B（B 管道模式自动接受）===");
    a.send(&format!("/send {b_name} {}", src.display()));
    a.wait_for(&format!("开始发送 {src_name}"), WAIT);
    b.wait_for(&format!("开始接收 {src_name}"), WAIT);

    println!("=== 等传输完成，校验内容一致 ===");
    b.wait_for("文件接收完成", WAIT);
    a.wait_for("文件发送完成", WAIT);

    let received = std::fs::read(&final_path).expect("读取接收文件失败");
    let sent = std::fs::read(&src).expect("读取源文件失败");
    assert_eq!(received.len(), sent.len(), "文件长度不一致");
    assert_eq!(received, sent, "文件内容不一致");

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&final_path);
    a.kill();
    b.kill();
}

/// 场景：同机跑多个实例，用 IPv6 自连不同 PeerId——
/// Part 1 走 `::1` 回环（验证 IPv6 拨号代码路径，不受防火墙影响）；
/// Part 2 走本机全局 IPv6（验证真实网卡 + Windows 防火墙 + NDP 环回）。
/// 不作为串行 suite 场景（suite 已较长），由 standalone_ipv6_connect 单独运行。
fn ipv6_loopback_and_global_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    let cache_a = scenario_cache_dir("v6_a");
    let cache_b = scenario_cache_dir("v6_b");
    let cache_c = scenario_cache_dir("v6_c");
    let (cred_a, cred_b, cred_c) = load_creds3();
    println!("=== 场景: IPv6 自连（::1 回环 + 全局地址）===");

    println!("=== 启动节点 A / B（P2P_DISCOVERY=off，防同机 mDNS 自动互连）===");
    let (mut a, _) = spawn_chat_off(bin, &cache_a, &cred_a, MNEMONIC_USER1);
    let (mut b, b_listen) = spawn_chat_off(bin, &cache_b, &cred_b, MNEMONIC_USER2);
    let b_id = parse_peer_id(&b_listen);
    let b_port = listen_port(&b_listen);
    // 先抓取全局 IPv6 监听行（ip6 全局行在 B 后续输出被消费前出现），供 Part 2 用
    let global_line = wait_global_ipv6_listen(&b, Duration::from_secs(20));

    // Part 1：A 经 ::1 拨 B（v4/v6 监听同端口，::1 地址由端口构造，无需等 ::1 输出行）
    println!("=== Part 1: A 经 ::1 拨 B ===");
    let b_loop = format!("/ip6/::1/tcp/{b_port}/p2p/{b_id}");
    a.send(&format!("/dial {b_loop}"));
    a.wait_for(&format!("已连接对端: {b_id}"), WAIT);
    wait_mutual_trust(&a, &cred_a.name, &b, &cred_b.name);
    a.send("IPv6 回环自连测试消息");
    b.wait_for("[对方] IPv6 回环自连测试消息", WAIT);
    println!("=== Part 1 通过 ===");

    // Part 2：C 经全局 IPv6 拨 B（真实网卡 + 防火墙）
    if std::env::var("P2P_E2E_SKIP_GLOBAL_IPV6").is_ok() {
        println!("=== 环境变量 P2P_E2E_SKIP_GLOBAL_IPV6 已设，跳过全局 IPv6 部分 ===");
    } else if let Some(g) = global_line {
        let b_global = listen_addr(&g);
        println!("=== Part 2: C 经全局 IPv6 拨 B（{b_global}）===");
        let (mut c, _) = spawn_chat_off(bin, &cache_c, &cred_c, MNEMONIC_USER3);
        c.send(&format!("/dial {b_global}"));
        // 防火墙 drop 会让 TCP 长时间超时，等待放宽到 35s
        c.wait_for(&format!("已连接对端: {b_id}"), Duration::from_secs(35));
        wait_mutual_trust(&c, &cred_c.name, &b, &cred_b.name);
        c.send("IPv6 全局地址自连测试消息");
        // B 焦点仍在 A（Part 1），C 的来信为非焦点格式 `[名字] 消息`，按内容匹配即可
        b.wait_for("IPv6 全局地址自连测试消息", WAIT);
        println!("=== Part 2 通过 ===");
        c.kill();
    } else {
        println!("=== 本机无全局 IPv6 地址，跳过全局 IPv6 部分 ===");
    }

    a.kill();
    b.kill();
}

/// 独立文件传输测试（不加入 suite，单独运行隔离验证）
#[test]
fn standalone_file_transfer() {
    file_transfer_scenario();
}

/// 独立 IPv6 自连测试（不加入串行 suite，单独运行隔离验证）。
/// 前置：Windows 防火墙需放行 p2p_rust_app 入站，否则全局 IPv6 部分会失败
/// （`netsh advfirewall firewall add rule ...`），::1 回环部分不受防火墙影响。
#[test]
fn standalone_ipv6_connect() {
    ipv6_loopback_and_global_scenario();
}

/// 未互信钩子边界：A 注册 `chat.text` 未互信钩子（未互信也显示，带 `[未信任]` 标记），
/// B 未注册（未互信丢弃）——验证"未互信处理是每端本地策略、线缆协议互通"（跨版本配置）。
fn untrusted_hook_boundary_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    let cache_a = scenario_cache_dir("uh_a");
    let cache_b = scenario_cache_dir("uh_b");
    let (cred_a, cred_b) = load_creds();
    let a_name = cred_a.name.clone();
    let b_name = cred_b.name.clone();
    println!("=== 场景: 未互信钩子边界（A 注册显示 / B 未注册丢弃）===");

    // A 带 P2P_E2E_UNTRUSTED_HOOK 启动（注册 chat.text 未互信钩子）；B 不带
    let mut a = Node::spawn_with_env(bin, &cache_a, "advertise", &[("P2P_E2E_UNTRUSTED_HOOK", "1")]);
    a.wait_for("=== 主菜单 ===", Duration::from_secs(10));
    login_restore(&mut a, &cred_a, MNEMONIC_USER1);
    let a_listen = a.wait_for("监听地址: /ip4/127.0.0.1", Duration::from_secs(20));
    let a_addr = listen_addr(&a_listen);
    let a_id = parse_peer_id(&a_listen);

    let (mut b, b_listen) = spawn_into_chat(bin, &cache_b, &cred_b, MNEMONIC_USER2);
    let b_id = parse_peer_id(&b_listen);

    b.send(&format!("/dial {a_addr}"));
    b.wait_for(&format!("已连接对端: {a_id}"), WAIT);
    a.wait_for(&format!("已连接对端: {b_id}"), WAIT);
    // 等双方 hello 上线通知（会话名就绪），否则 /trust 按名解析不到
    b.wait_for(&format!("对方已上线: {a_name}"), WAIT);
    a.wait_for(&format!("对方已上线: {b_name}"), WAIT);

    // B 取消信任 A → revoke 传播 → 双方互信断裂（进入未互信态）
    println!("=== B /trust !A → 双方未互信 ===");
    b.send(&format!("/trust !{a_name}"));
    b.wait_for(&format!("已取消信任: {a_name}"), WAIT);
    a.wait_for(&format!("对方已取消信任: {b_name}"), WAIT);

    println!("=== B→A：A 注册了钩子 → 未互信也显示 `[未信任] ...` ===");
    b.send("untrusted msg from B");
    a.wait_for(&format!("[未信任] {b_name}: untrusted msg from B"), WAIT);

    println!("=== A→B：B 未注册钩子 → 丢弃（B 不显示）===");
    a.send("untrusted msg from A");
    let dropped_b = b
        .wait_for_optional("untrusted msg from A", Duration::from_secs(5))
        .is_none();
    assert!(dropped_b, "B 未注册未互信钩子，应丢弃 A 的未互信消息");

    println!("=== B 重新信任 A → 互信恢复，双向正常显示 ===");
    b.send(&format!("/trust {a_name}"));
    b.wait_for(&format!("已信任: {a_name}"), WAIT);
    a.wait_for(&format!("对方已信任你: {b_name}"), WAIT);
    a.send("restored after retrust");
    b.wait_for("[对方] restored after retrust", WAIT);
    b.send("back after retrust");
    a.wait_for("[对方] back after retrust", WAIT);

    a.kill();
    b.kill();
}

/// 逻辑测试串行 suite：功能正确性场景。若拆成并行 #[test]，同机 mDNS 会跨测试互相发现。
#[test]
fn p2p_chat_logic_suite() {
    basic_chat_scenario();
    chat_by_name_scenario();
    discovery_mode_scenario();
    multi_session_scenario();
    group_chat_scenario();
    trust_management_and_contact_name_resolution_scenario();
    symmetric_trust_scenario();
    untrusted_hook_boundary_scenario();
    file_transfer_scenario();
}
