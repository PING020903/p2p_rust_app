//! 稳定性测试（显式运行）：重复登录/下线、掉线重连、同 ID 冲突、非法/意外操作、应用阻塞心跳。
//! 默认不随 `cargo test` 运行；需显式执行：
//! `cargo test --test p2p_chat_stability -- --ignored --test-threads=1`

mod common;
use common::*;
use std::thread;
use std::time::Duration;

/// 场景3：B 主动下线/上线循环，每轮发送 ≤64 字节随机消息
fn graceful_offline_online_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    let cache_a = scenario_cache_dir("s3_a");
    let cache_b = scenario_cache_dir("s3_b");
    let (cred_a, cred_b) = load_creds();
    println!("=== 场景3: 主动上下线循环 x{CYCLES_GRACEFUL} ===");

    let (mut a, a_listen) = spawn_into_chat(bin, &cache_a, &cred_a, MNEMONIC_USER1);
    let a_addr = listen_addr(&a_listen);
    let a_id = parse_peer_id(&a_listen);

    let mut b = Node::spawn(bin, &cache_b);
    b.wait_for("=== 主菜单 ===", Duration::from_secs(10));

    for i in 0..CYCLES_GRACEFUL {
        println!("=== 第 {} 轮: 主动上线 ===", i + 1);
        let b_listen = if i == 0 {
            login_restore(&mut b, &cred_b, MNEMONIC_USER2);
            b.wait_for("监听地址: /ip4/127.0.0.1", Duration::from_secs(20))
        } else {
            enter_chat(&mut b, &cred_b)
        };
        let b_id = parse_peer_id(&b_listen);
        b.send(&format!("/dial {a_addr}"));
        b.wait_for(&format!("已连接对端: {a_id}"), WAIT);
        a.wait_for(&format!("已连接对端: {b_id}"), WAIT);
        // 首轮建立互信（后续轮次身份缓存持久化互信状态，重连自动恢复）
        if i == 0 {
            wait_mutual_trust(&a, &cred_a.name, &b, &cred_b.name);
        }

        let msg = random_msg(i as u64 + 1);
        assert!(msg.len() <= 64, "消息长度须 ≤64 字节");
        println!("=== 第 {} 轮: 发送 {} 字节随机消息 ===", i + 1, msg.len());
        b.send(&msg);
        a.wait_for(&format!("[对方] {msg}"), WAIT);

        println!("=== 第 {} 轮: 主动下线（/q）===", i + 1);
        b.send("/q");
        a.wait_for("对方已正常退出", WAIT);
    }

    a.kill();
    b.kill();
}

/// 场景4：kill 进程模拟掉线（无 Bye），隔一段时间后重新上线
fn kill_offline_online_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    let cache_a = scenario_cache_dir("s4_a");
    let cache_b = scenario_cache_dir("s4_b");
    let (cred_a, cred_b) = load_creds();
    println!("=== 场景4: kill 掉线循环 x{CYCLES_KILL} ===");

    let (mut a, a_listen) = spawn_into_chat(bin, &cache_a, &cred_a, MNEMONIC_USER1);
    let a_addr = listen_addr(&a_listen);
    let a_id = parse_peer_id(&a_listen);

    for i in 0..CYCLES_KILL {
        println!("=== 第 {} 轮: 上线 ===", i + 1);
        let mut b = Node::spawn(bin, &cache_b);
        b.wait_for("=== 主菜单 ===", Duration::from_secs(10));
        login_restore(&mut b, &cred_b, MNEMONIC_USER2);
        let b_listen = b.wait_for("监听地址: /ip4/127.0.0.1", Duration::from_secs(20));
        let b_id = parse_peer_id(&b_listen);
        b.send(&format!("/dial {a_addr}"));
        b.wait_for(&format!("已连接对端: {a_id}"), WAIT);
        a.wait_for(&format!("已连接对端: {b_id}"), WAIT);
        if i == 0 {
            wait_mutual_trust(&a, &cred_a.name, &b, &cred_b.name);
        }

        let msg = random_msg(i as u64 + 101);
        assert!(msg.len() <= 64, "消息长度须 ≤64 字节");
        println!("=== 第 {} 轮: 发送 {} 字节随机消息 ===", i + 1, msg.len());
        b.send(&msg);
        a.wait_for(&format!("[对方] {msg}"), WAIT);

        println!("=== 第 {} 轮: kill 进程模拟掉线 ===", i + 1);
        b.kill();
        a.wait_for("连接已关闭", WAIT);

        println!("=== 隔 3 秒后重新上线 ===");
        thread::sleep(Duration::from_secs(3));
    }

    a.kill();
}

