use colored::Colorize;
use std::fmt;

/// 彩色打印组件
/// 提供简单易用的彩色打印功能，方便调试和检查代码运行状态

/// 颜色枚举，简化颜色选择
#[derive(Debug, Clone, Copy)]
pub enum Color {
    Red,
    Green,
    Blue,
    Yellow,
    Cyan,
    Magenta,
    White,
    Black,
}

/// 将Color枚举转换为colored库的颜色
fn to_colored_color(color: Color) -> colored::Color {
    match color {
        Color::Red => colored::Color::Red,
        Color::Green => colored::Color::Green,
        Color::Blue => colored::Color::Blue,
        Color::Yellow => colored::Color::Yellow,
        Color::Cyan => colored::Color::Cyan,
        Color::Magenta => colored::Color::Magenta,
        Color::White => colored::Color::White,
        Color::Black => colored::Color::Black,
    }
}

/// 基础彩色打印函数
/// # 参数
/// - color: 颜色枚举
/// - text: 要打印的文本
/// # 示例
/// ```
/// color_print::color_print(color_print::Color::Red, "错误信息");
/// ```
pub fn color_print(color: Color, text: &str) {
    let colored_text = text.color(to_colored_color(color));
    println!("{}", colored_text);
}

/// 成功信息打印（绿色）
pub fn print_success(text: &str) {
    color_print(Color::Green, &format!("✓ 成功: {}", text));
}

/// 警告信息打印（黄色）
pub fn print_warning(text: &str) {
    color_print(Color::Yellow, &format!("⚠ 警告: {}", text));
}

/// 错误信息打印（红色）
pub fn print_error(text: &str) {
    color_print(Color::Red, &format!("✗ 错误: {}", text));
}

/// 信息打印（蓝色）
pub fn print_info(text: &str) {
    color_print(Color::Blue, &format!("ℹ 信息: {}", text));
}

/// 调试打印宏，显示文件名和行号
/// # 用法
/// ```
/// debug_print!("变量值: {}", x);
/// ```
/// 输出格式: [文件名:行号] 消息内容
#[macro_export]
macro_rules! debug_print {
    ($($arg:tt)*) => {
        {
            use colored::Colorize;
            let location = format!("{}:{}", file!(), line!());
            let message = format!("{}", format_args!($($arg)*));
            println!("{} {}", location.cyan(), message);
        }
    };
}

/// 带颜色的调试打印宏
/// # 用法
/// ```
/// color_debug_print!(Color::Red, "变量值: {}", x);
/// ```
#[macro_export]
macro_rules! color_debug_print {
    ($color:expr, $($arg:tt)*) => {
        {
            use colored::Colorize;
            use $crate::Color;
            let location = format!("{}:{}", file!(), line!());
            let message = format!("{}", format_args!($($arg)*));
            let colored_message = match $color {
                Color::Red => message.red(),
                Color::Green => message.green(),
                Color::Blue => message.blue(),
                Color::Yellow => message.yellow(),
                Color::Cyan => message.cyan(),
                Color::Magenta => message.magenta(),
                Color::White => message.white(),
                Color::Black => message.black(),
            };
            println!("{} {}", location.cyan(), colored_message);
        }
    };
}

/// 测试函数，展示各种彩色打印功能
pub fn demo() {
    println!("=== 彩色打印组件演示 ===");
    
    // 基础颜色测试
    println!("\n1. 基础颜色测试:");
    color_print(Color::Red, "红色文本");
    color_print(Color::Green, "绿色文本");
    color_print(Color::Blue, "蓝色文本");
    color_print(Color::Yellow, "黄色文本");
    
    // 预定义级别测试
    println!("\n2. 预定义级别测试:");
    print_success("操作成功完成");
    print_warning("需要注意的情况");
    print_error("发生了一个错误");
    print_info("这是一条信息");
    
    // 调试打印测试
    println!("\n3. 调试打印测试:");
    let x = 42;
    let name = "Rust";
    debug_print!("变量 x 的值: {}", x);
    debug_print!("学习语言: {}", name);
    
    // 带颜色的调试打印测试
    println!("\n4. 带颜色的调试打印测试:");
    color_debug_print!(Color::Magenta, "这是品红色的调试信息");
    color_debug_print!(Color::Cyan, "这是青色的调试信息");
    
    println!("\n=== 演示结束 ===");
}
