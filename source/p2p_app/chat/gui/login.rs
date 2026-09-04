//! 聊天应用 GUI 登录页：卡片视图 + 表单编排。
//!
//! 编排只调用 `p2p` 领域 API（load_keystores/decrypt_mnemonic/save_keystore/...），
//! 产出 `LoginOutcome` 后由 GuiApp 以 `run_engine(Some(outcome))` 启动引擎。
//! 密码只在表单内流转（masked 输入），不进命令框、不落 interact.log。

use egui::Color32;

use crate::p2p::identity::{
    generate_mnemonic, keypair_from_mnemonic, load_keystores, IdentityInfo, LoginOutcome,
};
use crate::p2p_app::chat::login_common::{
    check_password_pair, confirm_first_words, persist_identity, unlock_cached, validate_birthday,
    validate_name, validate_password,
};

/// 登录流程来源（资料页与密码页共用）
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    New,
    Restore,
}

/// 性别（表单枚举 → L2 域值 M/F/O）
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gender {
    Male,
    Female,
    Other,
}

impl Gender {
    fn ch(self) -> char {
        match self {
            Gender::Male => 'M',
            Gender::Female => 'F',
            Gender::Other => 'O',
        }
    }

    fn all() -> [Gender; 3] {
        [Gender::Male, Gender::Female, Gender::Other]
    }

    fn label(self) -> &'static str {
        match self {
            Gender::Male => "男",
            Gender::Female => "女",
            Gender::Other => "保密",
        }
    }
}

/// 缓存身份条目（登录菜单列表项）
pub struct CachedIdentity {
    pub index: usize,
    pub name: String,
    pub peer_id: String,
}

/// 登录页状态机：菜单 →（缓存解锁 / 新身份向导 / 助记词恢复）→ `LoginOutcome`
pub enum LoginState {
    /// 登录菜单：缓存身份列表 + 新身份 + 恢复入口
    Menu {
        identities: Vec<CachedIdentity>,
        error: Option<String>,
    },
    /// 缓存解锁（密码错误内联提示，可重试）
    Unlock {
        index: usize,
        name: String,
        password: String,
        error: Option<String>,
    },
    /// 资料收集（新建/恢复共用；恢复携带已验证助记词）
    Profile {
        stage: Stage,
        phrase: Option<String>,
        name: String,
        birthday: String,
        gender: Gender,
        error: Option<String>,
    },
    /// 新身份：助记词展示 + 抄写确认（前 3 词）
    MnemonicConfirm {
        profile: IdentityInfo,
        phrase: String,
        words: [String; 3],
        error: Option<String>,
    },
    /// 密码设置（二次确认；新建用生成的助记词，恢复用输入的助记词）
    NewPassword {
        profile: IdentityInfo,
        phrase: String,
        pwd: String,
        pwd2: String,
        error: Option<String>,
    },
    /// 助记词恢复输入
    RestorePhrase { phrase: String, error: Option<String> },
}

/// 重建登录菜单（返回/重试后刷新缓存列表）
fn menu_state() -> LoginState {
    LoginState::Menu {
        identities: load_cached(),
        error: None,
    }
}

/// 读取缓存身份列表（L2 API）
pub fn load_cached() -> Vec<CachedIdentity> {
    load_keystores()
        .into_iter()
        .enumerate()
        .map(|(i, (ks, info))| CachedIdentity {
            index: i + 1,
            name: info.name,
            peer_id: ks.peer_id,
        })
        .collect()
}

/// 校验并归一资料 → `IdentityInfo`（姓名/生日走共享内核；性别经表单枚举无脏值）
fn build_profile(name: &str, birthday: &str, gender: Gender) -> Result<IdentityInfo, String> {
    Ok(IdentityInfo {
        name: validate_name(name)?,
        birthday: validate_birthday(birthday)?,
        gender: gender.ch(),
    })
}

/// 校验密码并检查两次输入一致（共享内核）
fn check_password(pwd: &str, pwd2: &str) -> Result<String, String> {
    check_password_pair(pwd, pwd2)
}

/// 缓存解锁：密码规则（内核）→ 解密派生核对（内核）
fn unlock(index: usize, password: &str) -> Result<LoginOutcome, String> {
    validate_password(password)?;
    unlock_cached(index, password)
}

fn show_error(ui: &mut egui::Ui, error: &Option<String>) {
    if let Some(e) = error {
        ui.colored_label(Color32::from_rgb(220, 80, 80), format!("⚠ {e}"));
    }
}

fn short_id(peer_id: &str) -> String {
    format!("{}…", &peer_id[..peer_id.len().min(12)])
}