/// 场景5：身份缓存回环——助记词恢复登录（自动加密保存）→ 退出重进 → 选缓存身份 + 只输密码
/// （先故意输错验证密码校验）
fn cache_login_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    let cache = scenario_cache_dir("s5");
    let (cred_a, _cred_b) = load_creds();
    println!("=== 场景5: 身份缓存回环 ===");

    let mut a = Node::spawn(bin, &cache);
    a.wait_for("=== 主菜单 ===", Duration::from_secs(10));
    login_restore(&mut a, &cred_a, MNEMONIC_USER1);
    a.wait_for("监听地址: /ip4/127.0.0.1", WAIT);

    println!("=== 退出聊天后重新进入，走缓存登录 ===");
    a.send("/q");
    a.wait_for("=== 主菜单 ===", WAIT);
    a.send("4");
    a.wait_for("缓存身份:", WAIT);

    println!("=== 先输错密码，验证校验 ===");
    a.send("1");
    a.send("wrong-password");
    a.wait_for("密码错误", WAIT);

    println!("=== 输正确密码（免姓名/生日/性别/助记词）===");
    a.send(&cred_a.password);
    a.wait_for("登录成功: ", WAIT);
    a.wait_for("监听地址: /ip4/127.0.0.1", WAIT);

    a.kill();
}

/// 场景6：同 ID 冲突——两节点同一助记词，后者登录必须被拒绝
fn duplicate_id_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    let cache_a = scenario_cache_dir("s6_a");
    let cache_b = scenario_cache_dir("s6_b");
    let (cred_a, _cred_b) = load_creds();
    println!("=== 场景6: 同 ID 冲突拒绝 ===");

    let (mut a, _a_listen) = spawn_into_chat(bin, &cache_a, &cred_a, MNEMONIC_USER1);

    println!("=== B 用同一助记词登录，必须被拒绝 ===");
    let mut b = Node::spawn(bin, &cache_b);
    b.wait_for("=== 主菜单 ===", Duration::from_secs(10));
    login_restore(&mut b, &cred_a, MNEMONIC_USER1);
    b.wait_for("该角色 ID 已在线", Duration::from_secs(30));

    a.kill();
    b.kill();
}

/// 场景10：应用任务卡在交互 await（/backup 密码提示未回答），传输任务须独立维持心跳，
/// 连接不被心跳超时断开——这是三层架构（L1 传输任务）的核心验收点。
fn app_blocked_heartbeat_still_alive_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    let cache_a = scenario_cache_dir("s10_a");
    let cache_b = scenario_cache_dir("s10_b");
    let (cred_a, cred_b) = load_creds();
    println!("=== 场景10: 应用卡在密码交互，传输任务心跳仍存活 ===");

    let (mut a, a_listen) = spawn_into_chat(bin, &cache_a, &cred_a, MNEMONIC_USER1);
    let a_addr = listen_addr(&a_listen);
    let a_id = parse_peer_id(&a_listen);

    let (mut b, b_listen) = spawn_into_chat(bin, &cache_b, &cred_b, MNEMONIC_USER2);
    let b_id = parse_peer_id(&b_listen);
    b.send(&format!("/dial {a_addr}"));
    a.wait_for(&format!("已连接对端: {b_id}"), WAIT);
    b.wait_for(&format!("已连接对端: {a_id}"), WAIT);
    wait_mutual_trust(&a, &cred_a.name, &b, &cred_b.name);

    println!("=== A 触发 /backup 并故意不输密码 → 应用任务阻塞 ===");
    a.send("/backup");
    a.wait_for("请输入密码以解锁本身份", WAIT);

    println!("=== 阻塞 17 秒（> 心跳超时 15s）：传输任务应保持 A-B 心跳 ===");
    let timed_out = b
        .wait_for_optional("心跳超时", Duration::from_secs(17))
        .is_some();
    assert!(!timed_out, "A 的传输任务被应用阻塞连累：B 判定 A 心跳超时");

    println!("=== 补输密码解锁 A 应用任务 ===");
    a.send(&cred_a.password);
    a.wait_for("助记词是唯一备份", WAIT);

    println!("=== 连接仍存活：A 发消息 B 收到 ===");
    a.send("alive after app block");
    b.wait_for("[对方] alive after app block", WAIT);

    a.kill();
    b.kill();
}

