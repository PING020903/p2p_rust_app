//! 通用 P2P 通讯层：身份/keystore、联系人簿（TOFU）、mDNS 发现模式、
//! 隐身监听器、身份基础服务（L2）、传输层（L1）。与聊天协议无关，文件传输等多协议复用。
//!
//! 各子模块：
//! - `identity`   助记词↔Ed25519、keystore 加解密、影子探测
//! - `contacts`   TOFU 联系人簿 + 指纹
//! - `identity_service` L2 身份基础服务（登录会话 + 信任判定 + Hello/Bye 存在处理）
//! - `node`       L1 传输层（线缆帧 / NodeBehaviour / 重连助手）
//! - `discovery`  发现模式（advertise/stealth/off）
//! - `mdns_stealth` 隐身模式只收不发的 mDNS 监听器

pub mod contacts;
pub mod discovery;
pub mod identity;
pub mod identity_service;
pub mod mdns_stealth;
pub mod node;

pub use discovery::*;
pub use identity::*;
