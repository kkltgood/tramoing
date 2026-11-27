//! On-demand owner-PID socket snapshots. No ETW/helper connection is needed.
//!
//! IP Helper exposes UDP bindings, not UDP peers. TCP listeners likewise have
//! no meaningful remote endpoint. Never infer a peer from a wildcard address.
//! API tables are not atomic with process lookup: verify creation time and path
//! through a fresh process handle, and reject identities born after the query.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::mem::{offset_of, size_of};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::ptr::null_mut;

use tm_core::{
    AppKey, Connection, ConnectionEndpoint, ConnectionProtocol, ConnectionReport, ConnectionState,
};
use tm_sampler::SamplerError;
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, FILETIME};
use windows_sys::Win32::NetworkManagement::IpHelper::*;
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;

const MAX_TABLE_BYTES: usize = 4 * 1024 * 1024;
const MAX_TABLE_ATTEMPTS: usize = 4;

#[derive(Clone, Copy)]
enum TableKind {
    Tcp4,
    Tcp6,
    Udp4,
    Udp6,
}

impl TableKind {
    fn label(self) -> &'static str {
        match self {
            Self::Tcp4 => "TCP/IPv4",
            Self::Tcp6 => "TCP/IPv6",
            Self::Udp4 => "UDP/IPv4",
            Self::Udp6 => "UDP/IPv6",
        }
    }

    fn load(self) -> Result<Vec<Connection>, String> {
        let bytes = read_table(|buffer, length| unsafe {
            match self {
                Self::Tcp4 | Self::Tcp6 => GetExtendedTcpTable(
                    buffer,
                    length,
                    0,
                    if matches!(self, Self::Tcp4) {
                        AF_INET
                    } else {
                        AF_INET6
                    } as u32,
                    TCP_TABLE_OWNER_PID_ALL,
                    0,
                ),
                Self::Udp4 | Self::Udp6 => GetExtendedUdpTable(
                    buffer,
                    length,
                    0,
                    if matches!(self, Self::Udp4) {
                        AF_INET
                    } else {
                        AF_INET6
                    } as u32,
                    UDP_TABLE_OWNER_PID,
                    0,
                ),
            }
        })?;
        // SAFETY: each API/class pair returns exactly the corresponding table;
        // these OWNER_PID row types contain only integers and byte arrays.
        unsafe {
            match self {
                Self::Tcp4 => {
                    rows::<MIB_TCPROW_OWNER_PID>(&bytes, offset_of!(MIB_TCPTABLE_OWNER_PID, table))
                        .map(|rows| rows.into_iter().map(tcp4).collect())
                }
                Self::Tcp6 => rows::<MIB_TCP6ROW_OWNER_PID>(
                    &bytes,
                    offset_of!(MIB_TCP6TABLE_OWNER_PID, table),
                )
                .map(|rows| rows.into_iter().map(tcp6).collect()),
                Self::Udp4 => {
                    rows::<MIB_UDPROW_OWNER_PID>(&bytes, offset_of!(MIB_UDPTABLE_OWNER_PID, table))
                        .map(|rows| rows.into_iter().map(udp4).collect())
                }
                Self::Udp6 => rows::<MIB_UDP6ROW_OWNER_PID>(
                    &bytes,
                    offset_of!(MIB_UDP6TABLE_OWNER_PID, table),
                )
                .map(|rows| rows.into_iter().map(udp6).collect()),
            }
        }
    }
}

