//! chat 应用线缆载荷：各语义 tag 的 binary 负载结构（cbor 序列化）。
//! tag 常量与载荷结构一一对应——注册表（session）与 handler 共用。

use serde::{Deserialize, Serialize};

/// chat 应用注册的自定义语义标签（text=Custom(tag) 承载协议语义）
pub(crate) const TAG_CHAT_TEXT: &str = "chat.text";
pub(crate) const TAG_GROUP_INVITE: &str = "chat.group_invite";
pub(crate) const TAG_GROUP_LEAVE: &str = "chat.group_leave";
pub(crate) const TAG_GROUP_MEMBER_LIST: &str = "chat.group_member_list";
pub(crate) const TAG_GROUP_OWNER_TRANSFER: &str = "chat.group_owner_transfer";

/// chat.text 负载
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChatTextPayload {
    pub(crate) text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GroupInvitePayload {
    pub(crate) group_id: String,
    pub(crate) group_name: String,
    pub(crate) version: u64,
    pub(crate) members: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GroupLeavePayload {
    pub(crate) group_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GroupMemberListPayload {
    pub(crate) group_id: String,
    pub(crate) version: u64,
    pub(crate) members: Vec<String>,
}

/// 群主退群时一步顺位转移：携带新群主 + 移除群主后的名单（版本门控整体替换）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GroupOwnerTransferPayload {
    pub(crate) group_id: String,
    pub(crate) new_creator: String,
    pub(crate) version: u64,
    pub(crate) members: Vec<String>,
}
