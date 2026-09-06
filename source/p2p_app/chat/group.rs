//! 群域：本地注册表模型（群主中心 + 版本化名单扇出）、topic、成员拨号、持久化。
//!
//! **群主为中心**的单一权威模型：群主（creator）是成员表唯一权威——仅群主可邀请新成员、
//! 处理成员退群；每次成员变更版本 +1，并向最新名单所有成员 1v1 扇出全量名单（版本化整体替换）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use libp2p::{Multiaddr, PeerId};
use serde::{Deserialize, Serialize};

use crate::p2p_app::chat::ctx::AsyncOp;
use crate::p2p::identity_service::IdentityService;
use crate::p2p::{cache_dir, seam};
use crate::p2p_app::chat::payloads::GroupMemberListPayload;

/// 群：本地注册表（id/name/members）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Group {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) members: Vec<String>, // peer_id 字符串
    #[serde(default)]
    pub(crate) version: u64, // 成员变更计数，仅接受更高版本
    #[serde(default)]
    pub(crate) creator: String, // 群主 peer_id（唯一权威）
    /// 常驻接收（per-node 本地偏好，不随名单传播）：常驻群成员上线自动拨号维持 mesh，
    /// 普通群只在聚焦时按需连接（防"所有群都 mesh"的通讯风暴）
    #[serde(default)]
    pub(crate) resident: bool,
}

/// 群消息载荷（gossipsub data，JSON 编码）。
/// 群文本经 gossipsub 分发；成员名单由群主 1v1 扇出（见 payloads::GroupMemberListPayload），不走 gossip
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum GroupPayload {
    Text {
        group_id: String,
        text: String,
        /// 发送者自己的显示名（Signed 签名保证来源真实，名字是展示元数据）
        sender: String,
    },
}

/// 群 topic 字符串（L1 不解释 topic，直接透传；订阅/发布/接收须用同一格式）
pub(crate) fn group_topic(group_id: &str) -> String {
    format!("/group/{group_id}/v1")
}

/// 群主向目标成员 1v1 扇出名单更新（版本化整体替换）：同步排入命令队列，主循环统一消费
pub(crate) fn fanout_member_list(
    ops: &mut VecDeque<AsyncOp>,
    group_id: &str,
    version: u64,
    members: &[String],
    targets: &[PeerId],
) {
    let payload = serde_cbor::to_vec(&GroupMemberListPayload {
        group_id: group_id.to_string(),
        version,
        members: members.to_vec(),
    })
    .unwrap_or_default();
    for p in targets {
        ops.push_back(AsyncOp::Cmd(seam::Cmd::Send {
            peer: *p,
            tag: crate::p2p_app::chat::payloads::TAG_GROUP_MEMBER_LIST.to_string(),
            payload: Some(payload.clone()),
        }));
    }
}

/// 事件 handler（async）用的异步扇出：直接 await cmd_tx
pub(crate) async fn fanout_member_list_async(
    cmd_tx: &tokio::sync::mpsc::Sender<seam::Cmd>,
    group_id: &str,
    version: u64,
    members: &[String],
    targets: &[PeerId],
) {
    let payload = serde_cbor::to_vec(&GroupMemberListPayload {
        group_id: group_id.to_string(),
        version,
        members: members.to_vec(),
    })
    .unwrap_or_default();
    for p in targets {
        let _ = cmd_tx
            .send(seam::Cmd::Send {
                peer: *p,
                tag: crate::p2p_app::chat::payloads::TAG_GROUP_MEMBER_LIST.to_string(),
                payload: Some(payload.clone()),
            })
            .await;
    }
}

