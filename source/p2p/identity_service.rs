//! L2 身份基础服务：身份会话（登录/影子探测/keystore）、联系人簿（TOFU）、
//! 信任判定、Hello/Bye 存在处理。供 L3 业务与文件传输等多协议复用。
//!
//! 与聊天协议无关——`IdentityService` 不感知 Frame/群/会话，只回答
//! "我是谁 / 对方是谁 / 是否可信 / 首次接触该怎么做"。

use colored::Colorize;
use libp2p::{identity::Keypair, PeerId};
use std::error::Error;

use super::contacts::{fingerprint_of, ContactBook, ContactEntry};
use super::identity::{
    decrypt_mnemonic, generate_mnemonic, keypair_from_mnemonic, load_keystores,
    probe_duplicate_id, probe_window, save_keystore, valid_password, IdentityInfo, LoginOutcome,
};

/// 输入行迭代器（stdin 被管道接管时逐行读取）
pub type StdinLines = tokio::io::Lines<tokio::io::BufReader<tokio::io::Stdin>>;

/// 新身份助记词抄写确认词数
const MNEMONIC_CONFIRM_WORDS: usize = 3;

/// L2 身份服务：持有身份会话（keypair/资料/节点ID）+ 联系人簿（TOFU 信任状态）。
/// 身份与信任是所有上层业务的根依赖——任何业务要回答"对方是谁/是否可信"都经这里。
pub struct IdentityService {
    keypair: Keypair,
    my_id: PeerId,
    info: IdentityInfo,
    contacts: ContactBook,
}

impl IdentityService {
    /// 登录并建立身份会话：菜单（新身份/恢复/缓存解锁）+ 影子探测防同 ID 双在线 +
    /// 加载联系人簿。ID 冲突时内部重试登录。
    pub async fn login(
        stdin: &mut StdinLines,
        interactive: bool,
    ) -> Result<Self, Box<dyn Error>> {
        loop {
            let outcome = login_flow(stdin, interactive).await?;
            let my_id = outcome.keypair.public().to_peer_id();
            println!(
                "{}",
                format!("登录成功: {} (节点ID {my_id})", outcome.info.name).green()
            );
            match probe_duplicate_id(my_id, probe_window()).await? {
                Some(addr) => {
                    eprintln!(
                        "{}",
                        format!("该角色 ID 已在线（发现于 {addr}），同一 ID 不能同时上线").red()
                    );
                    eprintln!(
                        "{}",
                        "请改用其他身份，或先关闭占用该 ID 的设备后重试".yellow()
                    );
                }
                None => {
                    let contacts = ContactBook::load(&my_id);
                    return Ok(IdentityService {
                        keypair: outcome.keypair,
                        my_id,
                        info: outcome.info,
                        contacts,
                    });
                }
            }
        }
    }

    pub fn my_id(&self) -> &PeerId {
        &self.my_id
    }

    pub fn my_name(&self) -> &str {
        &self.info.name
    }

    pub fn keypair(&self) -> &Keypair {
        &self.keypair
    }

    /// 信任判定（根 API）：该 peer 是否已被信任
    pub fn is_verified(&self, peer: &PeerId) -> bool {
        self.contacts.verified(&peer.to_string())
    }

    /// 联系人条目（无则 None）
    pub fn contact(&self, peer: &PeerId) -> Option<&ContactEntry> {
        self.contacts.get(&peer.to_string())
    }

    /// 联系人的显示名（非空时返回；未知节点返回 None）
    pub fn contact_name(&self, peer: &PeerId) -> Option<String> {
        self.contact(peer)
            .map(|e| e.name.clone())
            .filter(|n| !n.is_empty())
    }

    /// 标记/取消信任联系人（显式置位：`/trust !名` 真正取消）
    pub fn trust(&mut self, peer: &PeerId, name: &str, verified: bool) {
        self.contacts.ensure_contact(peer, name, false);
        self.contacts.set_verified(peer, verified);
    }

