//! 侧栏展示辅助：信任徽标、已发现节点视图、侧栏快照推送、监听地址打印。

use std::collections::{HashMap, HashSet};

use colored::Colorize;
use libp2p::{Multiaddr, PeerId};

use crate::p2p::identity_service::IdentityService;
use crate::p2p::seam::is_global_ipv6_listen;
use crate::p2p_app::chat::display;
use crate::p2p_app::chat::group::Group;

/// 信任徽标（对称信任）：[互信] 双向信任 / [我信任] 单方已信任 / [未信任] 默认
pub(crate) fn trust_badge(effective: bool, my_verified: bool) -> colored::ColoredString {
    if effective {
        "  [互信]".green()
    } else if my_verified {
        "  [我信任/对方未确认]".yellow()
    } else {
        "  [未信任]".yellow()
    }
}

/// 侧栏"已发现节点"视图：registered 地址表中不在联系人簿的节点（未握手）。
/// 纯函数供单测——mDNS 发现即见（不等 hello），跨网段 /dial 后亦见。
pub(crate) fn discovered_views(
    registered: &HashMap<PeerId, Vec<Multiaddr>>,
    known_contact_ids: &HashSet<String>,
    connected: &HashSet<PeerId>,
) -> Vec<crate::uievent::DiscoveredView> {
    let mut out: Vec<crate::uievent::DiscoveredView> = registered
        .keys()
        .filter(|p| !known_contact_ids.contains(&p.to_string()))
        .map(|p| crate::uievent::DiscoveredView {
            peer_id: p.to_string(),
            online: connected.contains(p),
        })
        .collect();
    out.sort_by(|a, b| a.peer_id.cmp(&b.peer_id));
    out
}

/// 侧栏快照推送：联系人（信任徽标/在线/焦点）+ 已发现节点（未握手）+ 群列表。
/// 推送点：命令处理后（/trust /chat /group 等）与每个传输事件处理后。
/// CLI 无事件通道时 no-op（display::sidebar 内部判定）。
pub(crate) fn push_sidebar(
    identity: &IdentityService,
    groups: &HashMap<String, Group>,
    connected: &HashSet<PeerId>,
    focused: &Option<PeerId>,
    focused_group: &Option<String>,
    registered: &HashMap<PeerId, Vec<Multiaddr>>,
) {
    use crate::uievent::{ContactView, GroupView};
    let entries = identity.contact_entries();
    let known: HashSet<String> = entries.iter().map(|e| e.peer_id.clone()).collect();
    let contacts = entries
        .into_iter()
        .filter_map(|e| {
            let peer: PeerId = e.peer_id.parse().ok()?;
            let name = if e.name.is_empty() {
                e.peer_id.chars().take(10).collect()
            } else {
                e.name
            };
            Some(ContactView {
                peer_id: e.peer_id,
                name,
                online: connected.contains(&peer),
                focused: *focused == Some(peer),
                effective_trusted: e.verified && e.their_trust,
                i_trust: e.verified,
            })
        })
        .collect();
    let discovered = discovered_views(registered, &known, connected);
    let mut gvs: Vec<GroupView> = groups
        .values()
        .map(|g| GroupView {
            name: g.name.clone(),
            focused: focused_group.as_deref() == Some(g.id.as_str()),
            member_count: g.members.len(),
        })
        .collect();
    gvs.sort_by(|a, b| a.name.cmp(&b.name));
    display::sidebar(contacts, discovered, gvs);
}

/// 打印本机可分享地址：全局 IPv6 直连地址（标题一次 + 逐条列出），其余监听地址另列（/listen）
pub(crate) fn print_listen_addrs(addrs: &[Multiaddr], peer_id: &PeerId) {
    let globals: Vec<&Multiaddr> = addrs
        .iter()
        .filter(|a| is_global_ipv6_listen(a))
        .collect();
    if globals.is_empty() {
        println!(
            "{}",
            "本机暂无全局 IPv6 直连地址（跨城市需中继，后续支持；若刚启动可稍后重试 /listen）"
                .yellow()
        );
    } else {
        println!(
            "{}",
            "全局IPv6直连地址（任选一条分享，对方 /dial 即连；需路由器放行该端口）:".cyan()
        );
        for a in globals {
            println!("{}", format!("  {a}/p2p/{peer_id}").cyan());
        }
        println!(
            "{}",
            "（若分享的地址失效，重新 /listen 获取最新）".dimmed()
        );
    }
    let others: Vec<&Multiaddr> = addrs
        .iter()
        .filter(|a| !is_global_ipv6_listen(a))
        .collect();
    if !others.is_empty() {
        println!("{}", "其他监听地址:".dimmed());
        for a in others {
            println!("  {a}/p2p/{peer_id}");
        }
    }
}