/// 拨号群成员（跳过自己/已连接/无已知地址）：常驻群保持 mesh 与聚焦群按需连接的共用入口
pub(crate) fn dial_group_members(
    ops: &mut VecDeque<AsyncOp>,
    g: &Group,
    my_id: &PeerId,
    connected: &HashSet<PeerId>,
    registered: &HashMap<PeerId, Vec<Multiaddr>>,
) {
    let my_id_str = my_id.to_string();
    for m in &g.members {
        if m == &my_id_str {
            continue;
        }
        let Ok(pid) = m.parse::<PeerId>() else {
            continue;
        };
        if connected.contains(&pid) {
            continue;
        }
        if registered.get(&pid).map(|a| !a.is_empty()).unwrap_or(false) {
            ops.push_back(AsyncOp::Cmd(seam::Cmd::DialPeer(pid)));
        }
    }
}

fn groups_path(my_peer_id: &PeerId) -> PathBuf {
    let dir = cache_dir().unwrap_or_else(|_| PathBuf::from("."));
    dir.join(format!("groups_{my_peer_id}.json"))
}

pub(crate) fn load_groups(my_peer_id: &PeerId) -> HashMap<String, Group> {
    let path = groups_path(my_peer_id);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<Group>>(&s).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|mut g| {
            dedup_members(&mut g.members);
            (g.id.clone(), g)
        })
        .collect()
}

pub(crate) fn save_groups(
    my_peer_id: &PeerId,
    groups: &HashMap<String, Group>,
) -> Result<(), String> {
    let path = groups_path(my_peer_id);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建群目录失败: {e}"))?;
    }
    let list: Vec<&Group> = groups.values().collect();
    let s = serde_json::to_string_pretty(&list).map_err(|e| format!("群序列化失败: {e}"))?;
    std::fs::write(&path, s).map_err(|e| format!("写入群注册表失败: {e}"))
}

/// 保序去重成员名单（幽灵/重复防御：加载、接收名单、处理退群后统一归一化）
pub(crate) fn dedup_members(members: &mut Vec<String>) {
    let mut seen: HashSet<String> = HashSet::new();
    members.retain(|m| seen.insert(m.clone()));
}

/// 群主顺位转移的"下一位"：members 数组里群主之后的下一个成员；
/// 群主在末尾时回卷取第一个非群主成员；名单只有群主返回 None（解散）
pub(crate) fn next_creator(members: &[String], creator: &str) -> Option<String> {
    let pos = members.iter().position(|m| m == creator)?;
    members[pos + 1..]
        .iter()
        .find(|m| *m != creator)
        .or_else(|| members[..pos].iter().find(|m| *m != creator))
        .cloned()
}

/// 群主标签：`群主 {昵称} ({peerID})`（昵称用 peer_name 解析；解析失败直接显 raw id）
pub(crate) fn group_owner_label(
    g: &Group,
    conversations: &HashMap<PeerId, crate::p2p_app::chat::ctx::Conversation>,
    identity: &IdentityService,
) -> String {
    match g.creator.parse::<PeerId>() {
        Ok(owner) => format!(
            "群主 {} ({owner})",
            crate::p2p_app::chat::ctx::peer_name(&owner, conversations, identity)
        ),
        Err(_) => format!("群主 {}", g.creator),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_members_keeps_order_and_removes_dups() {
        let mut m = vec!["A".into(), "B".into(), "A".into(), "C".into(), "B".into()];
        dedup_members(&mut m);
        assert_eq!(m, vec!["A", "B", "C"]);
        let mut single = vec!["X".into()];
        dedup_members(&mut single);
        assert_eq!(single, vec!["X"]);
    }

    #[test]
    fn next_creator_wraps_after_owner() {
        let members: Vec<String> = vec!["A".into(), "B".into(), "C".into()];
        assert_eq!(next_creator(&members, "A").as_deref(), Some("B"));
        assert_eq!(next_creator(&members, "B").as_deref(), Some("C"));
        // 群主在末尾：回卷取第一个非群主
        assert_eq!(next_creator(&members, "C").as_deref(), Some("A"));
        // 仅自己：无下一位（解散）
        let solo = vec!["A".into()];
        assert_eq!(next_creator(&solo, "A"), None);
        // 群主不在名单（数据异常防御）：不猜测继任者
        assert_eq!(next_creator(&members, "Z"), None);
    }
}
