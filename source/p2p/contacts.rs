//! 通用联系人层：TOFU（Trust On First Use）指纹核对 + 本地联系人簿。
//! 与聊天协议无关——身份级信任，文件传输等多协议复用。

use libp2p::PeerId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;

use super::identity::cache_dir;

/// 联系人条目：peer_id 即身份指纹（公钥哈希），额外派生短指纹便于人工核对
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContactEntry {
    pub peer_id: String,
    pub name: String,
    pub fingerprint: String,
    pub verified: bool,
    pub first_seen: u64,
    pub last_seen: u64,
}

/// 本地联系人簿（TOFU：首次接触记录指纹，之后凭指纹识别，防身份切换）。
/// 明文存储——peer_id/名字本就是公开元数据
pub struct ContactBook {
    path: PathBuf,
    entries: HashMap<String, ContactEntry>,
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// SSH 风格短指纹：PeerId 字节的 SHA-256 前 16 字节，冒号分组十六进制
pub fn fingerprint_of(peer_id: &PeerId) -> String {
    let hash = Sha256::digest(peer_id.to_bytes());
    hash[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

impl ContactBook {
    fn path_for(my_peer_id: &PeerId) -> PathBuf {
        let dir = cache_dir().unwrap_or_else(|_| PathBuf::from("."));
        dir.join(format!("contacts_{my_peer_id}.json"))
    }

    pub fn load(my_peer_id: &PeerId) -> ContactBook {
        let path = Self::path_for(my_peer_id);
        let entries = match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str::<Vec<ContactEntry>>(&s).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
        .into_iter()
        .map(|e| (e.peer_id.clone(), e))
        .collect();
        ContactBook { path, entries }
    }

    pub fn save(&self) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut list: Vec<&ContactEntry> = self.entries.values().collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        if let Ok(s) = serde_json::to_string_pretty(&list) {
            let _ = std::fs::write(&self.path, s);
        }
    }

    pub fn get(&self, peer_id: &str) -> Option<&ContactEntry> {
        self.entries.get(peer_id)
    }

    pub fn verified(&self, peer_id: &str) -> bool {
        self.entries.get(peer_id).map(|e| e.verified).unwrap_or(false)
    }

    /// 首次接触插入 / 已存在则更新名字与最近见时间；verified 参数为 OR 合并
    pub fn ensure_contact(&mut self, peer: &PeerId, name: &str, verified: bool) {
        let pid = peer.to_string();
        let now = unix_now();
        match self.entries.get_mut(&pid) {
            Some(e) => {
                if !name.is_empty() {
                    e.name = name.to_string();
                }
                e.last_seen = now;
                if verified {
                    e.verified = true;
                }
            }
            None => {
                self.entries.insert(
                    pid.clone(),
                    ContactEntry {
                        peer_id: pid,
                        name: name.to_string(),
                        fingerprint: fingerprint_of(peer),
                        verified,
                        first_seen: now,
                        last_seen: now,
                    },
                );
            }
        }
        self.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::p2p::identity::keypair_from_mnemonic;

    const MNEMONIC_A: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    const MNEMONIC_B: &str =
        "legal winner thank year wave sausage worth useful legal winner thank yellow";

    #[test]
    fn fingerprint_is_stable_and_distinct() {
        let a = keypair_from_mnemonic(MNEMONIC_A).unwrap().public().to_peer_id();
        let b = keypair_from_mnemonic(MNEMONIC_B).unwrap().public().to_peer_id();
        let fa1 = fingerprint_of(&a);
        let fa2 = fingerprint_of(&a);
        let fb = fingerprint_of(&b);
        assert_eq!(fa1, fa2);
        assert_ne!(fa1, fb);
        assert_eq!(fa1.split(':').count(), 16);
    }

    #[test]
    fn contact_book_persists_across_load() {
        let dir = std::env::temp_dir().join(format!("p2p_contact_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::set_var("P2P_ID_CACHE_DIR", &dir);
        }
        let a = keypair_from_mnemonic(MNEMONIC_A).unwrap();
        let a_id = a.public().to_peer_id();
        let b = keypair_from_mnemonic(MNEMONIC_B).unwrap();
        let b_id = b.public().to_peer_id();

        {
            let mut book = ContactBook::load(&a_id);
            assert!(!book.verified(&a_id.to_string()));
            book.ensure_contact(&b_id, "bob", true);
        }
        let book = ContactBook::load(&a_id);
        assert!(book.verified(&b_id.to_string()));
        let entry = book.get(&b_id.to_string()).unwrap();
        assert_eq!(entry.name, "bob");
        assert!(!entry.fingerprint.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("P2P_ID_CACHE_DIR");
        }
    }
}
