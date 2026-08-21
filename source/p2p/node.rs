//! L1 传输层：协议无关的线缆帧、swarm 行为（NodeBehaviour）、连接/重连/发现/
//! 心跳管理，以及独立运行的传输任务（`P2pNode::run`）。
//!
//! 本模块只回答"怎么连、怎么发、怎么保活"，不感知聊天/群等业务语义——
//! Hello/Bye 等节点信号经 text 通道原样透传，由上层（L2/L3）解释。
//! 传输任务**永不阻塞于应用任务**：应用卡在 TOFU/密码等交互时，心跳照常维持。

use colored::Colorize;
use futures::StreamExt;
use libp2p::{
    gossipsub::{self, IdentTopic, MessageAuthenticity},
    identity::Keypair, mdns, multiaddr::Protocol, noise, ping,
    request_response::{self, ProtocolSupport},
    swarm::behaviour::toggle::Toggle,
    swarm::SwarmEvent,
    tcp, yamux, Multiaddr, PeerId, StreamProtocol, Swarm, SwarmBuilder,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error;
use std::net::Ipv6Addr;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use super::discovery::DiscoveryMode;
use super::mdns_stealth::StealthMdns;

/// control 字段：传输层控制指令（心跳维持）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Control {
    Heartbeat,
}

/// text 字段：节点间短消息（**非用户内容**），可扩展自定义操作信号
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeMsg {
    Hello(String), // 上线 + 对方名字
    Bye,           // 下线
}

/// 1v1 通道通用帧：control（传输控制）/ text（节点信号）/ binary（用户内容）按需填充。
/// L1 对 text/binary 内容不解释，只负责透传。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    pub control: Option<Control>,
    pub text: Option<NodeMsg>,
    pub binary: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest(pub Frame);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse(pub bool);

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(15);
pub const BYE_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// 应用任务 → 传输任务的命令（协议无关；topic 为不透明字符串，L1 不解释）
#[derive(Debug, Clone)]
pub enum P2pCommand {
    Dial { addr: Multiaddr },
    /// 拨号已知地址的 peer（按 known_addrs 逐个尝试）
    DialPeer(PeerId),
    Send { peer: PeerId, frame: Frame },
    /// 标记对方已主动退出：不再心跳、断开后不再自动重连（hello 之外的协议由 L2/L3 解释）
    MarkBye(PeerId),
    Subscribe { topic: String },
    Unsubscribe { topic: String },
    Publish { topic: String, data: Vec<u8> },
    Shutdown,
}

/// 传输任务 → 应用任务的事件
#[derive(Debug)]
pub enum P2pEvent {
    PeerConnected(PeerId),
    PeerDisconnected { peer: PeerId, bye: bool },
    PeerDiscovered { peer: PeerId, addr: Multiaddr },
    Message { from: PeerId, frame: Frame },
    Gossip { source: PeerId, data: Vec<u8> },
    SendFailure { peer: PeerId, error: String },
}

/// mDNS 行为开关：Advertise 模式启用 libp2p-mdns；隐身/关闭模式用 Toggle 关闭
#[derive(libp2p::swarm::NetworkBehaviour)]
pub struct NodeBehaviour {
    pub mdns: Toggle<mdns::tokio::Behaviour>,
    pub gossipsub: gossipsub::Behaviour,
    pub ping: ping::Behaviour,
    pub chat: request_response::cbor::Behaviour<ChatRequest, ChatResponse>,
}

pub fn build_swarm(
    keypair: Keypair,
    mode: DiscoveryMode,
) -> Result<Swarm<NodeBehaviour>, Box<dyn Error>> {
    let swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(|key| {
            let peer_id = key.public().to_peer_id();
            let mdns = match mode {
                DiscoveryMode::AdvertiseAndDiscover => {
                    Some(mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id)?)
                }
                DiscoveryMode::DiscoverOnly | DiscoveryMode::Off => None,
            };
            let gossipsub = gossipsub::Behaviour::new(
                MessageAuthenticity::Signed(key.clone()),
                gossipsub::Config::default(),
            )
            .map_err(|e| Box::new(std::io::Error::other(e)) as Box<dyn std::error::Error + Send + Sync>)?;
            Ok(NodeBehaviour {
                mdns: Toggle::from(mdns),
                gossipsub,
                ping: ping::Behaviour::default(),
                chat: request_response::cbor::Behaviour::new(
                    [(StreamProtocol::new("/chat/7.0.0"), ProtocolSupport::Full)],
                    request_response::Config::default(),
                ),
            })
        })?
        .build();
    Ok(swarm)
}

