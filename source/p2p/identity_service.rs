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
    decrypt_mnemonic, load_keystores, probe_duplicate_id, probe_window, IdentityInfo, LoginOutcome,
};

/// 输入行迭代器（input 被管道接管时逐行读取）
pub type StdinLines = tokio::io::Lines<tokio::io::BufReader<tokio::io::Stdin>>;

/// 统一输入消息：
/// - `Line`：CLI 语义行——按 `/` 前缀分流（命令树 vs 文本消息），终端/管道逐行产出
/// - `ChatText`：GUI 文本框的纯聊天文本——**绕过命令解析**直接发送到当前焦点（多行原样）
pub enum InputMsg {
    Line(String),
    ChatText(String),
}

/// 输入源抽象：CLI/e2e 读终端或管道（Stdin，逐行产出 Line）；GUI 读 UI 输入通道（Channel）。
pub enum LineSource {
    Stdin(StdinLines),
    Channel(tokio::sync::mpsc::UnboundedReceiver<InputMsg>),
}

impl LineSource {
    /// 聊天循环输入：Stdin 每行包装为 Line；Channel 原样透传 GUI 消息
    pub async fn next_input(&mut self) -> Option<InputMsg> {
        match self {
            LineSource::Stdin(lines) => {
                lines.next_line().await.ok().flatten().map(InputMsg::Line)
            }
            LineSource::Channel(rx) => rx.recv().await,
        }
    }

    /// 交互提示场景的原始行读取（登录/确认；ChatText 亦取其文本）。
    /// 登录等阶段 GUI 文本框禁用、命令框以 Line 发送，故此处只会收到 Line。
    pub async fn next_raw_line(&mut self) -> Option<String> {
        match self.next_input().await {
            Some(InputMsg::Line(s)) => Some(s),
            Some(InputMsg::ChatText(t)) => Some(t),
            None => None,
        }
    }

    /// 带提示符读取一行（登录/确认等交互场景；I/O 属输入抽象自身）
    pub async fn prompt(&mut self, prompt: &str) -> Result<String, Box<dyn Error>> {
        use std::io::Write;
        print!("{prompt}");
        std::io::stdout().flush()?;
        self.next_raw_line()
            .await
            .ok_or_else(|| -> Box<dyn Error> { "输入结束".into() })
    }

    /// 带提示符读取密码：交互终端不回显（rpassword）；管道环境（测试/脚本）退回行读取
    pub async fn prompt_secret(
        &mut self,
        interactive: bool,
        prompt: &str,
    ) -> Result<String, Box<dyn Error>> {
        if interactive {
            Ok(rpassword::prompt_password(prompt)?)
        } else {
            self.prompt(prompt).await
        }
    }
}

/// L2 内化信号枚举：hello/bye/trust 同一组，仅 L2 认识，L3 业务不触碰。
/// `Frame.text` 线缆仍是字符串，应用侧用 `from_str`/`as_str` 与本枚举互转，
/// 用于门禁白名单（内化信号一律放行）与类型安全的内化信号常量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextTag {
    Hello,
    Bye,
    TrustConfirm,
    TrustRevoke,
}

impl TextTag {
    /// 字符串 tag → 内化枚举（"hello"→Hello 等；非内化信号返回 None）
    pub fn from_str(s: &str) -> Option<TextTag> {
        match s {
            "hello" => Some(TextTag::Hello),
            "bye" => Some(TextTag::Bye),
            "trust.confirm" => Some(TextTag::TrustConfirm),
            "trust.revoke" => Some(TextTag::TrustRevoke),
            _ => None,
        }
    }

    /// 内化枚举 → 线缆字符串 tag
    pub fn as_str(&self) -> &'static str {
        match self {
            TextTag::Hello => "hello",
            TextTag::Bye => "bye",
            TextTag::TrustConfirm => "trust.confirm",
            TextTag::TrustRevoke => "trust.revoke",
        }
    }
}

