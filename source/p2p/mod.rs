//! 通用 P2P 通讯层：身份/keystore、联系人簿（TOFU）、mDNS 发现模式、
//! 隐身监听器。与聊天协议无关，文件传输等多协议复用。
//!
//! 各子模块：
//! - `identity`   助记词↔Ed25519、keystore 加解密、影子探测
//! - `contacts`   TOFU 联系人簿 + 指纹
//! - `discovery`  发现模式（advertise/stealth/off）
//! - `mdns_stealth` 隐身模式只收不发的 mDNS 监听器

pub mod contacts;
pub mod discovery;
pub mod identity;
pub mod mdns_stealth;

pub use contacts::*;
pub use discovery::*;
pub use identity::*;