/// Run only on a background thread. Paths are queried fresh on every request;
/// exited or protected processes are omitted with an explicit partial warning.
pub fn for_app(app: &AppKey) -> Result<ConnectionReport, SamplerError> {
    let path = app.as_str();
    if !Path::new(path).is_absolute()
        || path.encode_utf16().count() > 32768
        || path.chars().any(char::is_control)
    {
        return Err(SamplerError::Source(
            "Connection details require the application's full executable path".into(),
        ));
    }

    let mut at = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    unsafe { GetSystemTimeAsFileTime(&mut at) };
    let started = ((at.dwHighDateTime as u64) << 32) | at.dwLowDateTime as u64;
    let mut candidates = Vec::new();
    let mut failures = Vec::new();
    let mut completed = 0;
    for kind in [
        TableKind::Tcp4,
        TableKind::Tcp6,
        TableKind::Udp4,
        TableKind::Udp6,
    ] {
        match kind.load() {
            Ok(rows) => {
                completed += 1;
                candidates.extend(rows);
            }
            Err(error) => failures.push(format!("{}: {error}", kind.label())),
        }
    }
    if completed == 0 {
        return Err(SamplerError::Source(failures.join("; ")));
    }
    let (connections, skipped) =
        select_app(candidates, path, started, crate::proc::executable_identity);
    if skipped > 0 {
        failures.push(format!(
            "Could not verify {skipped} protected, exited or newly started process identities; this snapshot may be incomplete"
        ));
    }
    Ok(ConnectionReport {
        connections,
        warning: (!failures.is_empty()).then(|| failures.join("; ")),
    })
}

fn select_app(
    candidates: Vec<Connection>,
    path: &str,
    started: u64,
    mut identity: impl FnMut(u32) -> Option<(u64, String)>,
) -> (Vec<Connection>, usize) {
    let pids: HashSet<_> = candidates
        .iter()
        .map(|row| row.pid)
        .filter(|pid| *pid != 0)
        .collect();
    let mut matches = HashMap::new();
    let mut skipped = 0;
    for pid in pids {
        match identity(pid) {
            Some((created, actual)) if created <= started => {
                matches.insert(pid, crate::network_policy::same_path(&actual, path));
            }
            _ => skipped += 1,
        }
    }
    let mut connections: Vec<_> = candidates
        .into_iter()
        .filter(|row| matches.get(&row.pid) == Some(&true))
        .collect();
    connections.sort();
    (connections, skipped)
}

/// The kernel writes into u64-aligned storage. Connection churn may grow the
/// table between calls, so retry a bounded number of times with a hard cap.
fn read_table(mut query: impl FnMut(*mut c_void, &mut u32) -> u32) -> Result<Vec<u8>, String> {
    let mut required = 0;
    let result = query(null_mut(), &mut required);
    if result != 0 && result != ERROR_INSUFFICIENT_BUFFER {
        return Err(format!("Windows error {result}"));
    }
    if result == 0 && required == 0 {
        return Ok(0u32.to_ne_bytes().to_vec());
    }
    for _ in 0..MAX_TABLE_ATTEMPTS {
        if !(size_of::<u32>()..=MAX_TABLE_BYTES).contains(&(required as usize)) {
            return Err(format!(
                "Invalid or oversized connection table ({required} bytes)"
            ));
        }
        let mut storage = vec![0u64; (required as usize).div_ceil(size_of::<u64>())];
        let capacity = storage.len() * size_of::<u64>();
        let mut written = capacity as u32;
        let result = query(storage.as_mut_ptr().cast(), &mut written);
        if result == ERROR_INSUFFICIENT_BUFFER {
            required = written;
            continue;
        }
        if result != 0 {
            return Err(format!("Windows error {result}"));
        }
        if written as usize > capacity || written < size_of::<u32>() as u32 {
            return Err("Invalid connection table length".into());
        }
        // SAFETY: zero-initialized storage is readable and the returned length
        // was checked against its allocated capacity before forming the slice.
        return Ok(unsafe {
            std::slice::from_raw_parts(storage.as_ptr().cast::<u8>(), written as usize).to_vec()
        });
    }
    Err("Connection table kept changing; retry the snapshot".into())
}

