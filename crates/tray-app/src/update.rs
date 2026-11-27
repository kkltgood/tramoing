//! 更新检测与下载。
//!
//! 为什么不引 reqwest：它拖着 tokio，为一个 GET 请求把编译时间和二进制
//! 体积都翻一倍不划算。ureq 是阻塞式的小客户端，配 `native-tls`
//! （Windows 走 SChannel、macOS 走 Security.framework）连 TLS 后端都是系统的，
//! 不需要 nasm/CMake 也能编。
//!
//! 所有网络操作都在后台线程里：`update()` 每帧都会被调用，一次 DNS 卡顿
//! 就能让整个面板僵住。界面只通过 channel 收事件，和采样线程是同一套模式。
//!
//! 默认读取官方 GitHub Release 的 latest.json 附件，不依赖 GitHub API 配额。
//! 可通过 TRAMOING_UPDATE_URL 覆盖地址或用空值禁用，见 docs/development.md。

pub(crate) mod download;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const REPOSITORY_URL: &str = env!("CARGO_PKG_REPOSITORY");
pub const RELEASES_URL: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/releases/latest");
pub const DEFAULT_MANIFEST_URL: &str = concat!(
    env!("CARGO_PKG_REPOSITORY"),
    "/releases/latest/download/latest.json"
);

/// 运行时环境变量 > 构建时配置 > 官方 GitHub Releases；空值显式禁用。
pub fn manifest_url() -> Option<String> {
    resolve_manifest_url(
        std::env::var("TRAMOING_UPDATE_URL").ok().as_deref(),
        option_env!("TRAMOING_UPDATE_URL"),
    )
}

fn resolve_manifest_url(runtime: Option<&str>, build: Option<&str>) -> Option<String> {
    let url = runtime.or(build).unwrap_or(DEFAULT_MANIFEST_URL).trim();
    (!url.is_empty()).then(|| url.to_owned())
}

/// 当前版本。和安装包版本、`Cargo.toml` 的 workspace version 是同一个来源。
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// 清单里按平台分的键。写错一个字符，用户就永远收不到更新，所以这里是
/// 唯一真源，发布脚本也照这个拼。
pub const PLATFORM: &str = if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
    "windows-x86_64"
} else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
    "windows-aarch64"
} else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    "macos-aarch64"
} else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
    "macos-x86_64"
} else {
    "unsupported"
};

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub pub_date: String,
    /// 没有对应平台的包时退回这个页面，让用户自己挑。
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub platforms: std::collections::BTreeMap<String, Asset>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Asset {
    pub url: String,
    /// 小写十六进制。为空表示清单没给，这时只提示不下载——
    /// 没有校验的安装包不该被自动拉起。
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub size: u64,
}

/// 发现的新版本。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Available {
    pub version: String,
    pub notes: String,
    /// 发布日期。界面上不显示（页脚放不下），但日志里带上，
    /// 排查「为什么这台机器还提示旧版本」时有用。
    pub pub_date: String,
    pub asset: Option<Asset>,
    pub homepage: String,
}

#[derive(Debug, PartialEq)]
pub enum InstallAction {
    Prompt { version: String },
    InstallOnLaunch,
}

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum InstallPolicy {
    #[default]
    Ask,
    NextLaunch,
    Manual,
}

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum DownloadMode {
    #[default]
    Paused,
    Automatic,
    Exhausted,
}

impl DownloadMode {
    fn restored_error(self) -> Option<String> {
        (self == Self::Exhausted).then(|| {
            "自动下载已停止，请重试或手动下载 / Download stopped; retry or download manually".into()
        })
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct PendingDownload {
    #[serde(flatten)]
    release: Available,
    #[serde(default)]
    platform: String,
    #[serde(default)]
    install_policy: InstallPolicy,
    #[serde(default)]
    download_mode: DownloadMode,
}

impl PendingDownload {
    fn save(&self, root: &Path) -> Result<(), String> {
        serde_json::to_vec(self)
            .map_err(std::io::Error::other)
            .and_then(|bytes| tm_store::files::atomic_write(&root.join("pending.json"), &bytes))
            .map_err(|error| format!("无法保存更新任务：{error}"))
    }
}

/// 后台线程发回界面的事件。
pub enum Event {
    /// `None` = 已是最新
    Checked(Option<Available>),
    Failed(String),
    Progress(f32),
    DownloadRetrying {
        delay: Duration,
    },
    DownloadPaused,
    Downloaded {
        version: String,
        path: PathBuf,
    },
    DownloadFailed(String),
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    InstallPrepared(Result<(), String>),
}

/// 界面侧的状态机。没有任何网络动作，只是把事件攒成一个可画的形状。
pub struct Updater {
    tx: Sender<Event>,
    rx: Receiver<Event>,
    pub phase: Phase,
    /// `phase` 最近一次变化的时刻。`Latest` / `Error` 靠它自动消退——
    /// 「已是最新」赖在页脚不走，用户会以为下次检查也是这个结论。
    phase_at: Instant,
    download_stop: Option<Sender<()>>,
    root: PathBuf,
    pending_download: Option<PendingDownload>,
    check_result: Option<Result<Option<String>, String>>,
    ready_before_check: Option<Phase>,
    download_error: Option<String>,
    install_action: Option<InstallAction>,
    verifying_cache: bool,
    wake: Arc<dyn Fn() + Send + Sync>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    /// 还没查过（或用户关掉了自动检查）
    Idle,
    Checking,
    /// 已是最新。页脚短暂显示一下，让手动检查有反馈。
    Latest,
    Available(Available),
    Downloading {
        version: String,
        pct: f32,
    },
    Retrying {
        version: String,
        pct: f32,
        retry_at: Instant,
    },
    Pausing {
        version: String,
        pct: f32,
        error: Option<String>,
    },
    Paused {
        version: String,
        pct: f32,
        error: Option<String>,
    },
    /// 下载并校验完了，等用户确认更新
    Ready {
        version: String,
        path: PathBuf,
        launch_error: Option<String>,
    },
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Installing {
        version: String,
        path: PathBuf,
    },
    Restarting,
    Error(String),
}

impl Default for Updater {
    fn default() -> Self {
        Self::new()
    }
}

impl Updater {
    pub fn new() -> Self {
        Self::with_waker(|| {})
    }

    pub fn with_waker(wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx,
            phase: Phase::Idle,
            phase_at: Instant::now(),
            download_stop: None,
            root: tm_store::data_dir().join("update"),
            pending_download: None,
            check_result: None,
            ready_before_check: None,
            download_error: None,
            install_action: None,
            verifying_cache: false,
            wake: Arc::new(wake),
        }
    }

    fn terminal_sender(&self) -> impl FnOnce(Event) + Send + 'static {
        let tx = self.tx.clone();
        let wake = self.wake.clone();
        move |event| {
            if tx.send(event).is_ok() {
                wake();
            }
        }
    }

    pub fn take_check_result(&mut self) -> Option<Result<Option<String>, String>> {
        self.check_result.take()
    }

    pub fn take_download_error(&mut self) -> Option<String> {
        self.download_error.take()
    }

    pub fn release_page(&self) -> &str {
        let release = match &self.phase {
            Phase::Available(available) => Some(available),
            _ => self
                .pending_download
                .as_ref()
                .map(|pending| &pending.release),
        };
        release
            .map(|available| available.homepage.as_str())
            .filter(|url| http_url(url).is_ok())
            .unwrap_or(RELEASES_URL)
    }

    pub fn take_install_action(&mut self) -> Option<InstallAction> {
        self.install_action.take()
    }

    pub fn is_install_deferred(&self) -> bool {
        self.pending_download
            .as_ref()
            .is_some_and(|pending| pending.install_policy == InstallPolicy::NextLaunch)
    }

    fn update_pending(&mut self, policy: InstallPolicy, mode: DownloadMode) -> Result<(), String> {
        self.update_pending_with(policy, mode, PendingDownload::save)
    }

