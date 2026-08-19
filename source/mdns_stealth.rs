//! 隐身模式下的最小 mDNS 监听器：只收不发，不向局域网广播本机在线状态。
//!
//! libp2p-mdns 节点会周期性发送组播 PTR 查询，并对自己收到的组播查询回以
//! **组播**响应（DNS TXT 记录携带 `dnsaddr=<multiaddr>`，见 libp2p-mdns 的
//! `MdnsPeer::new`）。因此监听 224.0.0.251:5353 并解析其中的 `dnsaddr=` TXT
//! 记录，即可还原 (PeerId, Multiaddr)，无需本机广播任何身份/地址。
//!
//! 仅实现 IPv4；IPv6 组播在后续版本按需补齐。

use std::net::{Ipv4Addr, SocketAddr};

use libp2p::{
    multiaddr::Protocol,
    Multiaddr, PeerId,
};
use socket2::{Domain, Protocol as SockProtocol, Socket, Type};

const MDNS_PORT: u16 = 5353;
const MDNS_MULTICAST: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const DNS_TXT: u16 = 16;

/// 只收不发的 mDNS 组播监听器
pub struct StealthMdns {
    socket: tokio::net::UdpSocket,
}

impl StealthMdns {
    /// 绑定 5353 并加入组播组。SO_REUSEADDR 保证与其它 mDNS 实现共存。
    pub fn new() -> std::io::Result<StealthMdns> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(SockProtocol::UDP))?;
        socket.set_reuse_address(true)?;
        #[cfg(unix)]
        socket.set_reuse_port(true)?;
        socket.bind(&SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), MDNS_PORT).into())?;
        socket.set_multicast_loop_v4(true)?;
        socket.join_multicast_v4(&MDNS_MULTICAST, &Ipv4Addr::UNSPECIFIED)?;
        socket.set_nonblocking(true)?;
        let udp = tokio::net::UdpSocket::from_std(socket.into())?;
        Ok(StealthMdns { socket: udp })
    }

    /// 等待并解析下一条发现的节点；None 表示 socket 出错（调用方应终止任务）
    pub async fn next_discovery(&mut self) -> Option<(PeerId, Multiaddr)> {
        let mut buf = [0u8; 65535];
        loop {
            let (len, _from) = match self.socket.recv_from(&mut buf).await {
                Ok(v) => v,
                Err(_) => return None,
            };
            for (peer_id, addr) in parse_mdns_packet(&buf[..len]) {
                return Some((peer_id, addr));
            }
        }
    }
}

/// 解析 mDNS 报文中的所有 `dnsaddr=` TXT 记录 → (PeerId, Multiaddr)
fn parse_mdns_packet(buf: &[u8]) -> Vec<(PeerId, Multiaddr)> {
    if buf.len() < 12 {
        return Vec::new();
    }
    let qdcount = u16::from_be_bytes([buf[4], buf[5]]) as usize;
    let ancount = u16::from_be_bytes([buf[6], buf[7]]) as usize;
    let nscount = u16::from_be_bytes([buf[8], buf[9]]) as usize;
    let arcount = u16::from_be_bytes([buf[10], buf[11]]) as usize;

    let mut pos = 12;
    for _ in 0..qdcount {
        pos = match skip_name(buf, pos) {
            Some(p) => p,
            None => return Vec::new(),
        };
        pos += 4; // qtype + qclass
        if pos > buf.len() {
            return Vec::new();
        }
    }

    let mut out = Vec::new();
    for _ in 0..(ancount + nscount + arcount) {
        let name_end = match skip_name(buf, pos) {
            Some(p) => p,
            None => return out,
        };
        if name_end + 10 > buf.len() {
            return out;
        }
        let rtype = u16::from_be_bytes([buf[name_end], buf[name_end + 1]]);
        let rdlength =
            u16::from_be_bytes([buf[name_end + 8], buf[name_end + 9]]) as usize;
        let rdata_start = name_end + 10;
        let rdata_end = rdata_start + rdlength;
        if rdata_end > buf.len() {
            return out;
        }
        if rtype == DNS_TXT {
            for s in txt_strings(&buf[rdata_start..rdata_end]) {
                if let Some(rest) = s.strip_prefix(b"dnsaddr=") {
                    let text = String::from_utf8_lossy(rest);
                    if let Ok(ma) = text.parse::<Multiaddr>() {
                        if let Some((peer_id, addr)) = extract_peer(ma) {
                            out.push((peer_id, addr));
                        }
                    }
                }
            }
        }
        pos = rdata_end;
    }
    out
}

