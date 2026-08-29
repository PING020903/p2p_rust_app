//! L2 传输适配层：对 L3 隐藏 L1 的 `Frame`/`control`/`P2pCommand`/`P2pEvent`。
//!
//! L3 只见到"信号语义"：`Cmd::Send { peer, tag, payload }` / `Event::Signal { from, tag, payload }`。
//! Frame 组装（text=tag + binary=payload）、control 拆解（心跳过滤）全部收口在本层，
//! 经一个适配任务在 L3 通道与 L1 通道之间翻译——L3 永远不碰 L1 的线缆帧。

use libp2p::{identity::Keypair, Multiaddr, PeerId};
use std::collections::HashMap;
use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::{mpsc, oneshot};

use super::node::{Frame, P2pCommand, P2pEvent, P2pNode};
pub use super::node::{is_global_ipv6_listen, BYE_HANDSHAKE_TIMEOUT};

/// 语义信号处理器（async）：处理一个信号（`Event::Signal` 的 tag）的 payload 负载。
/// `'borrow` 是 handler 调用时的借用生命周期，`'ctx` 是上下文（C::Ctx）内部引用的生命周期。
/// 返回是否成功处理（未注册/解析失败返回 false）。
/// 上下文经 `SignalCtx` 的 GAT 暴露带生命周期的具体类型（本项目为 `AppCtx<'a>`），
/// 使注册表可泛化于上下文而不把其生命周期钉死在注册表类型上。
pub type SignalHandler<C> = Box<
    dyn for<'borrow, 'ctx> FnMut(
            &'borrow mut <C as SignalCtx>::Ctx<'ctx>,
            &'borrow PeerId,
            Option<&'borrow [u8]>,
        ) -> Pin<Box<dyn Future<Output = bool> + 'borrow>>,
>;

/// 上下文提供者：把带生命周期的上下文类型暴露为 GAT，供 handler 对任意借用/上下文生命周期泛化
pub trait SignalCtx {
    type Ctx<'a>;
}

/// 语义注册表：tag → async handler（查表分发，无业务 match）。
/// 另维护**未互信钩子**表（按 tag）：业务信号来自未互信对端时，走该 tag 的钩子；
/// 未注册钩子的 tag 默认执行空函数（丢弃——payload 无人引用，自然回收）。
pub struct SignalRegistry<C: SignalCtx> {
    handlers: HashMap<String, SignalHandler<C>>,
    untrusted: HashMap<String, SignalHandler<C>>,
}

impl<C: SignalCtx> SignalRegistry<C> {
    pub fn new() -> Self {
        SignalRegistry {
            handlers: HashMap::new(),
            untrusted: HashMap::new(),
        }
    }

    /// 注册业务信号 handler（互信 / L2 内化信号路径）
    pub fn register<H>(&mut self, tag: &str, handler: H)
    where
        H: for<'borrow, 'ctx> FnMut(
                &'borrow mut C::Ctx<'ctx>,
                &'borrow PeerId,
                Option<&'borrow [u8]>,
            ) -> Pin<Box<dyn Future<Output = bool> + 'borrow>>
            + 'static,
    {
        self.handlers.insert(tag.to_string(), Box::new(handler));
    }

    /// L2 提供给 L3 的 API：按 tag 注册"未互信业务信号"的兜底处理（async，与应用 handler 同签名）。
    /// 不注册的 tag 默认空函数 = 丢弃。
    pub fn register_untrusted<H>(&mut self, tag: &str, handler: H)
    where
        H: for<'borrow, 'ctx> FnMut(
                &'borrow mut C::Ctx<'ctx>,
                &'borrow PeerId,
                Option<&'borrow [u8]>,
            ) -> Pin<Box<dyn Future<Output = bool> + 'borrow>>
            + 'static,
    {
        self.untrusted.insert(tag.to_string(), Box::new(handler));
    }

