//! per-identity 设置存储：`settings_{peer_id}.json`（读-改-写，避免整文件覆盖丢键）。
//! 供发现模式（discovery_mode）、下载目录（download_dir）等共享同一配置文件。

use libp2p::PeerId;
use serde_json::Value;

use super::identity::cache_dir;
use std::path::PathBuf;

/// 设置文件路径：`cache_dir()/settings_{peer_id}.json`
fn settings_path(peer_id: &PeerId) -> Result<PathBuf, String> {
    Ok(cache_dir()?.join(format!("settings_{peer_id}.json")))
}

/// 读取设置文件；缺失/损坏返回空对象
pub fn load(peer_id: &PeerId) -> Value {
    settings_path(peer_id)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| Value::Object(Default::default()))
}

/// 写单个键（读-改-写）：保留其它键不覆盖
pub fn save_setting(peer_id: &PeerId, key: &str, value: &str) -> Result<(), String> {
    let path = settings_path(peer_id)?;
    let dir = path
        .parent()
        .ok_or_else(|| "设置文件无父目录".to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("创建配置目录失败: {e}"))?;
    let mut json = load(peer_id);
    json[key] = Value::String(value.to_string());
    std::fs::write(&path, serde_json::to_string(&json).map_err(|e| format!("序列化失败: {e}"))?)
        .map_err(|e| format!("写入配置失败: {e}"))
}

/// 读取字符串设置（缺失返回 None）
fn load_str(peer_id: &PeerId, key: &str) -> Option<String> {
    load(peer_id)
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// 读取下载目录设置
pub fn load_download_dir(peer_id: &PeerId) -> Option<String> {
    load_str(peer_id, "download_dir")
}

/// 保存下载目录设置
pub fn save_download_dir(peer_id: &PeerId, dir: &str) -> Result<(), String> {
    save_setting(peer_id, "download_dir", dir)
}

/// 读取文件接收前确认开关（缺失默认 true=每次弹卡片确认；仅影响 GUI/Ask 模式）
pub fn load_confirm_file_receive(peer_id: &PeerId) -> bool {
    load_str(peer_id, "confirm_file_receive").map(|s| s != "0").unwrap_or(true)
}

/// 保存文件接收前确认开关（"1"=确认 / "0"=自动接收）
pub fn save_confirm_file_receive(peer_id: &PeerId, enabled: bool) -> Result<(), String> {
    save_setting(peer_id, "confirm_file_receive", if enabled { "1" } else { "0" })
}
