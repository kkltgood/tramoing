//! Socket ownership from a single nettop snapshot, independent of byte attribution.

use std::collections::HashMap;

use tm_core::{
    AppKey, Connection, ConnectionEndpoint, ConnectionProtocol, ConnectionReport, ConnectionState,
};

use crate::flows::FlowRow;

#[derive(Default)]
pub(crate) struct Snapshot {
    pub rows: Vec<FlowRow>,
    /// Reuse resolutions already needed for traffic accounting. Idle/listening
    /// processes are resolved lazily, at most once per PID/name in this snapshot.
    pub owners: HashMap<(u32, String), Option<AppKey>>,
}

impl Snapshot {
    pub fn report(
        &mut self,
        app: &AppKey,
        mut resolve: impl FnMut(u32, &str) -> Option<AppKey>,
    ) -> ConnectionReport {
        let mut connections = Vec::new();
        for row in &self.rows {
            let owner = self
                .owners
                .entry((row.pid, row.proc_name.clone()))
                .or_insert_with(|| resolve(row.pid, &row.proc_name));
            if owner.as_ref() != Some(app) {
                continue;
            }
            let protocol = match row.proto.as_str() {
                "tcp4" => ConnectionProtocol::Tcp4,
                "tcp6" => ConnectionProtocol::Tcp6,
                "udp4" => ConnectionProtocol::Udp4,
                "udp6" => ConnectionProtocol::Udp6,
                _ => continue,
            };
            connections.push(Connection {
                pid: row.pid,
                protocol,
                local: endpoint(&row.local_ip, row.local_port),
                remote: (!wildcard(&row.remote_ip) || row.remote_port != 0)
                    .then(|| endpoint(&row.remote_ip, row.remote_port)),
                state: state(&row.state, row.proto.starts_with("udp"), row.local_port),
            });
        }
        ConnectionReport {
            connections,
            warning: None,
            unverified_processes: 0,
        }
    }
}

fn wildcard(address: &str) -> bool {
    matches!(address, "" | "*" | "*:*" | "*.*")
}

fn endpoint(address: &str, port: u16) -> ConnectionEndpoint {
    ConnectionEndpoint {
        address: if wildcard(address) {
            String::new()
        } else {
            address.into()
        },
        port,
    }
}

