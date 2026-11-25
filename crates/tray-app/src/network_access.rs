//! Windows 网络控制的异步状态。只有助手核验后的系统规则能显示“已断网”。
//! 磁盘缓存只保留恢复入口，启动/失败后显示待核验，不乐观更新开关。

use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use eframe::egui;
use tm_windows::network_policy::{same_path, validate_path, Request, Rule};

pub struct NetworkAccess {
    pub rules: Vec<Rule>,
    pub verified: bool,
    pub error: Option<String>,
    pub pending_path: Option<String>,
    job: Option<Receiver<Result<Vec<Rule>, String>>>,
    next_refresh: Instant,
    mock: bool,
}

impl NetworkAccess {
    pub fn new(mock: bool) -> Self {
        let rules = if mock {
            Vec::new()
        } else {
            std::fs::read(cache_path())
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Vec<Rule>>(&bytes).ok())
                .unwrap_or_default()
                .into_iter()
                .filter(|r| validate_path(&r.path).is_ok())
                .collect()
        };
        Self {
            rules,
            verified: false,
            error: None,
            pending_path: None,
            job: None,
            next_refresh: Instant::now(),
            mock,
        }
    }

    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    pub fn is_demo(&self) -> bool {
        self.mock
    }

    pub fn rule(&self, path: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| same_path(&r.path, path))
    }

    pub fn refresh(&mut self, ctx: &egui::Context) {
        self.start(Request::List {}, ctx);
    }

    pub fn set(&mut self, path: &str, blocked: bool, ctx: &egui::Context) {
        if let Err(error) = validate_path(path) {
            self.error = Some(error);
            return;
        }
        self.start(
            Request::Set {
                path: path.into(),
                blocked,
            },
            ctx,
        );
    }

    fn start(&mut self, request: Request, ctx: &egui::Context) {
        if self.busy() || self.mock {
            return;
        }
        self.pending_path = match &request {
            Request::Set { path, .. } => Some(path.clone()),
            _ => None,
        };
        if let Request::Set {
            path,
            blocked: true,
        } = &request
        {
            if self.rule(path).is_none() {
                self.rules.push(Rule {
                    path: path.clone(),
                    complete: false,
                });
            }
            self.save_cache();
        }
        let wake = ctx.clone();
        let (tx, rx) = mpsc::channel();
        match std::thread::Builder::new()
            .name("network-control".into())
            .spawn(move || {
                let result = tm_windows::network_control::request(&request).and_then(|reply| {
                    match reply.error {
                        Some(error) => Err(error),
                        None => Ok(reply.rules),
                    }
                });
                let _ = tx.send(result);
                wake.request_repaint();
            }) {
            Ok(_) => {
                self.job = Some(rx);
            }
            Err(error) => {
                log::error!(target: "tramoing::network", "启动控制任务失败：{error}");
                self.error = Some(error.to_string());
                self.verified = false;
                self.pending_path = None;
            }
        }
    }

    pub fn tick(&mut self, ctx: &egui::Context) -> Option<String> {
        let result = self.job.as_ref().and_then(|job| match job.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err("网络控制任务意外结束".into())),
        });
        let mut message = None;
        if let Some(result) = result {
            self.job = None;
            let changed = self.pending_path.take();
            match result {
                Ok(rules) => {
                    if self.error.is_some() {
                        log::info!(target: "tramoing::network", "网络控制助手已恢复连接");
                    }
                    self.rules = rules;
                    self.verified = true;
                    self.error = None;
                    self.save_cache();
                    if let Some(path) = changed {
                        log::info!(target: "tramoing::network", "网络规则已核验，blocked={}", self.rule(&path).is_some());
                        message = Some(format!(
                            "{}：{}",
                            tm_core::pretty_name(&path),
                            if self.rule(&path).is_some() {
                                "已断网"
                            } else {
                                "已移除断网限制"
                            }
                        ));
                    }
                }
                Err(error) => {
                    if self.error.as_deref() != Some(error.as_str()) || changed.is_some() {
                        log::error!(target: "tramoing::network", "网络控制失败：{error}");
                    }
                    self.verified = false;
                    if changed.is_some() {
                        message = Some(error.clone());
                    }
                    self.error = Some(error);
                }
            }
            self.next_refresh = Instant::now() + Duration::from_secs(10);
        }
        if !self.mock && !self.busy() && Instant::now() >= self.next_refresh {
            self.refresh(ctx);
        }
        message
    }

    pub fn unavailable_reason(&self) -> Option<&str> {
        if self.mock {
            Some("演示模式不修改系统网络规则")
        } else if let Some(error) = self.error.as_deref() {
            Some(error)
        } else if !self.verified {
            Some("正在核验系统网络规则")
        } else {
            None
        }
    }

    fn save_cache(&self) {
        if self.mock {
            return;
        }
        if let Ok(bytes) = serde_json::to_vec(&self.rules) {
            if let Err(error) = tm_store::files::atomic_write(&cache_path(), &bytes) {
                crate::update::log_line(&format!("网络规则缓存保存失败：{error}"));
            }
        }
    }
}

fn cache_path() -> std::path::PathBuf {
    tm_store::data_dir().join("network-rules-cache.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_change_preserves_restore_entry_but_invalidates_confirmed_state() {
        let mut state = NetworkAccess::new(true);
        state.rules = vec![Rule {
            path: r"C:\app.exe".into(),
            complete: true,
        }];
        state.verified = true;
        state.pending_path = Some(r"C:\app.exe".into());
        let (tx, rx) = mpsc::channel();
        tx.send(Err("助手不可用".into())).unwrap();
        state.job = Some(rx);
        assert_eq!(
            state.tick(&egui::Context::default()),
            Some("助手不可用".into())
        );
        assert!(!state.verified);
        assert!(state.rule(r"C:\app.exe").is_some());
        assert!(!state.busy());
    }
}
