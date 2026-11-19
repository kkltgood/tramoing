//! Kernel-Network 事件的纯解析逻辑。只计经过非回环连接的收发字节。
//!
//! Version 0 的共同前缀为 PID、size、目的地址、源地址、目的端口、源端口。
//! IPv4 地址占 4 字节，IPv6 占 16 字节；地址和端口按网络字节序存放。
//! 本地代理的回环转发不计费，其真正的网络流量仍归属代理进程。

use std::net::{Ipv4Addr, Ipv6Addr};

use crate::agg::Dir;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkEvent {
    pub pid: u32,
    pub dir: Dir,
    pub bytes: u64,
}

/// 只接受 manifest 中已知布局的 Send/Receive。未知事件、截断数据和回环都跳过。
pub fn decode_event(id: u16, version: u8, data: &[u8]) -> Option<NetworkEvent> {
    if version != 0 {
        return None;
    }
    let (dir, ipv6) = match id {
        10 | 42 => (Dir::Tx, false),
        11 | 43 => (Dir::Rx, false),
        26 | 58 => (Dir::Tx, true),
        27 | 59 => (Dir::Rx, true),
        _ => return None,
    };
    let loopback = if ipv6 {
        // 8 字节 PID/size + 两个 16 字节地址 + 两个端口。
        let prefix = data.get(..44)?;
        let dest = Ipv6Addr::from(<[u8; 16]>::try_from(&prefix[8..24]).ok()?);
        let source = Ipv6Addr::from(<[u8; 16]>::try_from(&prefix[24..40]).ok()?);
        ipv6_loopback(dest) || ipv6_loopback(source)
    } else {
        let prefix = data.get(..20)?;
        let dest = Ipv4Addr::new(prefix[8], prefix[9], prefix[10], prefix[11]);
        let source = Ipv4Addr::new(prefix[12], prefix[13], prefix[14], prefix[15]);
        dest.is_loopback() || source.is_loopback()
    };
    if loopback {
        return None;
    }
    Some(NetworkEvent {
        pid: u32::from_le_bytes(data[..4].try_into().ok()?),
        dir,
        bytes: u32::from_le_bytes(data[4..8].try_into().ok()?) as u64,
    })
}

fn ipv6_loopback(ip: Ipv6Addr) -> bool {
    ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agg::Aggregator;
    use std::net::IpAddr;

    fn payload(pid: u32, bytes: u32, source: &str, dest: &str) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&pid.to_le_bytes());
        out.extend_from_slice(&bytes.to_le_bytes());
        for ip in [dest, source] {
            match ip.parse::<IpAddr>().unwrap() {
                IpAddr::V4(v4) => out.extend_from_slice(&v4.octets()),
                IpAddr::V6(v6) => out.extend_from_slice(&v6.octets()),
            }
        }
        out.extend_from_slice(&443u16.to_be_bytes());
        out.extend_from_slice(&50000u16.to_be_bytes());
        // Send 事件还有 start/end time、seqnum、connid；解析只使用共同前缀。
        out.extend_from_slice(&[0; 16]);
        out
    }

    #[test]
    fn keeps_tcp_udp_and_private_network_traffic_in_both_families() {
        for (send, source, dest) in [
            (10, "192.168.1.2", "203.0.113.127"),
            (42, "10.0.0.2", "10.0.0.3"),
            (26, "fd00::2", "2001:db8::127"),
            (58, "fe80::2", "fe80::3"),
        ] {
            let data = payload(1234, 8192, source, dest);
            for (id, dir) in [(send, Dir::Tx), (send + 1, Dir::Rx)] {
                assert_eq!(
                    decode_event(id, 0, &data),
                    Some(NetworkEvent {
                        pid: 1234,
                        dir,
                        bytes: 8192
                    })
                );
            }
        }
    }

    #[test]
    fn filters_entire_ipv4_loopback_range_and_ipv6_mapped_loopback() {
        for (ids, loopback, external) in [
            ([10, 11, 42, 43], "127.0.0.1", "192.168.1.2"),
            ([10, 11, 42, 43], "127.45.6.7", "192.168.1.2"),
            ([26, 27, 58, 59], "::1", "2001:db8::2"),
            ([26, 27, 58, 59], "::ffff:127.0.0.1", "::ffff:192.168.1.2"),
        ] {
            for id in ids {
                assert_eq!(
                    decode_event(id, 0, &payload(1, 1024, loopback, external)),
                    None
                );
                assert_eq!(
                    decode_event(id, 0, &payload(1, 1024, external, loopback)),
                    None
                );
            }
        }
    }

    #[test]
    fn proxy_forwarding_counts_network_bytes_once() {
        for (send, local, remote, loopback) in [
            (10, "192.168.1.2", "203.0.113.1", "127.0.0.1"),
            (42, "192.168.1.2", "203.0.113.1", "127.0.0.1"),
            (26, "fd00::2", "2001:db8::1", "::1"),
            (58, "fd00::2", "2001:db8::1", "::1"),
        ] {
            let mut agg = Aggregator::new();
            let down = 8 * 1024 * 1024;
            let up = 1024;
            for (id, pid, bytes, source, dest) in [
                (send + 1, 1, down, remote, local),  // 代理从网络下载
                (send, 1, down, loopback, loopback), // 代理转给浏览器
                (send + 1, 2, down, loopback, loopback),
                (send, 2, up, loopback, loopback), // 浏览器向代理上传
                (send + 1, 1, up, loopback, loopback),
                (send, 1, up, local, remote), // 代理上传到网络
            ] {
                if let Some(event) = decode_event(id, 0, &payload(pid, bytes, source, dest)) {
                    agg.add(event.pid, event.dir, event.bytes);
                }
            }
            assert_eq!(agg.total(), (down as u64, up as u64));
            assert_eq!(agg.len(), 1, "回环客户端不增加总流量");
        }
    }

    #[test]
    fn ignores_unknown_versions_non_traffic_events_and_truncated_addresses() {
        let v4 = payload(1, 100, "192.168.1.2", "203.0.113.1");
        let v6 = payload(1, 100, "fd00::2", "2001:db8::1");
        assert!(decode_event(10, 1, &v4).is_none());
        for id in [1, 12, 13, 14, 15, 16, 17, 28, 29, 30, 31, 32, 33, 999] {
            assert!(decode_event(id, 0, &v4).is_none());
        }
        for len in 0..20 {
            assert!(decode_event(11, 0, &v4[..len]).is_none());
        }
        for len in 0..44 {
            assert!(decode_event(27, 0, &v6[..len]).is_none());
        }
    }
}