/// # Safety
/// T must be a plain OWNER_PID row whose every bit pattern is valid. All bounds
/// are checked before read_unaligned; table offsets come from the SDK structs.
unsafe fn rows<T: Copy>(bytes: &[u8], offset: usize) -> Result<Vec<T>, String> {
    let count_bytes: [u8; 4] = bytes
        .get(..4)
        .ok_or("Truncated connection table header")?
        .try_into()
        .unwrap();
    let count = u32::from_ne_bytes(count_bytes) as usize;
    let needed = count
        .checked_mul(size_of::<T>())
        .and_then(|n| n.checked_add(offset))
        .ok_or("Invalid connection table row count")?;
    if offset < 4 || needed > bytes.len() {
        return Err("Truncated connection table rows".into());
    }
    Ok((0..count)
        .map(|i| {
            // SAFETY: entire row lies in the checked table range; the output Vec
            // need not retain the alignment of the original API allocation.
            unsafe {
                bytes
                    .as_ptr()
                    .add(offset + i * size_of::<T>())
                    .cast::<T>()
                    .read_unaligned()
            }
        })
        .collect())
}

fn port(value: u32) -> u16 {
    u16::from_be(value as u16)
}

fn endpoint4(address: u32, value: u32) -> ConnectionEndpoint {
    ConnectionEndpoint {
        address: Ipv4Addr::from(address.to_ne_bytes()).to_string(),
        port: port(value),
    }
}

fn endpoint6(address: [u8; 16], scope: u32, value: u32) -> ConnectionEndpoint {
    let address = Ipv6Addr::from(address);
    ConnectionEndpoint {
        address: if scope == 0 {
            address.to_string()
        } else {
            format!("{address}%{scope}")
        },
        port: port(value),
    }
}

fn tcp_state(state: u32) -> ConnectionState {
    match state as i32 {
        MIB_TCP_STATE_CLOSED => ConnectionState::Closed,
        MIB_TCP_STATE_LISTEN => ConnectionState::Listen,
        MIB_TCP_STATE_SYN_SENT => ConnectionState::SynSent,
        MIB_TCP_STATE_SYN_RCVD => ConnectionState::SynReceived,
        MIB_TCP_STATE_ESTAB => ConnectionState::Established,
        MIB_TCP_STATE_FIN_WAIT1 => ConnectionState::FinWait1,
        MIB_TCP_STATE_FIN_WAIT2 => ConnectionState::FinWait2,
        MIB_TCP_STATE_CLOSE_WAIT => ConnectionState::CloseWait,
        MIB_TCP_STATE_CLOSING => ConnectionState::Closing,
        MIB_TCP_STATE_LAST_ACK => ConnectionState::LastAck,
        MIB_TCP_STATE_TIME_WAIT => ConnectionState::TimeWait,
        _ => ConnectionState::Unknown,
    }
}

fn tcp4(row: MIB_TCPROW_OWNER_PID) -> Connection {
    Connection {
        pid: row.dwOwningPid,
        protocol: ConnectionProtocol::Tcp4,
        local: endpoint4(row.dwLocalAddr, row.dwLocalPort),
        remote: (row.dwState != MIB_TCP_STATE_LISTEN as u32)
            .then(|| endpoint4(row.dwRemoteAddr, row.dwRemotePort)),
        state: tcp_state(row.dwState),
    }
}

fn tcp6(row: MIB_TCP6ROW_OWNER_PID) -> Connection {
    Connection {
        pid: row.dwOwningPid,
        protocol: ConnectionProtocol::Tcp6,
        local: endpoint6(row.ucLocalAddr, row.dwLocalScopeId, row.dwLocalPort),
        remote: (row.dwState != MIB_TCP_STATE_LISTEN as u32)
            .then(|| endpoint6(row.ucRemoteAddr, row.dwRemoteScopeId, row.dwRemotePort)),
        state: tcp_state(row.dwState),
    }
}

fn udp4(row: MIB_UDPROW_OWNER_PID) -> Connection {
    Connection {
        pid: row.dwOwningPid,
        protocol: ConnectionProtocol::Udp4,
        local: endpoint4(row.dwLocalAddr, row.dwLocalPort),
        remote: None,
        state: ConnectionState::Bound,
    }
}