/// L1 传输节点：持有 swarm 与全部连接/地址/重连/心跳状态，在独立任务中运行。
pub struct P2pNode {
    swarm: Swarm<NodeBehaviour>,
    /// 连接计数（多路复用下可 >1），只在 0→1 / 1→0 时向应用上报
    conn_count: HashMap<PeerId, u32>,
    dialing: HashSet<PeerId>,
    /// 已知地址簿（发现/dial 登记；重连耗尽清空）
    known_addrs: HashMap<PeerId, Vec<Multiaddr>>,
    reconnect_peer: Option<PeerId>,
    reconnect_pending: Vec<Multiaddr>,
    reconnect_queue: VecDeque<PeerId>,
    /// 用户手动拨号集合：失败时报红色（非自动恢复）
    user_dials: HashSet<PeerId>,
    /// 已主动退出的 peer：不心跳、断开不重连
    bye_peers: HashSet<PeerId>,
    /// 各 peer 最近收到 chat 消息/响应的时间（心跳超时判离线）
    last_rx: HashMap<PeerId, Instant>,
    heartbeat: tokio::time::Interval,
    /// 隐身模式监听上报通道
    stealth_rx: Option<mpsc::Receiver<(PeerId, Multiaddr)>>,
    v6_listen_issued: bool,
}

