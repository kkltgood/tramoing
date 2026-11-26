//! Optional temperature components. Local inspection never starts a download;
//! every network operation follows an explicit action in settings.

#![cfg_attr(not(windows), allow(dead_code))]

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use crate::component_install::{
    self, CachedDownload, Inspection, InstallOutcome, Prepared, Progress,
};

#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    Idle,
    Checking,
    Missing { bytes: u64, reused: Vec<String> },
    Downloading { pct: f32, label: String },
    Pausing { pct: f32, label: String },
    Paused { pct: f32, error: Option<String> },
    Ready { error: Option<String> },
    Installing,
    Installed { needs_restart: bool },
    Unsupported,
    Error(String),
}

type InspectionResult = Result<(Inspection, CachedDownload), String>;

enum Job {
    Inspect(Receiver<InspectionResult>),
    Download {
        progress: Receiver<Progress>,
        result: Receiver<Result<Option<Prepared>, String>>,
        stop: Sender<()>,
    },
    Install(Receiver<Result<InstallOutcome, String>>),
}

enum Completion {
    Inspected(InspectionResult),
    Downloaded(Result<Option<Prepared>, String>),
    Installed(Result<InstallOutcome, String>),
}

pub struct Manager {
    pub phase: Phase,
    plan: Option<Inspection>,
    prepared: Option<Prepared>,
    job: Option<Job>,
    cache: PathBuf,
}

impl Manager {
    pub fn new() -> Self {
        Self {
            phase: Phase::Idle,
            plan: None,
            prepared: None,
            job: None,
            cache: tm_store::data_dir().join("components"),
        }
    }

    pub fn is_busy(&self) -> bool {
        self.job.is_some()
    }

    pub fn is_installing(&self) -> bool {
        matches!(self.phase, Phase::Installing)
    }

    pub fn inspect(&mut self) {
        if self.is_busy() {
            return;
        }
        // UI captures and demos must never inspect or modify the real machine.
        if std::env::var_os("TM_FORCE_MOCK").is_some() {
            self.phase = Phase::Installed {
                needs_restart: false,
            };
            return;
        }
        let (tx, rx) = mpsc::channel();
        let cache = self.cache.clone();
        match std::thread::Builder::new()
            .name("sensor-component-check".into())
            .spawn(move || {
                let result = component_install::inspect().and_then(|plan| {
                    let cached = plan.cached_download(&cache)?;
                    Ok((plan, cached))
                });
                let _ = tx.send(result);
            }) {
            Ok(_) => {
                self.phase = Phase::Checking;
                self.job = Some(Job::Inspect(rx));
            }
            Err(error) => self.phase = Phase::Error(error.to_string()),
        }
    }