/// 登录页视图：渲染当前状态并处理表单交互。
/// 返回 `Some(LoginOutcome)` 表示登录成功，调用方应启动引擎并切换到聊天布局。
pub fn view(state: &mut LoginState, ui: &mut egui::Ui) -> Option<LoginOutcome> {
    ui.add_space(4.0);
    match state {
        LoginState::Menu { identities, error } => {
            ui.heading("P2P 聊天登录");
            ui.add_space(4.0);
            ui.weak("选择缓存身份登录，或创建新身份 / 从助记词恢复");
            ui.add_space(10.0);

            let mut chosen: Option<(usize, String)> = None;
            for id in identities.iter() {
                ui.horizontal(|ui| {
                    if ui.button(format!("{}. {}", id.index, id.name)).clicked() {
                        chosen = Some((id.index, id.name.clone()));
                    }
                    ui.weak(format!("节点ID {}", short_id(&id.peer_id)));
                });
            }
            ui.add_space(6.0);
            let mut next: Option<LoginState> = None;
            ui.horizontal(|ui| {
                if ui.button("新身份").clicked() {
                    next = Some(LoginState::Profile {
                        stage: Stage::New,
                        phrase: None,
                        name: String::new(),
                        birthday: String::new(),
                        gender: Gender::Male,
                        error: None,
                    });
                }
                if ui.button("从助记词恢复").clicked() {
                    next = Some(LoginState::RestorePhrase {
                        phrase: String::new(),
                        error: None,
                    });
                }
            });
            show_error(ui, error);

            if let Some((index, name)) = chosen {
                next = Some(LoginState::Unlock {
                    index,
                    name,
                    password: String::new(),
                    error: None,
                });
            }
            if let Some(n) = next {
                *state = n;
            }
            None
        }
        LoginState::Unlock { index, name, password, error } => {
            ui.heading(format!("解锁身份：{name}"));
            ui.add_space(10.0);

            let mut submit = false;
            let mut back = false;
            ui.horizontal(|ui| {
                ui.label("密码");
                let resp = ui.add(
                    egui::TextEdit::singleline(password)
                        .password(true)
                        .desired_width(240.0),
                );
                if resp.lost_focus() && ui.ctx().input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
            });
            show_error(ui, error);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("解锁").clicked() {
                    submit = true;
                }
                if ui.button("返回").clicked() {
                    back = true;
                }
            });

            if back {
                *state = menu_state();
            } else if submit {
                match unlock(*index, password) {
                    Ok(outcome) => return Some(outcome),
                    Err(reason) => *error = Some(reason),
                }
                password.clear();
            }
            None
        }
        LoginState::Profile { stage, phrase, name, birthday, gender, error } => {
            let title = match stage {
                Stage::New => "新身份资料",
                Stage::Restore => "恢复身份资料",
            };
            ui.heading(title);
            ui.add_space(10.0);

            ui.horizontal(|ui| {
                ui.label("姓名");
                ui.add(egui::TextEdit::singleline(name).desired_width(240.0));
            });
            ui.horizontal(|ui| {
                ui.label("生日");
                ui.add(
                    egui::TextEdit::singleline(birthday)
                        .hint_text("YYYY-MM-DD")
                        .desired_width(240.0),
                );
            });
            ui.horizontal(|ui| {
                ui.label("性别");
                for g in Gender::all() {
                    ui.radio_value(gender, g, g.label());
                }
            });
            show_error(ui, error);
            ui.add_space(6.0);
            let mut next = false;
            let mut back = false;
            ui.horizontal(|ui| {
                if ui.button("下一步").clicked() {
                    next = true;
                }
                if ui.button("返回").clicked() {
                    back = true;
                }
            });

            if back {
                *state = menu_state();
            } else if next {
                match build_profile(name, birthday, *gender) {
                    Ok(profile) => match stage {
                        Stage::New => match generate_mnemonic() {
                            Ok(generated) => {
                                *state = LoginState::MnemonicConfirm {
                                    profile,
                                    phrase: generated,
                                    words: Default::default(),
                                    error: None,
                                };
                            }
                            Err(reason) => *error = Some(reason),
                        },
                        Stage::Restore => {
                            let restored = phrase.take().unwrap_or_default();
                            *state = LoginState::NewPassword {
                                profile,
                                phrase: restored,
                                pwd: String::new(),
                                pwd2: String::new(),
                                error: None,
                            };
                        }
                    },
                    Err(reason) => *error = Some(reason),
                }
            }
            None
        }
        LoginState::MnemonicConfirm { profile, phrase, words, error } => {
            ui.heading("抄写你的助记词");
            ui.add_space(4.0);
            ui.colored_label(
                Color32::from_rgb(230, 180, 0),
                "这是唯一备份：丢失即永久丢失身份，泄露即身份被窃取",
            );
            ui.add_space(6.0);

            // 只读展示（每帧用副本渲染，内容可选中复制且不回写状态）
            let mut display = phrase.clone();
            ui.add(
                egui::TextEdit::multiline(&mut display)
                    .desired_rows(2)
                    .desired_width(460.0)
                    .font(egui::TextStyle::Monospace),
            );
            ui.add_space(6.0);
            ui.weak(format!("请抄下助记词，输入前 {} 个词确认:", words.len()));
            ui.horizontal(|ui| {
                for word in words.iter_mut() {
                    ui.add(
                        egui::TextEdit::singleline(word)
                            .desired_width(110.0)
                            .font(egui::TextStyle::Monospace),
                    );
                }
            });
            show_error(ui, error);
            ui.add_space(6.0);
            let mut proceed = false;
            let mut back = false;
            ui.horizontal(|ui| {
                if ui.button("我已抄好，继续").clicked() {
                    proceed = true;
                }
                if ui.button("返回").clicked() {
                    back = true;
                }
            });

            if back {
                *state = menu_state();
            } else if proceed {
                if confirm_first_words(phrase, words, words.len()) {
                    let confirmed = std::mem::replace(profile, dummy_profile());
                    *state = LoginState::NewPassword {
                        profile: confirmed,
                        phrase: phrase.clone(),
                        pwd: String::new(),
                        pwd2: String::new(),
                        error: None,
                    };
                } else {
                    *error = Some("确认词不匹配，请检查抄写内容".into());
                }
            }
            None
        }
        LoginState::NewPassword { profile, phrase, pwd, pwd2, error } => {
            ui.heading("设置密码（加密保存本机 keystore）");
            ui.add_space(10.0);

            let mut submit = false;
            ui.horizontal(|ui| {
                ui.label("密码");
                ui.add(
                    egui::TextEdit::singleline(pwd)
                        .password(true)
                        .desired_width(240.0),
                );
            });
            ui.horizontal(|ui| {
                ui.label("确认");
                let resp = ui.add(
                    egui::TextEdit::singleline(pwd2)
                        .password(true)
                        .desired_width(240.0),
                );
                if resp.lost_focus() && ui.ctx().input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
            });
            show_error(ui, error);
            ui.add_space(6.0);
            let mut submit = false;
            let mut back = false;
            ui.horizontal(|ui| {
                if ui.button("完成登录").clicked() {
                    submit = true;
                }
                if ui.button("返回").clicked() {
                    back = true;
                }
            });

            if back {
                *state = menu_state();
            } else if submit {
                match check_password(pwd, pwd2)
                    .and_then(|p| persist_identity(profile.clone(), phrase, &p))
                {
                    Ok(outcome) => return Some(outcome),
                    Err(reason) => *error = Some(reason),
                }
                pwd.clear();
                pwd2.clear();
            }
            None
        }
        LoginState::RestorePhrase { phrase, error } => {
            ui.heading("从助记词恢复身份");
            ui.add_space(4.0);
            ui.weak("输入 12 个英文助记词（空格分隔）");
            ui.add_space(6.0);

            ui.add(
                egui::TextEdit::multiline(phrase)
                    .hint_text("助记词（12 个英文词，空格分隔）")
                    .desired_rows(2)
                    .desired_width(460.0)
                    .font(egui::TextStyle::Monospace),
            );
            show_error(ui, error);
            ui.add_space(6.0);
            let mut next = false;
            let mut back = false;
            ui.horizontal(|ui| {
                if ui.button("下一步").clicked() {
                    next = true;
                }
                if ui.button("返回").clicked() {
                    back = true;
                }
            });

            if back {
                *state = menu_state();
            } else if next {
                match keypair_from_mnemonic(phrase.trim()) {
                    Ok(_) => {
                        *state = LoginState::Profile {
                            stage: Stage::Restore,
                            phrase: Some(phrase.trim().to_string()),
                            name: String::new(),
                            birthday: String::new(),
                            gender: Gender::Male,
                            error: None,
                        };
                    }
                    Err(reason) => *error = Some(reason),
                }
            }
            None
        }
    }
}

