//! 当前设备的局域网地址：后台读取系统网卡，每 5 秒刷新，不查询公网服务。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, UdpSocket};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
struct Reading {
    address: Option<IpAddr>,
    at: Instant,
}

pub struct Monitor {
    latest: Arc<Mutex<Option<Reading>>>,
    stop: mpsc::Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl Monitor {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self::start(REFRESH_INTERVAL, sample, wake)
    }

    fn start(
        interval: Duration,
        mut sample: impl FnMut() -> Option<IpAddr> + Send + 'static,
        wake: impl Fn() + Send + 'static,
    ) -> Self {
        let latest: Arc<Mutex<Option<Reading>>> = Arc::new(Mutex::new(None));
        let output = latest.clone();
        let (stop, stopped) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("device-ip".into())
            .spawn(move || loop {
                // 系统查询时不持锁，界面读取永远不等待网卡枚举。
                let address = sample();
                let changed = {
                    let mut latest = output.lock().unwrap();
                    let changed = latest.is_none_or(|previous| previous.address != address);
                    *latest = Some(Reading {
                        address,
                        at: Instant::now(),
                    });
                    changed
                };
                if changed {
                    wake();
                }
                match stopped.recv_timeout(interval) {
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            });
        let worker = match worker {
            Ok(worker) => Some(worker),
            Err(error) => {
                log::warn!(target: "tramoing::device_ip", "无法读取本机 IP：{error}");
                None
            }
        };
        Self {
            latest,
            stop,
            worker,
        }
    }

    pub fn address(&self) -> Option<IpAddr> {
        if self
            .worker
            .as_ref()
            .is_none_or(|worker| worker.is_finished())
        {
            return None;
        }
        self.latest
            .lock()
            .ok()?
            .filter(|reading| reading.at.elapsed() <= REFRESH_INTERVAL * 3)?
            .address
    }

    pub fn is_loading(&self) -> bool {
        self.worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
            && self.latest.lock().is_ok_and(|latest| latest.is_none())
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Clone, Copy)]
struct Candidate {
    address: IpAddr,
    metric: u32,
}

fn usable(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            !ip.is_unspecified()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_multicast()
                && !ip.is_broadcast()
        }
        IpAddr::V6(ip) => {
            !ip.is_unspecified()
                && !ip.is_loopback()
                && !ip.is_unicast_link_local()
                && !ip.is_multicast()
                && ip.to_ipv4_mapped().is_none()
        }
    }
}

fn choose(candidates: impl IntoIterator<Item = Candidate>, routes: &[IpAddr]) -> Option<IpAddr> {
    candidates
        .into_iter()
        .filter(|candidate| usable(candidate.address))
        .min_by_key(|candidate| {
            (
                candidate.address.is_ipv6(),
                !routes.contains(&candidate.address),
                candidate.metric,
                candidate.address,
            )
        })
        .map(|candidate| candidate.address)
}