    /// 按联系人名反查 peer（允许重名时取第一个；用于 /trust /chat 等按名解析）
    pub fn contact_by_name(&self, name: &str) -> Option<PeerId> {
        self.contacts
            .find_by_name(name)
            .and_then(|e| e.peer_id.parse().ok())
    }

    /// 联系人指纹（无记录则现算；供人工复核）
    pub fn fingerprint(&self, peer: &PeerId) -> String {
        self.contact(peer)
            .map(|e| e.fingerprint.clone())
            .unwrap_or_else(|| fingerprint_of(peer))
    }

    /// 对方上线（Hello）的存在处理：首次接触做 TOFU 指纹核对（交互终端人工确认、
    /// 管道环境按 SSH accept-new 语义自动信任），更新联系人名字与最近见时间；
    /// 已存在联系人保持既有信任状态（OR 合并，不会降级）。
    pub async fn on_peer_hello(
        &mut self,
        stdin: &mut StdinLines,
        interactive: bool,
        peer: &PeerId,
        name: &str,
    ) -> Result<(), Box<dyn Error>> {
        let pid = peer.to_string();
        if self.contacts.get(&pid).is_none() {
            if interactive {
                println!("{}", "首次连接，请核对对方身份指纹:".yellow());
                println!("  指纹: {}", fingerprint_of(peer).dimmed());
                println!("  节点ID: {pid}");
                let ans = read_line(stdin, "是否信任该节点（记录为联系人）? (y/n): ").await?;
                let trusted = ans.trim().eq_ignore_ascii_case("y");
                self.contacts.ensure_contact(peer, name, trusted);
                if trusted {
                    println!("{}", format!("已记录并信任: {name}").green());
                } else {
                    println!("{}", format!("已记录但未信任: {name}").yellow());
                }
            } else {
                self.contacts.ensure_contact(peer, name, true);
            }
        } else {
            self.contacts.ensure_contact(peer, name, false);
        }
        Ok(())
    }

    /// 对方主动下线（Bye）的存在处理：记录最近见时间；返回是否已知联系人
    pub fn on_peer_bye(&mut self, peer: &PeerId) -> bool {
        let known = self.contacts.get(&peer.to_string()).is_some();
        if known {
            self.contacts.mark_seen(peer);
        }
        known
    }

    /// /backup：重新查看本身份助记词（需再输密码解锁 keystore）
    pub async fn backup(
        &mut self,
        stdin: &mut StdinLines,
        interactive: bool,
    ) -> Result<(), Box<dyn Error>> {
        let stored = load_keystores();
        if let Some((ks, _)) = stored
            .iter()
            .find(|(k, _)| k.peer_id == self.my_id.to_string())
        {
            println!("{}", "请输入密码以解锁本身份".yellow());
            let password = read_secret(stdin, interactive, "密码: ").await?;
            match decrypt_mnemonic(
                &password,
                &ks.salt,
                &ks.nonce,
                &ks.enc,
                ks.kdf_m,
                ks.kdf_t,
                ks.kdf_p,
            ) {
                Ok(phrase) => {
                    print_mnemonic_guide(&phrase);
                    println!("{}", "助记词是唯一备份，请妥善保管".dimmed());
                }
                Err(reason) => eprintln!("{}", reason.red()),
            }
        } else {
            eprintln!(
                "{}",
                "未找到本身份的 keystore（身份未在本机加密保存过）".yellow()
            );
        }
        Ok(())
    }
}

// ---- 登录交互（L2 身份会话建立）----

async fn read_line(stdin: &mut StdinLines, prompt: &str) -> Result<String, Box<dyn Error>> {
    use std::io::Write;
    print!("{prompt}");
    std::io::stdout().flush()?;
    match stdin.next_line().await? {
        Some(l) => Ok(l),
        None => Err("输入结束".into()),
    }
}

/// 读取密码：交互终端不回显（rpassword）；管道环境（测试/脚本）退回行读取
async fn read_secret(
    stdin: &mut StdinLines,
    interactive: bool,
    prompt: &str,
) -> Result<String, Box<dyn Error>> {
    if interactive {
        Ok(rpassword::prompt_password(prompt)?)
    } else {
        read_line(stdin, prompt).await
    }
}

