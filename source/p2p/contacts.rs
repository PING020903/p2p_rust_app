//! 通用联系人层：TOFU（Trust On First Use）指纹核对 + 本地联系人簿。
//! 与聊天协议无关——身份级信任，文件传输等多协议复用。

use libp2p::PeerId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;

use super::identity::cache_dir;

/// 测试共享锁：`P2P_ID_CACHE_DIR` 是进程级环境变量，凡依赖它的测试须持有此锁串行执行
#[cfg(test)]
pub(crate) static CACHE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    /// 按名字精确查找联系人（返回条目；允许多个联系人同名时取第一个）
    pub fn find_by_name(&self, name: &str) -> Option<&ContactEntry> {
        self.entries.values().find(|e| e.name == name)
    }

    /// 仅刷新最近见时间（存在层记录；不改变名字与信任状态）
    pub fn mark_seen(&mut self, peer: &PeerId) {
        let pid = peer.to_string();
        if let Some(e) = self.entries.get_mut(&pid) {
            e.last_seen = unix_now();
            self.save();
        }
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

    /// 显式置位信任状态（true/false）。条目不存在时先按 OR 合并插入兜底。
    /// 与 `ensure_contact` 的 OR 合并（只会置真）不同：`/trust !名` 取消信任走这里。
    pub fn set_verified(&mut self, peer: &PeerId, verified: bool) {
        let pid = peer.to_string();
        if !self.entries.contains_key(&pid) {
            self.ensure_contact(peer, "", false);
        }
        if let Some(e) = self.entries.get_mut(&pid) {
            e.verified = verified;
            e.last_seen = unix_now();
            self.save();
        }
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
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
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

    #[test]
    fn set_verified_can_untrust_and_retrust() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("p2p_set_ver_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::set_var("P2P_ID_CACHE_DIR", &dir);
        }
        let a = keypair_from_mnemonic(MNEMONIC_A).unwrap();
        let a_id = a.public().to_peer_id();
        let b = keypair_from_mnemonic(MNEMONIC_B).unwrap();
        let b_id = b.public().to_peer_id();

        let mut book = ContactBook::load(&a_id);
        book.ensure_contact(&b_id, "bob", true);
        assert!(book.verified(&b_id.to_string()));
        // ensure_contact OR 合并不会降级
        book.ensure_contact(&b_id, "bob", false);
        assert!(book.verified(&b_id.to_string()));
        // set_verified 显式取消
        book.set_verified(&b_id, false);
        assert!(!book.verified(&b_id.to_string()));
        // 重新信任
        book.set_verified(&b_id, true);
        assert!(book.verified(&b_id.to_string()));

        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("P2P_ID_CACHE_DIR");
        }
    }

    #[test]
    fn find_by_name_matches_contact_name() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("p2p_find_name_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::set_var("P2P_ID_CACHE_DIR", &dir);
        }
        let a = keypair_from_mnemonic(MNEMONIC_A).unwrap();
        let a_id = a.public().to_peer_id();
        let b = keypair_from_mnemonic(MNEMONIC_B).unwrap();
        let b_id = b.public().to_peer_id();

        let mut book = ContactBook::load(&a_id);
        book.ensure_contact(&b_id, "bob", true);
        let found = book.find_by_name("bob").expect("按名字应能找到");
        assert_eq!(found.peer_id, b_id.to_string());
        assert!(book.find_by_name("alice").is_none());

        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("P2P_ID_CACHE_DIR");
        }
    }
}
