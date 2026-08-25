//! 通用发现层：mDNS 发现模式（advertise / stealth 隐身 / off）+ per-identity 持久化。
//! 隐身监听器见 `super::mdns_stealth`。

use libp2p::PeerId;

use super::settings;

/// mDNS 发现模式：广播+发现 / 隐身（只收不发）/ 关闭
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryMode {
    AdvertiseAndDiscover,
    DiscoverOnly,
    Off,
}

impl Default for DiscoveryMode {
    fn default() -> Self {
        DiscoveryMode::AdvertiseAndDiscover
    }
}

impl DiscoveryMode {
    pub fn parse(s: &str) -> Option<DiscoveryMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "advertise" | "ad" => Some(DiscoveryMode::AdvertiseAndDiscover),
            "stealth" | "discover" | "listen" => Some(DiscoveryMode::DiscoverOnly),
            "off" | "none" => Some(DiscoveryMode::Off),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            DiscoveryMode::AdvertiseAndDiscover => "advertise（广播+发现）",
            DiscoveryMode::DiscoverOnly => "stealth（隐身：只发现不广播）",
            DiscoveryMode::Off => "off（关闭 mDNS）",
        }
    }
}

/// 读取发现模式：测试环境变量优先（P2P_DISCOVERY），否则读 per-identity 设置文件
pub fn load_discovery_mode(peer_id: &PeerId) -> DiscoveryMode {
    if let Ok(v) = std::env::var("P2P_DISCOVERY") {
        return DiscoveryMode::parse(&v).unwrap_or_default();
    }
    settings::load(peer_id)
        .get("discovery_mode")
        .and_then(|m| m.as_str())
        .and_then(DiscoveryMode::parse)
        .unwrap_or_default()
}

pub fn save_discovery_mode(peer_id: &PeerId, mode: DiscoveryMode) -> Result<(), String> {
    settings::save_setting(
        peer_id,
        "discovery_mode",
        match mode {
            DiscoveryMode::AdvertiseAndDiscover => "advertise",
            DiscoveryMode::DiscoverOnly => "stealth",
            DiscoveryMode::Off => "off",
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_mode_parse() {
        assert_eq!(
            DiscoveryMode::parse("advertise"),
            Some(DiscoveryMode::AdvertiseAndDiscover)
        );
        assert_eq!(
            DiscoveryMode::parse("STEALTH"),
            Some(DiscoveryMode::DiscoverOnly)
        );
        assert_eq!(DiscoveryMode::parse("off"), Some(DiscoveryMode::Off));
        assert_eq!(DiscoveryMode::parse("bogus"), None);
        assert_eq!(
            DiscoveryMode::default(),
            DiscoveryMode::AdvertiseAndDiscover
        );
    }
}