fn normalize_birthday(raw: &str) -> Result<String, String> {
    let parts: Vec<&str> = raw.trim().split('-').collect();
    if parts.len() != 3 {
        return Err("生日格式应为 YYYY-MM-DD，如 1990-01-01".into());
    }
    let (y, m, d): (u32, u32, u32) = (
        parts[0]
            .parse()
            .map_err(|_| "年份应为数字".to_string())?,
        parts[1]
            .parse()
            .map_err(|_| "月份应为数字".to_string())?,
        parts[2]
            .parse()
            .map_err(|_| "日期应为数字".to_string())?,
    );
    if !(1900..=2100).contains(&y) {
        return Err(format!("年份 {y} 超出范围 1900-2100"));
    }
    if !(1..=12).contains(&m) {
        return Err(format!("月份 {m} 超出范围 1-12"));
    }
    if !(1..=31).contains(&d) {
        return Err(format!("日期 {d} 超出范围 1-31"));
    }
    Ok(format!("{y:04}-{m:02}-{d:02}"))
}

fn normalize_gender(raw: &str) -> Result<char, String> {
    match raw.trim() {
        "男" | "M" | "m" => Ok('M'),
        "女" | "F" | "f" => Ok('F'),
        "保密" | "O" | "o" => Ok('O'),
        other => Err(format!("性别须为 男/M、女/F 或 保密/O，当前: {other}")),
    }
}