/// 场景11：群主离线禁止退群（单写者一致性，防名单发散/幽灵）+ 群主退群一步顺位转移 +
/// 新群主能加人（群不冻结）
fn owner_offline_leave_ban_and_transfer_scenario() {
    let bin = env!("CARGO_BIN_EXE_p2p_rust_app");
    let cache_a = scenario_cache_dir("s11_a");
    let cache_b = scenario_cache_dir("s11_b");
    let cache_c = scenario_cache_dir("s11_c");
    let cache_d = scenario_cache_dir("s11_d");
    let (cred_a, cred_b, cred_c, cred_d) = load_creds4();
    let b_name = cred_b.name.clone();
    let c_name = cred_c.name.clone();
    let d_name = cred_d.name.clone();
    let group = "testgrp";
    println!("=== 场景11: 群主离线退群被拒 + 顺位转移 + 新群主加人 ===");

    println!("=== 启动 A/B/C，A 建群加 B、C ===");
    let (mut a, a_listen) = spawn_into_chat(bin, &cache_a, &cred_a, MNEMONIC_USER1);
    let a_addr = listen_addr(&a_listen);
    let a_id = parse_peer_id(&a_listen);
    let (mut b, b_listen) = spawn_into_chat(bin, &cache_b, &cred_b, MNEMONIC_USER2);
    let b_addr = listen_addr(&b_listen);
    let b_id = parse_peer_id(&b_listen);
    let (mut c, c_listen) = spawn_into_chat(bin, &cache_c, &cred_c, MNEMONIC_USER3);
    let c_id = parse_peer_id(&c_listen);

    b.send(&format!("/dial {a_addr}"));
    b.wait_for(&format!("已连接对端: {a_id}"), WAIT);
    c.send(&format!("/dial {a_addr}"));
    c.wait_for(&format!("已连接对端: {a_id}"), WAIT);

    a.send(&format!("/group new {group}"));
    a.wait_for(&format!("已创建并聚焦群聊: {group}"), WAIT);
    a.send(&format!("/group add {group} {b_name}"));
    a.wait_for(&format!("已将 {b_name} 加入群"), WAIT);
    b.wait_for(&format!("被邀请加入群聊: {group}"), WAIT);
    a.send(&format!("/group add {group} {c_name}"));
    a.wait_for(&format!("已将 {c_name} 加入群"), WAIT);
    c.wait_for(&format!("被邀请加入群聊: {group}"), WAIT);

    println!("=== 群主 A 离线：B 退群被拒（单写者一致性，防幽灵）===");
    a.kill();
    b.wait_for(&format!("连接已关闭: {a_id}"), WAIT);
    b.send(&format!("/group leave {group}"));
    b.wait_for("群主不在线，无法退群", WAIT);
    b.send("/group list");
    b.wait_for(&format!("{group}（3 人，名单版本 2"), WAIT);

    println!("=== A 重新登录，B/C 重连 A ===");
    let mut a = Node::spawn(bin, &cache_a);
    a.wait_for("=== 主菜单 ===", Duration::from_secs(10));
    let a_listen2 = enter_chat(&mut a, &cred_a);
    let a_addr2 = listen_addr(&a_listen2);
    b.send(&format!("/dial {a_addr2}"));
    b.wait_for(&format!("已连接对端: {a_id}"), WAIT);
    c.send(&format!("/dial {a_addr2}"));
    c.wait_for(&format!("已连接对端: {a_id}"), WAIT);
    a.wait_for(&format!("已连接对端: {b_id}"), WAIT);
    a.wait_for(&format!("已连接对端: {c_id}"), WAIT);

    println!("=== A 退群：一步顺位转移给名单下一位 B ===");
    a.send(&format!("/group leave {group}"));
    a.wait_for(&format!("群主已顺位转移给 {b_name}"), WAIT);
    b.wait_for("群主已转移给你，你已成为群主", WAIT);
    c.wait_for("群主已顺位转移给", WAIT);

    println!("=== 新群主 B 加人 D：群不再冻结 ===");
    let (mut d, d_listen) = spawn_into_chat(bin, &cache_d, &cred_d, MNEMONIC_USER4);
    let d_id = parse_peer_id(&d_listen);
    d.send(&format!("/dial {b_addr}"));
    d.wait_for(&format!("已连接对端: {b_id}"), WAIT);
    b.wait_for(&format!("已连接对端: {d_id}"), WAIT);
    b.wait_for(&format!("对方已上线: {d_name}"), WAIT);
    b.send(&format!("/group add {group} {d_name}"));
    b.wait_for(&format!("已将 {d_name} 加入群 {group}（名单版本 4"), WAIT);
    d.wait_for(&format!("被邀请加入群聊: {group}"), WAIT);
    c.wait_for(&format!("成员名单已更新（版本 4"), WAIT);

    a.kill();
    b.kill();
    c.kill();
    d.kill();
}

/// 稳定性测试 suite：重复登录/下线、掉线重连、同 ID 冲突、应用阻塞心跳、群主离线边界。
/// `#[ignore]`：默认不随 `cargo test` 运行，显式执行才跑。
#[test]
#[ignore]
fn p2p_chat_stability_suite() {
    graceful_offline_online_scenario();
    kill_offline_online_scenario();
    cache_login_scenario();
    duplicate_id_scenario();
    app_blocked_heartbeat_still_alive_scenario();
    owner_offline_leave_ban_and_transfer_scenario();
}
