//! 通用身份层：BIP39 助记词 ↔ Ed25519 确定性身份、本地加密 keystore、
//! 影子探测防同 ID 双在线。与聊天协议无关，文件传输等多协议复用。

use bip39::Mnemonic;
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Key, Nonce,
};
use colored::Colorize;
use futures::StreamExt;
use libp2p::{
    identity::Keypair, mdns, noise, swarm::SwarmEvent, tcp, yamux, Multiaddr, PeerId,
    SwarmBuilder,
};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// Argon2id KDF 参数（仅用于本地 keystore 加密；逐文件记录，可独立升级，不再绑定身份）
pub const ARGON2_M_KIB: u32 = 19456;
pub const ARGON2_T: u32 = 2;
pub const ARGON2_P: u32 = 1;

/// 新身份助记词词数（12 词 = 128 bit 熵）
pub const MNEMONIC_WORD_COUNT: usize = 12;

/// 本地 keystore：明文头只含公开资料与 KDF/密文参数，
/// 私密部分（身份助记词）用密码派生的密钥加密存放
pub struct Keystore {
    pub peer_id: String,
    pub salt: Vec<u8>,
    pub nonce: Vec<u8>,
    pub enc: Vec<u8>,
    pub kdf_m: u32,
    pub kdf_t: u32,
    pub kdf_p: u32,
}

/// 资料信息（姓名/生日/性别）：绑定在密钥上的元数据，不再参与身份派生
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityInfo {
    pub name: String,
    pub birthday: String,
    pub gender: char,
}

/// 登录结果
pub struct LoginOutcome {
    pub keypair: Keypair,
    pub info: IdentityInfo,
}

/// 助记词 → 32 字节 Ed25519 种子（BIP39 PBKDF2 输出取前 32 字节）
pub fn seed_from_mnemonic(phrase: &str) -> Result<[u8; 32], String> {
    let mnemonic = Mnemonic::parse(phrase.trim())
        .map_err(|_| "助记词无效（须为 12 个标准英文词，空格分隔）".to_string())?;
    let seed = mnemonic.to_seed("");
    let mut out = [0u8; 32];
    out.copy_from_slice(&seed[..32]);
    Ok(out)
}

pub fn keypair_from_seed(seed: [u8; 32]) -> Result<Keypair, String> {
    Keypair::ed25519_from_bytes(seed).map_err(|e| format!("生成密钥失败: {e}"))
}

/// 助记词 → 确定性 Ed25519 密钥对（同一助记词在任何机器派生同一身份）
pub fn keypair_from_mnemonic(phrase: &str) -> Result<Keypair, String> {
    keypair_from_seed(seed_from_mnemonic(phrase)?)
}

/// 生成新身份助记词（12 词 = 128 bit 熵）
pub fn generate_mnemonic() -> Result<String, String> {
    Mnemonic::generate_in_with(&mut OsRng, bip39::Language::English, MNEMONIC_WORD_COUNT)
        .map(|m| m.to_string())
        .map_err(|e| format!("生成助记词失败: {e}"))
}

/// 由密码派生 keystore 加密密钥（Argon2id）
fn kdf_key(password: &str, salt: &[u8], m: u32, t: u32, p: u32) -> Result<[u8; 32], String> {
    let params =
        argon2::Params::new(m, t, p, Some(32)).map_err(|e| format!("Argon2 参数错误: {e}"))?;
    let argon2 = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut key = [0u8; 32];
    argon2
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| format!("密钥派生失败: {e}"))?;
    Ok(key)
}

/// 加密助记词 → (密文, salt, nonce)。Argon2id 派生密钥 + ChaCha20-Poly1305 认证加密
fn encrypt_mnemonic(
    mnemonic: &str,
    password: &str,
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>), String> {
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    let key = kdf_key(password, &salt, ARGON2_M_KIB, ARGON2_T, ARGON2_P)?;
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), mnemonic.as_bytes())
        .map_err(|e| format!("加密失败: {e}"))?;
    Ok((ciphertext, salt.to_vec(), nonce.to_vec()))
}

