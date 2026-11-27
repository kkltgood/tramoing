//! Display-only grouping: preserve every socket record under its main endpoint.

use std::collections::BTreeMap;
use std::net::IpAddr;

use eframe::egui;
use tm_core::{Connection, ConnectionEndpoint, ConnectionProtocol, ConnectionState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum Kind {
    Local,
    Remote,
    Listener,
    Bound,
}

pub(super) struct Group<'a> {
    pub kind: Kind,
    pub protocol: ConnectionProtocol,
    pub endpoint: &'a ConnectionEndpoint,
    pub rows: Vec<&'a Connection>,
}

impl Group<'_> {
    /// State/PID/local-port churn must not collapse an expanded destination.
    pub fn id(&self) -> egui::Id {
        egui::Id::new(("connection-group", self.kind, self.protocol, self.endpoint))
    }

    pub fn uniform_state(&self) -> Option<ConnectionState> {
        let first = self.rows.first()?.state;
        self.rows
            .iter()
            .all(|row| row.state == first)
            .then_some(first)
    }
}

pub(super) fn collect(rows: &[Connection]) -> Vec<Group<'_>> {
    // Sort by section, displayed endpoint, then protocol. Endpoint references
    // compare by value, so independently allocated strings still coalesce.
    let mut grouped =
        BTreeMap::<(Kind, &ConnectionEndpoint, ConnectionProtocol), Vec<&Connection>>::new();
    for row in rows {
        let (kind, endpoint) = if row.state == ConnectionState::Listen {
            (Kind::Listener, &row.local)
        } else if let Some(remote) = &row.remote {
            (
                if is_loopback(&remote.address) {
                    Kind::Local
                } else {
                    Kind::Remote
                },
                remote,
            )
        } else {
            (Kind::Bound, &row.local)
        };
        grouped
            .entry((kind, endpoint, row.protocol))
            .or_default()
            .push(row);
    }
    grouped
        .into_iter()
        .map(|((kind, endpoint, protocol), mut rows)| {
            rows.sort_by(|a, b| {
                a.pid
                    .cmp(&b.pid)
                    .then_with(|| a.local.cmp(&b.local))
                    .then_with(|| a.remote.cmp(&b.remote))
                    .then_with(|| a.state.cmp(&b.state))
            });
            Group {
                kind,
                protocol,
                endpoint,
                rows,
            }
        })
        .collect()
}

