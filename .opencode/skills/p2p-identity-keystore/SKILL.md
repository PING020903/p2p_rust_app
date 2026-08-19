---
name: p2p-identity-keystore
description: 本项目身份与本地 keystore 用法参考。Use when editing or generating code that touches identity, mnemonic (bip39), keystore encryption (argon2 + chacha20poly1305), login_flow, /backup, /restore, or the <peer_id>.key file format. Covers deterministic Ed25519 identity from BIP39 mnemonic and the encrypted on-disk keystore.
---

# 身份与 keystore（本项目）

## 1. 身份派生：助记词 → Ed25519

- 生成：`bip39::Mnemonic::generate_in_with(&mut OsRng, bip39::Language::English, 12)`
  （12 词 = 128 bit 熵；bip39 2.x 需 `rand` feature；`Display` 得助记词串）
- 解析/校验：`Mnemonic::parse(phrase)`（含校验和校验）
- 种子：`mnemonic.to_seed("")` → 64 字节 PBKDF2 输出，**取前 32 字节**做 Ed25519 种子
- 密钥：`Keypair::ed25519_from_bytes(seed)` → 确定性身份，同一助记词 → 同一 PeerId

```rust
fn keypair_from_mnemonic(phrase: &str) -> Result<Keypair, String> {
    let mnemonic = Mnemonic::parse(phrase.trim()).map_err(|_| "助记词无效")?;
    let seed = mnemonic.to_seed("");
    let mut out = [0u8; 32];
    out.copy_from_slice(&seed[..32]);
    Keypair::ed25519_from_bytes(out).map_err(|e| format!("生成密钥失败: {e}"))
}
```

## 2. keystore 加密（密码只护本地密钥，不参与身份）

- KDF：`argon2::Argon2id`（m=19456KiB, t=2, p=1）`hash_password_into(密码, 随机16B盐, &mut key[32])`
- 加密：`ChaCha20Poly1305` AEAD 加密助记词；Nonce 12 字节（随机）
- 解密失败（AEAD 校验）= **密码错误**，也防密文篡改
- 密码规则 8~128 字节（argon2 最低输入长度）；`valid_password()` 判断

## 3. 文件格式 `<peer_id>.key`

明文头 + 密文参数（公开元数据可明文；私密助记词加密）：

```
format=1
name=...
birthday=...
gender=...
peer_id=...
kdf_m=19456
kdf_t=2
kdf_p=1
salt=<hex 16B>
nonce=<hex 12B>
enc=<hex 密文+tag>
```

- `kdf_m/t/p` **逐文件记录** → 参数可独立升级，不改变身份
  （旧"Argon2id 参数是身份一部分、改了全换 ID"的历史约束已解除）
- 目录：`P2P_ID_CACHE_DIR` 环境变量覆盖，默认 `~/.p2p_rust_app/`
- 读取：`load_keystores()` 返回 `Vec<(Keystore, IdentityInfo)>`，损坏条目跳过

## 4. 登录流程（`login_flow`）

菜单：`[角色登录]` → 缓存身份列表 / `0` 新身份 / `r` 从助记词恢复

- **新身份**：收集资料（姓名/生日/性别，纯元数据）→ 生成助记词展示一次 →
  回输前 3 词确认 → 设密码 → 自动加密保存 keystore
- **恢复**：输助记词 → 资料 → 密码 → 自动保存 keystore
- **缓存解锁**：选号 → 密码（错 3 次回菜单），`decrypt_mnemonic` 失败即密码错误
- `/backup`：重看助记词（需再输密码解锁 keystore）
- 资料（姓名/生日/性别）**不再是身份的一部分**，仅作可签名元数据