fn udp6(row: MIB_UDP6ROW_OWNER_PID) -> Connection {
    Connection {
        pid: row.dwOwningPid,
        protocol: ConnectionProtocol::Udp6,
        local: endpoint6(row.ucLocalAddr, row.dwLocalScopeId, row.dwLocalPort),
        remote: None,
        state: ConnectionState::Bound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
    use std::time::{Duration, Instant};

    fn wire_port(port: u16) -> u32 {
        port.to_be() as u32
    }
    fn wire_ip(address: [u8; 4]) -> u32 {
        u32::from_ne_bytes(address)
    }

    fn tcp_row() -> MIB_TCPROW_OWNER_PID {
        MIB_TCPROW_OWNER_PID {
            dwState: MIB_TCP_STATE_ESTAB as u32,
            dwLocalAddr: wire_ip([192, 0, 2, 10]),
            dwLocalPort: wire_port(51234),
            dwRemoteAddr: wire_ip([198, 51, 100, 20]),
            dwRemotePort: wire_port(443),
            dwOwningPid: 123,
        }
    }

    #[test]
    fn tcp_ipv4_preserves_network_byte_order_and_real_states() {
        let connection = tcp4(tcp_row());
        assert_eq!(connection.pid, 123);
        assert_eq!(connection.protocol, ConnectionProtocol::Tcp4);
        assert_eq!(connection.local.address, "192.0.2.10");
        assert_eq!(connection.local.port, 51234);
        let remote = connection.remote.unwrap();
        assert_eq!(remote.address, "198.51.100.20");
        assert_eq!(remote.port, 443);
        assert_eq!(connection.state, ConnectionState::Established);
        for (raw, expected) in [
            (1, ConnectionState::Closed),
            (2, ConnectionState::Listen),
            (3, ConnectionState::SynSent),
            (4, ConnectionState::SynReceived),
            (5, ConnectionState::Established),
            (6, ConnectionState::FinWait1),
            (7, ConnectionState::FinWait2),
            (8, ConnectionState::CloseWait),
            (9, ConnectionState::Closing),
            (10, ConnectionState::LastAck),
            (11, ConnectionState::TimeWait),
            (12, ConnectionState::Unknown),
            (u32::MAX, ConnectionState::Unknown),
        ] {
            assert_eq!(tcp_state(raw), expected);
        }
    }

    #[test]
    fn ipv6_scopes_and_udp_bindings_are_not_fabricated_peers() {
        let local = "fe80::1234".parse::<Ipv6Addr>().unwrap().octets();
        let remote = "2001:db8::99".parse::<Ipv6Addr>().unwrap().octets();
        let row = MIB_TCP6ROW_OWNER_PID {
            ucLocalAddr: local,
            dwLocalScopeId: 17,
            dwLocalPort: wire_port(8080),
            ucRemoteAddr: remote,
            dwRemoteScopeId: 0,
            dwRemotePort: wire_port(9000),
            dwState: MIB_TCP_STATE_ESTAB as u32,
            dwOwningPid: 222,
        };
        let tcp = tcp6(row);
        assert_eq!(tcp.local.address, "fe80::1234%17");
        assert_eq!(tcp.local.port, 8080);
        assert_eq!(tcp.remote.unwrap().address, "2001:db8::99");
        assert_eq!(
            tcp6(MIB_TCP6ROW_OWNER_PID {
                dwState: MIB_TCP_STATE_LISTEN as u32,
                ..row
            })
            .remote,
            None
        );
        assert_eq!(
            tcp4(MIB_TCPROW_OWNER_PID {
                dwState: MIB_TCP_STATE_LISTEN as u32,
                ..tcp_row()
            })
            .remote,
            None
        );
        for udp in [
            udp4(MIB_UDPROW_OWNER_PID {
                dwLocalAddr: 0,
                dwLocalPort: wire_port(53),
                dwOwningPid: 1,
            }),
            udp6(MIB_UDP6ROW_OWNER_PID {
                ucLocalAddr: local,
                dwLocalScopeId: 17,
                dwLocalPort: wire_port(53),
                dwOwningPid: 1,
            }),
        ] {
            assert_eq!(udp.local.port, 53);
            assert_eq!(udp.remote, None);
            assert_eq!(udp.state, ConnectionState::Bound);
        }
    }

    #[test]
    fn table_read_retries_growth_with_aligned_storage_and_stays_bounded() {
        let mut calls = 0;
        let bytes = read_table(|ptr, size| {
            calls += 1;
            match calls {
                1 => {
                    assert!(ptr.is_null());
                    *size = 4;
                    ERROR_INSUFFICIENT_BUFFER
                }
                2 => {
                    assert_eq!(ptr as usize % 8, 0);
                    *size = 32;
                    ERROR_INSUFFICIENT_BUFFER
                }
                3 => {
                    assert_eq!(ptr as usize % 8, 0);
                    assert!(*size >= 32);
                    unsafe { ptr.cast::<u32>().write(0) };
                    *size = 4;
                    0
                }
                _ => panic!("too many calls"),
            }
        })
        .unwrap();
        assert_eq!(bytes, 0u32.to_ne_bytes());
        assert_eq!(calls, 3);

        let mut attempts = 0;
        assert!(read_table(|_, size| {
            attempts += 1;
            *size = 32;
            ERROR_INSUFFICIENT_BUFFER
        })
        .unwrap_err()
        .contains("kept changing"));
        assert_eq!(attempts, MAX_TABLE_ATTEMPTS + 1);
        assert!(read_table(|_, size| {
            *size = MAX_TABLE_BYTES as u32 + 1;
            ERROR_INSUFFICIENT_BUFFER
        })
        .unwrap_err()
        .contains("oversized"));
        assert!(read_table(|_, _| 5).unwrap_err().contains("5"));
    }

    #[test]
    fn table_rows_validate_offset_count_and_returned_buffer_size() {
        let row = tcp_row();
        let offset = offset_of!(MIB_TCPTABLE_OWNER_PID, table);
        let length = offset + size_of::<MIB_TCPROW_OWNER_PID>();
        let mut bytes = vec![0u8; length];
        bytes[..4].copy_from_slice(&1u32.to_ne_bytes());
        unsafe {
            bytes
                .as_mut_ptr()
                .add(offset)
                .cast::<MIB_TCPROW_OWNER_PID>()
                .write_unaligned(row);
        }
        let parsed = unsafe { rows::<MIB_TCPROW_OWNER_PID>(&bytes, offset) }.unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(tcp4(parsed[0]), tcp4(row));
        assert!(unsafe { rows::<MIB_TCPROW_OWNER_PID>(&bytes[..length - 1], offset) }.is_err());
        bytes[..4].copy_from_slice(&u32::MAX.to_ne_bytes());
        assert!(unsafe { rows::<MIB_TCPROW_OWNER_PID>(&bytes, offset) }.is_err());
        assert!(unsafe { rows::<MIB_TCPROW_OWNER_PID>(&[0, 0, 0], offset) }.is_err());
        assert!(unsafe { rows::<MIB_TCPROW_OWNER_PID>(&[0; 4], 2) }.is_err());

        let mut calls = 0;
        assert!(read_table(|_, size| {
            calls += 1;
            if calls == 1 {
                *size = 4;
                return ERROR_INSUFFICIENT_BUFFER;
            }
            *size = 100;
            0
        })
        .unwrap_err()
        .contains("length"));
    }

    #[test]
    fn fresh_full_paths_reject_pid_reuse_failed_queries_and_same_name_in_another_directory() {
        let candidate = |pid| Connection {
            pid,
            ..tcp4(tcp_row())
        };
        let mut calls = HashMap::<u32, usize>::new();
        let (connections, skipped) = select_app(
            vec![
                candidate(0),
                candidate(1),
                candidate(1),
                candidate(2),
                candidate(3),
                candidate(4),
                candidate(5),
            ],
            r"C:\Apps\browser.exe",
            100,
            |pid| {
                *calls.entry(pid).or_default() += 1;
                match pid {
                    1 => Some((90, r"c:\apps\BROWSER.EXE".into())),
                    2 => Some((90, r"C:\Other\browser.exe".into())),
                    3 => Some((101, r"C:\Apps\browser.exe".into())),
                    4 => None,
                    5 => Some((100, r"C:\Apps\browser.exe".into())),
                    _ => panic!("PID zero has no application identity"),
                }
            },
        );
        assert_eq!(
            connections.iter().map(|r| r.pid).collect::<Vec<_>>(),
            [1, 1, 5]
        );
        assert_eq!(skipped, 2);
        assert!(calls.values().all(|count| *count == 1));
    }

    #[test]
    fn unknown_app_keys_do_not_query_the_system() {
        assert!(for_app(&AppKey::new("pid 123")).is_err());
        assert!(for_app(&AppKey::new("browser.exe")).is_err());
        assert!(for_app(&AppKey::new("C:\\bad\0name.exe")).is_err());
    }

    fn local_socket_snapshot(ip: IpAddr) {
        let listener = TcpListener::bind(SocketAddr::new(ip, 0)).unwrap();
        let listening = listener.local_addr().unwrap();
        let client = TcpStream::connect_timeout(&listening, Duration::from_secs(2)).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        let udp = UdpSocket::bind(SocketAddr::new(ip, 0)).unwrap();
        // Even a connected UDP socket has no peer in OWNER_PID tables.
        udp.connect(listening).unwrap();
        let udp_address = udp.local_addr().unwrap();
        let executable = crate::proc::executable_path(std::process::id()).unwrap();
        let app = AppKey::new(executable);
        let tcp_protocol = if ip.is_ipv4() {
            ConnectionProtocol::Tcp4
        } else {
            ConnectionProtocol::Tcp6
        };
        let udp_protocol = if ip.is_ipv4() {
            ConnectionProtocol::Udp4
        } else {
            ConnectionProtocol::Udp6
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let report = for_app(&app).unwrap();
            let own: Vec<_> = report
                .connections
                .iter()
                .filter(|row| row.pid == std::process::id())
                .collect();
            let has_listener = own.iter().any(|row| {
                row.protocol == tcp_protocol
                    && row.local.port == listening.port()
                    && row.state == ConnectionState::Listen
                    && row.remote.is_none()
            });
            let has_client = own.iter().any(|row| {
                row.protocol == tcp_protocol
                    && row.local.port == client.local_addr().unwrap().port()
                    && row
                        .remote
                        .as_ref()
                        .is_some_and(|remote| remote.port == listening.port())
                    && row.state == ConnectionState::Established
            });
            let has_udp = own.iter().any(|row| {
                row.protocol == udp_protocol
                    && row.local.port == udp_address.port()
                    && row.state == ConnectionState::Bound
                    && row.remote.is_none()
            });
            if has_listener && has_client && has_udp {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "missing listener/client/UDP binding: {report:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        drop((accepted, client, listener, udp));
    }

    #[test]
    #[ignore = "requires a Windows desktop and local TCP/UDP sockets; no external network"]
    fn native_ipv4_reports_current_process_listeners_connections_and_udp_bindings() {
        local_socket_snapshot(Ipv4Addr::LOCALHOST.into());
    }

    #[test]
    #[ignore = "requires Windows with IPv6 loopback enabled; no external network"]
    fn native_ipv6_reports_current_process_listeners_connections_and_udp_bindings() {
        local_socket_snapshot(Ipv6Addr::LOCALHOST.into());
    }
}