    fn update_pending_with(
        &mut self,
        policy: InstallPolicy,
        mode: DownloadMode,
        save: impl FnOnce(&PendingDownload, &Path) -> Result<(), String>,
    ) -> Result<(), String> {
        let pending = self.pending_download.as_mut().ok_or("没有待安装的更新")?;
        pending.install_policy = policy;
        pending.download_mode = mode;
        if let Err(error) = save(pending, &self.root) {
            pending.install_policy = InstallPolicy::Manual;
            self.install_action = None;
            if let Err(cleanup) = std::fs::remove_file(self.root.join("pending.json")) {
                if cleanup.kind() != std::io::ErrorKind::NotFound {
                    return Err(format!("{error}；也无法清除旧的更新任务：{cleanup}"));
                }
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn defer_install(&mut self) -> Result<(), String> {
        if !matches!(self.phase, Phase::Ready { .. }) {
            return Err("更新安装包尚未就绪".into());
        }
        self.update_pending(InstallPolicy::NextLaunch, DownloadMode::Paused)?;
        self.install_action = None;
        Ok(())
    }

    fn set(&mut self, phase: Phase) {
        self.phase = phase;
        self.phase_at = Instant::now();
    }

    /// 让「已是最新」「检查失败」这类一次性提示自己消失。
    pub fn tick(&mut self) {
        if self.phase_at.elapsed() < Duration::from_secs(5) {
            return;
        }
        if matches!(self.phase, Phase::Latest | Phase::Error(_)) {
            self.set(Phase::Idle);
        }
    }

    pub fn is_busy(&self) -> bool {
        matches!(
            self.phase,
            Phase::Checking
                | Phase::Downloading { .. }
                | Phase::Retrying { .. }
                | Phase::Pausing { .. }
                | Phase::Installing { .. }
                | Phase::Restarting
        )
    }

    /// 新版本提示和已下载的安装包一直保留到用户处理，不被每日检查覆盖。
    pub fn can_auto_check(&self) -> bool {
        matches!(self.phase, Phase::Idle | Phase::Latest | Phase::Error(_))
    }

    pub fn can_check(&self) -> bool {
        if let Phase::Ready { launch_error, .. } = &self.phase {
            return self.install_action.is_none()
                && !self.is_install_deferred()
                && (launch_error.is_some()
                    || self
                        .pending_download
                        .as_ref()
                        .is_some_and(|pending| pending.install_policy == InstallPolicy::Manual));
        }
        self.can_auto_check()
            || matches!(
                self.phase,
                Phase::Available(_) | Phase::Paused { error: Some(_), .. }
            )
    }

    /// 起一个线程去拉清单。重复调用是安全的：正在查/正在下时直接忽略。
    pub fn check(&mut self) {
        self.check_source(manifest_url());
    }

    fn check_source(&mut self, source: Option<String>) {
        if !self.can_check() {
            return;
        }
        self.ready_before_check =
            matches!(self.phase, Phase::Ready { .. }).then(|| self.phase.clone());
        self.download_error = None;
        let Some(url) = source else {
            self.check_failed("未配置更新源 / Update source not configured".into());
            return;
        };
        self.set(Phase::Checking);
        let complete = self.terminal_sender();
        if let Err(e) = std::thread::Builder::new()
            .name("update-check".into())
            .spawn(move || {
                let ev = match fetch_manifest(&url)
                    .and_then(|m| available_update(m, CURRENT, PLATFORM))
                {
                    Ok(available) => Event::Checked(available),
                    Err(e) => Event::Failed(e),
                };
                complete(ev);
            })
        {
            self.check_failed(format!("无法启动更新检查：{e}"));
        }
    }

    fn check_failed(&mut self, error: String) {
        self.check_result = Some(Err(error.clone()));
        if let Some(ready) = self.ready_before_check.take() {
            self.set(ready);
        } else if let Some(pending) = &self.pending_download {
            let version = pending.release.version.clone();
            let pct = pending
                .release
                .asset
                .as_ref()
                .and_then(|asset| download::saved_progress(asset, &version, &self.root))
                .unwrap_or(0.0);
            self.set(Phase::Paused {
                version,
                pct,
                error: Some(error),
            });
        } else {
            self.set(Phase::Error(error));
        }
    }

    fn download(&mut self, avail: &Available, start_new_round: bool) {
        if self.is_busy() {
            return;
        }
        let Some(asset) = avail.asset.clone() else {
            return;
        };
        let root = self.root.clone();
        let pending = PendingDownload {
            release: avail.clone(),
            platform: PLATFORM.into(),
            install_policy: InstallPolicy::Ask,
            download_mode: DownloadMode::Automatic,
        };
        let saved = pending.save(&root);
        let version = avail.version.clone();
        self.pending_download = Some(pending);
        self.install_action = None;
        self.download_error = None;
        self.verifying_cache = false;
        let pct = download::saved_progress(&asset, &version, &root).unwrap_or(0.0);
        if let Err(error) = saved {
            self.download_failed(version, pct, error);
            return;
        }
        self.start_download_task(version.clone(), pct, move |tx, stopped| {
            download::run(&asset, &version, tx, &root, stopped, start_new_round)
        });
    }

    fn start_download_task(
        &mut self,
        version: String,
        pct: f32,
        work: impl FnOnce(&Sender<Event>, &Receiver<()>) -> Result<Option<PathBuf>, String>
            + Send
            + 'static,
    ) {
        let (stop, stopped) = channel();
        self.download_stop = Some(stop);
        self.set(Phase::Downloading {
            version: version.clone(),
            pct,
        });
        let tx = self.tx.clone();
        let complete = self.terminal_sender();
        let completed_version = version.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("update-download".into())
            .spawn(move || {
                complete(match work(&tx, &stopped) {
                    Ok(Some(path)) => Event::Downloaded {
                        version: completed_version,
                        path,
                    },
                    Ok(None) => Event::DownloadPaused,
                    Err(error) => Event::DownloadFailed(error),
                });
            })
        {
            self.download_stop = None;
            self.verifying_cache = false;
            self.download_failed(version, pct, format!("无法启动下载：{e}"));
        }
    }

    pub fn pause_download(&mut self) {
        if let Phase::Downloading { version, pct } | Phase::Retrying { version, pct, .. } =
            self.phase.clone()
        {
            let error = if self.pending_download.is_some() {
                self.update_pending(InstallPolicy::Ask, DownloadMode::Paused)
                    .err()
            } else {
                None
            };
            if let Some(error) = &error {
                log::error!(target: "tramoing::update", "无法保存暂停状态：{error}");
            }
            if let Some(stop) = &self.download_stop {
                let _ = stop.send(());
            }
            self.set(Phase::Pausing {
                version,
                pct,
                error,
            });
        }
    }

    pub fn resume_download(&mut self) {
        if matches!(self.phase, Phase::Paused { .. }) {
            if let Some(pending) = self.pending_download.clone() {
                self.download(&pending.release, true);
            }
        }
    }

    pub fn restore_download(&mut self) {
        self.restore_download_from(self.root.clone());
    }

    fn restore_download_from(&mut self, root: PathBuf) {
        if !matches!(self.phase, Phase::Idle) || self.pending_download.is_some() {
            return;
        }
        self.root = root.clone();
        let Ok(file) = std::fs::File::open(root.join("pending.json")) else {
            return;
        };
        let Ok(mut pending) = serde_json::from_reader::<_, PendingDownload>(file.take(1024 * 1024))
        else {
            return;
        };
        if pending.platform.is_empty() {
            pending.platform = PLATFORM.into();
            pending.install_policy = InstallPolicy::Ask;
            pending.download_mode = DownloadMode::Paused;
        } else if pending.platform != PLATFORM {
            return;
        }
        let available = &pending.release;
        let Ok(version) = parse_version(&available.version) else {
            return;
        };
        if !version
            .cmp_precedence(&parse_version(CURRENT).expect("valid package version"))
            .is_gt()
        {
            let _ = std::fs::remove_file(root.join("pending.json"));
            return;
        }
        let Some(asset) = available.asset.clone() else {
            return;
        };
        if http_url(&asset.url).is_err() {
            return;
        }
        let Some(pct) = download::saved_progress(&asset, &available.version, &root) else {
            return;
        };
        let version = available.version.clone();
        let error = pending.download_mode.restored_error();
        self.set(Phase::Paused {
            version: version.clone(),
            pct,
            error,
        });
        self.pending_download = Some(pending);
        if download::installer_path(&version, &root).is_ok_and(|path| path.is_file()) {
            // Hashing a large installer must not block startup or the UI. Reuse the
            // download lifecycle to prevent checks/resumes from racing this worker.
            self.verifying_cache = true;
            self.start_download_task(version.clone(), pct, move |_, stopped| {
                download::restore(&asset, &version, &root, stopped)
            });
        } else {
            self.after_incomplete_cache();
        }
    }

    fn after_incomplete_cache(&mut self) {
        let Some(pending) = &self.pending_download else {
            return;
        };
        if pending.install_policy == InstallPolicy::Ask
            && pending.download_mode == DownloadMode::Automatic
        {
            let release = pending.release.clone();
            self.download(&release, false);
        } else if pending.install_policy == InstallPolicy::NextLaunch {
            if let Err(error) = self.update_pending(InstallPolicy::Manual, DownloadMode::Paused) {
                log::warn!(target: "tramoing::update", "{error}");
                if let Phase::Paused { error: shown, .. } = &mut self.phase {
                    *shown = Some(error);
                }
            }
        }
    }

    fn download_failed(&mut self, version: String, pct: f32, mut error: String) {
        if let Err(save_error) = self.update_pending(InstallPolicy::Ask, DownloadMode::Exhausted) {
            error = format!("{error}；{save_error}");
        }
        log::warn!(target: "tramoing::update", "下载失败：{error}");
        self.download_error = Some(error.clone());
        self.set(Phase::Paused {
            version,
            pct,
            error: Some(error),
        });
    }

    fn forget_pending(&mut self) {
        self.install_action = None;
        self.download_error = None;
        if self.pending_download.take().is_some() {
            if let Err(error) = std::fs::remove_file(self.root.join("pending.json")) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    log::warn!(target: "tramoing::update", "无法移除过期的更新任务：{error}");
                }
            }
        }
    }

    fn verified(&mut self, version: String, path: PathBuf) {
        self.download_stop = None;
        self.set(Phase::Ready {
            version: version.clone(),
            path,
            launch_error: None,
        });
        match self
            .pending_download
            .as_ref()
            .map(|pending| pending.install_policy)
        {
            Some(InstallPolicy::NextLaunch) => {
                match self.update_pending(InstallPolicy::Manual, DownloadMode::Paused) {
                    Ok(()) => self.install_action = Some(InstallAction::InstallOnLaunch),
                    Err(error) => self.install_error(error),
                }
            }
            Some(InstallPolicy::Ask) => {
                self.install_action = Some(InstallAction::Prompt { version })
            }
            _ => {}
        }
    }

    fn install_error(&mut self, error: String) {
        log::error!(target: "tramoing::update", "启动更新失败：{error}");
        self.install_action = None;
        if let Phase::Ready { launch_error, .. } = &mut self.phase {
            *launch_error = Some(error);
        }
    }

    fn prepare_install(&mut self) -> bool {
        if !matches!(self.phase, Phase::Ready { .. }) {
            return false;
        }
        self.install_action = None;
        if let Err(error) = self.update_pending(InstallPolicy::Manual, DownloadMode::Paused) {
            self.install_error(error);
            return false;
        }
        true
    }

    fn download_progress(&self) -> Option<(String, f32)> {
        match &self.phase {
            Phase::Downloading { version, pct }
            | Phase::Retrying { version, pct, .. }
            | Phase::Pausing { version, pct, .. } => Some((version.clone(), *pct)),
            _ => None,
        }
    }

    /// 只有确认启动成功，界面才能退出。失败时保留已校验的包并允许重试。
    pub fn install(&mut self) -> bool {
        #[cfg(target_os = "macos")]
        {
            if !self.prepare_install() {
                return false;
            }
            let Phase::Ready { version, path, .. } = self.phase.clone() else {
                return false;
            };
            self.set(Phase::Installing {
                version: version.clone(),
                path: path.clone(),
            });
            let complete = self.terminal_sender();
            if let Err(error) = std::thread::Builder::new()
                .name("update-install".into())
                .spawn(move || {
                    let result = tm_macos::installation::start_update(&path, &version);
                    complete(Event::InstallPrepared(result));
                })
            {
                let _ = self.tx.send(Event::InstallPrepared(Err(format!(
                    "无法启动更新：{error}"
                ))));
            }
            false
        }
        #[cfg(not(target_os = "macos"))]
        self.install_with(run_installer)
    }

    #[cfg(any(not(target_os = "macos"), test))]
    fn install_with(&mut self, launch: impl FnOnce(&Path) -> Result<(), String>) -> bool {
        if !self.prepare_install() {
            return false;
        }
        let Phase::Ready { path, .. } = &self.phase else {
            return false;
        };
        match launch(path) {
            Ok(()) => {
                self.set(Phase::Restarting);
                true
            }
            Err(error) => {
                self.install_error(error);
                false
            }
        }
    }

    /// 把后台事件吸收进状态。返回 `true` 表示状态变了，界面需要重画。
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(ev) = self.rx.try_recv() {
            changed = true;
            match ev {
                Event::Checked(Some(a)) => {
                    self.ready_before_check = None;
                    log_line(&format!(
                        "发现新版本 {}（当前 {CURRENT}，发布于 {}）",
                        a.version, a.pub_date
                    ));
                    self.check_result = Some(Ok(Some(a.version.clone())));
                    self.set(Phase::Available(a.clone()));
                    if a.asset.is_some() {
                        self.download(&a, true);
                    } else {
                        self.forget_pending();
                    }
                }
                Event::Checked(None) => {
                    self.ready_before_check = None;
                    log_line("已是最新版本");
                    self.check_result = Some(Ok(None));
                    self.forget_pending();
                    self.set(Phase::Latest);
                }
                Event::Failed(e) => {
                    // 失败必须落盘：release 是 GUI 子系统，没有 stderr，
                    // 线上「怎么从来不提示」只能靠这个文件查。
                    log::error!(target: "tramoing::update", "检查更新失败：{e}");
                    self.check_failed(e);
                }
                Event::Progress(pct) => {
                    if let Phase::Pausing { pct: current, .. } = &mut self.phase {
                        *current = pct;
                    } else if let Some((version, _)) = self.download_progress() {
                        self.set(Phase::Downloading { version, pct });
                    }
                }
                Event::DownloadRetrying { delay } => {
                    if !matches!(self.phase, Phase::Pausing { .. }) {
                        if let Some((version, pct)) = self.download_progress() {
                            self.set(Phase::Retrying {
                                version,
                                pct,
                                retry_at: Instant::now() + delay,
                            });
                        }
                    }
                }
                Event::DownloadPaused => {
                    self.download_stop = None;
                    let verifying_cache = std::mem::take(&mut self.verifying_cache);
                    let user_paused = matches!(self.phase, Phase::Pausing { .. });
                    if let Some((version, pct)) = self.download_progress() {
                        let error = if let Phase::Pausing { error, .. } = &self.phase {
                            error.clone()
                        } else {
                            self.pending_download
                                .as_ref()
                                .and_then(|pending| pending.download_mode.restored_error())
                        };
                        self.set(Phase::Paused {
                            version,
                            pct,
                            error,
                        });
                        if verifying_cache && !user_paused {
                            self.after_incomplete_cache();
                        }
                    }
                }
                Event::Downloaded { version, path } => {
                    if !matches!(self.download_progress(), Some((ref expected, _)) if *expected == version)
                    {
                        continue;
                    }
                    // Keep the task until its version is installed, including when
                    // the user quits after download but before confirming installation.
                    log_line(&format!("已下载 {}：{}", version, path.display()));
                    self.verifying_cache = false;
                    if let Phase::Pausing {
                        error: Some(error), ..
                    } = &self.phase
                    {
                        self.download_stop = None;
                        self.set(Phase::Paused {
                            version,
                            pct: 1.0,
                            error: Some(error.clone()),
                        });
                    } else {
                        self.verified(version, path);
                    }
                }
                Event::DownloadFailed(e) => {
                    self.download_stop = None;
                    self.verifying_cache = false;
                    if let Phase::Pausing {
                        version,
                        pct,
                        error,
                    } = self.phase.clone()
                    {
                        self.set(Phase::Paused {
                            version,
                            pct,
                            error,
                        });
                    } else if let Some((version, pct)) = self.download_progress() {
                        self.download_failed(version, pct, e);
                    } else {
                        self.set(Phase::Error(e));
                    }
                }
                Event::InstallPrepared(result) => {
                    if let Phase::Installing { version, path } = self.phase.clone() {
                        match result {
                            Ok(()) => self.set(Phase::Restarting),
                            Err(error) => {
                                log::error!(target: "tramoing::update", "准备更新失败：{error}");
                                self.set(Phase::Ready {
                                    version,
                                    path,
                                    launch_error: Some(error),
                                });
                            }
                        }
                    }
                }
            }
        }
        changed
    }
}

