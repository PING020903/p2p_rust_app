use fnv::FnvHashMap;

pub type NodeId = usize;
pub const ROOT: NodeId = 0;

pub type Handler<C> = Box<dyn FnMut(&mut C, &[&str])>;
pub type DataHandler = Box<dyn FnMut(&[u8]) -> i32>;

#[derive(Debug, PartialEq)]
pub enum CmdError {
    NotFound,
}

struct Node<C> {
    token: String,
    help: Option<String>,
    handler: Option<Handler<C>>,
    data_handler: Option<DataHandler>,
    children: FnvHashMap<String, NodeId>,
}

pub struct CmdTree<C> {
    nodes: Vec<Node<C>>,
    active_data: Option<NodeId>,
}

impl<C> CmdTree<C> {
    pub fn new() -> Self {
        CmdTree {
            nodes: vec![Node {
                token: String::new(),
                help: None,
                handler: None,
                data_handler: None,
                children: FnvHashMap::default(),
            }],
            active_data: None,
        }
    }

    pub fn register<H>(&mut self, parent: NodeId, token: &str, handler: H) -> NodeId
    where
        H: FnMut(&mut C, &[&str]) + 'static,
    {
        self.register_inner(parent, token, Some(Box::new(handler)))
    }

    pub fn register_route(&mut self, parent: NodeId, token: &str) -> NodeId {
        self.register_inner(parent, token, None)
    }

    fn register_inner(
        &mut self,
        parent: NodeId,
        token: &str,
        handler: Option<Handler<C>>,
    ) -> NodeId {
        assert!(parent < self.nodes.len(), "无效的父节点引用: {parent}");
        if let Some(&existing) = self.nodes[parent].children.get(token) {
            if let Some(h) = handler {
                self.nodes[existing].handler = Some(h);
            }
            return existing;
        }
        let id = self.nodes.len();
        self.nodes.push(Node {
            token: token.to_string(),
            help: None,
            handler,
            data_handler: None,
            children: FnvHashMap::default(),
        });
        self.nodes[parent].children.insert(token.to_string(), id);
        id
    }

    pub fn set_help(&mut self, node: NodeId, text: &str) {
        assert!(node < self.nodes.len(), "无效的节点引用: {node}");
        self.nodes[node].help = Some(text.to_string());
    }

    pub fn set_data_handler<D>(&mut self, node: NodeId, dh: D)
    where
        D: FnMut(&[u8]) -> i32 + 'static,
    {
        assert!(node < self.nodes.len(), "无效的节点引用: {node}");
        self.nodes[node].data_handler = Some(Box::new(dh));
    }

    pub fn parse(&mut self, input: &str, ctx: &mut C) -> Result<(), CmdError> {
        let tokens = tokenize(input);
        if tokens.is_empty() {
            return Err(CmdError::NotFound);
        }
        if tokens[0] == "help" {
            self.show_help();
            return Ok(());
        }

        let mut current = ROOT;
        let mut best: Option<(NodeId, usize)> = None;
        for (depth, tok) in tokens.iter().enumerate() {
            match self.nodes[current].children.get(*tok) {
                Some(&next) => {
                    current = next;
                    if self.nodes[current].handler.is_some() {
                        best = Some((current, depth + 1));
                    }
                }
                None => break,
            }
        }

        match best {
            Some((node, depth)) => {
                self.active_data = Some(node);
                let args = &tokens[depth..];
                let handler = self.nodes[node].handler.as_mut().unwrap();
                handler(ctx, args);
                Ok(())
            }
            None => Err(CmdError::NotFound),
        }
    }

    pub fn feed_data(&mut self, buf: &[u8]) -> i32 {
        match self.active_data {
            Some(node) => match self.nodes[node].data_handler.as_mut() {
                Some(dh) => dh(buf),
                None => -1,
            },
            None => -1,
        }
    }

    pub fn show_help(&self) {
        let mut entries: Vec<(String, String)> = Vec::new();
        self.collect_help(ROOT, String::new(), &mut entries);
        entries.push(("help".to_string(), "显示本帮助".to_string()));
        let width = entries.iter().map(|(path, _)| path.len()).max().unwrap_or(0);
        println!("可用命令:");
        for (path, help) in entries {
            if help.is_empty() {
                println!("  {path}");
            } else {
                println!("  {path:<width$}  {help}");
            }
        }
    }

    fn collect_help(&self, node: NodeId, prefix: String, out: &mut Vec<(String, String)>) {
        let mut children: Vec<(&String, &NodeId)> = self.nodes[node].children.iter().collect();
        children.sort_by_key(|(token, _)| (*token).clone());
        for (token, &child) in children {
            let path = if prefix.is_empty() {
                token.to_string()
            } else {
                format!("{prefix} {token}")
            };
            if self.nodes[child].handler.is_some() {
                let help = self.nodes[child].help.clone().unwrap_or_default();
                out.push((path.clone(), help));
            }
            self.collect_help(child, path.clone(), out);
        }
    }

    pub fn show(&self) {
        println!("cmdTree:");
        self.show_walk(ROOT, 0);
    }

