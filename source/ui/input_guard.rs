//! 输入检查&修改层（GUI 专用）：发送到子进程前对输入行做规则化处理。
//!
//! 接口可扩展：实现 `Rule` 塞进 `InputGuard::process` 的规则链即可。
//! 规则链按序执行：`Allow` 继续下一条；`Rewrite` 用新文本继续；`Block` 立即拦截返回理由。
//! 最终 `Ok(文本)` = 允许发送；`Err((规则名, 理由))` = 拦截（GUI 展示 + 日志）。
//!
//! 按输入源分派：
//! - **命令框**（`command_box`）：拦终端逃逸穿透 + `/sendStrings`（多行只能走文本框）
//! - **文本框**（`text_box`）：纯文本语义——内容一律包成 `/sendStrings <N>` 协议发送，
//!   不存在命令解析，故规则链为空（接口保留，供未来加长度/敏感词等检查）

/// 单条规则的处理结果
pub enum Action {
    /// 放行，继续下一条规则
    Allow,
    /// 改写后继续（接口预留，当前无内置改写规则）
    #[allow(dead_code)]
    Rewrite(String),
    /// 拦截，携带理由
    Block(String),
}

/// 输入检查规则接口（检查&修改的扩展点）
pub trait Rule {
    fn name(&self) -> &'static str;
    fn apply(&self, line: &str) -> Action;
}

/// 规则链：按序处理输入
pub struct InputGuard {
    rules: Vec<Box<dyn Rule>>,
}

impl InputGuard {
    /// 命令输入框规则：防终端逃逸穿透 + 阻止多行命令入口
    pub fn command_box() -> Self {
        InputGuard {
            rules: vec![
                Box::new(BlockTerminalEscape),
                Box::new(BlockSendStrings),
            ],
        }
    }

    /// 文本框规则：纯文本语义，暂无内置规则（接口保留）
    pub fn text_box() -> Self {
        InputGuard { rules: Vec::new() }
    }

    /// 处理一行输入；返回 `Ok(最终文本)` 或 `Err((规则名, 拦截理由))`
    pub fn process(&self, mut line: String) -> Result<String, (String, String)> {
        for rule in &self.rules {
            match rule.apply(&line) {
                Action::Allow => {}
                Action::Rewrite(new) => line = new,
                Action::Block(reason) => return Err((rule.name().to_string(), reason)),
            }
        }
        Ok(line)
    }
}

/// 拦截终端逃逸命令：GUI 模式下禁止 `cmd/`、`ps/`、`sh/` 穿透操作命令行
/// （CLI 保留该功能；GUI 里它绕过应用、直控宿主命令行为逻辑漏洞）。
struct BlockTerminalEscape;

impl Rule for BlockTerminalEscape {
    fn name(&self) -> &'static str {
        "block_terminal_escape"
    }

    fn apply(&self, line: &str) -> Action {
        for prefix in ["cmd/", "ps/", "sh/"] {
            if line.starts_with(prefix) {
                return Action::Block(format!(
                    "GUI 模式禁止终端逃逸命令（{prefix}…）：该功能仅在 CLI 终端可用"
                ));
            }
        }
        Action::Allow
    }
}

/// 阻止命令框发起 `/sendStrings`：多行文本只能走文本框（避免行数收集态误吞后续输入）
struct BlockSendStrings;

impl Rule for BlockSendStrings {
    fn name(&self) -> &'static str {
        "block_sendstrings"
    }

    fn apply(&self, line: &str) -> Action {
        if line.starts_with("/sendStrings") {
            return Action::Block("多行文本请在文本框输入发送（/sendStrings 由 GUI 自动生成）".to_string());
        }
        Action::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_box_blocks_terminal_escape() {
        let g = InputGuard::command_box();
        for bad in ["cmd/cls", "ps/Get-Date", "sh/clear"] {
            assert!(g.process(bad.to_string()).is_err(), "{bad} 应被拦截");
        }
    }

    #[test]
    fn command_box_blocks_sendstrings() {
        let g = InputGuard::command_box();
        assert!(g.process("/sendStrings 3".into()).is_err());
        assert!(g.process("/sendStrings".into()).is_err());
    }

    #[test]
    fn command_box_allows_normal_commands() {
        let g = InputGuard::command_box();
        assert_eq!(g.process("/list".into()).unwrap(), "/list");
        assert_eq!(g.process("/chat 小张".into()).unwrap(), "/chat 小张");
        assert_eq!(g.process("/trust 小张".into()).unwrap(), "/trust 小张");
    }

    #[test]
    fn text_box_allows_anything() {
        // 纯文本语义：以 / 开头、含换行、含引号都不拦截（由 sendStrings 协议承载）
        let g = InputGuard::text_box();
        assert_eq!(g.process("/list".into()).unwrap(), "/list");
        assert_eq!(g.process("cmd/cls".into()).unwrap(), "cmd/cls");
        assert_eq!(g.process("第一行\n第二行".into()).unwrap(), "第一行\n第二行");
        assert_eq!(g.process("含\"引号\"".into()).unwrap(), "含\"引号\"");
    }
}