fn parse_version(version: &str) -> Result<semver::Version, String> {
    let version = version.trim();
    let version = version.strip_prefix(['v', 'V']).unwrap_or(version);
    semver::Version::parse(version).map_err(|e| format!("版本号无效：{e}"))
}

fn available_update(
    m: Manifest,
    current: &str,
    platform: &str,
) -> Result<Option<Available>, String> {
    let remote = parse_version(&m.version)?;
    let current = parse_version(current)?;
    // 构建元数据不改变 SemVer 优先级，1.0.0+build2 不是 1.0.0 的新版本。
    if !remote.cmp_precedence(&current).is_gt() {
        return Ok(None);
    }
    Ok(Some(Available {
        version: remote.to_string(),
        notes: m.notes,
        pub_date: m.pub_date,
        asset: m
            .platforms
            .get(platform)
            .filter(|asset| download::valid_sha256(&asset.sha256) && http_url(&asset.url).is_ok())
            .cloned(),
        homepage: if http_url(&m.homepage).is_ok() {
            m.homepage
        } else {
            RELEASES_URL.to_owned()
        },
    }))
}

fn http_url(raw: &str) -> Result<url::Url, String> {
    let url = url::Url::parse(raw).map_err(|e| format!("网址无效：{e}"))?;
    if !matches!(url.scheme(), "https" | "http") || url.host_str().is_none() {
        return Err("网址必须使用 http 或 https".into());
    }
    Ok(url)
}