fn is_loopback(address: &str) -> bool {
    // A numeric IPv6 zone qualifies its interface; it does not change whether
    // the address is loopback. Preserve the original scoped endpoint as the key.
    let address = address
        .split_once('%')
        .map_or(address, |(address, _)| address);
    match address.parse::<IpAddr>() {
        Ok(IpAddr::V4(address)) => address.is_loopback(),
        Ok(IpAddr::V6(address)) => {
            address.is_loopback() || address.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(address: &str, port: u16) -> ConnectionEndpoint {
        ConnectionEndpoint {
            address: address.into(),
            port,
        }
    }

    fn connection(pid: u32, local_port: u16, address: &str, remote_port: u16) -> Connection {
        Connection {
            pid,
            protocol: ConnectionProtocol::Tcp4,
            local: endpoint("192.0.2.1", local_port),
            remote: Some(endpoint(address, remote_port)),
            state: ConnectionState::Established,
        }
    }

    #[test]
    fn four_sockets_to_one_endpoint_keep_every_process_port_and_state() {
        let rows = vec![
            Connection {
                state: ConnectionState::CloseWait,
                ..connection(20, 50002, "198.51.100.2", 443)
            },
            connection(10, 50002, "198.51.100.2", 443),
            connection(20, 50001, "198.51.100.2", 443),
            connection(10, 50001, "198.51.100.2", 443),
        ];
        let groups = collect(&rows);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].kind, Kind::Remote);
        assert_eq!(groups[0].endpoint, &endpoint("198.51.100.2", 443));
        assert_eq!(
            groups[0]
                .rows
                .iter()
                .map(|row| (row.pid, row.local.port))
                .collect::<Vec<_>>(),
            [(10, 50001), (10, 50002), (20, 50001), (20, 50002)]
        );
        assert_eq!(groups[0].uniform_state(), None);
        assert_eq!(
            groups[0]
                .rows
                .iter()
                .filter(|row| row.state == ConnectionState::CloseWait)
                .count(),
            1
        );
    }

    #[test]
    fn only_numeric_loopback_targets_are_local() {
        for (address, expected) in [
            ("127.0.0.1", Kind::Local),
            ("127.99.2.3", Kind::Local),
            ("::1", Kind::Local),
            ("::1%lo0", Kind::Local),
            ("::ffff:127.0.0.1", Kind::Local),
            ("::ffff:7f01:203", Kind::Local),
            ("192.168.1.1", Kind::Remote),
            ("10.0.0.1", Kind::Remote),
            ("fe80::1%en0", Kind::Remote),
            ("::ffff:192.0.2.1", Kind::Remote),
            ("0.0.0.0", Kind::Remote),
            ("::", Kind::Remote),
            ("localhost", Kind::Remote),
            ("", Kind::Remote),
        ] {
            let rows = [connection(1, 50000, address, 443)];
            assert_eq!(collect(&rows)[0].kind, expected, "{address}");
        }
        // A loopback local bind does not make a remote peer a local target.
        let rows = [Connection {
            local: endpoint("127.0.0.1", 50000),
            ..connection(1, 0, "203.0.113.1", 443)
        }];
        assert_eq!(collect(&rows)[0].kind, Kind::Remote);
    }

    #[test]
    fn unknown_udp_peers_remain_bound_and_known_udp_peers_group_by_destination() {
        let rows = [
            Connection {
                pid: 1,
                protocol: ConnectionProtocol::Udp4,
                local: endpoint("127.0.0.1", 5353),
                remote: None,
                state: ConnectionState::Bound,
            },
            Connection {
                pid: 2,
                protocol: ConnectionProtocol::Udp4,
                local: endpoint("127.0.0.1", 5353),
                remote: None,
                state: ConnectionState::Unknown,
            },
            Connection {
                protocol: ConnectionProtocol::Udp4,
                ..connection(3, 53000, "192.0.2.53", 53)
            },
        ];
        let groups = collect(&rows);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].kind, Kind::Remote);
        assert_eq!(groups[0].endpoint, &endpoint("192.0.2.53", 53));
        assert_eq!(groups[1].kind, Kind::Bound);
        assert_eq!(groups[1].endpoint, &endpoint("127.0.0.1", 5353));
        assert_eq!(groups[1].rows.len(), 2);
        assert!(groups[1].rows.iter().all(|row| row.remote.is_none()));
    }

    #[test]
    fn listening_uses_local_endpoint_and_preserves_ipv6_scopes() {
        let row = |scope: &str| Connection {
            protocol: ConnectionProtocol::Tcp6,
            local: endpoint(scope, 8080),
            // Listen takes precedence even if an upstream row includes a peer.
            remote: Some(endpoint("::1", 80)),
            state: ConnectionState::Listen,
            ..connection(1, 0, "", 0)
        };
        let rows = [row("fe80::1%en1"), row("fe80::1%en0")];
        let groups = collect(&rows);
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().all(|group| group.kind == Kind::Listener));
        assert_eq!(groups[0].endpoint, &endpoint("fe80::1%en0", 8080));
        assert_eq!(groups[1].endpoint, &endpoint("fe80::1%en1", 8080));
        assert_ne!(groups[0].id(), groups[1].id());
        assert_eq!(groups[0].uniform_state(), Some(ConnectionState::Listen));
    }

    #[test]
    fn group_id_survives_connection_state_pid_and_local_port_changes() {
        let original = [connection(1, 50000, "203.0.113.1", 443)];
        let changed = [Connection {
            state: ConnectionState::CloseWait,
            ..connection(42, 60000, "203.0.113.1", 443)
        }];
        let before = collect(&original);
        let after = collect(&changed);
        assert_eq!(before[0].id(), after[0].id());
        assert_eq!(
            before[0].uniform_state(),
            Some(ConnectionState::Established)
        );
        assert_eq!(after[0].uniform_state(), Some(ConnectionState::CloseWait));
    }

    #[test]
    fn ordering_and_ids_separate_sections_endpoints_ports_and_protocols() {
        let mut rows = vec![
            Connection {
                remote: None,
                state: ConnectionState::Bound,
                ..connection(1, 80, "", 0)
            },
            Connection {
                remote: None,
                state: ConnectionState::Listen,
                ..connection(1, 80, "", 0)
            },
            connection(1, 50002, "203.0.113.2", 443),
            Connection {
                protocol: ConnectionProtocol::Udp4,
                ..connection(1, 50001, "203.0.113.1", 443)
            },
            connection(1, 50000, "203.0.113.1", 443),
            connection(1, 50003, "203.0.113.1", 80),
            connection(1, 50004, "127.0.0.1", 443),
        ];
        let groups = collect(&rows);
        let ids: Vec<_> = groups.iter().map(Group::id).collect();
        assert_eq!(
            groups.iter().map(|group| group.kind).collect::<Vec<_>>(),
            [
                Kind::Local,
                Kind::Remote,
                Kind::Remote,
                Kind::Remote,
                Kind::Remote,
                Kind::Listener,
                Kind::Bound
            ]
        );
        assert_eq!(groups[1].endpoint.port, 80);
        assert_eq!(groups[2].protocol, ConnectionProtocol::Tcp4);
        assert_eq!(groups[3].protocol, ConnectionProtocol::Udp4);
        assert_eq!(groups[4].endpoint.address, "203.0.113.2");
        assert_eq!(
            ids.iter().collect::<std::collections::HashSet<_>>().len(),
            ids.len()
        );
        rows.reverse();
        assert_eq!(
            collect(&rows).iter().map(Group::id).collect::<Vec<_>>(),
            ids
        );
    }

    #[test]
    fn empty_input_and_identical_records_do_not_invent_or_drop_sockets() {
        assert!(collect(&[]).is_empty());
        let row = connection(1, 50000, "203.0.113.1", 443);
        let rows = [row.clone(), row];
        let groups = collect(&rows);
        assert_eq!(groups[0].rows.len(), 2);
        assert_eq!(
            groups[0].uniform_state(),
            Some(ConnectionState::Established)
        );
    }
}