/// 解密助记词；密码错误返回"密码错误"，密文被篡改也会失败（AEAD 完整性）
pub fn decrypt_mnemonic(
    password: &str,
    salt: &[u8],
    nonce: &[u8],
    enc: &[u8],
    m: u32,
    t: u32,
    p: u32,
) -> Result<String, String> {
    let key = kdf_key(password, salt, m, t, p)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let plain = cipher
        .decrypt(Nonce::from_slice(nonce), enc)
        .map_err(|_| "密码错误".to_string())?;
    String::from_utf8(plain).map_err(|_| "keystore 数据损坏".to_string())
}

/// 身份缓存目录：`P2P_ID_CACHE_DIR` 覆盖，默认 `~/.p2p_rust_app/`
pub fn cache_dir() -> Result<PathBuf, String> {
    if let Ok(dir) = std::env::var("P2P_ID_CACHE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .ok_or_else(|| "无法确定用户主目录".to_string())?;
    Ok(PathBuf::from(home).join(".p2p_rust_app"))
}

/// 读取目录下全部 keystore（明文头 + 密文参数），损坏条目跳过
pub fn load_keystores() -> Vec<(Keystore, IdentityInfo)> {
    let dir = match cache_dir() {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut list = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("key") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let mut field: HashMap<String, String> = HashMap::new();
        for line in content.lines() {
            if let Some((k, v)) = line.split_once('=') {
                field.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        let need = |k: &str| field.get(k).cloned();
        let (Some(name), Some(birthday), Some(gender), Some(peer_id), Some(salt), Some(nonce), Some(enc)) =
            (
                need("name"),
                need("birthday"),
                need("gender"),
                need("peer_id"),
                need("salt"),
                need("nonce"),
                need("enc"),
            )
        else {
            continue;
        };
        let (Some(salt_b), Some(nonce_b), Some(enc_b)) = (
            hex::decode(&salt).ok(),
            hex::decode(&nonce).ok(),
            hex::decode(&enc).ok(),
        ) else {
            continue;
        };
        let Some(gender_c) = gender.chars().next() else {
            continue;
        };
        let kdf_m = field
            .get("kdf_m")
            .and_then(|v| v.parse().ok())
            .unwrap_or(ARGON2_M_KIB);
        let kdf_t = field
            .get("kdf_t")
            .and_then(|v| v.parse().ok())
            .unwrap_or(ARGON2_T);
        let kdf_p = field
            .get("kdf_p")
            .and_then(|v| v.parse().ok())
            .unwrap_or(ARGON2_P);
        list.push((
            Keystore {
                peer_id,
                salt: salt_b,
                nonce: nonce_b,
                enc: enc_b,
                kdf_m,
                kdf_t,
                kdf_p,
            },
            IdentityInfo {
                name,
                birthday,
                gender: gender_c,
            },
        ));
    }
    list.sort_by(|a, b| a.1.name.cmp(&b.1.name));
    list
}

/// 加密保存 keystore（自动落盘；明文头含公开资料，私密助记词加密）
pub fn save_keystore(
    info: &IdentityInfo,
    peer_id: &PeerId,
    mnemonic: &str,
    password: &str,
) -> Result<(), String> {
    let dir = cache_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建缓存目录失败: {e}"))?;
    let (ciphertext, salt, nonce) = encrypt_mnemonic(mnemonic, password)?;
    let path = dir.join(format!("{peer_id}.key"));
    let content = format!(
        "format=1\nname={}\nbirthday={}\ngender={}\npeer_id={peer_id}\n\
         kdf_m={ARGON2_M_KIB}\nkdf_t={ARGON2_T}\nkdf_p={ARGON2_P}\n\
         salt={}\nnonce={}\nenc={}\n",
        info.name,
        info.birthday,
        info.gender,
        hex::encode(&salt),
        hex::encode(&nonce),
        hex::encode(&ciphertext)
    );
    std::fs::write(&path, content).map_err(|e| format!("写入 keystore 失败: {e}"))?;
    println!(
        "{}",
        format!("身份已保存（加密）: {}", path.display()).dimmed()
    );
    Ok(())
}

pub fn valid_password(pwd: &str) -> bool {
    (8..=128).contains(&pwd.len())
}

/// 影子探测：用一次性随机身份（仅 mDNS，不监听不聊天）探测局域网内是否已存在
/// 与真实 ID 相同的节点。libp2p 的 mDNS 库会把与"本机身份"同 PeerId 的节点
/// 过滤掉（behaviour/iface/query.rs 的 filter），因此必须借影子身份绕过。
/// 仅 `AdvertiseAndDiscover` 发现模式有效（隐身/关闭模式不广播自身，探测无意义）。
pub async fn probe_duplicate_id(
    real_id: PeerId,
    window: Duration,
) -> Result<Option<Multiaddr>, Box<dyn std::error::Error>> {
    let mut probe_swarm = SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(|key| {
            let peer_id = key.public().to_peer_id();
            mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id)
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
        })?
        .build();
    println!(
        "{}",
        format!(
            "正在探测局域网内是否存在同 ID 节点（{} 秒）...",
            window.as_secs()
        )
        .dimmed()
    );
    let deadline = tokio::time::Instant::now() + window;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return Ok(None),
            event = probe_swarm.select_next_some() => {
                if let SwarmEvent::Behaviour(mdns::Event::Discovered(list)) = event {
                    if let Some((_, addr)) = list.iter().find(|(p, _)| *p == real_id) {
                        return Ok(Some(addr.clone()));
                    }
                }
            }
        }
    }
}

