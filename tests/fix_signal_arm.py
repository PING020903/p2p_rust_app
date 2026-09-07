# -*- coding: utf-8 -*-
"""P2.6 步4 修复：插回被误删的 Event::Signal 臂（干净版，含 hello 两段式）"""
import io

PATH = r"F:\cmake_study\p2p_rust_app\source\p2p_app\chat\session.rs"

arm = '''                            Event::Signal { from, tag, payload } => {
                                // 通道路由：control（L1 心跳）已被 seam 过滤，L3 只见信号帧。
                                // 无 match：构造应用上下文，按 tag 标签查 SignalRegistry 分发，
                                // await 注册的 handler（hello/bye/trust 由 L2 内化语义映射 + L3 钩子；
                                // chat.* 由 chat 业务 handler 处理）。
                                // L2 门禁（唯一收口）：内化信号（hello/bye/trust）一律放行；
                                // 业务信号（chat.*/file.*）须互信，否则走该 tag 的未互信钩子
                                // （L2 API `register_untrusted`；未注册 = 空函数 = 丢弃）
                                if !is_l2_signal(&tag) && !identity.effective_trusted(&from) {
                                    // 拦截可见性：未互信来源的业务信号被丢弃时明确提示
                                    // （每条都提示——单方面信任的"不通"必须可诊断）
                                    let who = peer_name(&from, &conversations, &identity);
                                    println!(
                                        "{}",
                                        format!(
                                            "收到未信任方 {who} 的业务消息已拦截（互信后可见）: {tag}"
                                        )
                                        .yellow()
                                    );
                                    let mut actx = AppCtx {
                                        identity: &mut identity,
                                        conversations: &mut conversations,
                                        groups: &mut groups,
                                        focused: &mut focused,
                                        input: &mut input,
                                        mode,
                                        hello_pending: None,
                                        cmd_tx: &cmd_tx,
                                        file: &mut file_state,
                                    };
                                    registry
                                        .handle_untrusted(&tag, &from, payload.as_deref(), &mut actx)
                                        .await;
                                    continue;
                                }
                                let mut actx = AppCtx {
                                    identity: &mut identity,
                                    conversations: &mut conversations,
                                    groups: &mut groups,
                                    focused: &mut focused,
                                    input: &mut input,
                                    mode,
                                    hello_pending: None,
                                    cmd_tx: &cmd_tx,
                                    file: &mut file_state,
                                };
                                let handled = registry
                                    .dispatch(&tag, &from, payload.as_deref(), &mut actx)
                                    .await;
                                if !handled {
                                    eprintln!(
                                        "{}",
                                        format!("未处理的自定义语义: {tag}").yellow()
                                    );
                                }
                                // hello 两段式：TOFU 首触挂起 → 登记待决确认
                                // （Interactive 同时拉起确认子窗口；Ask 卡片已由 L2 发出）
                                if let Some((peer, name)) = actx.hello_pending.take() {
                                    pending_confirm = Some(PendingConfirm::Tofu {
                                        peer,
                                        name: name.clone(),
                                    });
                                    #[cfg(windows)]
                                    if mode == ConfirmMode::Interactive {
                                        let fp = identity.fingerprint(&peer);
                                        spawn_tofu_window(&confirm_tx, &name, &fp, peer);
                                    }
                                    #[cfg(not(windows))]
                                    if mode == ConfirmMode::Interactive {
                                        // 非 Windows 退化：记录为未信任（known boundary）
                                        identity.complete_tofu(&peer, &name, false);
                                        println!(
                                            "{}",
                                            "非 Windows 暂不支持交互确认，已记录为未信任（可稍后 /trust 升级）"
                                                .yellow()
                                        );
                                        pending_confirm = None;
                                    }
                                    continue;
                                }
                            }
'''

lines = io.open(PATH, encoding="utf-8").read().splitlines()
# 核对插入点：753 行（1-based）应为 Event::Signal，754 应为 Event::Gossip
assert lines[752].strip().startswith("Event::Signal"), lines[752]
assert lines[753].strip().startswith("Event::Gossip"), lines[753]

out = lines[:752] + arm.splitlines() + lines[753:]
io.open(PATH, "w", encoding="utf-8", newline="\n").write("\n".join(out) + "\n")
print("Signal 臂已插回；总行数:", len(out))
