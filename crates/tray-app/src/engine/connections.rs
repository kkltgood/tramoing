//! On-demand connection snapshots. The UI never performs OS queries or retains
//! a previous application's result while a new request is being collected.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tm_core::{AppKey, ConnectionReport};

pub const DISPLAY_LIMIT: usize = 200;
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct Details {
    pub updated_at: Instant,
    pub total: usize,
    pub established: usize,
    pub listening: usize,
    pub result: Result<ConnectionReport, String>,
}

#[derive(Default)]
struct State {
    app: Option<AppKey>,
    revision: u64,
    details: Option<Details>,
}

#[derive(Clone, Default)]
pub struct Requests(Arc<Mutex<State>>);

impl Requests {
    pub fn select(&self, app: Option<AppKey>) {
        let mut state = self.0.lock().unwrap();
        if state.app != app {
            state.app = app;
            state.revision = state.revision.wrapping_add(1);
            state.details = None;
        }
    }

    pub fn details(&self, app: &AppKey) -> Option<Details> {
        let state = self.0.lock().unwrap();
        (state.app.as_ref() == Some(app))
            .then(|| state.details.clone())
            .flatten()
    }
}

pub struct Worker {
    requests: Requests,
    last_query: Option<(u64, Instant)>,
}

impl Worker {
    pub fn new(requests: Requests) -> Self {
        Self {
            requests,
            last_query: None,
        }
    }

    pub fn refresh(&mut self, query: impl FnOnce(&AppKey) -> Result<ConnectionReport, String>) {
        self.refresh_at(Instant::now(), query);
    }

    fn refresh_at(
        &mut self,
        now: Instant,
        query: impl FnOnce(&AppKey) -> Result<ConnectionReport, String>,
    ) {
        let (app, revision) = {
            let state = self.requests.0.lock().unwrap();
            let Some(app) = state.app.clone() else { return };
            (app, state.revision)
        };
        if self.last_query.is_some_and(|(last, at)| {
            last == revision && now.saturating_duration_since(at) < REFRESH_INTERVAL
        }) {
            return;
        }
        self.last_query = Some((revision, now));
        let mut result = query(&app);
        let (total, established, listening) = if let Ok(report) = &mut result {
            report.connections.sort_by(|a, b| {
                display_priority(a)
                    .cmp(&display_priority(b))
                    .then_with(|| a.cmp(b))
            });
            report.connections.dedup();
            let total = report.connections.len();
            let established = report
                .connections
                .iter()
                .filter(|row| row.state == tm_core::ConnectionState::Established)
                .count();
            let listening = report
                .connections
                .iter()
                .filter(|row| row.state == tm_core::ConnectionState::Listen)
                .count();
            report.connections.truncate(DISPLAY_LIMIT);
            (total, established, listening)
        } else {
            (0, 0, 0)
        };
        let mut state = self.requests.0.lock().unwrap();
        // A hidden panel or a new selection invalidates any in-flight query.
        if state.revision == revision {
            state.details = Some(Details {
                updated_at: now,
                total,
                established,
                listening,
                result,
            });
        }
    }
}

fn display_priority(connection: &tm_core::Connection) -> u8 {
    use tm_core::ConnectionState::*;
    match connection.state {
        Established => 0,
        SynSent | SynReceived => 1,
        Listen => 3,
        Bound => 4,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_core::{Connection, ConnectionEndpoint, ConnectionProtocol, ConnectionState};

    #[test]
    fn requests_are_throttled_and_inflight_results_cannot_cross_selection() {
        let requests = Requests::default();
        let mut worker = Worker::new(requests.clone());
        let now = Instant::now();
        worker.refresh_at(now, |_| panic!("hidden panel must not query"));
        requests.select(Some(AppKey::from("a")));
        worker.refresh_at(now, |_| Ok(ConnectionReport::default()));
        assert!(requests.details(&AppKey::from("a")).is_some());
        worker.refresh_at(now + Duration::from_secs(1), |_| panic!("must throttle"));
        worker.refresh_at(now + Duration::from_secs(2), |_| {
            requests.select(Some(AppKey::from("b")));
            Ok(ConnectionReport::default())
        });
        assert!(requests.details(&AppKey::from("a")).is_none());
        assert!(requests.details(&AppKey::from("b")).is_none());
        worker.refresh_at(now + Duration::from_secs(2), |_| Err("offline".into()));
        assert!(requests
            .details(&AppKey::from("b"))
            .unwrap()
            .result
            .is_err());
        requests.select(None);
        assert!(requests.details(&AppKey::from("b")).is_none());
    }

    #[test]
    fn rows_are_deduplicated_bounded_and_replaced_on_failure() {
        let requests = Requests::default();
        requests.select(Some(AppKey::from("a")));
        let mut worker = Worker::new(requests.clone());
        let now = Instant::now();
        worker.refresh_at(now, |_| {
            let mut rows: Vec<_> = (0..250)
                .rev()
                .map(|port| Connection {
                    pid: 1,
                    protocol: ConnectionProtocol::Tcp4,
                    local: ConnectionEndpoint {
                        address: "127.0.0.1".into(),
                        port,
                    },
                    remote: None,
                    state: ConnectionState::Listen,
                })
                .collect();
            rows.push(rows[0].clone());
            Ok(ConnectionReport {
                connections: rows,
                warning: None,
            })
        });
        let details = requests.details(&AppKey::from("a")).unwrap();
        assert_eq!(details.total, 250);
        let rows = details.result.unwrap().connections;
        assert_eq!(rows.len(), DISPLAY_LIMIT);
        assert_eq!(rows[0].local.port, 0);
        worker.refresh_at(now + REFRESH_INTERVAL, |_| Err("offline".into()));
        let details = requests.details(&AppKey::from("a")).unwrap();
        assert_eq!(details.total, 0);
        assert_eq!(details.result.unwrap_err(), "offline");
    }

    #[test]
    fn summary_counts_precede_display_limit_and_established_rows_stay_visible() {
        let requests = Requests::default();
        requests.select(Some(AppKey::from("app")));
        let mut worker = Worker::new(requests.clone());
        worker.refresh(|_| {
            let mut connections: Vec<_> = (0..250)
                .map(|port| Connection {
                    pid: 1,
                    protocol: ConnectionProtocol::Tcp4,
                    local: ConnectionEndpoint {
                        address: "127.0.0.1".into(),
                        port,
                    },
                    remote: None,
                    state: ConnectionState::Listen,
                })
                .collect();
            connections.push(Connection {
                pid: 999,
                protocol: ConnectionProtocol::Tcp4,
                local: ConnectionEndpoint {
                    address: "192.0.2.1".into(),
                    port: 52100,
                },
                remote: Some(ConnectionEndpoint {
                    address: "198.51.100.1".into(),
                    port: 443,
                }),
                state: ConnectionState::Established,
            });
            Ok(ConnectionReport {
                connections,
                warning: None,
            })
        });
        let details = requests.details(&AppKey::from("app")).unwrap();
        assert_eq!(
            (details.total, details.established, details.listening),
            (251, 1, 250)
        );
        let rows = details.result.unwrap().connections;
        assert_eq!(rows.len(), DISPLAY_LIMIT);
        assert_eq!(rows[0].state, ConnectionState::Established);
    }
}