/// 某 tag 是否为 L2 内化信号（门禁白名单：内化信号无论互信与否一律放行）
pub fn is_l2_signal(tag: &str) -> bool {
    TextTag::from_str(tag).is_some()
}

/// L2 身份服务：持有身份会话（keypair/资料/节点ID）+ 联系人簿（TOFU 信任状态）。
/// 身份与信任是所有上层业务的根依赖——任何业务要回答"对方是谁/是否可信"都经这里。
pub struct IdentityService {
    keypair: Keypair,
    my_id: PeerId,
    info: IdentityInfo,
    contacts: ContactBook,
}

/// 登录会话建立错误：类型化供前端分别处置（CLI 回菜单重试 / GUI 表单内联报错）
#[derive(Debug)]
pub enum LoginError {
    /// 角色 ID 已在线（同 ID 不能同时上线；addr = 影子探测发现的在线地址）
    IdInUse(libp2p::Multiaddr),
    /// 其他错误（IO / 加解密等，透传原错误）
    Other(Box<dyn Error>),
}

impl std::fmt::Display for LoginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoginError::IdInUse(addr) => write!(f, "角色 ID 已在线（发现于 {addr}）"),
            LoginError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl Error for LoginError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            LoginError::Other(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

impl IdentityService {
    /// 以既有登录凭据建立身份会话：影子探测防同 ID 双在线 + 加载联系人簿。
    ///
    /// 登录凭据（keypair/资料）由前端产出——CLI 文本菜单或 GUI 表单编排（应用层），
    /// 本方法只做会话建立，不涉及任何交互流程。
    /// ID 冲突返回 [`LoginError::IdInUse`]，重试策略由前端决定。
    pub async fn login_pre(outcome: LoginOutcome) -> Result<Self, LoginError> {
        let my_id = outcome.keypair.public().to_peer_id();
        println!(
            "{}",
            format!("登录成功: {} (节点ID {my_id})", outcome.info.name).green()
        );
        match probe_duplicate_id(my_id, probe_window()).await {
            Ok(Some(addr)) => Err(LoginError::IdInUse(addr)),
            Ok(None) => {
                let contacts = ContactBook::load(&my_id);
                Ok(IdentityService {
                    keypair: outcome.keypair,
                    my_id,
                    info: outcome.info,
                    contacts,
                })
            }
            Err(e) => Err(LoginError::Other(e)),
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

    /// 有效信任：互信才算数（我信任对方 且 对方信任我）——对称信任判定根 API
    pub fn effective_trusted(&self, peer: &PeerId) -> bool {
        self.contacts.effective_trusted(&peer.to_string())
    }

    /// L2 信任信号处理（trust.confirm=true / trust.revoke=false）：
    /// 对端告知"我信任你/我取消信任你"，更新 their_trust 并登记联系人。
    pub fn on_peer_trust_signal(&mut self, peer: &PeerId, name: &str, trusted: bool) {
        self.contacts.ensure_contact(peer, name, false);
        self.contacts.set_their_trust(peer, trusted);
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
        src: &mut LineSource,
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
                let ans = src.prompt("是否信任该节点（记录为联系人）? (y/n): ").await?;
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

    /// L2 处理对方上线（Hello）：先做存在层处理（TOFU/联系人簿），
    /// 再触发 L3 注册的钩子（参数回调，可在运行时替换）。L3 不直接处理原始帧。
    pub async fn handle_peer_hello<H>(
        &mut self,
        src: &mut LineSource,
        interactive: bool,
        peer: &PeerId,
        name: &str,
        mut on_hello: H,
    ) -> Result<(), Box<dyn Error>>
    where
        H: FnMut(&PeerId, &str),
    {
        self.on_peer_hello(src, interactive, peer, name).await?;
        on_hello(peer, name);
        Ok(())
    }

    /// L2 处理对方下线（Bye）：先做存在层处理（记录最近见），
    /// 再触发 L3 注册的钩子（参数回调，可在运行时替换）。
    pub fn handle_peer_bye<B>(&mut self, peer: &PeerId, mut on_bye: B)
    where
        B: FnMut(&PeerId),
    {
        self.on_peer_bye(peer);
        on_bye(peer);
    }

    /// /backup：重新查看本身份助记词（需再输密码解锁 keystore）
    pub async fn backup(
        &mut self,
        src: &mut LineSource,
        interactive: bool,
    ) -> Result<(), Box<dyn Error>> {
        let stored = load_keystores();
        if let Some((ks, _)) = stored
            .iter()
            .find(|(k, _)| k.peer_id == self.my_id.to_string())
        {
            println!("{}", "请输入密码以解锁本身份".yellow());
            let password = src.prompt_secret(interactive, "密码: ").await?;
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

// ---- 领域校验（CLI/GUI 前端共用的 L2 规则）----

/// 归一化生日（YYYY-MM-DD；容错单位数月/日，越界报错）
pub fn normalize_birthday(raw: &str) -> Result<String, String> {
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

/// 归一化性别（男/M、女/F、保密/O；前端表单与 CLI 共用的域校验）
pub fn normalize_gender(raw: &str) -> Result<char, String> {
    match raw.trim() {
        "男" | "M" | "m" => Ok('M'),
        "女" | "F" | "f" => Ok('F'),
        "保密" | "O" | "o" => Ok('O'),
        other => Err(format!("性别须为 男/M、女/F 或 保密/O，当前: {other}")),
    }
}

/// 展示助记词与安全提示（/backup 与 CLI 登录共用的显示辅助；随 /backup 迁移归属应用层）
pub(crate) fn print_mnemonic_guide(phrase: &str) {
    println!("{}", "=".repeat(60).yellow());
    println!(
        "{}",
        "你的身份助记词（12 词，唯一备份；丢失即永久丢失身份，泄露即身份被窃取）:".yellow()
    );
    println!("{}", phrase.red());
    println!("{}", "=".repeat(60).yellow());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::p2p::identity::keypair_from_mnemonic;

    #[test]
    fn text_tag_round_trip_and_l2_signal() {
        for (s, tag) in [
            ("hello", TextTag::Hello),
            ("bye", TextTag::Bye),
            ("trust.confirm", TextTag::TrustConfirm),
            ("trust.revoke", TextTag::TrustRevoke),
        ] {
            assert_eq!(TextTag::from_str(s), Some(tag));
            assert_eq!(tag.as_str(), s);
            assert!(is_l2_signal(s));
        }
        // 业务信号不是内化信号（L3 不触碰内化 text）
        for s in ["chat.text", "file.offer", "chat.group_invite", "bogus"] {
            assert_eq!(TextTag::from_str(s), None);
            assert!(!is_l2_signal(s));
        }
    }

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

    /// L2 存在钩子：handle_peer_hello/bye 在 L2 处理后触发 L3 提供的钩子（参数回调）
    #[tokio::test]
    async fn presence_hooks_fire_after_l2_processing() {
        use crate::p2p::contacts::CACHE_TEST_LOCK;
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("p2p_presence_hook_{}", std::process::id()));
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
        // 管道模式（interactive=false）下 hello 不读 input，可直接喂未使用的 input
        use tokio::io::AsyncBufReadExt;
        let mut input =
            LineSource::Stdin(tokio::io::BufReader::new(tokio::io::stdin()).lines());
        // hello 钩子触发 + 收到名字
        let mut hello_calls: Vec<(String, String)> = Vec::new();
        svc.handle_peer_hello(&mut input, false, &peer, "bob", |p, n| {
            hello_calls.push((p.to_string(), n.to_string()));
        })
        .await
        .unwrap();
        assert_eq!(hello_calls, vec![(peer.to_string(), "bob".into())]);
        // bye 钩子触发
        let mut bye_calls: Vec<String> = Vec::new();
        svc.handle_peer_bye(&peer, |p| bye_calls.push(p.to_string()));
        assert_eq!(bye_calls, vec![peer.to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("P2P_ID_CACHE_DIR");
        }
    }
}