pub fn probe_window() -> Duration {
    std::env::var("P2P_ID_PROBE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(5))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BIP39 官方测试向量助记词（仅测试用）
    const MNEMONIC_A: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    const MNEMONIC_B: &str =
        "legal winner thank year wave sausage worth useful legal winner thank yellow";

    #[test]
    fn mnemonic_identity_deterministic() {
        let ka = keypair_from_mnemonic(MNEMONIC_A).unwrap();
        let ka2 = keypair_from_mnemonic(MNEMONIC_A).unwrap();
        let kb = keypair_from_mnemonic(MNEMONIC_B).unwrap();
        assert_eq!(ka.public().to_peer_id(), ka2.public().to_peer_id());
        assert_ne!(ka.public().to_peer_id(), kb.public().to_peer_id());
    }

    #[test]
    fn mnemonic_invalid_rejected() {
        assert!(seed_from_mnemonic("this is not a valid bip39 phrase").is_err());
        assert!(seed_from_mnemonic("").is_err());
        assert!(keypair_from_mnemonic(MNEMONIC_A).is_ok());
    }

    #[test]
    fn generated_mnemonic_is_valid() {
        let phrase = generate_mnemonic().unwrap();
        let words: Vec<&str> = phrase.split_whitespace().collect();
        assert_eq!(words.len(), MNEMONIC_WORD_COUNT);
        assert!(keypair_from_mnemonic(&phrase).is_ok());
    }

    #[test]
    fn keystore_encrypt_decrypt_roundtrip() {
        let (enc, salt, nonce) = encrypt_mnemonic(MNEMONIC_A, "password-123").unwrap();
        let out = decrypt_mnemonic(
            "password-123",
            &salt,
            &nonce,
            &enc,
            ARGON2_M_KIB,
            ARGON2_T,
            ARGON2_P,
        )
        .unwrap();
        assert_eq!(out, MNEMONIC_A);
    }

    #[test]
    fn keystore_wrong_password_rejected() {
        let (enc, salt, nonce) = encrypt_mnemonic(MNEMONIC_A, "password-123").unwrap();
        let err = decrypt_mnemonic(
            "wrong-password",
            &salt,
            &nonce,
            &enc,
            ARGON2_M_KIB,
            ARGON2_T,
            ARGON2_P,
        )
        .unwrap_err();
        assert_eq!(err, "密码错误");
    }

    #[test]
    fn password_length_rule() {
        assert!(valid_password("12345678"));
        assert!(valid_password(&"x".repeat(128)));
        assert!(!valid_password("1234567"));
        assert!(!valid_password(""));
        assert!(!valid_password(&"x".repeat(129)));
    }
}