    fn show_walk(&self, node: NodeId, depth: usize) {
        let mut children: Vec<(&String, &NodeId)> = self.nodes[node].children.iter().collect();
        children.sort_by_key(|(token, _)| (*token).clone());
        for (token, &child) in children {
            let mut marks = String::new();
            if self.nodes[child].handler.is_some() {
                marks.push_str(" H");
            }
            if self.nodes[child].data_handler.is_some() {
                marks.push_str(" D");
            }
            println!("{}\"{token}\"{marks}", "  ".repeat(depth + 1));
            self.show_walk(child, depth + 1);
        }
    }
}

pub fn tokenize(input: &str) -> Vec<&str> {
    let mut tokens: Vec<&str> = Vec::new();
    let mut rest = input;
    loop {
        let trimmed = rest.trim_start_matches(char::is_whitespace);
        if trimmed.is_empty() {
            break;
        }
        if let Some(after_quote) = trimmed.strip_prefix('"') {
            match after_quote.find('"') {
                Some(end) => {
                    tokens.push(&after_quote[..end]);
                    rest = &after_quote[end + 1..];
                }
                None => {
                    tokens.push(after_quote);
                    break;
                }
            }
        } else {
            let end = trimmed
                .find(char::is_whitespace)
                .unwrap_or(trimmed.len());
            tokens.push(&trimmed[..end]);
            rest = &trimmed[end..];
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_basic() {
        assert_eq!(tokenize("cmd param"), vec!["cmd", "param"]);
        assert_eq!(tokenize("  a   b  "), vec!["a", "b"]);
        assert_eq!(tokenize(""), Vec::<&str>::new());
    }

    #[test]
    fn tokenize_quotes() {
        assert_eq!(tokenize("cmd \"hello world\""), vec!["cmd", "hello world"]);
        assert_eq!(tokenize("\"raw data\" cmd"), vec!["raw data", "cmd"]);
        assert_eq!(tokenize("\"unclosed"), vec!["unclosed"]);
        assert_eq!(tokenize("\"\""), vec![""]);
    }

    #[test]
    fn deepest_match_and_passthrough() {
        let mut tree: CmdTree<Vec<String>> = CmdTree::new();
        let test = tree.register(ROOT, "test", |ctx, _| ctx.push("test".into()));
        tree.register(test, "hardware", |ctx, args| {
            ctx.push(format!("hardware:{args:?}"))
        });

        let mut log: Vec<String> = Vec::new();
        tree.parse("test", &mut log).unwrap();
        tree.parse("test hardware", &mut log).unwrap();
        tree.parse("test hardware 0", &mut log).unwrap();
        assert_eq!(log, vec!["test", "hardware:[]", "hardware:[\"0\"]"]);
    }

    #[test]
    fn route_only_node_not_found() {
        let mut tree: CmdTree<()> = CmdTree::new();
        let wait = tree.register_route(ROOT, "wait");
        let dat = tree.register_route(wait, "dat");
        tree.register(dat, "names", |_, _| {});

        assert_eq!(tree.parse("wait", &mut ()), Err(CmdError::NotFound));
        assert_eq!(tree.parse("wait dat", &mut ()), Err(CmdError::NotFound));
        assert_eq!(tree.parse("nope", &mut ()), Err(CmdError::NotFound));
        assert!(tree.parse("wait dat names", &mut ()).is_ok());
    }

    #[test]
    fn multi_instance_no_collision() {
        let mut tree_a: CmdTree<Vec<String>> = CmdTree::new();
        let mut tree_b: CmdTree<Vec<String>> = CmdTree::new();
        tree_a.register(ROOT, "status", |ctx, _| ctx.push("A".into()));
        tree_b.register(ROOT, "status", |ctx, _| ctx.push("B".into()));

        let mut log_a: Vec<String> = Vec::new();
        let mut log_b: Vec<String> = Vec::new();
        tree_a.parse("status", &mut log_a).unwrap();
        tree_b.parse("status", &mut log_b).unwrap();
        assert_eq!(log_a, vec!["A"]);
        assert_eq!(log_b, vec!["B"]);
    }

    #[test]
    fn data_handler_activation() {
        let mut tree: CmdTree<()> = CmdTree::new();
        let recv = tree.register(ROOT, "recv", |_, _| {});
        tree.set_data_handler(recv, |buf| buf.len() as i32);

        assert_eq!(tree.feed_data(&[1, 2, 3]), -1);
        tree.parse("recv", &mut ()).unwrap();
        assert_eq!(tree.feed_data(&[1, 2, 3]), 3);
    }

    #[test]
    fn builtin_help_runs() {
        let mut tree: CmdTree<()> = CmdTree::new();
        tree.register(ROOT, "dial", |_, _| {});
        assert!(tree.parse("help", &mut ()).is_ok());
    }

    #[test]
    fn set_help_and_show() {
        let mut tree: CmdTree<()> = CmdTree::new();
        let dial = tree.register(ROOT, "dial", |_, _| {});
        tree.set_help(dial, "连接对方节点");
        let quit = tree.register(ROOT, "quit", |_, _| {});
        tree.set_help(quit, "退出");
        tree.show_help();
        tree.show();
    }
}
