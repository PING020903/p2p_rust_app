use std::io;
use colored::Colorize;

#[derive(Debug)]
pub struct PublicInfo {
    pub name: String,
    pub gender: String,
    pub class: String,
}

#[derive(Debug)]
pub struct PrivateInfo {
    qq: String,
    email: String,
    age: u8,
}

impl PrivateInfo {
    pub fn show(&self) {
        println!("    QQ:    {}", self.qq);
        println!("    邮箱:  {}", self.email);
        println!("    年龄:  {}", self.age);
    }
}

#[derive(Debug)]
pub struct Student {
    pub public_info: PublicInfo,
    private_info: PrivateInfo,
}

impl Student {
    pub fn new(name: &str, gender: &str, class: &str, qq: &str, email: &str, age: u8) -> Self {
        Student {
            public_info: PublicInfo {
                name: name.to_string(),
                gender: gender.to_string(),
                class: class.to_string(),
            },
            private_info: PrivateInfo {
                qq: qq.to_string(),
                email: email.to_string(),
                age,
            },
        }
    }

    pub fn show_public(&self) {
        println!("  班级: {} | 姓名: {} | 性别: {}", self.public_info.class, self.public_info.name, self.public_info.gender);
    }

    pub fn show_private(&self) {
        println!("  {} 的私密信息:", self.public_info.name);
        self.private_info.show();
    }
}

pub struct StudentManager {
    students: Vec<Student>,
}

impl StudentManager {
    pub fn new() -> Self {
        StudentManager { students: Vec::new() }
    }

    pub fn add(&mut self, student: Student) {
        self.students.push(student);
    }

    pub fn list_public(&self) {
        if self.students.is_empty() {
            println!("  (暂无学生)");
            return;
        }
        for s in &self.students {
            s.show_public();
        }
    }

    pub fn find_by_name(&self, name: &str) -> Vec<&Student> {
        self.students.iter().filter(|s: &&Student| s.public_info.name == name).collect()
    }

    pub fn status(&self) {
        println!("  已存储: {} 人 | 容量: {} 人", self.students.len(), self.students.capacity());
    }
}

fn read_line(prompt: &str) -> String {
    print!("{}", prompt);
    use std::io::Write;
    io::stdout().flush().unwrap();
    let mut buf = String::new();
    io::stdin().read_line(&mut buf).unwrap();
    buf.trim().to_string()
}

struct StudentCtx {
    manager: StudentManager,
    quit: bool,
}

pub fn run() {
    use crate::cmd_tree::{CmdError, CmdTree, ROOT};

    println!("=== 学生信息管理 ===");
    println!("命令: add / list / find / status / quit\n");

    let mut ctx = StudentCtx {
        manager: StudentManager::new(),
        quit: false,
    };

    let mut tree: CmdTree<StudentCtx> = CmdTree::new();
    let add = tree.register(ROOT, "add", |ctx, _| {
        let name: String = read_line("  姓名: ");
        let gender: String = read_line("  性别: ");
        let class: String = read_line("  班级: ");
        let qq: String = read_line("  QQ: ");
        let email: String = read_line("  邮箱: ");
        let age: u8 = read_line("  年龄: ").parse().unwrap_or(0);

        ctx.manager
            .add(Student::new(&name, &gender, &class, &qq, &email, age));
        println!("{}", "  添加成功".green());
    });
    tree.set_help(add, "添加学生（交互式输入信息）");
    let list = tree.register(ROOT, "list", |ctx, _| ctx.manager.list_public());
    tree.set_help(list, "列出所有学生的公开信息");
    let find = tree.register(ROOT, "find", |ctx, _| {
        let name: String = read_line("  查找姓名: ");
        let results: Vec<&Student> = ctx.manager.find_by_name(&name);
        if results.is_empty() {
            println!("{} {}", "未找到:".red(), name);
        } else {
            println!("  找到 {} 人:", results.len());
            for s in &results {
                s.show_public();
                s.show_private();
            }
        }
    });
    tree.set_help(find, "按姓名查找学生");
    let status = tree.register(ROOT, "status", |ctx, _| ctx.manager.status());
    tree.set_help(status, "显示存储状态");
    let quit = tree.register(ROOT, "quit", |ctx, _| ctx.quit = true);
    tree.set_help(quit, "退出");
    let q = tree.register(ROOT, "q", |ctx, _| ctx.quit = true);
    tree.set_help(q, "退出");

    loop {
        let cmd: String = read_line("> ").to_lowercase();
        if cmd.is_empty() {
            continue;
        }

        if let Err(CmdError::NotFound) = tree.parse(&cmd, &mut ctx) {
            println!("{} 未知命令", "错误:".red());
        }
        if ctx.quit {
            println!("{}", "再见！".yellow());
            break;
        }
    }
}