    /// 按标签分发并 await handler；未注册的 tag 返回 false
    pub async fn dispatch(
        &mut self,
        tag: &str,
        from: &PeerId,
        payload: Option<&[u8]>,
        ctx: &mut C::Ctx<'_>,
    ) -> bool {
        match self.handlers.get_mut(tag) {
            Some(h) => {
                let fut = h(ctx, from, payload);
                fut.await
            }
            None => false,
        }
    }

    /// 未互信业务信号：调该 tag 注册的未互信钩子；未注册则空操作（丢弃）
    pub async fn handle_untrusted(
        &mut self,
        tag: &str,
        from: &PeerId,
        payload: Option<&[u8]>,
        ctx: &mut C::Ctx<'_>,
    ) {
        if let Some(h) = self.untrusted.get_mut(tag) {
            let fut = h(ctx, from, payload);
            let _ = fut.await;
        }
    }
}


/// L3 → L1 的命令（高层语义；L1 的 Frame 由本层组装）
#[derive(Debug)]
pub enum Cmd {
    Dial { addr: Multiaddr },
    DialPeer(PeerId),
    /// 发送一个信号帧：tag=协议语义标签 + payload=该标签的 binary 负载
    Send {
        peer: PeerId,
        tag: String,
        payload: Option<Vec<u8>>,
    },
    MarkBye(PeerId),
    Subscribe { topic: String },
    Unsubscribe { topic: String },
    Publish { topic: String, data: Vec<u8> },
    GetListenAddr(oneshot::Sender<Vec<Multiaddr>>),
    Shutdown,
}

/// L1 → L3 的事件（Frame 已解开为 tag+payload；control 心跳被过滤，L3 无感）
#[derive(Debug)]
pub enum Event {
    Connected(PeerId),
    Disconnected { peer: PeerId, bye: bool },
    Discovered { peer: PeerId, addr: Multiaddr },
    /// 收到一个信号帧：tag=协议语义标签 + payload=该标签的 binary 负载
    Signal {
        from: PeerId,
        tag: String,
        payload: Option<Vec<u8>>,
    },
    Gossip { source: PeerId, data: Vec<u8> },
    SendFailure { peer: PeerId, error: String },
}

/// L3 侧传输句柄：命令发送端 + 事件接收端
pub struct Transport {
    pub cmd_tx: mpsc::Sender<Cmd>,
    pub ev_rx: mpsc::UnboundedReceiver<Event>,
}

