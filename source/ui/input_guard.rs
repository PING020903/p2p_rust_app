//! 输入检查&修改层（GUI 专用）：发送到子进程前对输入行做规则化处理。
//!
//! 接口可扩展：实现 `Rule` 塞进 `InputGuard::process` 的规则链即可。
//! 规则链按序执行：`Allow` 继续下一条；`Rewrite` 用新文本继续；`Block` 立即拦截返回理由。
//! 最终 `Ok(文本)` = 允许发送；`Err(理由)` = 拦截（GUI 展示 + 日志）。

/// 单条规则的处理结果
pub enum Action {
    /// 放行，继续下一条规则
    Allow,
    /// 改写后继续（如多行折叠）
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
    /// 默认规则链：拦截终端逃逸 + 多行折叠
    pub fn new() -> Self {
        InputGuard {
            rules: vec![
                Box::new(BlockTerminalEscape),
                Box::new(CollapseNewlines),
            ],
        }
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

/// 多行输入折叠：内部换行合并为空格，避免一条消息被 CLI 逐行读取拆成多条
struct CollapseNewlines;

impl Rule for CollapseNewlines {
    fn name(&self) -> &'static str {
        "collapse_newlines"
    }

    fn apply(&self, line: &str) -> Action {
        if line.contains('\n') || line.contains('\r') {
            let joined = line.split_whitespace().collect::<Vec<_>>().join(" ");
            Action::Rewrite(joined)
        } else {
            Action::Allow
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_terminal_escape() {
        let g = InputGuard::new();
        for bad in ["cmd/cls", "ps/Get-Date", "sh/clear"] {
            assert!(g.process(bad.to_string()).is_err(), "{bad} 应被拦截");
        }
    }

    #[test]
    fn allows_normal_input() {
        let g = InputGuard::new();
        assert_eq!(g.process("/list".into()).unwrap(), "/list");
        assert_eq!(g.process("你好，收到吗？".into()).unwrap(), "你好，收到吗？");
    }

    #[test]
    fn collapses_newlines() {
        let g = InputGuard::new();
        assert_eq!(
            g.process("第一行\n第二行".into()).unwrap(),
            "第一行 第二行"
        );
        assert_eq!(g.process("  a\r\nb  ".into()).unwrap(), "a b");
    }

    #[test]
    fn rewrite_then_block() {
        // 多行折叠不影响逃逸拦截：折叠规则在逃逸规则之后，先被拦截
        let g = InputGuard::new();
        assert!(g.process("ps/\nls".into()).is_err());
    }
}
