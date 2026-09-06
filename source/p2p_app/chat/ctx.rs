//! 会话上下文：命令树/Control/发送路径共用的可变状态束 + 异步动作队列。

use std::collections::{HashMap, HashSet, VecDeque};

use colored::Colorize;
use libp2p::{Multiaddr, PeerId};

use crate::lineio::{ConfirmMode, LineSource};
use crate::p2p::identity_service::IdentityService;
use crate::p2p::seam;
use crate::p2p_app::chat::group::Group;

/// 命令 handler 产出的异步动作：同步逻辑跑在指令树 handler 里，真正需要 `.await`
/// 的 I/O（发命令给传输适配层 / 读密码）排进 `ChatCtx.ops`，由主循环统一消费。
/// 这本质是"同步生产者 → 异步消费者"的 ring buffer 解耦。
pub(crate) enum AsyncOp {
    Cmd(seam::Cmd),
    Backup,
    /// 查询本机监听地址并打印（/listen）
    Listen,
}

/// 命令上下文：一次性持有全部可变状态，供指令树 handler 直接读写。
/// 每次解析一行命令前临时构造（借用随本次处理结束释放），quit 置位表示请求退出。
pub(crate) struct ChatCtx<'a> {
    pub(crate) identity: &'a mut IdentityService,
    pub(crate) cmd_tx: &'a tokio::sync::mpsc::Sender<seam::Cmd>,
    pub(crate) input: &'a mut LineSource,
    pub(crate) mode: ConfirmMode,
    pub(crate) conversations: &'a mut HashMap<PeerId, Conversation>,
    pub(crate) groups: &'a mut HashMap<String, Group>,
    pub(crate) focused: &'a mut Option<PeerId>,
    pub(crate) focused_group: &'a mut Option<String>,
    pub(crate) connected: &'a HashSet<PeerId>,
    pub(crate) registered: &'a mut HashMap<PeerId, Vec<Multiaddr>>,
    /// 待消费的异步动作队列（VecDeque 即可增长的环状缓冲）
    pub(crate) ops: VecDeque<AsyncOp>,
    pub(crate) quit: bool,
    pub(crate) file: &'a mut crate::file_transfer::FileTransferState,
}

impl<'a> ChatCtx<'a> {
    /// 按名字/节点ID 解析目标 peer：会话名 → 联系人名（L2）→ 直接解析节点ID
    pub(crate) fn resolve(&self, target: &str) -> Option<PeerId> {
        self.conversations
            .iter()
            .find(|(_, c)| c.name == target)
            .map(|(p, _)| *p)
            .or_else(|| self.identity.contact_by_name(target))
            .or_else(|| target.parse::<PeerId>().ok())
    }

    /// 按群名解析群 id
    pub(crate) fn group_id(&self, name: &str) -> Option<String> {
        self.groups
            .iter()
            .find(|(_, g)| g.name == name)
            .map(|(id, _)| id.clone())
    }
}

/// 构造一次性会话上下文（借用随本次处理结束释放；命令/文本/Control 各输入分支共用）
#[allow(clippy::too_many_arguments)]
pub(crate) fn make_chat_ctx<'a>(
    identity: &'a mut IdentityService,
    cmd_tx: &'a tokio::sync::mpsc::Sender<seam::Cmd>,
    input: &'a mut LineSource,
    mode: ConfirmMode,
    conversations: &'a mut HashMap<PeerId, Conversation>,
    groups: &'a mut HashMap<String, Group>,
    focused: &'a mut Option<PeerId>,
    focused_group: &'a mut Option<String>,
    connected: &'a HashSet<PeerId>,
    registered: &'a mut HashMap<PeerId, Vec<Multiaddr>>,
    file: &'a mut crate::file_transfer::FileTransferState,
) -> ChatCtx<'a> {
    ChatCtx {
        identity,
        cmd_tx,
        input,
        mode,
        conversations,
        groups,
        focused,
        focused_group,
        connected,
        registered,
        ops: VecDeque::new(),
        quit: false,
        file,
    }
}

/// 消费 ctx 排队的异步动作（同步生产者 → 异步消费者；命令与 Control 分支共用）
pub(crate) async fn consume_ops(ctx: &mut ChatCtx<'_>) {
    while let Some(op) = ctx.ops.pop_front() {
        match op {
            AsyncOp::Cmd(c) => {
                if let Err(e) = ctx.cmd_tx.send(c).await {
                    eprintln!("{}", format!("命令发送失败: {e}").red());
                }
            }
            AsyncOp::Backup => {
                if let Err(e) = ctx.identity.backup(ctx.input, ctx.mode).await {
                    eprintln!("{}", format!("备份失败: {e}").red());
                }
            }
            AsyncOp::Listen => {
                let (tx, rx) = tokio::sync::oneshot::channel();
                if ctx.cmd_tx.send(seam::Cmd::GetListenAddr(tx)).await.is_ok() {
                    if let Ok(addrs) = rx.await {
                        super::sidebar::print_listen_addrs(&addrs, ctx.identity.my_id());
                        // GUI"我的地址"：与打印行同源同形（拼 /p2p/{节点ID}）
                        for a in &addrs {
                            crate::p2p_app::chat::display::listen_addr(format!(
                                "{a}/p2p/{}",
                                ctx.identity.my_id()
                            ));
                        }
                    }
                }
            }
        }
    }
}

/// 向命令队列排入"发命令"动作（字段级借用，可在 handler 持有其它字段借用时调用）
pub(crate) fn push_cmd(ops: &mut VecDeque<AsyncOp>, cmd: seam::Cmd) {
    ops.push_back(AsyncOp::Cmd(cmd));
}

/// 一个 1v1 会话：与某 peer 的聊天上下文（连接可多路共存）
pub(crate) struct Conversation {
    pub(crate) name: String, // 对方角色名（Hello 更新；未知为空）
    pub(crate) greeted: bool, // 是否已发过 Hello（重连后重置，避免漏问候）
    pub(crate) bye: bool, // 对方已主动退出（不再心跳/重连）
    pub(crate) pending_dial: bool, // /chat 后尚无地址，等待 mDNS 发现自动拨号
    pub(crate) send_confirmed: bool, // 未信任联系人首次发消息是否已确认（D3）
}

impl Conversation {
    pub(crate) fn new() -> Self {
        Conversation {
            name: String::new(),
            greeted: false,
            bye: false,
            pending_dial: false,
            send_confirmed: false,
        }
    }
}

/// 由 peer 解析显示名：先查 1v1 会话名，再查 L2 联系人，兜底完整节点ID
pub(crate) fn peer_name(
    peer: &PeerId,
    conversations: &HashMap<PeerId, Conversation>,
    identity: &IdentityService,
) -> String {
    if let Some(c) = conversations.get(peer) {
        if !c.name.is_empty() {
            return c.name.clone();
        }
    }
    if let Some(n) = identity.contact_name(peer) {
        return n;
    }
    peer.to_string()
}
