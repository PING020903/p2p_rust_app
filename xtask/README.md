# xtask — 构建与产物归档工具

构建 + 按系统归档可执行文件的辅助工具（**零依赖**，纯 std 实现）。
一套代码在 Windows / Linux 构建后，产物按系统目录统一收集。

## 用法

```bash
cargo xtask build [--release] [--target <triple>]
```

| 参数 | 说明 |
|---|---|
| （无参数） | debug 构建 |
| `--release` | release 构建 |
| `--target <triple>` | 透传给 cargo 的编译目标（预留；归档命名仍按宿主，见文末说明） |

## 它做什么

1. 执行 `cargo build`（`p2p_rust_app` CLI 与 `p2p_rust_app_gui` 两个 bin 一起构建）
2. 把构建产物复制到 `target/{debug|release}/bin/<os>-<arch>/`

`<os>-<arch>` 由 `std::env::consts` 自动检测（`windows-x86_64` / `linux-x86_64` / `macos-aarch64` …），
可执行文件后缀自动匹配（Windows `.exe` / Linux 无后缀），无需任何手动配置。

## 产物布局

```
target/
  debug/                    # --release 时为 release/
    bin/
      windows-x86_64/       # 在 Windows 上执行 cargo xtask build 生成
        p2p_rust_app.exe
        p2p_rust_app_gui.exe
      linux-x86_64/         # 在 WSL/Linux 上执行同一命令生成
        p2p_rust_app
        p2p_rust_app_gui
```

## 双平台工作流（一套代码，两平台产物）

```bash
# Windows PowerShell
cargo xtask build

# WSL / Linux（首次建议先装 rustup stable + libxkbcommon-x11-0，见 README 主文档）
cargo xtask build
```

两个平台各自原生构建（**不做交叉编译**，最省的现实路线），产物按 `<os>-<arch>` 目录
自然汇集在同一 `target/bin/` 下——GUI 与 CLI 双 bin 一并归档。

## 注意事项

- 普通 `cargo build` / `cargo run` / `cargo test` 行为完全不变（归档动作只在 xtask 里）
- debug 产物带完整调试符号，单文件数百 MB 属正常；对外发布用 `--release`
- `--target` 交叉编译场景：构建可透传，但归档目录命名仍按宿主平台（接入 zig/cross 等
  交叉工具链后再扩展为按 target 命名）
- WSL/Linux 环境备注：发行版 apt 的 cargo 版本偏旧（egui 0.36 需 rustc ≥ 1.95），
  建议改用 rustup stable；GUI 中文显示由**内置 CJK 字体兜底**保证（无需手装字库，
  安装 `fonts-noto-cjk` 可获得系统级渲染增强）
