---
name: rust-for-c-dev
description: Use when editing or generating Rust code in this project. After any code change, report what was done and why, explaining Rust concepts with C analogies. Use ONLY when Rust code is being written, edited, or reviewed.
---

# Rust for C Developers

The project owner is experienced in C but new to Rust. After every code edit, provide a concise report in Chinese:

## Report Format

1. **做了什么** — 列出修改/新增的文件和关键代码段
2. **为什么这么做** — 解释设计选择，用 C 的概念类比 Rust 的语法和机制

## Teaching Style: 实践论（Bottom-Up）

The owner's learning philosophy: concrete implementation → extract general rule → use rule to guide new practice. Skipping the first step is unacceptable.

**Mandatory explanation order:**

1. **先走内存** — 任何概念先展示"内存里实际发生了什么"（谁分配、谁持有、谁释放、数据在哪）
2. **再给 C 等价代码** — 用 C 写出功能完全等价的实现，让 owner 能逐行对照
3. **最后才总结规则** — 在前两步的基础上，一句话提炼 Rust 的抽象规则

**Forbidden:**
- 不允许先抛抽象概念再解释（如"所有权保证内存安全"这种开头）
- 不允许说"记住这个用法就行"
- 不允许跳过底层机制直接讲惯用法

**When owner questions something:**
- 说明 owner 的心智模型和实际执行流程有偏差
- 用逐步执行追踪（step-by-step trace）来纠正，展示每一步变量的状态
- 不要重复抽象解释，换一层更具体的实现细节来讲

## C Analogy Reference

| Rust 概念 | C 类比 |
|-----------|--------|
| `&` / `&mut` 引用 | 指针，但编译器保证有效性 |
| 所有权 (Ownership) | 谁 malloc 谁 free，但编译器自动决定何时 free |
| 生命周期 `'a` | 指针的有效作用域，编译器强制检查 |
| `enum` + `match` | tagged union + switch，但类型安全 |
| `trait` | 函数指针表（类似 vtable / 接口） |
| `Option<T>` / `Result<T,E>` | 返回值 + errno / NULL 指针，但强制处理 |
| `Vec<T>` | 自动扩容的动态数组 (malloc + realloc + free) |
| `String` vs `&str` | `char*`（拥有堆内存） vs `const char*`（借用） |
| `Box<T>` | `malloc` 返回的指针，离开作用域自动 `free` |
| `unsafe` 块 | 告诉编译器"我自己管内存"，等同于纯 C 模式 |
| 模式匹配解构 | 类似 union 访问成员，但编译器保证类型正确 |
| `impl` 块 | .h 声明结构体，.c 写函数，只是合在一个文件里 |
| 泛型 `<T>` | 类似 C 的 `void*` + 宏，但类型安全且零开销 |
| `Vec::new()` | `buf=NULL; len=0; cap=0;` |
| `Vec::push()` | `if(len==cap) realloc; buf[len++]=item;` |
| `Vec::pop()` | `len--;`（不缩容，cap 不变） |
| `unwrap_or(default)` | 解析失败返回默认值，不 abort |
| `unwrap()` | `if(failed) abort();` |
| `?` 操作符 | `if(failed) return err;` 的语法糖 |

## Rules

- 解释要简洁，不要写成教程，点到为止
- 如果改动涉及多个文件，按文件分组说明
- 如果某个 Rust 惯用法没有直接的 C 类比，直接说明区别即可
- 不要在没有编辑代码时主动触发此 skill
- 用中文回复所有解释
