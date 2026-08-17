use std::io;
use colored::Colorize;

#[derive(Debug, Clone)]
enum Token {
    Number(f64),
    Op(char),
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let mut tokens: Vec<Token> = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        if c == ' ' || c == '\t' {
            i += 1;
            continue;
        }

        if c == '+' || c == '-' || c == '*' || c == '/' {
            tokens.push(Token::Op(c));
            i += 1;
            continue;
        }

        if c.is_ascii_digit() || c == '.' {
            let start = i;
            let mut dot_count = 0;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                if chars[i] == '.' {
                    dot_count += 1;
                    if dot_count > 1 {
                        return Err(format!("非法数字格式，位置 {}", start));
                    }
                }
                i += 1;
            }
            let num_str: String = chars[start..i].iter().collect();
            let value: f64 = num_str
                .parse()
                .map_err(|_| format!("无法解析数字: {}", num_str))?;
            tokens.push(Token::Number(value));
            continue;
        }

        return Err(format!("无法识别的字符: '{}'", c));
    }

    Ok(tokens)
}

fn evaluate(tokens: &[Token]) -> Result<f64, String> {
    if tokens.is_empty() {
        return Err("表达式为空".to_string());
    }

    let first = &tokens[0];
    let mut values: Vec<f64> = match first {
        Token::Number(n) => vec![*n],
        Token::Op(_) => return Err("表达式不能以运算符开头".to_string()),
    };

    let mut i = 1;
    while i < tokens.len() {
        let op = match &tokens[i] {
            Token::Op(op) => *op,
            Token::Number(_) => return Err("缺少运算符（两个数字相邻）".to_string()),
        };

        if i + 1 >= tokens.len() {
            return Err("表达式不能以运算符结尾".to_string());
        }

        let num = match &tokens[i + 1] {
            Token::Number(n) => *n,
            Token::Op(_) => return Err("运算符后面必须跟数字".to_string()),
        };

        match op {
            '*' => {
                let last = values.len() - 1;
                values[last] *= num;
            }
            '/' => {
                if num == 0.0 {
                    return Err("除数不能为零".to_string());
                }
                let last = values.len() - 1;
                values[last] /= num;
            }
            '+' => values.push(num),
            '-' => values.push(-num),
            _ => return Err(format!("未知运算符: {}", op)),
        }

        i += 2;
    }

    Ok(values.iter().sum())
}

struct CalcCtx {
    quit: bool,
}

pub fn run() {
    use crate::cmd_tree::{CmdError, CmdTree, ROOT};

    println!("=== 简易计算器 ===");
    println!("支持 +, -, *, / 运算，先乘除后加减");
    println!("输入 'q' 退出\n");

    let mut ctx = CalcCtx { quit: false };
    let mut tree: CmdTree<CalcCtx> = CmdTree::new();
    let q = tree.register(ROOT, "q", |ctx, _| ctx.quit = true);
    tree.set_help(q, "退出计算器");
    let big_q = tree.register(ROOT, "Q", |ctx, _| ctx.quit = true);
    tree.set_help(big_q, "退出计算器");

    loop {
        print!("{}", "> ".green());
        use std::io::Write;
        io::stdout().flush().unwrap();

        let mut input = String::new();
        if io::stdin().read_line(&mut input).unwrap() == 0 {
            break;
        }
        let input = input.trim();
        if input.is_empty() {
            continue;
        }

        match tree.parse(input, &mut ctx) {
            Ok(()) => {}
            Err(CmdError::NotFound) => match tokenize(input) {
                Ok(tokens) => match evaluate(&tokens) {
                    Ok(result) => println!("= {}", result),
                    Err(e) => println!("{} {}", "计算错误:".red(), e),
                },
                Err(e) => println!("{} {}", "解析错误:".red(), e),
            },
        }
        if ctx.quit {
            println!("{}", "再见！".yellow());
            break;
        }
    }
}