/// 占位资料（`std::mem::replace` 转移 profile 时的哨兵；随即被覆盖，不会被保存）
fn dummy_profile() -> IdentityInfo {
    IdentityInfo {
        name: String::new(),
        birthday: String::new(),
        gender: 'O',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gender_maps_to_l2_domain_values() {
        assert_eq!(Gender::Male.ch(), 'M');
        assert_eq!(Gender::Female.ch(), 'F');
        assert_eq!(Gender::Other.ch(), 'O');
        assert_eq!(Gender::all().len(), 3);
    }

    #[test]
    fn profile_compose_uses_kernel_rules() {
        // 走共享内核：姓名/生日规则与 CLI 完全一致，性别由表单枚举映射
        assert!(build_profile("", "1990-01-01", Gender::Male).is_err());
        assert!(build_profile("alice", "1990/01/01", Gender::Male).is_err());
        let profile = build_profile("  alice ", "1990-1-1", Gender::Female).unwrap();
        assert_eq!(profile.name, "alice");
        assert_eq!(profile.birthday, "1990-01-01");
        assert_eq!(profile.gender, 'F');
    }

    #[test]
    fn unlock_validates_password_rule_before_decrypt() {
        // 规则错误不解密（不触达 keystore），文本来自共享内核
        match unlock(1, "short") {
            Err(reason) => assert_eq!(reason, "密码须为 8~128 字节"),
            Ok(_) => panic!("规则错误不应触达解锁"),
        }
    }
}
