//! CJK 字体加载：egui 默认字体（Hack + emoji）不含中文字形。
//!
//! 三级策略：
//! 1. 系统 CJK 字体（按平台候选路径逐个探测，Windows msyh/simhei、Linux Noto CJK/文泉驿、macOS PingFang）
//! 2. **内置兜底**：仓库 `assets/fonts/` 的 Noto Sans CJK SC（OFL 授权，include_bytes 编译进二进制）——
//!    任何环境（裸容器/最小 WSL/无字体系统）都能显示中文
//! 3. 全部落空（理论上不可能，内置必成功）→ 打警告
//!
//! `P2P_FONT_FORCE_EMBEDDED=1` 可跳过系统探测，强制用内置字体（测试用）。

use std::path::Path;

/// 各平台候选 CJK 字体路径（按优先级）
const CJK_FONT_CANDIDATES: &[&str] = &[
    // Windows
    "C:\\Windows\\Fonts\\msyh.ttc",
    "C:\\Windows\\Fonts\\msyhbd.ttc",
    "C:\\Windows\\Fonts\\simhei.ttf",
    "C:\\Windows\\Fonts\\simsun.ttc",
    // Linux（Noto CJK / 文泉驿常见安装路径）
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    // macOS
    "/System/Library/Fonts/PingFang.ttc",
    "/System/Library/Fonts/STHeiti Light.ttc",
];

/// 内置兜底字体：Noto Sans CJK SC Regular（OFL 授权，SimplifiedChinese 静态 OTF）
const EMBEDDED_CJK_FONT: &[u8] = include_bytes!("../../assets/fonts/NotoSansCJKsc-Regular.otf");
const EMBEDDED_CJK_NAME: &str = "NotoSansCJKsc-Regular（内置）";

/// 字体加载结果（供运行日志记录）
pub struct LoadedFont {
    /// 人类可读的来源描述
    pub source: String,
}

/// 注入 CJK 字体到 egui；返回实际加载的字体（写运行日志用）
pub fn install(ctx: &egui::Context) -> LoadedFont {
    // 测试开关：强制走内置兜底
    let force_embedded = std::env::var("P2P_FONT_FORCE_EMBEDDED").is_ok();

    if !force_embedded {
        if let Some(path) = pick_system_font(CJK_FONT_CANDIDATES) {
            let name = Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string());
            if let (Some(name), Ok(bytes)) = (name, std::fs::read(path)) {
                if !bytes.is_empty() {
                    inject(ctx, &name, &bytes);
                    return LoadedFont {
                        source: format!("系统字体: {path}"),
                    };
                }
            }
        }
    }

    // 内置兜底：系统候选全部落空（或强制开关）时使用
    inject(ctx, EMBEDDED_CJK_NAME, EMBEDDED_CJK_FONT);
    LoadedFont {
        source: format!("内置兜底: {EMBEDDED_CJK_NAME}（include_bytes）"),
    }
}

/// 在候选路径中找到第一个存在的字体文件
fn pick_system_font<'a>(candidates: &[&'a str]) -> Option<&'a str> {
    candidates
        .iter()
        .copied()
        .find(|p| std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false))
}

/// 把字体注入 egui 字体家族末尾（fallback；Proportional 与 Monospace 都覆盖）
fn inject(ctx: &egui::Context, name: &str, bytes: &[u8]) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        name.to_string(),
        std::sync::Arc::new(egui::FontData::from_owned(bytes.to_vec())),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push(name.to_string());
    }
    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_system_font_skips_missing_paths() {
        assert_eq!(pick_system_font(&["/nonexistent/a.ttc", "/nonexistent/b.ttf"]), None);
    }

    #[test]
    fn pick_system_font_finds_existing() {
        // 用本文件自身路径充当"存在的字体文件"（只测存在性判定，不测字体格式）
        let self_path = file!(); // source/ui/fonts.rs
        assert_eq!(pick_system_font(&[self_path]), Some(self_path));
    }

    #[test]
    fn embedded_font_present_and_valid_signature() {
        assert!(EMBEDDED_CJK_FONT.len() > 1_000_000, "内置字体应大于 1MB");
        // OpenType 魔数：OTTO（CFF 轮廓）或 0x00010000（TrueType 轮廓）
        let magic = &EMBEDDED_CJK_FONT[0..4];
        let is_otto = magic == b"OTTO";
        let is_ttf = magic == [0x00, 0x01, 0x00, 0x00];
        assert!(is_otto || is_ttf, "内置字体魔数异常: {magic:?}");
    }

    #[test]
    fn embedded_font_is_last_resort_after_all_candidates() {
        // 候选链全部不存在时 pick 返回 None → install 走内置兜底（流程约定）
        assert!(pick_system_font(&[]).is_none());
    }
}
