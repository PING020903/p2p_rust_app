# -*- coding: utf-8 -*-
"""P2.5 步3 切片：chat.rs → commands.rs + session.rs（按行号精确切分）"""
import io

SRC = r"F:\cmake_study\p2p_rust_app\source\chat.rs"
OUT_CMD = r"F:\cmake_study\p2p_rust_app\source\p2p_app\chat\commands.rs"
OUT_SESS = r"F:\cmake_study\p2p_rust_app\source\p2p_app\chat\session.rs"

lines = io.open(SRC, encoding="utf-8").read().splitlines()

def sl(a, b):
    """1-based 闭区间切片"""
    return lines[a - 1 : b]

assert lines[61].startswith("fn build_tree"), lines[61]
assert lines[818] == "}", lines[818]
assert lines[820].startswith("/// 解析 `/sendStrings`"), lines[820]
assert lines[930].startswith("pub async fn run_engine"), lines[930]
assert lines[1416] == "#[cfg(test)]", lines[1416]
assert lines[35].startswith("/// 终端逃逸"), lines[35]
assert lines[51] == "}", lines[51]

cmd_header = '''//! chat 命令树：/dial /chat /list /trust /group /send /backup /q 等全部注册（CLI 文本层）。
//! 框架在 crate 根 cmd_tree.rs（全局组件）；本模块是 chat 域的注册内容——
//! 对应固件惯例：CommandParse/（框架）与 userTasks_cmds.c（域内注册）分离。

use colored::Colorize;
use libp2p::multiaddr::Protocol;
use rand::{rngs::OsRng, RngCore};
use std::collections::VecDeque;

use crate::cmd_tree::{CmdError, CmdTree, ROOT};
use crate::lineio::LineSource;
use crate::p2p::identity_service::{IdentityService, TextTag};
use crate::p2p::seam;
use crate::p2p::save_download_dir;
use crate::p2p_app::chat::ctx::{push_cmd, ChatCtx, Conversation};
use crate::p2p_app::chat::dial::{parse_dial_addr, print_dial_template};
use crate::p2p_app::chat::group::{
    dedup_members, dial_group_members, fanout_member_list, group_owner_label, group_topic,
    load_groups, next_creator, save_groups, Group,
};
use crate::p2p_app::chat::payloads::{
    GroupInvitePayload, GroupOwnerTransferPayload, TAG_CHAT_TEXT, TAG_GROUP_LEAVE,
};
use crate::p2p_app::chat::sidebar::trust_badge;
'''

sess_header = '''//! 会话主循环：run/run_engine/run_node——引擎入口与 select 事件循环（CLI/GUI 共用）。
//! 输入抽象 LineSource 已屏蔽模式差异；命令树来自 commands::build_tree。

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::io::IsTerminal;
use tokio::io::AsyncBufReadExt;

use colored::Colorize;
use libp2p::{Multiaddr, PeerId};

use crate::cmd_tree::{CmdError, CmdTree};
use crate::lineio::{InputMsg, LineSource};
use crate::p2p::identity::LoginOutcome;
use crate::p2p::identity_service::{is_l2_signal, IdentityService, TextTag};
use crate::p2p::seam::{self, Event, SignalRegistry, BYE_HANDSHAKE_TIMEOUT};
use crate::p2p::{load_discovery_mode, save_discovery_mode, save_download_dir, DiscoveryMode};
use crate::p2p_app::chat::commands::build_tree;
use crate::p2p_app::chat::control::handle_control;
use crate::p2p_app::chat::ctx::{consume_ops, make_chat_ctx, peer_name, ChatCtx, Conversation};
use crate::p2p_app::chat::display;
use crate::p2p_app::chat::group::{group_topic, save_groups, Group, GroupPayload};
use crate::p2p_app::chat::handlers::{
    display_untrusted_text, on_chat_text, on_group_invite, on_group_leave, on_group_member_list,
    on_group_owner_transfer, on_peer_bye_signal, on_peer_hello_signal, on_trust_signal, AppCtx,
};
use crate::p2p_app::chat::payloads::{ChatTextPayload, TAG_CHAT_TEXT};
use crate::p2p_app::chat::sidebar::{push_sidebar, trust_badge};
'''

cmd_body = sl(36, 52) + [""] + sl(62, 819)      # run_terminal_escape + build_tree
sess_body = sl(821, 1415)                        # sendstrings 解析/发送 + run_node 主循环
tests_body = sl(1418, 1481)                      # mod tests { use super::*; ... }（不含 #[cfg(test)] 行）

io.open(OUT_CMD, "w", encoding="utf-8", newline="\n").write(cmd_header + "\n".join(cmd_body) + "\n")
io.open(OUT_SESS, "w", encoding="utf-8", newline="\n").write(
    sess_header + "\n".join(sess_body) + "\n\n#[cfg(test)]\n" + "\n".join(tests_body) + "\n"
)
print("commands.rs:", len(cmd_header.splitlines()) + len(cmd_body), "行")
print("session.rs:", len(sess_header.splitlines()) + len(sess_body) + 2 + len(tests_body), "行")
