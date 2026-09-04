//! 登录纯逻辑共享内核：CLI 文本流程与 GUI 表单共用的校验/编排函数。
//!
//! 零 I/O、零渲染——错误以字符串返回，提示文案与红黄染色由前端负责；
//! 流程策略（重试次数、提示顺序、UX 分叉如密码二次确认）留在各前端，
//! 本内核保证**规则语义单一来源**（词数、密码规则、解锁与保存路径）。

use crate::p2p::identity::{
    decrypt_mnemonic, keypair_from_mnemonic, load_keystores, save_keystore, valid_password,
    IdentityInfo, LoginOutcome,
};
use crate::p2p::identity_service::normalize_birthday;

/// 姓名校验：非空且不超过 64 字节
pub fn validate_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() || name.len() > 64 {
        return Err("姓名不能为空且不超过 64 字节".into());
    }
    Ok(name.to_string())
}

/// 生日校验（YYYY-MM-DD，容错单位数月/日，越界报错；规则在 L2 normalize_birthday）
pub fn validate_birthday(raw: &str) -> Result<String, String> {
    normalize_birthday(raw)
}

/// 密码规则校验（8~128 字节）
pub fn validate_password(pwd: &str) -> Result<String, String> {
    if !valid_password(pwd) {
        return Err("密码须为 8~128 字节".into());
    }
    Ok(pwd.to_string())
}

/// 新身份/恢复密码设置：规则 + 两次输入一致（GUI 二次确认 UX；CLI 单次入口不走此函数）
pub fn check_password_pair(pwd: &str, pwd2: &str) -> Result<String, String> {
    let pwd = validate_password(pwd)?;
    if pwd != pwd2 {
        return Err("两次输入的密码不一致".into());
    }
    Ok(pwd)
}

/// 助记词抄写确认：输入词与助记词前 `words` 个词一致
/// （大小写不敏感且宽容首尾空白——比 CLI 历史实现更宽容，属超集不影响 e2e 标准输入；
/// 长度守卫必须有：空输入/词数不足一律不通过，也防 zip 空迭代器的空真陷阱）
pub fn confirm_first_words(phrase: &str, answers: &[String], words: usize) -> bool {
    let first: Vec<&str> = phrase.split_whitespace().take(words).collect();
    answers.len() >= words
        && answers
            .iter()
            .zip(first.iter())
            .all(|(got, want)| got.trim().eq_ignore_ascii_case(want))
}

/// 缓存解锁：解密助记词 → 派生身份并核对 keystore 归属。
/// 密码规则校验由前端先调 [`validate_password`]（各前端错误呈现不同）。
/// 序号越界返回错误（CLI 前端自先做边界检查，不会触达此分支）。
pub fn unlock_cached(index: usize, password: &str) -> Result<LoginOutcome, String> {
    let cached = load_keystores();
    let Some((ks, info)) = cached.get(index - 1) else {
        return Err("该身份不存在，请返回菜单刷新".into());
    };
    match decrypt_mnemonic(
        password,
        &ks.salt,
        &ks.nonce,
        &ks.enc,
        ks.kdf_m,
        ks.kdf_t,
        ks.kdf_p,
    ) {
        Ok(phrase) => match keypair_from_mnemonic(&phrase) {
            Ok(kp) if kp.public().to_peer_id().to_string() == ks.peer_id => Ok(LoginOutcome {
                keypair: kp,
                info: IdentityInfo {
                    name: info.name.clone(),
                    birthday: info.birthday.clone(),
                    gender: info.gender,
                },
            }),
            Ok(_) => Err("keystore 与派生身份不符，数据可能损坏".into()),
            Err(reason) => Err(reason),
        },
        Err(reason) => Err(reason),
    }
}

/// 由助记词 + 密码派生身份并加密保存 keystore（新建/恢复/表单向导共用保存路径）。
/// 助记词非法或保存失败返回原错误文本。
pub fn persist_identity(
    profile: IdentityInfo,
    phrase: &str,
    password: &str,
) -> Result<LoginOutcome, String> {
    let keypair = keypair_from_mnemonic(phrase)?;
    let peer_id = keypair.public().to_peer_id();
    save_keystore(&profile, &peer_id, phrase, password)?;
    Ok(LoginOutcome { keypair, info: profile })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::p2p::contacts::CACHE_TEST_LOCK;

    const PHRASE_A: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    #[test]
    fn name_and_birthday_validation() {
        assert!(validate_name("").is_err());
        assert!(validate_name("   ").is_err());
        assert!(validate_name(&"a".repeat(65)).is_err());
        assert_eq!(validate_name("  alice ").unwrap(), "alice");

        assert!(validate_birthday("1990/01/01").is_err());
        assert!(validate_birthday("1990-13-01").is_err());
        assert_eq!(validate_birthday("1990-1-1").unwrap(), "1990-01-01");
    }

    #[test]
    fn password_rules_and_pair() {
        assert_eq!(
            validate_password("short"),
            Err("密码须为 8~128 字节".into())
        );
        assert!(validate_password("password-123").is_ok());
        assert_eq!(
            check_password_pair("password-123", "password-456"),
            Err("两次输入的密码不一致".into())
        );
        assert!(check_password_pair("password-123", "password-123").is_ok());
    }

    #[test]
    fn confirm_words_case_insensitive() {
        let words = 3;
        // PHRASE_A 前 3 词均为 abandon——用大小写/空白变体验证宽容度
        let answers: Vec<String> = vec![
            " Abandon ".to_string(),
            "ABANDON".to_string(),
            "abandon".to_string(),
        ];
        assert!(confirm_first_words(PHRASE_A, &answers, words));
        let wrong: Vec<String> = vec!["abandon".to_string(), "wrong".to_string(), "abandon".to_string()];
        assert!(!confirm_first_words(PHRASE_A, &wrong, words));
        // 空输入不算确认
        assert!(!confirm_first_words(PHRASE_A, &[], words));
    }

    #[test]
    fn unlock_wrong_and_right_password() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("p2p_login_common_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::set_var("P2P_ID_CACHE_DIR", &dir);
        }

        let profile = IdentityInfo {
            name: "alice".into(),
            birthday: "1990-01-01".into(),
            gender: 'F',
        };
        let outcome = persist_identity(profile, PHRASE_A, "password-123").unwrap();
        let expected_id = outcome.keypair.public().to_peer_id().to_string();

        // 错误密码 → L2 原错误文本（"密码错误"）
        match unlock_cached(1, "wrong-password") {
            Err(reason) => assert_eq!(reason, "密码错误"),
            Ok(_) => panic!("错误密码不应解锁成功"),
        }
        // 正确密码 → 凭据与 keystore 归属一致
        let outcome = unlock_cached(1, "password-123").unwrap();
        assert_eq!(
            outcome.keypair.public().to_peer_id().to_string(),
            expected_id
        );
        assert_eq!(outcome.info.name, "alice");
        // 越界序号
        assert!(unlock_cached(99, "password-123").is_err());
    }
}
