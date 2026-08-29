//! L2 传输适配层：对 L3 隐藏 L1 的 `Frame`/`control`/`P2pCommand`/`P2pEvent`。
//!
//! L3 只见到"信号语义"：`Cmd::Send { peer, tag, payload }` / `Event::Signal { from, tag, payload }`。
//! Frame 组装（text=tag + binary=payload）、control 拆解（心跳过滤）全部收口在本层，
//! 经一个适配任务在 L3 通道与 L1 通道之间翻译——L3 永远不碰 L1 的线缆帧。

use libp2p::{identity::Keypair, Multiaddr, PeerId};
use std::error::Error;
use tokio::sync::{mpsc, oneshot};

use super::node::{Frame, P2pCommand, P2pEvent, P2pNode};
pub use super::node::{is_global_ipv6_listen, BYE_HANDSHAKE_TIMEOUT};

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