/// 交互收集资料信息（姓名/生日/性别）
async fn prompt_profile(stdin: &mut StdinLines) -> Result<IdentityInfo, Box<dyn Error>> {
    let name = loop {
        let raw = read_line(stdin, "姓名: ").await?;
        let name = raw.trim().to_string();
        if name.is_empty() || name.len() > 64 {
            eprintln!("{}", "姓名不能为空且不超过 64 字节".yellow());
        } else {
            break name;
        }
    };
    let birthday = loop {
        let raw = read_line(stdin, "生日 (YYYY-MM-DD): ").await?;
        match normalize_birthday(&raw) {
            Ok(b) => break b,
            Err(reason) => eprintln!("{}", reason.yellow()),
        }
    };
    let gender = loop {
        let raw = read_line(stdin, "性别 (男/M 女/F 保密/O): ").await?;
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
    stdin: &mut StdinLines,
    interactive: bool,
) -> Result<String, Box<dyn Error>> {
    loop {
        let pwd = read_secret(stdin, interactive, "密码: ").await?;
        if valid_password(&pwd) {
            return Ok(pwd);
        }
        eprintln!("{}", "密码须为 8~128 字节".yellow());
    }
}

/// 展示助记词与安全提示
fn print_mnemonic_guide(phrase: &str) {
    println!("{}", "=".repeat(60).yellow());
    println!(
        "{}",
        "你的身份助记词（12 词，唯一备份；丢失即永久丢失身份，泄露即身份被窃取）:".yellow()
    );
    println!("{}", phrase.red());
    println!("{}", "=".repeat(60).yellow());
}

/// 登录流程：新身份生成 / 助记词恢复 / 缓存 keystore 解锁。
/// 新身份与恢复都会自动加密保存 keystore；同 ID 冲突由调用方在探测后处理。
async fn login_flow(
    stdin: &mut StdinLines,
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
        let input = read_line(stdin, "请选择: ").await?;
        let input = input.trim();

        if input == "0" {
            // 新身份：生成助记词，展示一次并要求抄写确认
            let info = prompt_profile(stdin).await?;
            let phrase = loop {
                let phrase = match generate_mnemonic() {
                    Ok(p) => p,
                    Err(reason) => {
                        eprintln!("{}", reason.red());
                        continue;
                    }
                };
                print_mnemonic_guide(&phrase);
                let confirm = read_line(
                    stdin,
                    &format!("请抄下助记词，输入前 {MNEMONIC_CONFIRM_WORDS} 个词确认: "),
                )
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
            let password = prompt_password(stdin, interactive).await?;
            let keypair = keypair_from_mnemonic(&phrase)?;
            let peer_id = keypair.public().to_peer_id();
            save_keystore(&info, &peer_id, &phrase, &password)?;
            return Ok(LoginOutcome { keypair, info });
        } else if input == "r" {
            // 从助记词恢复身份（跨设备迁移 / 备份恢复）
            let phrase = read_line(stdin, "助记词（12 个英文词，空格分隔）: ").await?;
            let keypair = match keypair_from_mnemonic(&phrase) {
                Ok(kp) => kp,
                Err(reason) => {
                    eprintln!("{}", reason.red());
                    continue;
                }
            };
            let info = prompt_profile(stdin).await?;
            let password = prompt_password(stdin, interactive).await?;
            let peer_id = keypair.public().to_peer_id();
            save_keystore(&info, &peer_id, &phrase, &password)?;
            return Ok(LoginOutcome { keypair, info });
        } else if let Ok(n) = input.parse::<usize>() {
            if n >= 1 && n <= cached.len() {
                // 缓存解锁：密码错误最多重试 3 次
                let (ks, info) = &cached[n - 1];
                for _ in 0..3 {
                    let password = read_secret(stdin, interactive, "密码: ").await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn birthday_normalization() {
        assert_eq!(normalize_birthday("1990-1-1").unwrap(), "1990-01-01");
        assert_eq!(normalize_birthday(" 2000-12-05 ").unwrap(), "2000-12-05");
        assert!(normalize_birthday("1990/1/1").is_err());
        assert!(normalize_birthday("1899-01-01").is_err());
        assert!(normalize_birthday("1990-13-01").is_err());
        assert!(normalize_birthday("1990-01-32").is_err());
    }

    #[test]
    fn gender_normalization() {
        assert_eq!(normalize_gender("男").unwrap(), 'M');
        assert_eq!(normalize_gender("m").unwrap(), 'M');
        assert_eq!(normalize_gender("女").unwrap(), 'F');
        assert_eq!(normalize_gender("保密").unwrap(), 'O');
        assert!(normalize_gender("x").is_err());
    }

    /// 信任判定：is_verified / trust / contact 走 L2 服务
    #[test]
    fn trust_judgment_via_service() {
        use crate::p2p::contacts::CACHE_TEST_LOCK;
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("p2p_identity_svc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::set_var("P2P_ID_CACHE_DIR", &dir);
        }
        let phrase = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        let keypair = keypair_from_mnemonic(phrase).unwrap();
        let my_id = keypair.public().to_peer_id();
        let peer = {
            let kp = keypair_from_mnemonic(
                "legal winner thank year wave sausage worth useful legal winner thank yellow",
            )
            .unwrap();
            kp.public().to_peer_id()
        };
        let mut svc = IdentityService {
            keypair,
            my_id,
            info: IdentityInfo {
                name: "alice".into(),
                birthday: "1990-01-01".into(),
                gender: 'M',
            },
            contacts: ContactBook::load(&my_id),
        };
        assert!(!svc.is_verified(&peer));
        assert!(svc.contact(&peer).is_none());
        assert_eq!(svc.contact_name(&peer), None);
        assert_eq!(svc.contact_by_name("bob"), None);
        svc.trust(&peer, "bob", true);
        assert!(svc.is_verified(&peer));
        assert_eq!(svc.contact_name(&peer), Some("bob".into()));
        assert_eq!(svc.contact_by_name("bob"), Some(peer));
        assert!(!svc.fingerprint(&peer).is_empty());
        // 显式取消信任（新语义：/trust !名 真正取消）
        svc.trust(&peer, "bob", false);
        assert!(!svc.is_verified(&peer));
        assert_eq!(svc.contact_by_name("bob"), Some(peer));
        assert!(svc.on_peer_bye(&peer));
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("P2P_ID_CACHE_DIR");
        }
    }
}
