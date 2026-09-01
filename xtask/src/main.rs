use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 归档的可执行文件：CLI + GUI 双 bin（一套代码两平台产物统一收集）
const BINS: &[&str] = &["p2p_rust_app", "p2p_rust_app_gui"];

fn main() {
    if let Err(e) = run() {
        eprintln!("错误: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let sub = args.next().ok_or_else(|| "用法: cargo xtask <build> [--release] [--target <triple>]".to_string())?;
    match sub.as_str() {
        "build" => {
            let mut release = false;
            let mut target: Option<String> = None;
            let mut rest = Vec::new();
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--release" => release = true,
                    "--target" => {
                        target = Some(args.next().ok_or("--target 缺少参数")?);
                    }
                    other => rest.push(other.to_string()),
                }
            }
            let profile = if release { "release" } else { "debug" };
            build_and_copy(profile, target.as_deref(), &rest)
        }
        other => Err(format!("未知子命令: {other}（当前仅支持 build）")),
    }
}

fn repo_root() -> Result<PathBuf, String> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let xtask_dir = Path::new(manifest_dir);
    Ok(xtask_dir
        .parent()
        .ok_or("无法确定仓库根目录")?
        .to_path_buf())
}

fn target_dir(repo_root: &Path) -> PathBuf {
    match env::var("CARGO_TARGET_DIR") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => repo_root.join("target"),
    }
}

fn build_and_copy(profile: &str, target: Option<&str>, extra: &[String]) -> Result<(), String> {
    let repo = repo_root()?;

    let mut cmd = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".to_string()));
    cmd.arg("build");
    if let Some(t) = target {
        cmd.arg("--target").arg(t);
    }
    if profile == "release" {
        cmd.arg("--release");
    }
    cmd.args(extra);
    let status = cmd
        .status()
        .map_err(|e| format!("无法启动 cargo build: {e}"))?;
    if !status.success() {
        return Err("cargo build 失败".to_string());
    }

    let exe_suffix = env::consts::EXE_SUFFIX;

    let target_dir = target_dir(&repo);
    let src_dir = match target {
        None => target_dir.join(profile),
        Some(t) => target_dir.join(t).join(profile),
    };

    let os = env::consts::OS;
    let arch = env::consts::ARCH;
    let os_dir = format!("{os}-{arch}");
    let dest_dir = target_dir.join(profile).join("bin").join(&os_dir);
    fs::create_dir_all(&dest_dir)
        .map_err(|e| format!("创建目录 {} 失败: {e}", dest_dir.display()))?;

    for bin in BINS {
        let bin_name = format!("{bin}{exe_suffix}");
        let src = src_dir.join(&bin_name);
        if !src.exists() {
            return Err(format!("未找到构建产物: {}", src.display()));
        }
        let dest = dest_dir.join(&bin_name);
        fs::copy(&src, &dest).map_err(|e| {
            format!("复制 {} -> {} 失败: {e}", src.display(), dest.display())
        })?;
        println!("已输出: {}", dest.display());
    }
    Ok(())
}
