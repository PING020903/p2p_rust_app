//! CLI 文本登录流程：菜单（缓存解锁 / 新身份 / 助记词恢复）产出 `LoginOutcome`，
//! 再经 `IdentityService::login_pre` 建立会话（影子探测防同 ID 双在线）。
//!
//! 自 L2 迁出的应用层交互流程——提示文案与重试语义保持逐字节不变（e2e 兜底）。

use std::error::Error;

use colored::Colorize;

use crate::p2p::identity::{
    generate_mnemonic, keypair_from_mnemonic, load_keystores, IdentityInfo, LoginOutcome,
};
use crate::lineio::LineSource;
use crate::p2p::identity_service::{
    normalize_gender, print_mnemonic_guide, IdentityService, LoginError,
};
use crate::p2p_app::chat::login_common::{
    confirm_first_words, persist_identity, unlock_cached, validate_birthday, validate_name,
    validate_password,
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
                let answers: Vec<String> =
                    confirm.split_whitespace().map(String::from).collect();
                if confirm_first_words(&phrase, &answers, MNEMONIC_CONFIRM_WORDS) {
                    break phrase;
                }
                eprintln!("{}", "确认词不匹配，请重新抄写".yellow());
            };
            let password = prompt_password(src, interactive).await?;
            return persist_identity(info, &phrase, &password).map_err(Into::into);
        } else if input == "r" {
            // 从助记词恢复身份（跨设备迁移 / 备份恢复）
            let phrase = src.prompt("助记词（12 个英文词，空格分隔）: ").await?;
            // 校验助记词合法（派生与保存由共享内核 persist_identity 完成）
            if let Err(reason) = keypair_from_mnemonic(&phrase) {
                eprintln!("{}", reason.red());
                continue;
            };
            let info = prompt_profile(src).await?;
            let password = prompt_password(src, interactive).await?;
            return persist_identity(info, &phrase, &password).map_err(Into::into);
        } else if let Ok(n) = input.parse::<usize>() {
            if n >= 1 && n <= cached.len() {
                // 缓存解锁：密码错误最多重试 3 次（规则校验黄色提示，其余错误红色——文案不变）
                for _ in 0..3 {
                    let password = src.prompt_secret(interactive, "密码: ").await?;
                    if let Err(reason) = validate_password(&password) {
                        eprintln!("{}", reason.yellow());
                        continue;
                    }
                    match unlock_cached(n, &password) {
                        Ok(outcome) => return Ok(outcome),
                        Err(reason) => eprintln!("{}", reason.red()),
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

/// 交互收集资料信息（姓名/生日/性别；校验规则来自共享内核，提示与重试为本前端职责）
async fn prompt_profile(src: &mut LineSource) -> Result<IdentityInfo, Box<dyn Error>> {
    let name = loop {
        let raw = src.prompt("姓名: ").await?;
        match validate_name(&raw) {
            Ok(n) => break n,
            Err(reason) => eprintln!("{}", reason.yellow()),
        }
    };
    let birthday = loop {
        let raw = src.prompt("生日 (YYYY-MM-DD): ").await?;
        match validate_birthday(&raw) {
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

/// 交互收集并校验密码（规则来自共享内核）
async fn prompt_password(
    src: &mut LineSource,
    interactive: bool,
) -> Result<String, Box<dyn Error>> {
    loop {
        let pwd = src.prompt_secret(interactive, "密码: ").await?;
        match validate_password(&pwd) {
            Ok(pwd) => return Ok(pwd),
            Err(reason) => eprintln!("{}", reason.yellow()),
        }
    }
}