impl P2pNode {
    /// 构建 swarm 并开始监听（tcp/0 + 后续 ip6 复用端口）
    pub fn new(keypair: Keypair, mode: DiscoveryMode) -> Result<Self, Box<dyn Error>> {
        let mut swarm = build_swarm(keypair, mode)?;
        swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;
        let stealth_rx = if mode == DiscoveryMode::DiscoverOnly {
            let (tx, rx) = mpsc::channel(32);
            tokio::spawn(async move {
                let mut listener = match StealthMdns::new() {
                    Ok(l) => l,
                    Err(e) => {
                        eprintln!("{}", format!("隐身监听启动失败: {e}").yellow());
                        return;
                    }
                };
                loop {
                    match listener.next_discovery().await {
                        Some((pid, addr)) => {
                            if tx.send((pid, addr)).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
            });
            Some(rx)
        } else {
            None
        };
        Ok(P2pNode {
            swarm,
            conn_count: HashMap::new(),
            dialing: HashSet::new(),
            known_addrs: HashMap::new(),
            reconnect_peer: None,
            reconnect_pending: Vec::new(),
            reconnect_queue: VecDeque::new(),
            user_dials: HashSet::new(),
            bye_peers: HashSet::new(),
            last_rx: HashMap::new(),
            heartbeat: tokio::time::interval(HEARTBEAT_INTERVAL),
            stealth_rx,
            v6_listen_issued: false,
        })
    }

    /// 传输任务主循环：命令 / 心跳 / 隐身发现 / swarm 事件。
    /// 命令通道关闭或收到 Shutdown 时退出。
    pub async fn run(
        mut self,
        mut commands: mpsc::Receiver<P2pCommand>,
        events: mpsc::UnboundedSender<P2pEvent>,
    ) {
        loop {
            tokio::select! {
                cmd = commands.recv() => {
                    match cmd {
                        Some(c) => {
                            if self.apply_command(c) {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                _ = self.heartbeat.tick() => self.heartbeat_tick(),
                discovered = async {
                    match self.stealth_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some((pid, addr)) = discovered {
                        self.on_discovered(pid, addr, &events);
                    }
                }
                event = self.swarm.select_next_some() => {
                    self.on_swarm_event(event, &events);
                }
            }
        }
    }

    /// 执行应用命令；返回 true 表示应退出传输任务（Shutdown）
    fn apply_command(&mut self, cmd: P2pCommand) -> bool {
        match cmd {
            P2pCommand::Dial { addr } => {
                let target = addr.iter().find_map(|p| match p {
                    Protocol::P2p(pid) => Some(pid),
                    _ => None,
                });
                if let Some(p) = target {
                    let recorded = self.known_addrs.entry(p).or_default();
                    if !recorded.contains(&addr) {
                        recorded.push(addr.clone());
                    }
                }
                match self.swarm.dial(addr) {
                    Ok(()) => {
                        if let Some(p) = target {
                            self.dialing.insert(p);
                            self.user_dials.insert(p);
                        }
                    }
                    Err(e) => eprintln!("{}", format!("拨号失败: {e}").red()),
                }
            }
            P2pCommand::DialPeer(peer) => {
                enqueue_reconnect(peer, &mut self.reconnect_queue, &self.reconnect_peer);
                dial_next_reconnect(
                    &mut self.swarm,
                    &mut self.dialing,
                    &mut self.reconnect_peer,
                    &mut self.reconnect_pending,
                    &mut self.known_addrs,
                    &mut self.reconnect_queue,
                );
            }
            P2pCommand::Send { peer, frame } => {
                self.send_chat(&peer, frame);
            }
            P2pCommand::MarkBye(peer) => {
                self.bye_peers.insert(peer);
            }
            P2pCommand::Subscribe { topic } => {
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .gossipsub
                    .subscribe(&IdentTopic::new(&topic));
            }
            P2pCommand::Unsubscribe { topic } => {
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .gossipsub
                    .unsubscribe(&IdentTopic::new(&topic));
            }
            P2pCommand::Publish { topic, data } => {
                if let Err(e) = self
                    .swarm
                    .behaviour_mut()
                    .gossipsub
                    .publish(IdentTopic::new(&topic), data)
                {
                    eprintln!("{}", format!("群消息发送失败: {e}").yellow());
                }
            }
            P2pCommand::Shutdown => return true,
        }
        false
    }

    fn send_chat(&mut self, peer: &PeerId, frame: Frame) {
        self.swarm
            .behaviour_mut()
            .chat
            .send_request(peer, ChatRequest(frame));
    }

    /// 心跳：对全部已连接非 bye 的 peer 保活；超时判离线并断开
    fn heartbeat_tick(&mut self) {
        let connected: Vec<PeerId> = self.conn_count.keys().copied().collect();
        for p in connected {
            if self.bye_peers.contains(&p) {
                continue;
            }
            if self
                .last_rx
                .get(&p)
                .is_some_and(|t| t.elapsed() > HEARTBEAT_TIMEOUT)
            {
                println!(
                    "{}",
                    format!(
                        "心跳超时（超过 {} 秒无响应），判定对方离线: {p}",
                        HEARTBEAT_TIMEOUT.as_secs()
                    )
                    .yellow()
                );
                self.last_rx.remove(&p);
                let _ = self.swarm.disconnect_peer_id(p);
                continue;
            }
            self.send_chat(
                &p,
                Frame {
                    control: Some(Control::Heartbeat),
                    text: None,
                    binary: None,
                },
            );
        }
    }

    /// 节点被发现（mDNS 广播 或 隐身监听）：登记地址并上报（是否拨号由应用决策）
    fn on_discovered(
        &mut self,
        found_id: PeerId,
        addr: Multiaddr,
        events: &mpsc::UnboundedSender<P2pEvent>,
    ) {
        if found_id == *self.swarm.local_peer_id() {
            return;
        }
        println!("{}", format!("mDNS 发现节点: {found_id}").cyan());
        let recorded = self.known_addrs.entry(found_id).or_default();
        if !recorded.contains(&addr) {
            recorded.push(addr.clone());
        }
        let _ = events.send(P2pEvent::PeerDiscovered {
            peer: found_id,
            addr,
        });
    }

    /// 处理 swarm 事件：连接/断开/拨号失败/发现/消息 → 上报应用 + 维护 L1 状态
    fn on_swarm_event(
        &mut self,
        event: SwarmEvent<NodeBehaviourEvent>,
        events: &mpsc::UnboundedSender<P2pEvent>,
    ) {
        match event {
            SwarmEvent::NewListenAddr { address, .. } => {
                println!(
                    "{}",
                    format!("监听地址: {address}/p2p/{}", self.swarm.local_peer_id()).green()
                );
                if !self.v6_listen_issued
                    && address.iter().any(|p| matches!(p, Protocol::Ip4(_)))
                {
                    self.v6_listen_issued = true;
                    let port = address.iter().find_map(|p| match p {
                        Protocol::Tcp(port) => Some(port),
                        _ => None,
                    });
                    let mut v6_addr = Multiaddr::empty();
                    v6_addr.push(Protocol::Ip6(Ipv6Addr::UNSPECIFIED));
                    v6_addr.push(Protocol::Tcp(port.unwrap_or(0)));
                    if let Err(e) = self.swarm.listen_on(v6_addr) {
                        eprintln!(
                            "{}",
                            format!("ip6 复用 ip4 端口监听失败({e})，改用随机端口").yellow()
                        );
                        if let Ok(fallback) = "/ip6/::/tcp/0".parse::<Multiaddr>() {
                            let _ = self.swarm.listen_on(fallback);
                        }
                    }
                }
            }
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                self.dialing.remove(&peer_id);
                if self.reconnect_peer == Some(peer_id) {
                    self.reconnect_peer = None;
                    self.reconnect_pending.clear();
                    dial_next_reconnect(
                        &mut self.swarm,
                        &mut self.dialing,
                        &mut self.reconnect_peer,
                        &mut self.reconnect_pending,
                        &mut self.known_addrs,
                        &mut self.reconnect_queue,
                    );
                }
                let was_connected = self.conn_count.contains_key(&peer_id);
                *self.conn_count.entry(peer_id).or_insert(0) += 1;
                self.last_rx.insert(peer_id, Instant::now());
                if !was_connected {
                    let _ = events.send(P2pEvent::PeerConnected(peer_id));
                }
            }
            SwarmEvent::ConnectionClosed {
                peer_id,
                num_established,
                cause,
                ..
            } => {
                let cause_text = match &cause {
                    Some(c) => format!("，原因: {c}"),
                    None => String::new(),
                };
                println!(
                    "{}",
                    format!(
                        "连接已关闭: {peer_id}（剩余连接 {num_established}{cause_text}）"
                    )
                    .yellow()
                );
                if num_established == 0 {
                    self.dialing.remove(&peer_id);
                    self.last_rx.remove(&peer_id);
                    self.conn_count.remove(&peer_id);
                    let bye = self.bye_peers.contains(&peer_id);
                    if bye {
                        self.known_addrs.remove(&peer_id);
                        println!("{}", "对方已正常退出，不进行重连".dimmed());
                    } else if let Some(addrs) = self.known_addrs.get(&peer_id) {
                        if !addrs.is_empty() {
                            println!("{}", format!("尝试重连 {peer_id}...").cyan());
                            enqueue_reconnect(peer_id, &mut self.reconnect_queue, &self.reconnect_peer);
                            dial_next_reconnect(
                                &mut self.swarm,
                                &mut self.dialing,
                                &mut self.reconnect_peer,
                                &mut self.reconnect_pending,
                                &mut self.known_addrs,
                                &mut self.reconnect_queue,
                            );
                        }
                    }
                    let _ = events.send(P2pEvent::PeerDisconnected { peer: peer_id, bye });
                } else {
                    self.conn_count.insert(peer_id, num_established);
                }
            }
            SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                if let Some(p) = peer_id {
                    if self.user_dials.remove(&p) {
                        eprintln!("{}", format!("拨号 {p} 失败: {error}").red());
                    } else {
                        eprintln!(
                            "{}",
                            format!("拨号 {p} 失败（自动恢复中）: {error}").dimmed()
                        );
                    }
                    self.dialing.remove(&p);
                    if self.reconnect_peer == Some(p) {
                        dial_next_reconnect(
                            &mut self.swarm,
                            &mut self.dialing,
                            &mut self.reconnect_peer,
                            &mut self.reconnect_pending,
                            &mut self.known_addrs,
                            &mut self.reconnect_queue,
                        );
                    } else if let Some(addrs) = self.known_addrs.get(&p) {
                        if !addrs.is_empty() {
                            println!(
                                "{}",
                                format!("拨号失败，尝试 {p} 的其他已知地址...").cyan()
                            );
                            enqueue_reconnect(p, &mut self.reconnect_queue, &self.reconnect_peer);
                            dial_next_reconnect(
                                &mut self.swarm,
                                &mut self.dialing,
                                &mut self.reconnect_peer,
                                &mut self.reconnect_pending,
                                &mut self.known_addrs,
                                &mut self.reconnect_queue,
                            );
                        }
                    }
                } else {
                    eprintln!("{}", format!("拨号失败: {error}").red());
                }
            }
            SwarmEvent::Behaviour(NodeBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                for (found_id, addr) in list {
                    self.on_discovered(found_id, addr, events);
                }
            }
            SwarmEvent::Behaviour(NodeBehaviourEvent::Gossipsub(
                gossipsub::Event::Message { message, .. },
            )) => {
                if let Some(src) = message.source {
                    let _ = events.send(P2pEvent::Gossip {
                        source: src,
                        data: message.data,
                    });
                }
            }
            SwarmEvent::Behaviour(NodeBehaviourEvent::Chat(request_response::Event::Message {
                peer: from,
                message,
                ..
            })) => {
                match message {
                    request_response::Message::Request {
                        request, channel, ..
                    } => {
                        self.last_rx.insert(from, Instant::now());
                        let _ = events.send(P2pEvent::Message { from, frame: request.0 });
                        let _ = self
                            .swarm
                            .behaviour_mut()
                            .chat
                            .send_response(channel, ChatResponse(true));
                    }
                    request_response::Message::Response { .. } => {
                        self.last_rx.insert(from, Instant::now());
                    }
                }
            }
            SwarmEvent::Behaviour(NodeBehaviourEvent::Chat(
                request_response::Event::OutboundFailure { peer: p, error, .. },
            )) => {
                let _ = events.send(P2pEvent::SendFailure {
                    peer: p,
                    error: format!("{error}"),
                });
            }
            _ => {}
        }
    }
}

/// 将 peer 加入重连队列（不在当前重连中、且未在队列里才入队）
fn enqueue_reconnect(
    peer: PeerId,
    queue: &mut VecDeque<PeerId>,
    reconnect_peer: &Option<PeerId>,
) {
    if *reconnect_peer != Some(peer) && !queue.contains(&peer) {
        queue.push_back(peer);
    }
}

/// 重连驱动：依次处理重连队列，逐个尝试目标 peer 的已知地址。
/// 当前无在途目标时从队列取队首；地址耗尽则报失败、清地址、继续下一目标
fn dial_next_reconnect(
    swarm: &mut Swarm<NodeBehaviour>,
    dialing: &mut HashSet<PeerId>,
    reconnect_peer: &mut Option<PeerId>,
    pending: &mut Vec<Multiaddr>,
    known_addrs: &mut HashMap<PeerId, Vec<Multiaddr>>,
    queue: &mut VecDeque<PeerId>,
) {
    loop {
        if reconnect_peer.is_none() {
            let p = match queue.pop_front() {
                Some(p) => p,
                None => return,
            };
            *reconnect_peer = Some(p);
            *pending = known_addrs.get(&p).cloned().unwrap_or_default();
        }
        let target = reconnect_peer.as_ref().unwrap().clone();
        while let Some(ma) = pending.pop() {
            if swarm.dial(ma).is_ok() {
                dialing.insert(target);
                return;
            }
        }
        known_addrs.remove(&target);
        *reconnect_peer = None;
        eprintln!(
            "{}",
            format!("重连失败: {target} 的已知地址均无法连接，对方可能已退出").yellow()
        );
    }
}