fn agent_config() -> ureq::config::ConfigBuilder<ureq::typestate::AgentScope> {
    // native-tls + 系统证书库：企业环境里自签的根证书也能认，
    // 不用把根证书列表打进二进制。
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::NativeTls)
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    ureq::Agent::config_builder()
        .user_agent(concat!("Tramoing/", env!("CARGO_PKG_VERSION")))
        .tls_config(tls)
}

fn fetch_manifest(source: &str) -> Result<Manifest, String> {
    let url = http_url(source)?;
    let client: ureq::Agent = agent_config()
        .timeout_global(Some(Duration::from_secs(8)))
        .build()
        .into();
    let text = client
        .get(url.as_str())
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(404) => {
                "更新清单尚未发布，请稍后重试 / Update manifest not published yet".to_owned()
            }
            _ => format!("请求清单失败：{e}"),
        })?
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("读取清单失败：{e}"))?;
    // Windows 上拿记事本 / PowerShell 5.1 的 `Out-File -Encoding utf8` 改清单，
    // 开头会带一个 BOM。serde_json 不认，会直接报「expected value at line 1」，
    // 而且这个错误发生在用户机器上，很难查。这里顺手剥掉。
    let text = text.trim_start_matches('\u{feff}');
    serde_json::from_str(text).map_err(|e| format!("清单格式不对：{e}"))
}

#[cfg(any(windows, test))]
pub(crate) fn sha256_file(path: &Path) -> std::io::Result<String> {
    sha256_reader(std::fs::File::open(path)?)
}

fn sha256_reader(mut reader: impl Read) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// 用系统默认浏览器打开一个地址。清单没给本平台的包时用它兜底——
/// 至少让用户能自己下，而不是点了没反应。
pub fn open_url(url: &str) -> Result<(), String> {
    let url = http_url(url)?;
    #[cfg(windows)]
    {
        // 不走 `cmd /c start`：那个会先弹一个控制台窗口。
        // url.dll 的 FileProtocolHandler 是资源管理器双击链接走的同一条路。
        std::process::Command::new("rundll32")
            .arg("url.dll,FileProtocolHandler")
            .arg(url.as_str())
            .spawn()
            .map_err(|e| format!("打开浏览器失败：{e}"))?;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        open_on_macos(std::ffi::OsStr::new(url.as_str()))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = url;
        Err("当前平台不支持打开浏览器".to_owned())
    }
}

/// Windows 使用只有进度页的升级模式；成功启动后由调用方正常退出，
/// 安装器等待本进程结束再覆盖文件，完成后以普通权限重新启动。
/// UAC 取消或启动失败时返回错误，旧程序继续运行。
#[cfg(not(target_os = "macos"))]
fn run_installer(path: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        let executable = std::env::current_exe().map_err(|e| format!("读取程序路径失败：{e}"))?;
        let parameters = windows_update_parameters(&executable, std::process::id())?;
        tm_windows::launch::elevated(path, &parameters)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = path;
        Err("当前平台不支持自动安装".to_owned())
    }
}

#[cfg(windows)]
fn windows_update_parameters(executable: &Path, pid: u32) -> Result<std::ffi::OsString, String> {
    let directory = executable
        .parent()
        .filter(|path| path.is_absolute())
        .ok_or_else(|| "无法确定当前程序的安装目录".to_owned())?;
    // NSIS 的 /D= 必须是最后一个参数，目录即使含空格也不能加引号。
    // 用 OsString 原样交给 ShellExecute，避免经 shell 解析或损坏非 Unicode 路径。
    let mut parameters = std::ffi::OsString::from(format!("/UPDATE /APP_PID={pid} /D="));
    parameters.push(directory);
    Ok(parameters)
}