/// 启动传输：建 L3 通道 → 建 L1 节点 → 起适配任务翻译两向通道。
/// L3 只持有 `Transport`，不接触 `P2pNode`/`P2pCommand`/`P2pEvent`。
pub fn spawn_transport(
    keypair: Keypair,
    mode: super::discovery::DiscoveryMode,
) -> Result<Transport, Box<dyn Error>> {
    let (l3_cmd_tx, mut l3_cmd_rx) = mpsc::channel::<Cmd>(32);
    let (l3_ev_tx, l3_ev_rx) = mpsc::unbounded_channel::<Event>();
    let (l1_cmd_tx, l1_cmd_rx) = mpsc::channel::<P2pCommand>(32);
    let (l1_ev_tx, mut l1_ev_rx) = mpsc::unbounded_channel::<P2pEvent>();

    let node = P2pNode::new(keypair, mode)?;
    tokio::spawn(node.run(l1_cmd_rx, l1_ev_tx));

    // 适配任务：L3 Cmd → L1 P2pCommand（组装 Frame）；L1 P2pEvent → L3 Event（拆解 Frame）
    tokio::spawn(async move {
        loop {
            tokio::select! {
                cmd = l3_cmd_rx.recv() => {
                    match cmd {
                        Some(c) => {
                            if l1_cmd_tx.send(to_l1(c)).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                ev = l1_ev_rx.recv() => {
                    match ev {
                        Some(e) => {
                            if let Some(se) = to_l2(e) {
                                if l3_ev_tx.send(se).is_err() {
                                    break;
                                }
                            }
                        }
                        None => break,
                    }
                }
            }
        }
    });

    Ok(Transport {
        cmd_tx: l3_cmd_tx,
        ev_rx: l3_ev_rx,
    })
}

/// L3 命令 → L1 命令：帧组装（control=None + text=tag + binary=payload）收口在此
fn to_l1(cmd: Cmd) -> P2pCommand {
    match cmd {
        Cmd::Dial { addr } => P2pCommand::Dial { addr },
        Cmd::DialPeer(p) => P2pCommand::DialPeer(p),
        Cmd::Send {
            peer,
            tag,
            payload,
        } => P2pCommand::Send {
            peer,
            frame: Frame {
                control: None,
                text: Some(tag),
                binary: payload,
            },
        },
        Cmd::MarkBye(p) => P2pCommand::MarkBye(p),
        Cmd::Subscribe { topic } => P2pCommand::Subscribe { topic },
        Cmd::Unsubscribe { topic } => P2pCommand::Unsubscribe { topic },
        Cmd::Publish { topic, data } => P2pCommand::Publish { topic, data },
        Cmd::GetListenAddr(tx) => P2pCommand::GetListenAddr(tx),
        Cmd::Shutdown => P2pCommand::Shutdown,
    }
}

/// L1 事件 → L3 事件：拆解 Frame（control 心跳过滤，L3 无感）；无信号内容的帧丢弃
fn to_l2(ev: P2pEvent) -> Option<Event> {
    match ev {
        P2pEvent::PeerConnected(p) => Some(Event::Connected(p)),
        P2pEvent::PeerDisconnected { peer, bye } => Some(Event::Disconnected { peer, bye }),
        P2pEvent::PeerDiscovered { peer, addr } => Some(Event::Discovered { peer, addr }),
        P2pEvent::Message { from, frame } => {
            // control 心跳是 L1 内部交通，L3 无感；只转发带 text 标签的信号帧
            if frame.control.is_some() {
                return None;
            }
            Some(Event::Signal {
                from,
                tag: frame.text?,
                payload: frame.binary,
            })
        }
        P2pEvent::Gossip { source, data } => Some(Event::Gossip { source, data }),
        P2pEvent::SendFailure { peer, error } => Some(Event::SendFailure { peer, error }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl SignalCtx for () {
        type Ctx<'a> = ();
    }

    /// 未互信钩子路由：注册了钩子的 tag 走钩子；未注册的 tag 默认空函数（丢弃，无副作用）
    #[tokio::test]
    async fn registry_routes_normal_vs_untrusted() {
        let normal_hits = std::rc::Rc::new(std::cell::Cell::new(0));
        let untrusted_hits = std::rc::Rc::new(std::cell::Cell::new(0));
        let mut reg: SignalRegistry<()> = SignalRegistry::new();

        let nh = normal_hits.clone();
        reg.register("chat.text", move |_: &mut (), _: &PeerId, _: Option<&[u8]>| {
            nh.set(nh.get() + 1);
            Box::pin(async { true })
        });
        let uh = untrusted_hits.clone();
        reg.register_untrusted("chat.text", move |_: &mut (), _: &PeerId, _: Option<&[u8]>| {
            uh.set(uh.get() + 1);
            Box::pin(async { true })
        });

        let pid = PeerId::random();
        // 互信/内化路径走正常 handler
        let handled = reg.dispatch("chat.text", &pid, None, &mut ()).await;
        assert!(handled);
        assert_eq!(normal_hits.get(), 1);
        assert_eq!(untrusted_hits.get(), 0);
        // 未互信路径走该 tag 的钩子
        reg.handle_untrusted("chat.text", &pid, None, &mut ()).await;
        assert_eq!(normal_hits.get(), 1);
        assert_eq!(untrusted_hits.get(), 1);
        // 未注册钩子的 tag：空函数 = 丢弃（无副作用）
        reg.handle_untrusted("file.offer", &pid, None, &mut ()).await;
        assert_eq!(untrusted_hits.get(), 1);
        assert_eq!(normal_hits.get(), 1);
    }
}