    pub fn download(&mut self) {
        if self.is_busy() || !matches!(self.phase, Phase::Missing { .. } | Phase::Paused { .. }) {
            return;
        }
        let Some(plan) = self.plan.clone() else {
            return;
        };
        let cache = self.cache.clone();
        let (progress_tx, progress_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = mpsc::channel();
        let pct = self.progress();
        match std::thread::Builder::new()
            .name("sensor-component-download".into())
            .spawn(move || {
                let result = component_install::download(&plan, &cache, &progress_tx, &stop_rx);
                let _ = result_tx.send(result);
            }) {
            Ok(_) => {
                self.phase = Phase::Downloading {
                    pct,
                    label: "Tramoing".into(),
                };
                self.job = Some(Job::Download {
                    progress: progress_rx,
                    result: result_rx,
                    stop: stop_tx,
                });
            }
            Err(error) => {
                self.phase = Phase::Paused {
                    pct,
                    error: Some(error.to_string()),
                };
            }
        }
    }

    pub fn pause(&mut self) {
        if let (Some(Job::Download { stop, .. }), Phase::Downloading { pct, label }) =
            (&self.job, &self.phase)
        {
            let _ = stop.send(());
            self.phase = Phase::Pausing {
                pct: *pct,
                label: label.clone(),
            };
        }
    }

    pub fn install(&mut self) {
        if self.is_busy() || !matches!(self.phase, Phase::Ready { .. }) {
            return;
        }
        let Some(prepared) = self.prepared.clone() else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        match std::thread::Builder::new()
            .name("sensor-component-install".into())
            .spawn(move || {
                let _ = tx.send(component_install::install(&prepared));
            }) {
            Ok(_) => {
                self.phase = Phase::Installing;
                self.job = Some(Job::Install(rx));
            }
            Err(error) => {
                self.phase = Phase::Ready {
                    error: Some(error.to_string()),
                };
            }
        }
    }

    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        let completion = match &self.job {
            Some(Job::Inspect(rx)) => receive(rx).map(Completion::Inspected),
            Some(Job::Install(rx)) => receive(rx).map(Completion::Installed),
            Some(Job::Download {
                progress, result, ..
            }) => {
                for update in progress.try_iter() {
                    let pct = if update.pct.is_finite() {
                        update.pct.clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    self.phase = if matches!(self.phase, Phase::Pausing { .. }) {
                        Phase::Pausing {
                            pct,
                            label: update.name,
                        }
                    } else {
                        Phase::Downloading {
                            pct,
                            label: update.name,
                        }
                    };
                    changed = true;
                }
                receive(result).map(Completion::Downloaded)
            }
            None => None,
        };
        if let Some(completion) = completion {
            self.job = None;
            self.complete(completion);
            changed = true;
        }
        changed
    }

    fn complete(&mut self, completion: Completion) {
        match completion {
            Completion::Inspected(Ok((plan, cached))) => {
                self.prepared = None;
                self.phase = if !plan.supported {
                    Phase::Unsupported
                } else if plan.ready {
                    Phase::Installed {
                        needs_restart: plan.needs_restart,
                    }
                } else {
                    match cached {
                        CachedDownload::Ready(prepared) => {
                            self.prepared = Some(prepared);
                            Phase::Ready { error: None }
                        }
                        CachedDownload::Partial(pct) => Phase::Paused { pct, error: None },
                        CachedDownload::Empty => Phase::Missing {
                            bytes: plan.total_bytes,
                            reused: plan.reused.clone(),
                        },
                    }
                };
                self.plan = Some(plan);
            }
            Completion::Inspected(Err(error)) => {
                log::warn!(target: "tramoing::components", "{error}");
                self.phase = Phase::Error(error);
            }
            Completion::Downloaded(Ok(Some(prepared))) => {
                self.prepared = Some(prepared);
                self.phase = Phase::Ready { error: None };
            }
            Completion::Downloaded(result) => {
                let error = result.err();
                if let Some(error) = &error {
                    log::warn!(target: "tramoing::components", "{error}");
                }
                self.phase = Phase::Paused {
                    pct: self.progress(),
                    error,
                };
            }
            Completion::Installed(Ok(outcome)) => {
                self.phase = Phase::Installed {
                    needs_restart: outcome.needs_restart,
                };
            }
            Completion::Installed(Err(error)) => {
                log::warn!(target: "tramoing::components", "{error}");
                // Keep verified downloads after a cancelled UAC prompt or failed install.
                self.phase = Phase::Ready { error: Some(error) };
            }
        }
    }

    fn progress(&self) -> f32 {
        match &self.phase {
            Phase::Downloading { pct, .. }
            | Phase::Pausing { pct, .. }
            | Phase::Paused { pct, .. } => *pct,
            _ => 0.0,
        }
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        if let Some(Job::Download { stop, .. }) = &self.job {
            let _ = stop.send(());
        }
    }
}

fn receive<T>(rx: &Receiver<Result<T, String>>) -> Option<Result<T, String>> {
    match rx.try_recv() {
        Ok(value) => Some(value),
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => Some(Err("Component operation interrupted".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inspection(supported: bool, ready: bool) -> Inspection {
        Inspection {
            supported,
            ready,
            needs_restart: false,
            total_bytes: 1234,
            reused: vec!["PawnIO".into()],
            items: Vec::new(),
        }
    }

    #[test]
    fn inspection_never_starts_a_download_or_installation() {
        let mut manager = Manager::new();
        assert_eq!(manager.phase, Phase::Idle);
        assert!(!manager.is_busy());
        manager.complete(Completion::Inspected(Ok((
            inspection(true, false),
            CachedDownload::Empty,
        ))));
        assert_eq!(
            manager.phase,
            Phase::Missing {
                bytes: 1234,
                reused: vec!["PawnIO".into()],
            }
        );
        assert!(!manager.is_busy());
        assert!(manager.prepared.is_none());

        manager.complete(Completion::Inspected(Ok((
            inspection(false, false),
            CachedDownload::Empty,
        ))));
        assert_eq!(manager.phase, Phase::Unsupported);
        manager.download();
        assert!(!manager.is_busy());
    }

    #[test]
    fn pausing_preserves_progress_and_waits_for_downloader_to_finish() {
        let mut manager = Manager::new();
        let (progress_tx, progress) = mpsc::channel();
        let (result_tx, result) = mpsc::channel();
        let (stop, stopped) = mpsc::channel();
        manager.phase = Phase::Downloading {
            pct: 0.3,
            label: ".NET".into(),
        };
        manager.job = Some(Job::Download {
            progress,
            result,
            stop,
        });
        manager.pause();
        assert!(stopped.try_recv().is_ok());
        assert!(manager.is_busy());
        progress_tx
            .send(Progress {
                name: ".NET".into(),
                pct: 0.4,
            })
            .unwrap();
        assert!(manager.poll());
        assert!(matches!(manager.phase, Phase::Pausing { pct, .. } if pct == 0.4));
        result_tx.send(Ok(None)).unwrap();
        assert!(manager.poll());
        assert_eq!(
            manager.phase,
            Phase::Paused {
                pct: 0.4,
                error: None
            }
        );
        assert!(!manager.is_busy());
    }

    #[test]
    fn interrupted_operation_unblocks_ui_without_claiming_success() {
        let mut manager = Manager::new();
        let (tx, rx) = mpsc::channel();
        manager.phase = Phase::Installing;
        manager.job = Some(Job::Install(rx));
        drop(tx);
        assert!(manager.poll());
        assert!(!manager.is_busy());
        assert!(matches!(manager.phase, Phase::Ready { error: Some(_) }));
    }
}