/// 定位 DNS 名字字段的结束位置（处理压缩指针：字段到首个指针即止）
fn skip_name(buf: &[u8], start: usize) -> Option<usize> {
    let mut pos = start;
    loop {
        if pos >= buf.len() {
            return None;
        }
        let len = buf[pos];
        if len & 0xC0 == 0xC0 {
            if pos + 2 > buf.len() {
                return None;
            }
            return Some(pos + 2);
        } else if len == 0 {
            return Some(pos + 1);
        } else {
            let l = len as usize;
            if pos + 1 + l > buf.len() {
                return None;
            }
            pos += 1 + l;
        }
    }
}

/// 拆分 TXT RDATA 中的字符串序列
fn txt_strings(rdata: &[u8]) -> Vec<&[u8]> {
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < rdata.len() {
        let len = rdata[pos] as usize;
        pos += 1;
        if pos + len > rdata.len() {
            break;
        }
        out.push(&rdata[pos..pos + len]);
        pos += len;
    }
    out
}

/// 提取 multiaddr 中的 /p2p/ PeerId，返回 (PeerId, 完整地址)。
/// 地址保留 /p2p/ 段：与 libp2p-mdns 的发现结果一致，拨号时带目标身份。
fn extract_peer(ma: Multiaddr) -> Option<(PeerId, Multiaddr)> {
    let mut peer_id = None;
    for p in ma.iter() {
        if let Protocol::P2p(pid) = p {
            peer_id = Some(pid);
        }
    }
    let peer_id = peer_id?;
    Some((peer_id, ma))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEER_ID: &str = "12D3KooWP7CwQswqLKZbwvYd9wrEynnL9F2aKVP1X9huNASBTuqj";

    /// 手工构造一个含 dnsaddr TXT 记录的最小 DNS 响应报文
    fn build_packet(peer_id: &str, addr: &str) -> Vec<u8> {
        let mut p: Vec<u8> = Vec::new();
        // header: id=0, flags=0x8400(响应), qd=0, an=0, ns=0, ar=1
        p.extend_from_slice(&[0, 0, 0x84, 0x00, 0, 0, 0, 0, 0, 0, 0, 1]);
        // 附加区 RR: 名字 = "example.local"（标签无压缩）
        p.extend_from_slice(&[7]);
        p.extend_from_slice(b"example");
        p.extend_from_slice(&[5]);
        p.extend_from_slice(b"local");
        p.push(0);
        // type=TXT(16), class=IN(1)
        p.extend_from_slice(&[0, 16, 0, 1]);
        // ttl=30s
        p.extend_from_slice(&[0, 0, 0, 30]);
        // rdata
        let txt = format!("dnsaddr={addr}");
        let rdata: Vec<u8> = vec![txt.len() as u8]
            .into_iter()
            .chain(txt.as_bytes().iter().copied())
            .collect();
        p.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        p.extend_from_slice(&rdata);
        let _ = peer_id; // peer_id 已编码在 addr 的 /p2p/ 段中
        p
    }

    #[test]
    fn parse_dnsaddr_txt_record() {
        let peer: PeerId = PEER_ID.parse().unwrap();
        let addr = format!("/ip4/192.168.31.10/tcp/12082/p2p/{PEER_ID}");
        let pkt = build_packet(PEER_ID, &addr);
        let found = parse_mdns_packet(&pkt);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, peer);
        assert_eq!(found[0].1.to_string(), addr);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_mdns_packet(&[]).is_empty());
        assert!(parse_mdns_packet(&[0u8; 12]).is_empty());
        let pkt = build_packet(PEER_ID, "not-a-multiaddr");
        assert!(parse_mdns_packet(&pkt).is_empty());
    }
}
