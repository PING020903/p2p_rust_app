// 导入彩色打印模块
mod color_print;
use color_print::{Color};



mod calculator;
mod chat;
mod cmd_tree;
mod mdns_stealth;
mod student;

struct MainCtx {
    quit: bool,
}

fn main() {
    use cmd_tree::{CmdError, CmdTree, ROOT};
    use colored::Colorize;
    use std::io::{self, Write};

    let mut tree: CmdTree<MainCtx> = CmdTree::new();
    let c1 = tree.register(ROOT, "1", |_, _| calculator::run());
    tree.set_help(c1, "计算器");
    let c2 = tree.register(ROOT, "2", |_, _| student::run());
    tree.set_help(c2, "学生信息管理");
    let c3 = tree.register(ROOT, "3", |_, _| {
        simulate_code_execution();
        color_print::demo();
    });
    tree.set_help(c3, "彩色打印演示");
    let c4 = tree.register(ROOT, "4", |_, _| chat::run());
    tree.set_help(c4, "P2P 聊天");
    let cq = tree.register(ROOT, "q", |ctx, _| ctx.quit = true);
    tree.set_help(cq, "退出");
    let cQ = tree.register(ROOT, "Q", |ctx, _| ctx.quit = true);
    tree.set_help(cQ, "退出");

    let mut ctx = MainCtx { quit: false };

    loop {
        println!("\n=== 主菜单 ===");
        println!("  1. 计算器");
        println!("  2. 学生信息管理");
        println!("  3. 彩色打印演示");
        println!("  4. P2P 聊天");
        println!("  q. 退出");
        print!("{}", "> ".green());
        io::stdout().flush().unwrap();

        let mut input = String::new();
        if io::stdin().read_line(&mut input).unwrap() == 0 {
            break;
        }
        let input = input.trim();
        if input.is_empty() {
            continue;
        }

        if let Err(CmdError::NotFound) = tree.parse(input, &mut ctx) {
            println!("{} 无效选择", "错误:".red());
        }
        if ctx.quit {
            println!("{}", "再见！".yellow());
            break;
        }
    }
}

/// 模拟代码执行过程
fn simulate_code_execution() {
    color_print::print_info("开始执行任务...");
    
    // 模拟步骤1
    crate::debug_print!("步骤1: 准备数据");
    color_print::print_success("数据准备完成");
    
    // 模拟步骤2
    crate::debug_print!("步骤2: 处理数据");
    color_print::print_warning("数据量较大，处理可能需要时间");

    let text = "这是一个错误信息";
    color_print::print_error(text);
    
    // 模拟步骤3
    crate::debug_print!("步骤3: 保存结果");
    color_print::print_success("任务执行完成！");
}