fn state(value: &str, udp: bool, local_port: u16) -> ConnectionState {
    match value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .as_str()
    {
        "listen" | "listening" => ConnectionState::Listen,
        "synsent" => ConnectionState::SynSent,
        "synreceived" | "synrecv" | "synrcvd" => ConnectionState::SynReceived,
        "established" => ConnectionState::Established,
        "finwait1" => ConnectionState::FinWait1,
        "finwait2" => ConnectionState::FinWait2,
        "closewait" => ConnectionState::CloseWait,
        "closing" => ConnectionState::Closing,
        "lastack" => ConnectionState::LastAck,
        "timewait" => ConnectionState::TimeWait,
        "closed" => ConnectionState::Closed,
        "bound" => ConnectionState::Bound,
        "" if udp && local_port != 0 => ConnectionState::Bound,
        _ => ConnectionState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flows::FlowParser;

    #[test]
    fn rejected_process_generations_are_not_retried_or_assigned_to_another_app() {
        let mut snapshot = snapshot(&[
            "t,app.42,,,0,0,",
            "t,tcp4 127.0.0.1:8000<->*:*,lo0,Listen,0,0,",
            "t,tcp4 127.0.0.1:8001<->*:*,lo0,Listen,0,0,",
        ]);
        let mut lookups = 0;
        let app = AppKey::new("/new/app");
        assert!(snapshot
            .report(&app, |_, _| {
                lookups += 1;
                None
            })
            .connections
            .is_empty());
        assert_eq!(lookups, 1);
        assert!(snapshot
            .report(&app, |_, _| panic!(
                "rejected generation must stay rejected"
            ))
            .connections
            .is_empty());
    }

    fn snapshot(lines: &[&str]) -> Snapshot {
        let mut parser = FlowParser::new();
        parser.feed("time,,interface,state,bytes_in,bytes_out,");
        for line in lines {
            parser.feed(line);
        }
        Snapshot {
            rows: parser.flush(),
            owners: HashMap::new(),
        }
    }

    #[test]
    fn details_keep_socket_owners_and_include_listeners_loopback_and_udp() {
        let mut snapshot = snapshot(&[
            "t,browser.100,,,0,0,",
            "t,tcp4 127.0.0.1:52000<->127.0.0.1:7890,lo0,Established,100,0,",
            "t,proxy.200,,,0,0,",
            "t,tcp4 127.0.0.1:7890<->*:*,lo0,Listen,0,0,",
            "t,tcp4 127.0.0.1:7890<->127.0.0.1:52000,lo0,Established,0,100,",
            "t,tcp4 192.0.2.1:53000<->198.51.100.1:443,en0,Established,100,0,",
            "t,helper.101,,,0,0,",
            "t,udp6 *.5353<->*.*,lo0,,0,0,",
        ]);
        let browser = AppKey::new("/Applications/Browser.app");
        let proxy = AppKey::new("/Applications/Proxy.app");
        let mut calls = 0;
        let report = snapshot.report(&browser, |pid, _| {
            calls += 1;
            Some(if pid == 200 {
                proxy.clone()
            } else {
                browser.clone()
            })
        });
        assert_eq!(calls, 3);
        assert_eq!(report.connections.len(), 2);
        assert_eq!(report.connections[0].pid, 100);
        assert_eq!(
            report.connections[0].remote.as_ref().unwrap().address,
            "127.0.0.1"
        );
        assert_eq!(report.connections[1].pid, 101);
        assert_eq!(report.connections[1].protocol, ConnectionProtocol::Udp6);
        assert_eq!(report.connections[1].state, ConnectionState::Bound);
        assert!(report.connections[1].remote.is_none());
        let report = snapshot.report(&proxy, |_, _| panic!("resolved once per snapshot"));
        assert_eq!(report.connections.len(), 3);
        assert_eq!(report.connections[0].state, ConnectionState::Listen);
        assert!(report.connections[0].remote.is_none());
        assert_eq!(
            report.connections[2].remote.as_ref().unwrap().address,
            "198.51.100.1"
        );
        assert!(snapshot
            .report(&AppKey::new("missing"), |_, _| unreachable!())
            .connections
            .is_empty());
    }

    #[test]
    fn tcp_state_spellings_and_ipv6_endpoints_are_preserved() {
        for (raw, expected) in [
            ("Listen", ConnectionState::Listen),
            ("SynSent", ConnectionState::SynSent),
            ("SYN_RCVD", ConnectionState::SynReceived),
            ("Established", ConnectionState::Established),
            ("FIN_WAIT_1", ConnectionState::FinWait1),
            ("FinWait2", ConnectionState::FinWait2),
            ("CloseWait", ConnectionState::CloseWait),
            ("Closing", ConnectionState::Closing),
            ("LastAck", ConnectionState::LastAck),
            ("TimeWait", ConnectionState::TimeWait),
            ("Closed", ConnectionState::Closed),
            ("Bound", ConnectionState::Bound),
            ("unexpected", ConnectionState::Unknown),
        ] {
            assert_eq!(state(raw, false, 443), expected);
        }
        let mut snapshot = snapshot(&[
            "t,app.42,,,0,0,",
            "t,tcp6 fe80::1%en0.50000<->[2001:db8::2]:443,en0,Established,0,0,",
        ]);
        let app = AppKey::new("app");
        let report = snapshot.report(&app, |_, _| Some(app.clone()));
        let connection = &report.connections[0];
        assert_eq!(connection.local.address, "fe80::1%en0");
        assert_eq!(connection.local.port, 50000);
        assert_eq!(connection.remote.as_ref().unwrap().address, "2001:db8::2");
        assert_eq!(connection.remote.as_ref().unwrap().port, 443);
    }
}