fn sample() -> Option<IpAddr> {
    // UDP connect 仅选择本地路由，不握手，也不发送数据。目的地址是文档保留地址。
    // 只用于多张实体网卡的排序；即使默认路由走 VPN，也不把隧道地址当本机 LAN IP。
    let routes: Vec<_> = [("0.0.0.0:0", "192.0.2.1:9"), ("[::]:0", "[2001:db8::1]:9")]
        .into_iter()
        .filter_map(|(bind, destination)| {
            let socket = UdpSocket::bind(bind).ok()?;
            socket.connect(destination).ok()?;
            socket.local_addr().ok().map(|address| address.ip())
        })
        .collect();
    choose(platform::candidates(), &routes)
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::ffi::CStr;
    use std::os::fd::AsRawFd;

    // macOS 的 Wi-Fi / 以太网 / USB 网卡均为 enN；保留用户配置的 bond/vlan。
    // bridge/awdl/llw/utun 常用于共享网络、AirDrop 或 VPN，不能代表上联网卡。
    fn lan_interface(name: &str) -> bool {
        ["en", "bond", "vlan"].iter().any(|prefix| {
            name.strip_prefix(prefix).is_some_and(|suffix| {
                !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
            })
        })
    }

    // net/if.h 的 ifmediareq 使用 #pragma pack(4)，libc 尚未暴露这个结构体。
    #[repr(C, packed(4))]
    struct IfMediaReq {
        name: [u8; libc::IFNAMSIZ],
        current: i32,
        mask: i32,
        status: i32,
        active: i32,
        count: i32,
        list: *mut i32,
    }

    fn connected(socket: &UdpSocket, name: &[u8]) -> bool {
        if name.len() >= libc::IFNAMSIZ {
            return false;
        }
        let mut request = IfMediaReq {
            name: [0; libc::IFNAMSIZ],
            current: 0,
            mask: 0,
            status: 0,
            active: 0,
            count: 0,
            list: std::ptr::null_mut(),
        };
        request.name[..name.len()].copy_from_slice(name);
        // sys/sockio.h: _IOWR('i', 56, struct ifmediareq).
        let command = 0xc000_0000
            | (std::mem::size_of::<IfMediaReq>() as u64) << 16
            | (u64::from(b'i') << 8)
            | 56;
        // SAFETY: fd 是存活的 socket；请求布局和大小与本机 SDK 一致。
        let result = unsafe { libc::ioctl(socket.as_raw_fd(), command, &mut request) };
        // bond/vlan 驱动可能不支持 media 查询，此时仍依靠 getifaddrs 的 UP 标志。
        result != 0 || request.status & 1 == 0 || request.status & 2 != 0
    }

    pub(super) fn candidates() -> Vec<Candidate> {
        struct Interfaces(*mut libc::ifaddrs);
        impl Drop for Interfaces {
            fn drop(&mut self) {
                // SAFETY: 该链表由 getifaddrs 分配，直到这里都保持存活。
                unsafe { libc::freeifaddrs(self.0) };
            }
        }
        let Ok(socket) = UdpSocket::bind("0.0.0.0:0") else {
            return Vec::new();
        };
        let mut head = std::ptr::null_mut();
        // SAFETY: 输出指针有效，成功后的链表由 Interfaces 释放。
        if unsafe { libc::getifaddrs(&mut head) } != 0 {
            return Vec::new();
        }
        let interfaces = Interfaces(head);
        let mut cursor = interfaces.0;
        let mut addresses = Vec::new();
        while !cursor.is_null() {
            // SAFETY: 节点和 sockaddr 均属于仍存活的 getifaddrs 链表；先检查 family。
            unsafe {
                let interface = &*cursor;
                cursor = interface.ifa_next;
                if interface.ifa_name.is_null()
                    || interface.ifa_addr.is_null()
                    || interface.ifa_flags & libc::IFF_UP as u32 == 0
                    || interface.ifa_flags & (libc::IFF_LOOPBACK | libc::IFF_POINTOPOINT) as u32
                        != 0
                {
                    continue;
                }
                let name = CStr::from_ptr(interface.ifa_name);
                if !lan_interface(name.to_str().unwrap_or("")) {
                    continue;
                }
                let address = match i32::from((*interface.ifa_addr).sa_family) {
                    libc::AF_INET => {
                        let address = &*interface.ifa_addr.cast::<libc::sockaddr_in>();
                        IpAddr::V4(Ipv4Addr::from(address.sin_addr.s_addr.to_ne_bytes()))
                    }
                    libc::AF_INET6 => {
                        let address = &*interface.ifa_addr.cast::<libc::sockaddr_in6>();
                        IpAddr::V6(Ipv6Addr::from(address.sin6_addr.s6_addr))
                    }
                    _ => continue,
                };
                if usable(address) && connected(&socket, name.to_bytes()) {
                    addresses.push(Candidate {
                        address,
                        metric: libc::if_nametoindex(interface.ifa_name),
                    });
                }
            }
        }
        addresses
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn excludes_tunnels_airdrop_and_internet_sharing() {
            for name in [
                "utun0",
                "lo0",
                "awdl0",
                "llw0",
                "bridge100",
                "en",
                "en0junk",
            ] {
                assert!(!lan_interface(name), "{name}");
            }
            for name in ["en0", "en12", "bond0", "vlan1"] {
                assert!(lan_interface(name));
            }
            assert_eq!(std::mem::size_of::<IfMediaReq>(), 44);
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use windows_sys::Win32::{
        Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR},
        NetworkManagement::{
            IpHelper::{
                GetAdaptersAddresses, GetIfEntry2, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
                GAA_FLAG_SKIP_MULTICAST, IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211,
                IP_ADAPTER_ADDRESSES_LH, MIB_IF_ROW2,
            },
            Ndis::IfOperStatusUp,
        },
        Networking::WinSock::{
            IpDadStatePreferred, AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6,
        },
    };

    pub(super) fn candidates() -> Vec<Candidate> {
        // u64 保证结构体及内嵌链表所需的 8 字节对齐；15 KiB 是系统推荐初始大小。
        let mut size = 15 * 1024_u32;
        for _ in 0..3 {
            let mut buffer = vec![0_u64; (size as usize).div_ceil(8)];
            let head = buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
            // SAFETY: size 不超过实际分配容量；系统只在成功后返回有效链表。
            let status = unsafe {
                GetAdaptersAddresses(
                    u32::from(AF_UNSPEC),
                    GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER,
                    std::ptr::null(),
                    head,
                    &mut size,
                )
            };
            if status == ERROR_BUFFER_OVERFLOW {
                continue;
            }
            if status != NO_ERROR {
                return Vec::new();
            }
            let mut addresses = Vec::new();
            let mut cursor = head;
            while !cursor.is_null() {
                // SAFETY: 所有节点和地址都位于仍存活的 buffer 中，读取前检查 family/长度。
                unsafe {
                    let adapter = &*cursor;
                    cursor = adapter.Next;
                    if adapter.OperStatus != IfOperStatusUp
                        || !matches!(adapter.IfType, IF_TYPE_ETHERNET_CSMACD | IF_TYPE_IEEE80211)
                    {
                        continue;
                    }
                    let mut row: MIB_IF_ROW2 = std::mem::zeroed();
                    row.InterfaceLuid = adapter.Luid;
                    // HardwareInterface 是 InterfaceAndOperStatusFlags 的第一位。
                    if GetIfEntry2(&mut row) != NO_ERROR
                        || row.InterfaceAndOperStatusFlags._bitfield & 1 == 0
                    {
                        continue;
                    }
                    let mut unicast = adapter.FirstUnicastAddress;
                    while !unicast.is_null() {
                        let item = &*unicast;
                        unicast = item.Next;
                        let socket = item.Address.lpSockaddr;
                        if socket.is_null()
                            || item.DadState != IpDadStatePreferred
                            || item.Address.iSockaddrLength < std::mem::size_of::<u16>() as i32
                        {
                            continue;
                        }
                        let (address, metric) = match (*socket).sa_family {
                            AF_INET
                                if item.Address.iSockaddrLength
                                    >= std::mem::size_of::<SOCKADDR_IN>() as i32 =>
                            {
                                let socket = &*socket.cast::<SOCKADDR_IN>();
                                (
                                    IpAddr::V4(Ipv4Addr::from(
                                        socket.sin_addr.S_un.S_addr.to_ne_bytes(),
                                    )),
                                    adapter.Ipv4Metric,
                                )
                            }
                            AF_INET6
                                if item.Address.iSockaddrLength
                                    >= std::mem::size_of::<SOCKADDR_IN6>() as i32 =>
                            {
                                let socket = &*socket.cast::<SOCKADDR_IN6>();
                                (
                                    IpAddr::V6(Ipv6Addr::from(socket.sin6_addr.u.Byte)),
                                    adapter.Ipv6Metric,
                                )
                            }
                            _ => continue,
                        };
                        addresses.push(Candidate { address, metric });
                    }
                }
            }
            return addresses;
        }
        Vec::new()
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::Candidate;
    pub(super) fn candidates() -> Vec<Candidate> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }
    fn candidate(value: &str, metric: u32) -> Candidate {
        Candidate {
            address: ip(value),
            metric,
        }
    }

    #[test]
    fn selects_active_route_then_metric_and_prefers_ipv4() {
        let candidates = [
            candidate("192.168.1.4", 1),
            candidate("10.0.0.8", 20),
            candidate("2001:db8::8", 0),
        ];
        assert_eq!(choose(candidates, &[ip("10.0.0.8")]), Some(ip("10.0.0.8")));
        // VPN 路由的地址不在实体网卡中时，仍显示实体网卡地址。
        assert_eq!(
            choose(candidates, &[ip("198.18.0.1")]),
            Some(ip("192.168.1.4"))
        );
        assert_eq!(
            choose(candidates, &[ip("2001:db8::8")]),
            Some(ip("192.168.1.4"))
        );
    }

    #[test]
    fn rejects_unusable_addresses_and_supports_ipv6_only_networks() {
        let unusable = [
            "0.0.0.0",
            "127.0.0.1",
            "169.254.3.4",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fe80::123",
            "ff02::1",
            "::ffff:127.0.0.1",
        ];
        assert_eq!(choose(unusable.map(|value| candidate(value, 0)), &[]), None);
        assert_eq!(
            choose([candidate("fe80::123", 0), candidate("fd12::10", 1)], &[]),
            Some(ip("fd12::10"))
        );
        assert_eq!(choose([], &[]), None);
    }

    #[test]
    fn monitor_clears_disconnected_address_and_stops_without_waiting_for_interval() {
        let (values, samples) = mpsc::channel();
        let (changed, changes) = mpsc::channel();
        let monitor = Monitor::start(
            Duration::from_millis(10),
            move || samples.recv_timeout(Duration::from_secs(1)).ok().flatten(),
            move || {
                let _ = changed.send(());
            },
        );
        assert!(monitor.is_loading());
        values.send(Some(ip("192.168.1.4"))).unwrap();
        changes.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(monitor.address(), Some(ip("192.168.1.4")));
        values.send(None).unwrap();
        changes.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(monitor.address(), None);
        assert!(!monitor.is_loading());
        let before = Instant::now();
        drop(values);
        drop(monitor);
        assert!(before.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn stale_readings_are_not_displayed() {
        let (changed, changes) = mpsc::channel();
        let monitor = Monitor::start(
            Duration::from_secs(60),
            || Some(ip("10.0.0.2")),
            move || {
                let _ = changed.send(());
            },
        );
        changes.recv_timeout(Duration::from_secs(2)).unwrap();
        monitor.latest.lock().unwrap().as_mut().unwrap().at =
            Instant::now() - Duration::from_secs(30);
        assert_eq!(monitor.address(), None);
        let before = Instant::now();
        drop(monitor);
        assert!(before.elapsed() < Duration::from_secs(1));
    }

    #[test]
    #[ignore = "reads this machine's live network adapters"]
    fn reads_live_device_address() {
        let address = sample();
        println!("Current device LAN IP: {address:?}");
        assert!(address.is_some(), "requires a connected LAN interface");
    }
}
