//! CLI 文本登录流程：菜单（缓存解锁 / 新身份 / 助记词恢复）产出 `LoginOutcome`，
//! 再经 `IdentityService::login_pre` 建立会话（影子探测防同 ID 双在线）。
//!
//! 自 L2 迁出的应用层交互流程——提示文案与重试语义保持逐字节不变（e2e 兜底）。

use std::error::Error;

use colored::Colorize;

use crate::p2p::identity::{
    decrypt_mnemonic, generate_mnemonic, keypair_from_mnemonic, load_keystores, save_keystore,
    valid_password, IdentityInfo, LoginOutcome,
};
use crate::p2p::identity_service::{
    normalize_birthday, normalize_gender, print_mnemonic_guide, IdentityService, LineSource,
    LoginError,
};

/// 新身份助记词抄写确认词数
const MNEMONIC_CONFIRM_WORDS: usize = 3;

/// CLI 登录入口：菜单循环 → 会话建立。
/// ID 冲突（同 ID 双在线）打印提示后回菜单重试，与原 L2 文本路径行为一致。
pub async fn run(
    src: &mut LineSource,
    interactive: bool,
) -> Result<IdentityService, Box<dyn Error>> {
    loop {
        let outcome = login_menu(src, interactive).await?;
        match IdentityService::login_pre(outcome).await {
            Ok(svc) => return Ok(svc),
            Err(LoginError::IdInUse(addr)) => {
                eprintln!(
                    "{}",
                    format!("该角色 ID 已在线（发现于 {addr}），同一 ID 不能同时上线").red()
                );
                eprintln!(
                    "{}",
                    "请改用其他身份，或先关闭占用该 ID 的设备后重试".yellow()
                );
            }
            Err(LoginError::Other(e)) => return Err(e),
        }
    }
}