#[cfg(target_os = "macos")]
fn open_on_macos(target: &std::ffi::OsStr) -> Result<(), String> {
    let status = std::process::Command::new("/usr/bin/open")
        .arg("--")
        .arg(target)
        .status()
        .map_err(|e| format!("打开失败：{e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("系统无法打开该地址或文件：{status}"))
    }
}

/// 保留更新生命周期信息；日志由统一记录器限量保存。
pub fn log_line(msg: &str) {
    log::info!(target: "tramoing::update", "{msg}");
}

#[cfg(test)]
mod tests {
    use super::download::{download_agent, download_to, download_to_with_agent};
    use super::*;
    use std::io::Write;

    fn test_pending() -> PendingDownload {
        PendingDownload {
            release: Available {
                version: "999.0.0".into(),
                notes: String::new(),
                pub_date: String::new(),
                homepage: RELEASES_URL.into(),
                asset: Some(Asset {
                    url: "http://127.0.0.1:0/package".into(),
                    sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
                        .into(),
                    size: 3,
                }),
            },
            platform: PLATFORM.into(),
            install_policy: InstallPolicy::Ask,
            download_mode: DownloadMode::Automatic,
        }
    }

    fn settle(updater: &mut Updater) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while updater.is_busy() {
            updater.poll();
            assert!(Instant::now() < deadline, "updater worker did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn later_only_installs_once_on_a_new_launch_after_reverification() {
        let root = tempfile::tempdir().unwrap();
        let pending = test_pending();
        pending.save(root.path()).unwrap();
        let path = download::installer_path(&pending.release.version, root.path()).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"abc").unwrap();

        let restore = || {
            let mut updater = Updater::new();
            updater.restore_download_from(root.path().to_owned());
            settle(&mut updater);
            updater
        };
        let mut first = restore();
        assert!(matches!(
            first.take_install_action(),
            Some(InstallAction::Prompt { .. })
        ));
        assert!(!first.can_check());
        first.defer_install().unwrap();
        assert!(first.is_install_deferred());
        first.restore_download();
        assert!(first.take_install_action().is_none());
        assert!(!first.can_check());
        drop(first);

        let mut next = restore();
        assert_eq!(
            next.take_install_action(),
            Some(InstallAction::InstallOnLaunch)
        );
        assert_eq!(next.take_install_action(), None);
        assert!(!next.is_install_deferred());
        assert!(!next.install_with(|installer| {
            assert_eq!(installer, path);
            Err("cancelled".into())
        }));
        let ready_with_error = next.phase.clone();
        assert!(next.can_check());
        next.check_source(Some("file:///not-an-update-source".into()));
        assert_eq!(next.phase, Phase::Checking);
        settle(&mut next);
        assert_eq!(next.phase, ready_with_error);
        assert!(matches!(next.take_check_result(), Some(Err(_))));
        assert_eq!(next.take_install_action(), None);
        drop(next);

        let mut after_cancel = restore();
        assert!(matches!(after_cancel.phase, Phase::Ready { .. }));
        assert_eq!(after_cancel.take_install_action(), None);
        assert!(after_cancel.can_check());
        assert!(after_cancel.install_with(|_| Ok(())));
        assert_eq!(after_cancel.phase, Phase::Restarting);
    }

    #[test]
    fn deferred_installation_never_runs_for_a_damaged_missing_stale_or_other_platform_package() {
        for (bytes, version, platform) in [
            (Some("bad"), "999.0.0", PLATFORM),
            (None, "999.0.0", PLATFORM),
            (Some("abc"), CURRENT, PLATFORM),
            (Some("abc"), "999.0.0", "different-platform"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let mut pending = test_pending();
            pending.release.version = version.to_owned();
            pending.platform = platform.to_owned();
            pending.install_policy = InstallPolicy::NextLaunch;
            pending.download_mode = DownloadMode::Paused;
            pending.save(root.path()).unwrap();
            if let Some(body) = bytes {
                let path = download::installer_path(version, root.path()).unwrap();
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, body).unwrap();
            }
            let mut updater = Updater::new();
            updater.restore_download_from(root.path().to_owned());
            settle(&mut updater);
            assert!(matches!(updater.phase, Phase::Idle | Phase::Paused { .. }));
            assert_eq!(updater.take_install_action(), None);
            assert!(!updater.is_install_deferred());
        }
    }

    #[test]
    fn restarted_automatic_download_resumes_bytes_even_if_the_destination_is_corrupt() {
        for corrupt_destination in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (url, server) = serve_download(|mut stream| {
                stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Length: 2\r\nContent-Range: bytes 1-2/3\r\nConnection: close\r\n\r\nbc").unwrap();
            });
            let mut pending = test_pending();
            let asset = pending.release.asset.as_mut().unwrap();
            asset.url = url;
            let hash = asset.sha256.clone();
            pending.save(root.path()).unwrap();
            let dest = download::installer_path(&pending.release.version, root.path()).unwrap();
            let dir = dest.parent().unwrap();
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join(format!("{PLATFORM}-{hash}.part")), b"a").unwrap();
            if corrupt_destination {
                std::fs::write(&dest, b"bad").unwrap();
            }
            let mut updater = Updater::new();
            updater.restore_download_from(root.path().to_owned());
            settle(&mut updater);
            server.join().unwrap();
            assert!(matches!(updater.phase, Phase::Ready { .. }));
            assert_eq!(std::fs::read(dest).unwrap(), b"abc");
            assert!(matches!(
                updater.take_install_action(),
                Some(InstallAction::Prompt { .. })
            ));
        }
    }

    #[test]
    fn startup_obeys_the_existing_retry_budget_until_the_user_restarts_it() {
        let root = tempfile::tempdir().unwrap();
        let (url, server) = serve_download(|mut stream| {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc")
                .unwrap();
        });
        let mut pending = test_pending();
        let asset = pending.release.asset.as_mut().unwrap();
        asset.url = url;
        let version_dir = root.path().join(&pending.release.version);
        std::fs::create_dir(&version_dir).unwrap();
        std::fs::write(
            version_dir.join(format!("{PLATFORM}-{}.attempts", asset.sha256)),
            (download::MAX_RETRIES + 1).to_string(),
        )
        .unwrap();
        pending.save(root.path()).unwrap();

        let mut updater = Updater::new();
        updater.restore_download_from(root.path().to_owned());
        settle(&mut updater);
        assert!(matches!(
            updater.phase,
            Phase::Paused { error: Some(_), .. }
        ));
        assert!(updater.take_download_error().is_some());
        updater.resume_download();
        settle(&mut updater);
        server.join().unwrap();
        assert!(matches!(updater.phase, Phase::Ready { .. }));
    }

    #[test]
    fn explicit_pause_and_retry_exhaustion_survive_restart_without_network_or_repeated_prompts() {
        let root = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut pending = test_pending();
        pending.release.asset.as_mut().unwrap().url =
            format!("http://{}/setup", listener.local_addr().unwrap());
        pending.save(root.path()).unwrap();
        let mut first = Updater::new();
        first.root = root.path().to_owned();
        let version = pending.release.version.clone();
        first.pending_download = Some(pending);
        first.set(Phase::Downloading {
            version: version.clone(),
            pct: 0.5,
        });
        first.pause_download();
        drop(first);

        let mut next = Updater::new();
        next.restore_download_from(root.path().to_owned());
        assert!(matches!(next.phase, Phase::Paused { error: None, .. }));
        assert!(!next.can_check());
        next.set(Phase::Downloading { version, pct: 0.5 });
        next.tx
            .send(Event::DownloadFailed("8 retries exhausted".into()))
            .unwrap();
        next.poll();
        assert_eq!(
            next.take_download_error().as_deref(),
            Some("8 retries exhausted")
        );
        assert_eq!(next.take_download_error(), None);
        assert_eq!(next.release_page(), RELEASES_URL);
        drop(next);

        let mut restored = Updater::new();
        restored.restore_download_from(root.path().to_owned());
        assert!(matches!(
            restored.phase,
            Phase::Paused { error: Some(_), .. }
        ));
        assert_eq!(restored.take_download_error(), None);
        assert!(restored.can_check());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn a_new_check_replaces_an_exhausted_task_only_after_receiving_a_manifest() {
        let root = tempfile::tempdir().unwrap();
        let mut pending = test_pending();
        pending.download_mode = DownloadMode::Exhausted;
        pending.release.homepage = "https://example.com/old".into();
        pending.save(root.path()).unwrap();
        let mut updater = Updater::new();
        updater.restore_download_from(root.path().to_owned());
        updater.check_source(None);
        assert!(matches!(updater.take_check_result(), Some(Err(_))));
        assert!(matches!(
            updater.phase,
            Phase::Paused { error: Some(_), .. }
        ));
        assert_eq!(updater.release_page(), "https://example.com/old");

        let mut latest = manifest("1000.0.0");
        latest.homepage = "https://example.com/new".into();
        let mut no_digest = pending.release.asset.clone().unwrap();
        no_digest.sha256.clear();
        latest.platforms.insert(PLATFORM.into(), no_digest);
        let newer = available_update(latest, CURRENT, PLATFORM).unwrap();
        updater.tx.send(Event::Checked(newer)).unwrap();
        updater.poll();
        assert!(matches!(&updater.phase, Phase::Available(item) if item.asset.is_none()));
        assert_eq!(
            updater.take_check_result(),
            Some(Ok(Some("1000.0.0".into())))
        );
        assert_eq!(updater.release_page(), "https://example.com/new");
        assert!(!root.path().join("pending.json").exists());

        pending.save(root.path()).unwrap();
        let mut updater = Updater::new();
        updater.restore_download_from(root.path().to_owned());
        updater.tx.send(Event::Checked(None)).unwrap();
        updater.poll();
        assert_eq!(updater.phase, Phase::Latest);
        assert_eq!(updater.take_check_result(), Some(Ok(None)));
        assert_eq!(updater.release_page(), RELEASES_URL);
        assert!(!root.path().join("pending.json").exists());
    }

    #[test]
    fn an_explicit_pause_wins_over_a_racing_transfer_failure() {
        let root = tempfile::tempdir().unwrap();
        let mut updater = Updater::new();
        updater.root = root.path().to_owned();
        let pending = test_pending();
        pending.save(root.path()).unwrap();
        updater.set(Phase::Downloading {
            version: pending.release.version.clone(),
            pct: 0.5,
        });
        updater.pending_download = Some(pending);
        updater
            .tx
            .send(Event::DownloadFailed("stopped".into()))
            .unwrap();
        updater.pause_download();
        updater.poll();
        assert!(matches!(updater.phase, Phase::Paused { error: None, .. }));
        assert!(updater.take_download_error().is_none());
    }

    #[test]
    fn failure_to_save_revokes_the_old_automatic_task_without_deleting_the_package() {
        for mode in [DownloadMode::Paused, DownloadMode::Exhausted] {
            let root = tempfile::tempdir().unwrap();
            let mut pending = test_pending();
            pending.install_policy = InstallPolicy::NextLaunch;
            pending.save(root.path()).unwrap();
            let installer =
                download::installer_path(&pending.release.version, root.path()).unwrap();
            std::fs::create_dir_all(installer.parent().unwrap()).unwrap();
            std::fs::write(&installer, b"abc").unwrap();
            let mut updater = Updater::new();
            updater.root = root.path().to_owned();
            updater.pending_download = Some(pending);
            updater.install_action = Some(InstallAction::InstallOnLaunch);
            let error = updater
                .update_pending_with(InstallPolicy::Ask, mode, |_, _| Err("no space left".into()));
            assert_eq!(error.unwrap_err(), "no space left");
            assert!(!updater.is_install_deferred());
            assert_eq!(updater.take_install_action(), None);
            assert!(!root.path().join("pending.json").exists());
            let mut next = Updater::new();
            next.restore_download_from(root.path().to_owned());
            assert_eq!(next.phase, Phase::Idle);
            assert_eq!(next.take_install_action(), None);
            assert_eq!(std::fs::read(installer).unwrap(), b"abc");
        }
    }

    #[test]
    fn an_unwritable_pause_surfaces_the_error_even_when_verification_already_completed() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("pending.json")).unwrap();
        let mut pending = test_pending();
        pending.install_policy = InstallPolicy::NextLaunch;
        let version = pending.release.version.clone();
        let mut updater = Updater::new();
        updater.root = root.path().to_owned();
        updater.pending_download = Some(pending);
        updater.set(Phase::Downloading {
            version: version.clone(),
            pct: 0.0,
        });
        updater.pause_download();
        updater
            .tx
            .send(Event::Downloaded {
                version,
                path: root.path().join("verified"),
            })
            .unwrap();
        updater.poll();
        assert!(
            matches!(&updater.phase, Phase::Paused { error: Some(message), .. } if message.contains("无法清除旧的更新任务"))
        );
        assert!(!updater.is_install_deferred());
        assert_eq!(updater.take_install_action(), None);
    }

    #[test]
    fn preparing_an_update_only_requests_exit_after_the_worker_is_ready() {
        let mut updater = Updater::new();
        let path = PathBuf::from("verified.dmg");
        updater.set(Phase::Installing {
            version: "1.2.3".into(),
            path: path.clone(),
        });
        assert!(updater.is_busy());
        assert!(!updater.can_auto_check());
        updater
            .tx
            .send(Event::InstallPrepared(Err("copy failed".into())))
            .unwrap();
        updater.poll();
        assert_eq!(
            updater.phase,
            Phase::Ready {
                version: "1.2.3".into(),
                path: path.clone(),
                launch_error: Some("copy failed".into()),
            }
        );
        updater.set(Phase::Installing {
            version: "1.2.3".into(),
            path,
        });
        updater.tx.send(Event::InstallPrepared(Ok(()))).unwrap();
        updater.poll();
        assert_eq!(updater.phase, Phase::Restarting);
    }

    #[test]
    fn cancelled_or_failed_install_keeps_the_verified_package_for_retry() {
        let path = PathBuf::from("verified-setup.exe");
        let mut updater = Updater::new();
        let root = tempfile::tempdir().unwrap();
        updater.root = root.path().to_owned();
        updater.pending_download = Some(test_pending());
        updater.set(Phase::Ready {
            version: "1.2.3".into(),
            path: path.clone(),
            launch_error: None,
        });
        for error in ["cancelled authorization", "failed to start installer"] {
            assert!(!updater.install_with(|_| Err(error.into())));
            assert!(!updater.can_auto_check());
            updater.tick();
            assert_eq!(
                updater.phase,
                Phase::Ready {
                    version: "1.2.3".into(),
                    path: path.clone(),
                    launch_error: Some(error.into()),
                }
            );
        }
        assert!(updater.install_with(|retry| {
            assert_eq!(retry, path);
            Ok(())
        }));
        assert_eq!(updater.phase, Phase::Restarting);
        assert!(!updater.install_with(|_| panic!("installer launched twice")));
    }

    #[cfg(windows)]
    #[test]
    fn update_targets_the_running_installation_and_waits_for_its_process() {
        for directory in [r"C:\Program Files\Tramoing", r"D:\应用 & Tools\自定义目录"] {
            let executable = Path::new(directory).join("tramoing.exe");
            assert_eq!(
                windows_update_parameters(&executable, 1234).unwrap(),
                std::ffi::OsString::from(format!("/UPDATE /APP_PID=1234 /D={directory}"))
            );
        }
        assert!(windows_update_parameters(Path::new("tramoing.exe"), 1234).is_err());
    }

    #[test]
    fn official_source_is_default_and_overrides_can_explicitly_disable_it() {
        assert_eq!(
            resolve_manifest_url(None, None).as_deref(),
            Some(DEFAULT_MANIFEST_URL)
        );
        assert_eq!(
            resolve_manifest_url(None, Some(" https://example.com/build.json ")).as_deref(),
            Some("https://example.com/build.json")
        );
        assert_eq!(
            resolve_manifest_url(
                Some("http://localhost/runtime.json"),
                Some("https://example.com/build.json")
            )
            .as_deref(),
            Some("http://localhost/runtime.json")
        );
        assert_eq!(resolve_manifest_url(Some("  "), None), None);
        assert_eq!(resolve_manifest_url(None, Some("")), None);
        assert_eq!(
            resolve_manifest_url(Some(""), Some(DEFAULT_MANIFEST_URL)),
            None
        );
    }

    #[test]
    fn daily_checks_do_not_discard_a_pending_update_or_download() {
        let mut updater = Updater::new();
        assert!(updater.can_auto_check());
        for phase in [
            Phase::Checking,
            Phase::Downloading {
                version: "1.0.0".into(),
                pct: 0.5,
            },
            Phase::Ready {
                version: "1.0.0".into(),
                path: PathBuf::from("setup.exe"),
                launch_error: None,
            },
            Phase::Available(Available {
                version: "1.0.0".into(),
                notes: String::new(),
                pub_date: String::new(),
                asset: None,
                homepage: String::new(),
            }),
        ] {
            updater.set(phase);
            assert!(!updater.can_auto_check());
        }
        for phase in [Phase::Latest, Phase::Error("offline".into()), Phase::Idle] {
            updater.set(phase);
            assert!(updater.can_auto_check());
        }
    }

    #[test]
    fn github_manifest_selects_the_matching_installer_or_release_page() {
        let parse = || {
            serde_json::from_value::<Manifest>(serde_json::json!({
            "version": "1.2.0", "notes": "发布说明", "pub_date": "2026-11-21T12:00:00Z",
            "homepage": "https://github.com/kkltgood/tramoing/releases/tag/v1.2.0",
            "platforms": {
                "windows-x86_64": { "url": "https://example.com/windows-x64-setup.exe", "sha256": "a".repeat(64), "size": 12 },
                "windows-aarch64": { "url": "https://example.com/windows-arm64-setup.exe", "sha256": "b".repeat(64), "size": 23 },
                "macos-aarch64": { "url": "https://example.com/macos-arm64.dmg", "sha256": "c".repeat(64), "size": 34 },
                "macos-x86_64": { "url": "https://example.com/macos-x64.dmg", "sha256": "d".repeat(64), "size": 45 }
            }
        })).unwrap()
        };
        for (platform, filename, size) in [
            ("windows-x86_64", "windows-x64-setup.exe", 12),
            ("windows-aarch64", "windows-arm64-setup.exe", 23),
            ("macos-aarch64", "macos-arm64.dmg", 34),
            ("macos-x86_64", "macos-x64.dmg", 45),
            ("unsupported", "", 0),
        ] {
            let available = available_update(parse(), "1.1.0", platform)
                .unwrap()
                .unwrap();
            assert_eq!(available.version, "1.2.0");
            assert_eq!(available.notes, "发布说明");
            assert!(available.homepage.ends_with("/releases/tag/v1.2.0"));
            if platform == "unsupported" {
                assert!(available.asset.is_none());
            } else {
                let asset = available.asset.unwrap();
                assert_eq!(asset.size, size);
                assert_eq!(asset.url, format!("https://example.com/{filename}"));
            }
        }
        assert!(available_update(parse(), "1.2.0", PLATFORM)
            .unwrap()
            .is_none());
        assert!(available_update(parse(), "2.0.0", PLATFORM)
            .unwrap()
            .is_none());
    }

    #[test]
    fn update_checks_require_a_configured_source_and_accept_a_local_manifest() {
        let mut updater = Updater::new();
        updater.check_source(None);
        assert!(matches!(updater.phase, Phase::Error(_)));
        assert!(!updater.is_busy());
        assert!(!updater.poll());

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "update did not request the configured source"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() <= 8192, "request headers too large");
            }
            assert!(request.starts_with(b"GET /latest.json HTTP/1.1\r\n"));
            let body = format!("\u{feff}{{\"version\":\"{CURRENT}\"}}");
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });

        updater.check_source(Some(format!("http://{address}/latest.json")));
        let deadline = Instant::now() + Duration::from_secs(5);
        while updater.is_busy() {
            updater.poll();
            assert!(Instant::now() < deadline, "update check timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
        server.join().unwrap();
        assert_eq!(updater.phase, Phase::Latest);
    }

    #[test]
    fn background_check_automatically_downloads_and_wakes_the_ui_after_verification() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let manifest = serde_json::json!({
            "version": "999.0.0",
            "platforms": { PLATFORM: {
                "url": format!("http://{address}/setup.exe"),
                "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
                "size": 3
            }}
        })
        .to_string();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            for (target, body) in [("/latest.json", manifest.as_str()), ("/setup.exe", "abc")] {
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "update request timed out");
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(e) => panic!("accept: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                    assert!(request.len() < 8192);
                }
                assert!(request.starts_with(format!("GET {target} HTTP/1.1\r\n").as_bytes()));
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        let root = tempfile::tempdir().unwrap();
        let (wake, wakes) = channel();
        let mut updater = Updater::with_waker(move || wake.send(()).unwrap());
        updater.root = root.path().to_owned();
        updater.check_source(Some(format!("http://{address}/latest.json")));
        assert_eq!(updater.phase, Phase::Checking);
        wakes.recv_timeout(Duration::from_secs(5)).unwrap();
        updater.poll();
        assert_eq!(
            updater.take_check_result(),
            Some(Ok(Some("999.0.0".into())))
        );
        assert_eq!(updater.take_check_result(), None);
        wakes.recv_timeout(Duration::from_secs(5)).unwrap();
        updater.poll();
        let Phase::Ready { version, path, .. } = updater.phase.clone() else {
            panic!("expected verified download, got {:?}", updater.phase);
        };
        assert_eq!(version, "999.0.0");
        assert_eq!(path.parent().unwrap(), root.path().join("999.0.0"));
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
        assert!(!updater.can_auto_check());
        assert_eq!(
            updater.take_install_action(),
            Some(InstallAction::Prompt { version })
        );
        assert_eq!(updater.take_install_action(), None);
        assert!(wakes.try_recv().is_err());
        server.join().unwrap();
    }

    fn manifest(version: &str) -> Manifest {
        serde_json::from_value(serde_json::json!({"version": version})).unwrap()
    }

    #[test]
    fn invalid_versions_are_errors_and_build_metadata_is_not_an_update() {
        assert!(available_update(manifest("broken"), "1.0.0", PLATFORM).is_err());
        assert!(
            available_update(manifest("1.0.0+build2"), "1.0.0", PLATFORM)
                .unwrap()
                .is_none()
        );
        let update = available_update(manifest(" v1.10.0 "), "1.9.0", PLATFORM)
            .unwrap()
            .unwrap();
        assert_eq!(update.version, "1.10.0");
    }

    #[test]
    fn only_web_urls_can_be_opened_from_a_manifest() {
        for input in [
            "file:///tmp/app",
            "javascript:alert(1)",
            "-a Calculator",
            "x-apple.systempreferences:foo",
        ] {
            assert!(http_url(input).is_err(), "{input}");
        }
        assert!(http_url("https://example.com/releases").is_ok());
        assert!(http_url("http://127.0.0.1:8000/latest.json").is_ok());
    }

    #[test]
    fn downloads_use_safe_names_and_preserve_partial_files_after_disconnects() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            for response in [
                "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc",
                "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc",
                "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nbad",
                "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\na",
            ] {
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "download did not contact local server"
                            );
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(e) => panic!("accept: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    assert!(request.len() < 16 * 1024, "oversized request headers");
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let asset = Asset {
            url: format!("http://{address}/download?name=setup.exe&signature=a/b#fragment"),
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
            size: 3,
        };
        let (tx, _rx) = channel();
        let path = download_to(&asset, "1.2.3", &tx, dir.path()).unwrap();
        assert_eq!(path.parent().unwrap(), dir.path().join("1.2.3"));
        assert_eq!(
            path.extension().unwrap(),
            if cfg!(windows) { "exe" } else { "dmg" }
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
        // 同一版本重试时可以提交新文件，Windows 也不应因目标已存在而失败。
        assert_eq!(download_to(&asset, "1.2.3", &tx, dir.path()).unwrap(), path);
        assert!(download_to(&asset, "1.2.3", &tx, dir.path())
            .unwrap_err()
            .contains("sha256"));
        let unknown_size = Asset {
            size: 0,
            ..asset.clone()
        };
        assert!(download_to(&unknown_size, "1.2.3", &tx, dir.path()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            3
        );
        assert!(download_to(&asset, "../escape", &tx, dir.path()).is_err());
        let bad_hash = Asset {
            sha256: "z".repeat(64),
            ..asset
        };
        assert!(download_to(&bad_hash, "2.0.0", &tx, dir.path()).is_err());
        assert!(!dir.path().join("2.0.0").exists());
        server.join().unwrap();
    }

    fn serve_download(
        respond: impl FnOnce(std::net::TcpStream) + Send + 'static,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "download request timed out");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_nodelay(true).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 8192, "oversized request headers");
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            respond(stream);
        });
        (format!("http://{address}/setup.exe"), server)
    }

    #[test]
    fn slow_downloads_keep_running_while_bytes_arrive() {
        use sha2::{Digest, Sha256};
        let body = b"abcdefghijklmnop";
        let timeout = Duration::from_millis(500);
        // Cover both release manifests with a size and sources with no known length.
        for known_size in [true, false] {
            let (url, server) = serve_download(move |mut stream| {
                let length = if known_size {
                    format!("Content-Length: {}\r\n", body.len())
                } else {
                    String::new()
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\n{length}Connection: close\r\n\r\n"
                )
                .unwrap();
                // Total transfer time exceeds the timeout, but each network read progresses.
                for byte in body {
                    std::thread::sleep(Duration::from_millis(75));
                    if stream.write_all(&[*byte]).is_err() {
                        break;
                    }
                }
            });
            let asset = Asset {
                url,
                sha256: format!("{:x}", Sha256::digest(body)),
                size: if known_size { body.len() as u64 } else { 0 },
            };
            let dir = tempfile::tempdir().unwrap();
            let (tx, _rx) = channel();
            let started = Instant::now();
            let result =
                download_to_with_agent(&asset, "1.2.3", &tx, dir.path(), &download_agent(timeout));
            let elapsed = started.elapsed();
            server.join().unwrap();
            let path = result.expect("a progressing download must not hit a total timeout");
            assert!(elapsed > timeout);
            assert_eq!(std::fs::read(&path).unwrap(), body);
            assert_eq!(
                std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
                1
            );
        }
    }

    #[test]
    fn stalled_downloads_time_out_and_preserve_the_verified_installer() {
        let (release, wait) = channel();
        let (url, server) = serve_download(move |mut stream| {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\na")
                .unwrap();
            // Hold the connection open until the client finishes; EOF must not cause the error.
            let _ = wait.recv_timeout(Duration::from_secs(5));
        });
        let asset = Asset {
            url,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
            size: 3,
        };
        let dir = tempfile::tempdir().unwrap();
        let version_dir = dir.path().join("1.2.3");
        std::fs::create_dir_all(&version_dir).unwrap();
        let dest = version_dir.join(if cfg!(windows) {
            "Tramoing-setup.exe"
        } else {
            "Tramoing.dmg"
        });
        std::fs::write(&dest, b"abc").unwrap();
        let (tx, _rx) = channel();
        let result = download_to_with_agent(
            &asset,
            "1.2.3",
            &tx,
            dir.path(),
            &download_agent(Duration::from_millis(250)),
        );
        let _ = release.send(());
        server.join().unwrap();
        let error = result.unwrap_err();
        assert!(error.contains("timeout"), "{error}");
        assert!(error.contains("timeout: per read"), "{error}");
        assert!(error.contains("1/3 字节"), "{error}");
        assert_eq!(std::fs::read(&dest).unwrap(), b"abc");
        assert_eq!(std::fs::read_dir(&version_dir).unwrap().count(), 3);
        let partial = std::fs::read_dir(&version_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "part"))
            .unwrap();
        assert_eq!(std::fs::read(partial).unwrap(), b"a");
    }

    #[test]
    fn downloads_time_out_when_the_server_never_sends_headers() {
        let (release, wait) = channel();
        let (url, server) = serve_download(move |_stream| {
            let _ = wait.recv_timeout(Duration::from_secs(5));
        });
        let asset = Asset {
            url,
            sha256: "a".repeat(64),
            size: 3,
        };
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = channel();
        let result = download_to_with_agent(
            &asset,
            "1.2.3",
            &tx,
            dir.path(),
            &download_agent(Duration::from_millis(250)),
        );
        let _ = release.send(());
        server.join().unwrap();
        let error = result.unwrap_err();
        assert!(error.contains("timeout"), "{error}");
        assert_eq!(
            std::fs::read_dir(dir.path().join("1.2.3")).unwrap().count(),
            2
        );
    }

    #[test]
    fn pausing_waits_for_the_worker_and_resume_does_not_overlap_downloads() {
        let mut updater = Updater::new();
        let (stop, stopped) = channel();
        updater.download_stop = Some(stop);
        updater.set(Phase::Downloading {
            version: "999.0.0".into(),
            pct: 0.4,
        });
        updater
            .tx
            .send(Event::DownloadRetrying {
                delay: Duration::from_secs(30),
            })
            .unwrap();
        updater.poll();
        assert!(matches!(updater.phase, Phase::Retrying { pct, .. } if pct == 0.4));
        assert!(updater.is_busy());
        assert!(!updater.can_auto_check());
        updater.pause_download();
        stopped.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(updater.phase, Phase::Pausing { .. }));
        assert!(updater.is_busy());
        updater.resume_download();
        assert!(matches!(updater.phase, Phase::Pausing { .. }));
        // A late progress/retry event must not undo the pause request.
        updater.tx.send(Event::Progress(0.5)).unwrap();
        updater
            .tx
            .send(Event::DownloadRetrying {
                delay: Duration::from_secs(30),
            })
            .unwrap();
        updater.tx.send(Event::DownloadPaused).unwrap();
        updater.poll();
        assert!(matches!(updater.phase, Phase::Paused { pct, error: None, .. } if pct == 0.5));
        assert!(!updater.is_busy());
        assert!(!updater.can_auto_check());
        updater.phase_at = Instant::now() - Duration::from_secs(10);
        updater.tick();
        assert!(matches!(updater.phase, Phase::Paused { .. }));
    }

    #[test]
    fn restarting_keeps_a_completed_update_until_its_version_is_installed() {
        let root = tempfile::tempdir().unwrap();
        let (url, server) = serve_download(|mut stream| {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc")
                .unwrap();
        });
        let available = Available {
            version: "999.0.0".into(),
            notes: String::new(),
            pub_date: String::new(),
            homepage: String::new(),
            asset: Some(Asset {
                url,
                size: 3,
                sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
            }),
        };
        tm_store::files::atomic_write(
            &root.path().join("pending.json"),
            &serde_json::to_vec(&available).unwrap(),
        )
        .unwrap();
        let mut updater = Updater::new();
        updater.restore_download_from(root.path().to_owned());
        assert!(matches!(updater.phase, Phase::Paused { .. }));
        updater.resume_download();
        let deadline = Instant::now() + Duration::from_secs(5);
        while updater.is_busy() {
            updater.poll();
            assert!(Instant::now() < deadline, "resume did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
        server.join().unwrap();
        assert!(matches!(updater.phase, Phase::Ready { .. }));
        assert!(root.path().join("pending.json").exists());
        let Phase::Ready { path, .. } = &updater.phase else {
            unreachable!();
        };
        let completed_path = path.clone();
        drop(updater);

        // The server has exited. Restarting must verify the saved installer locally,
        // retaining the user's confirmation step instead of downloading it again.
        let mut updater = Updater::new();
        updater.restore_download_from(root.path().to_owned());
        let deadline = Instant::now() + Duration::from_secs(5);
        while updater.is_busy() {
            updater.poll();
            assert!(Instant::now() < deadline, "restore did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            updater.phase,
            Phase::Ready {
                version: available.version.clone(),
                path: completed_path,
                launch_error: None,
            }
        );
        assert!(root.path().join("pending.json").exists());

        let already_installed = Available {
            version: CURRENT.into(),
            ..available
        };
        tm_store::files::atomic_write(
            &root.path().join("pending.json"),
            &serde_json::to_vec(&already_installed).unwrap(),
        )
        .unwrap();
        let mut updater = Updater::new();
        updater.restore_download_from(root.path().to_owned());
        assert_eq!(updater.phase, Phase::Idle);
        assert!(!root.path().join("pending.json").exists());
    }

    #[test]
    fn restarting_recovers_an_installer_completed_before_the_ui_polled() {
        let root = tempfile::tempdir().unwrap();
        let (url, server) = serve_download(|mut stream| {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc")
                .unwrap();
        });
        let available = Available {
            version: "999.0.0".into(),
            notes: String::new(),
            pub_date: String::new(),
            homepage: String::new(),
            asset: Some(Asset {
                url,
                size: 3,
                sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
            }),
        };
        let mut updater = Updater::new();
        updater.root = root.path().to_owned();
        updater.download(&available, true);
        let completed_path = loop {
            match updater.rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                Event::Downloaded { path, .. } => break path,
                Event::Progress(_) => {}
                _ => panic!("unexpected download event"),
            }
        };
        assert!(matches!(updater.phase, Phase::Downloading { .. }));
        drop(updater); // The UI never applied the completion event.
        server.join().unwrap();

        let mut restored = Updater::new();
        restored.restore_download_from(root.path().to_owned());
        let deadline = Instant::now() + Duration::from_secs(5);
        while restored.is_busy() {
            restored.poll();
            assert!(Instant::now() < deadline, "restore did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            restored.phase,
            Phase::Ready {
                version: available.version,
                path: completed_path,
                launch_error: None,
            }
        );
        assert!(root.path().join("pending.json").exists());
    }

    #[test]
    fn restore_verifies_cached_installers_and_leaves_incomplete_tasks_paused_offline() {
        let hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        for (body, size, expected_hash, ready) in [
            (Some("abc"), 3, hash, true),
            (Some("abc"), 0, hash, true),
            (Some("bad"), 3, hash, false), // Corrupt bytes of the same length.
            (Some("abc"), 4, hash, false), // A correct hash cannot waive a size mismatch.
            (
                Some("abc"),
                3,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                false,
            ),
            (None, 3, hash, false),
        ] {
            let root = tempfile::tempdir().unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let available = Available {
                version: "999.0.0".into(),
                notes: String::new(),
                pub_date: String::new(),
                homepage: String::new(),
                asset: Some(Asset {
                    url: format!("http://{}/setup.exe", listener.local_addr().unwrap()),
                    size,
                    sha256: expected_hash.into(),
                }),
            };
            tm_store::files::atomic_write(
                &root.path().join("pending.json"),
                &serde_json::to_vec(&available).unwrap(),
            )
            .unwrap();
            let dir = root.path().join(&available.version);
            std::fs::create_dir(&dir).unwrap();
            let path = download::installer_path(&available.version, root.path()).unwrap();
            if let Some(body) = body {
                std::fs::write(&path, body).unwrap();
            }
            let partial = dir.join(format!("{PLATFORM}-{expected_hash}.part"));
            std::fs::write(&partial, b"a").unwrap();
            let mut restored = Updater::new();
            restored.restore_download_from(root.path().to_owned());
            let deadline = Instant::now() + Duration::from_secs(5);
            while restored.is_busy() {
                restored.poll();
                assert!(Instant::now() < deadline, "restore did not finish");
                std::thread::sleep(Duration::from_millis(5));
            }
            if ready {
                assert_eq!(
                    restored.phase,
                    Phase::Ready {
                        version: available.version,
                        path,
                        launch_error: None,
                    }
                );
            } else {
                assert_eq!(
                    restored.phase,
                    Phase::Paused {
                        version: available.version,
                        pct: 1.0 / size as f32,
                        error: None,
                    }
                );
            }
            assert_eq!(std::fs::read(partial).unwrap(), b"a");
            assert!(root.path().join("pending.json").exists());
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock,
                "restoring must not request the server"
            );
        }
    }

    #[test]
    fn manifest_parses_and_missing_fields_default() {
        let m: Manifest = serde_json::from_str(
            r#"{"version":"1.2.0","platforms":{"windows-x86_64":{"url":"https://x/a.exe","sha256":"aa","size":10}}}"#,
        )
        .unwrap();
        assert_eq!(m.version, "1.2.0");
        assert_eq!(m.notes, "");
        assert_eq!(m.platforms["windows-x86_64"].size, 10);
    }

    #[test]
    fn version_compare_respects_semver_not_lexicographic() {
        let newer = |a: &str, b: &str| {
            semver::Version::parse(a).unwrap() > semver::Version::parse(b).unwrap()
        };
        assert!(newer("0.10.0", "0.9.0"), "字典序会判反");
        assert!(!newer("1.0.0", "1.0.0"));
        assert!(!newer("1.0.0-beta", "1.0.0"), "预发布版不能盖过正式版");
    }

    #[test]
    fn sha256_matches_known_vector() {
        let dir = std::env::temp_dir().join("tramoing-update-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("abc.txt");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            sha256_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_file(&p);
    }
}
