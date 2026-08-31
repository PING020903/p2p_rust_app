//! CJK 字体加载：egui 默认字体（Hack + emoji）不含中文字形，
//! 运行时按平台加载系统字体作为回退（Windows 验证目标；Linux/macOS 顺带覆盖）。

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
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    // macOS
    "/System/Library/Fonts/PingFang.ttc",
    "/System/Library/Fonts/STHeiti Light.ttc",
];

/// 加载第一个存在的 CJK 字体，注入 egui 字体家族末尾（fallback）。
/// 找不到时打警告（中文会渲染为方框），不影响英文界面。
pub fn install(ctx: &egui::Context) {
    for path in CJK_FONT_CANDIDATES {
        let Some(name) = Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
        else {
            continue;
        };
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            name.clone(),
            std::sync::Arc::new(egui::FontData::from_owned(bytes)),
        );
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push(name.clone());
        }
        ctx.set_fonts(fonts);
        return;
    }
    eprintln!("警告: 未找到 CJK 字体，中文可能显示为方框");
}