/// 登录菜单流程：新身份生成 / 助记词恢复 / 缓存 keystore 解锁。
/// 新身份与恢复都会自动加密保存 keystore。
async fn login_menu(
    src: &mut LineSource,
    interactive: bool,
) -> Result<LoginOutcome, Box<dyn Error>> {
    loop {
        let cached = load_keystores();
        println!("{}", "[角色登录]".green());
        if cached.is_empty() {
            println!("{}", "暂无本地身份".dimmed());
        } else {
            println!("缓存身份:");
            for (i, (ks, info)) in cached.iter().enumerate() {
                println!("  {}. {}  ({})", i + 1, info.name, ks.peer_id);
            }
        }
        println!("  0. 新身份登录");
        println!("  r. 从助记词恢复");
        let input = src.prompt("请选择: ").await?;
        let input = input.trim();

        if input == "0" {
            // 新身份：生成助记词，展示一次并要求抄写确认
            let info = prompt_profile(src).await?;
            let phrase = loop {
                let phrase = match generate_mnemonic() {
                    Ok(p) => p,
                    Err(reason) => {
                        eprintln!("{}", reason.red());
                        continue;
                    }
                };
                print_mnemonic_guide(&phrase);
                let confirm = src
                    .prompt(&format!(
                        "请抄下助记词，输入前 {MNEMONIC_CONFIRM_WORDS} 个词确认: "
                    ))
                    .await?;
                let first: Vec<&str> = phrase
                    .split_whitespace()
                    .take(MNEMONIC_CONFIRM_WORDS)
                    .collect();
                let got: Vec<&str> = confirm.split_whitespace().collect();
                if got.len() >= MNEMONIC_CONFIRM_WORDS
                    && got[..MNEMONIC_CONFIRM_WORDS] == first[..]
                {
                    break phrase;
                }
                eprintln!("{}", "确认词不匹配，请重新抄写".yellow());
            };
            let password = prompt_password(src, interactive).await?;
            let keypair = keypair_from_mnemonic(&phrase)?;
            let peer_id = keypair.public().to_peer_id();
            save_keystore(&info, &peer_id, &phrase, &password)?;
            return Ok(LoginOutcome { keypair, info });
        } else if input == "r" {
            // 从助记词恢复身份（跨设备迁移 / 备份恢复）
            let phrase = src.prompt("助记词（12 个英文词，空格分隔）: ").await?;
            let keypair = match keypair_from_mnemonic(&phrase) {
                Ok(kp) => kp,
                Err(reason) => {
                    eprintln!("{}", reason.red());
                    continue;
                }
            };
            let info = prompt_profile(src).await?;
            let password = prompt_password(src, interactive).await?;
            let peer_id = keypair.public().to_peer_id();
            save_keystore(&info, &peer_id, &phrase, &password)?;
            return Ok(LoginOutcome { keypair, info });
        } else if let Ok(n) = input.parse::<usize>() {
            if n >= 1 && n <= cached.len() {
                // 缓存解锁：密码错误最多重试 3 次
                let (ks, info) = &cached[n - 1];
                for _ in 0..3 {
                    let password = src.prompt_secret(interactive, "密码: ").await?;
                    if !valid_password(&password) {
                        eprintln!("{}", "密码须为 8~128 字节".yellow());
                        continue;
                    }
                    match decrypt_mnemonic(
                        &password,
                        &ks.salt,
                        &ks.nonce,
                        &ks.enc,
                        ks.kdf_m,
                        ks.kdf_t,
                        ks.kdf_p,
                    ) {
                        Ok(phrase) => match keypair_from_mnemonic(&phrase) {
                            Ok(kp) if kp.public().to_peer_id().to_string() == ks.peer_id => {
                                return Ok(LoginOutcome {
                                    keypair: kp,
                                    info: IdentityInfo {
                                        name: info.name.clone(),
                                        birthday: info.birthday.clone(),
                                        gender: info.gender,
                                    },
                                });
                            }
                            Ok(_) => {
                                eprintln!("{}", "keystore 与派生身份不符，数据可能损坏".red());
                            }
                            Err(reason) => {
                                eprintln!("{}", reason.red());
                            }
                        },
                        Err(reason) => {
                            eprintln!("{}", reason.red());
                        }
                    }
                }
                eprintln!("{}", "连续多次密码错误，返回选择菜单".yellow());
            } else {
                eprintln!("{}", "序号无效，请重新选择".yellow());
            }
        } else {
            eprintln!("{}", "无效选择，请输入序号、0 或 r".yellow());
        }
    }
}

/// 交互收集资料信息（姓名/生日/性别）
async fn prompt_profile(src: &mut LineSource) -> Result<IdentityInfo, Box<dyn Error>> {
    let name = loop {
        let raw = src.prompt("姓名: ").await?;
        let name = raw.trim().to_string();
        if name.is_empty() || name.len() > 64 {
            eprintln!("{}", "姓名不能为空且不超过 64 字节".yellow());
        } else {
            break name;
        }
    };
    let birthday = loop {
        let raw = src.prompt("生日 (YYYY-MM-DD): ").await?;
        match normalize_birthday(&raw) {
            Ok(b) => break b,
            Err(reason) => eprintln!("{}", reason.yellow()),
        }
    };
    let gender = loop {
        let raw = src.prompt("性别 (男/M 女/F 保密/O): ").await?;
        match normalize_gender(&raw) {
            Ok(g) => break g,
            Err(reason) => eprintln!("{}", reason.yellow()),
        }
    };
    Ok(IdentityInfo {
        name,
        birthday,
        gender,
    })
}

/// 交互收集并校验密码
async fn prompt_password(
    src: &mut LineSource,
    interactive: bool,
) -> Result<String, Box<dyn Error>> {
    loop {
        let pwd = src.prompt_secret(interactive, "密码: ").await?;
        if valid_password(&pwd) {
            return Ok(pwd);
        }
        eprintln!("{}", "密码须为 8~128 字节".yellow());
    }
}
