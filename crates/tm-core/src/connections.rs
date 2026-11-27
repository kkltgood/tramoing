//! Current socket information, independent of traffic accounting and history.

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionEndpoint {
    /// Numeric address (including an IPv6 scope when present); empty means wildcard.
    pub address: String,
    pub port: u16,
}

impl std::fmt::Display for ConnectionEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let address = if self.address.is_empty() {
            "*"
        } else {
            &self.address
        };
        if address.contains(':') {
            write!(f, "[{address}]:{}", self.port)
        } else {
            write!(f, "{address}:{}", self.port)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConnectionProtocol {
    Tcp4,
    Tcp6,
    Udp4,
    Udp6,
}

impl ConnectionProtocol {
    pub fn label(self) -> &'static str {
        match self {
            Self::Tcp4 => "TCP · IPv4",
            Self::Tcp6 => "TCP · IPv6",
            Self::Udp4 => "UDP · IPv4",
            Self::Udp6 => "UDP · IPv6",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConnectionState {
    Unknown,
    Listen,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
    Closed,
    Bound,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Connection {
    pub pid: u32,
    pub protocol: ConnectionProtocol,
    pub local: ConnectionEndpoint,
    /// None for listening sockets and when the OS does not supply a peer.
    pub remote: Option<ConnectionEndpoint>,
    pub state: ConnectionState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectionReport {
    pub connections: Vec<Connection>,
    /// Partial connection-table collection failure; successful rows may still be shown.
    pub warning: Option<String>,
    /// Distinct nonzero PIDs in the system connection tables whose current
    /// identity could not be verified. This is not a count of missing connections
    /// for the selected application, since those PIDs could not be attributed.
    pub unverified_processes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_keep_ipv6_scope_and_port_unambiguous() {
        for (address, port, expected) in [
            ("192.0.2.1", 443, "192.0.2.1:443"),
            ("fe80::1%en0", 5353, "[fe80::1%en0]:5353"),
            ("", 8080, "*:8080"),
        ] {
            assert_eq!(
                ConnectionEndpoint {
                    address: address.into(),
                    port
                }
                .to_string(),
                expected
            );
        }
    }
}
