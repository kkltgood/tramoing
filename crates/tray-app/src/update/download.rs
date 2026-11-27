//! Resumable installer downloads. Only a fully verified file becomes installable.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{
    agent_config, http_url, log_line, parse_version, sha256_reader, Asset, Event, PLATFORM,
};

const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) const MAX_RETRIES: u32 = 8;

#[cfg(any(windows, test))]
pub(crate) fn component_path(version: &str, root: &Path) -> Result<PathBuf, String> {
    Ok(root
        .join(parse_version(version)?.to_string())
        .join("payload.exe"))
}

/// Components have a separate cache root and a finite fallback policy. The same
/// pinned digest applies to every source, including the static backup.
#[cfg(any(windows, test))]
pub(crate) fn component(
    asset: &Asset,
    version: &str,
    tx: &Sender<Event>,
    root: &Path,
    stop: &Receiver<()>,
    fallbacks: &[String],
) -> Result<Option<PathBuf>, String> {
    for source in std::iter::once(&asset.url).chain(fallbacks) {
        let url = http_url(source)?;
        if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
            return Err("组件下载必须使用无凭据的 HTTPS 地址".into());
        }
    }
    let client: ureq::Agent = agent_config()
        .https_only(true)
        .timeout_resolve(Some(Duration::from_secs(15)))
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_send_request(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(IDLE_TIMEOUT))
        .timeout_per_read(Some(IDLE_TIMEOUT))
        .timeout_per_write(Some(IDLE_TIMEOUT))
        .build()
        .into();
    component_with_agent(
        asset,
        version,
        tx,
        root,
        stop,
        fallbacks,
        ComponentPolicy {
            client: &client,
            delay: Duration::from_secs(2),
        },
    )
}

#[cfg(any(windows, test))]
struct ComponentPolicy<'a> {
    client: &'a ureq::Agent,
    delay: Duration,
}

#[cfg(any(windows, test))]
fn component_with_agent(
    asset: &Asset,
    version: &str,
    tx: &Sender<Event>,
    root: &Path,
    stop: &Receiver<()>,
    fallbacks: &[String],
    policy: ComponentPolicy<'_>,
) -> Result<Option<PathBuf>, String> {
    if asset.size == 0 {
        return Err("组件清单缺少文件大小".into());
    }
    if stopped(Some(stop)) {
        return Ok(None);
    }
    // Validate hash/version and acquire the partial-file lock before inspecting
    // a completed cache entry. A cache hit never needs a network request.
    let mut download = PartialDownload::open(asset, version, root)?;
    download.dest = component_path(version, root)?;
    match File::open(&download.dest) {
        Ok(file) => {
            let size = file.metadata().map_err(|e| e.to_string())?.len();
            if size == asset.size
                && sha256_reader(file).map_err(|e| e.to_string())? == download.hash
            {
                if stopped(Some(stop)) {
                    return Ok(None);
                }
                let _ = tx.send(Event::Progress(1.0));
                return Ok(Some(download.dest));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("无法校验已有组件下载：{error}")),
    }
    let mut last_error = "组件下载失败".to_owned();
    for source in std::iter::once(&asset.url).chain(fallbacks) {
        let url = http_url(source)?;
        let mut no_progress = 0;
        let mut failures = 0;
        download.has_inherited_prefix = download.len()? > 0;
        loop {
            if stopped(Some(stop)) {
                download.sync()?;
                return Ok(None);
            }
            // ETags identify a representation at one source. A mirror may use
            // another ETag for the exact same pinned bytes, so never send the
            // previous host's If-Range value when changing source or resuming.
            if download.state.source.as_deref() != Some(url.as_str()) {
                download.state.etag = None;
                download.state.source = Some(url.to_string());
                download.save_state()?;
            }
            let before = download.len()?;
            download.progress(tx)?;
            match download.attempt(policy.client, url.as_str(), tx, Some(stop)) {
                Ok(true) => return download.finish().map(Some),
                Ok(false) => {
                    if download.len()? > before {
                        no_progress = 0;
                        continue;
                    } else {
                        no_progress += 1;
                        failures += 1;
                        last_error = "组件下载没有取得进展".into();
                    }
                }
                Err(AttemptError::Paused) => {
                    download.sync()?;
                    return Ok(None);
                }
                Err(AttemptError::ChecksumMismatchWithInheritedPrefix) => {
                    // The saved prefix may be corrupt even when this source is good.
                    // attempt() discarded it. Give this source one try from zero.
                    continue;
                }
                Err(error) => {
                    download.sync()?;
                    let retryable = matches!(error, AttemptError::Retry(_));
                    last_error = error.message();
                    log::warn!(target: "tramoing::components", "{}: {last_error}", url.host_str().unwrap_or_default());
                    failures += 1;
                    if download.len()? > before {
                        no_progress = 0;
                    } else {
                        no_progress += 1;
                    }
                    if !retryable {
                        // Hash mismatches have already reset the bad partial;
                        // a fallback gets a fresh chance with the same digest.
                        break;
                    }
                }
            }
            // Switch after two stalled requests; even a repeatedly dropping
            // source that adds a few bytes cannot consume unbounded retries.
            if no_progress >= 2 || failures >= 6 {
                break;
            }
            let _ = tx.send(Event::DownloadRetrying {
                delay: policy.delay,
            });
            match stop.recv_timeout(policy.delay) {
                Err(RecvTimeoutError::Timeout) => {}
                Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                    download.sync()?;
                    return Ok(None);
                }
            }
        }
    }
    Err(format!("组件下载来源均不可用，请重试：{last_error}"))
}

pub(super) fn installer_path(version: &str, root: &Path) -> Result<PathBuf, String> {
    Ok(root
        .join(parse_version(version)?.to_string())
        .join(if cfg!(windows) {
            "Tramoing-setup.exe"
        } else {
            "Tramoing.dmg"
        }))
}

/// Recheck a completed download on a worker, without making any network requests.
/// A missing, replaced or damaged package leaves the saved task available to resume.
pub(super) fn restore(
    asset: &Asset,
    version: &str,
    root: &Path,
    stop: &Receiver<()>,
) -> Result<Option<PathBuf>, String> {
    if stopped(Some(stop)) {
        return Ok(None);
    }
    let path = installer_path(version, root)?;
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("打开已下载的安装包失败：{error}")),
    };
    let metadata = file
        .metadata()
        .map_err(|e| format!("读取安装包信息失败：{e}"))?;
    if !metadata.is_file() || (asset.size > 0 && metadata.len() != asset.size) {
        return Ok(None);
    }
    let hash = sha256_reader(file).map_err(|e| format!("校验已下载的安装包失败：{e}"))?;
    if hash != asset.sha256.trim().to_ascii_lowercase() || stopped(Some(stop)) {
        return Ok(None);
    }
    Ok(Some(path))
}

pub(super) fn valid_sha256(value: &str) -> bool {
    let hash = value.trim();
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) fn saved_progress(asset: &Asset, version: &str, root: &Path) -> Option<f32> {
    if !valid_sha256(&asset.sha256) {
        return None;
    }
    let hash = asset.sha256.trim().to_ascii_lowercase();
    let path = root
        .join(parse_version(version).ok()?.to_string())
        .join(format!("{PLATFORM}-{hash}.part"));
    let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let total = if asset.size > 0 {
        Some(asset.size)
    } else {
        File::open(path.with_extension("json"))
            .ok()
            .and_then(|file| serde_json::from_reader::<_, PartialState>(file.take(8192)).ok())
            .and_then(|state| state.total)
    };
    Some(
        total
            .filter(|total| *total > 0)
            .map(|total| (len as f32 / total as f32).clamp(0.0, 1.0))
            .unwrap_or(0.0),
    )
}

pub(super) fn download_agent(idle_timeout: Duration) -> ureq::Agent {
    // A progressing transfer has no total deadline. Each stalled network operation
    // is bounded, including DNS, TLS/proxy setup and response headers.
    agent_config()
        .timeout_resolve(Some(Duration::from_secs(30)))
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_send_request(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(idle_timeout))
        .timeout_per_read(Some(idle_timeout))
        .timeout_per_write(Some(idle_timeout))
        .build()
        .into()
}

pub(super) fn run(
    asset: &Asset,
    version: &str,
    tx: &Sender<Event>,
    root: &Path,
    stop: &Receiver<()>,
    new_round: bool,
) -> Result<Option<PathBuf>, String> {
    run_with_agent(
        asset,
        version,
        tx,
        root,
        stop,
        TransferPolicy {
            client: &download_agent(IDLE_TIMEOUT),
            new_round,
            delay: retry_delay,
        },
    )
}

fn retry_delay(failures: u32) -> Duration {
    Duration::from_secs((2u64 << failures.min(4)).min(30))
}

fn stopped(stop: Option<&Receiver<()>>) -> bool {
    stop.is_some_and(|stop| !matches!(stop.try_recv(), Err(TryRecvError::Empty)))
}

struct TransferPolicy<'a, Delay> {
    client: &'a ureq::Agent,
    new_round: bool,
    delay: Delay,
}

fn run_with_agent(
    asset: &Asset,
    version: &str,
    tx: &Sender<Event>,
    root: &Path,
    stop: &Receiver<()>,
    policy: TransferPolicy<'_, impl Fn(u32) -> Duration>,
) -> Result<Option<PathBuf>, String> {
    let canonical_version = parse_version(version)?.to_string();
    let url = http_url(&asset.url)?;
    if !valid_sha256(&asset.sha256) {
        return Err("清单没有提供有效的 sha256，已跳过下载".into());
    }
    log_line(&format!(
        "下载 {version}：清单大小 {} 字节，来源 {}",
        asset.size,
        url.host_str().unwrap_or_default(),
    ));
    let attempts_path = root.join(canonical_version).join(format!(
        "{PLATFORM}-{}.attempts",
        asset.sha256.trim().to_ascii_lowercase()
    ));
    let mut attempts = if policy.new_round {
        0
    } else {
        match File::open(&attempts_path) {
            Ok(file) => serde_json::from_reader::<_, u32>(file.take(32))
                .map_err(|error| format!("无法读取下载重试次数：{error}"))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(format!("无法打开下载重试记录：{error}")),
        }
    };
    loop {
        if stopped(Some(stop)) {
            return Ok(None);
        }
        if attempts > MAX_RETRIES {
            return Err(format!(
                "自动重试已达 {MAX_RETRIES} 次上限 / Automatic retry limit reached"
            ));
        }
        attempts += 1;
        tm_store::files::atomic_write(&attempts_path, attempts.to_string().as_bytes())
            .map_err(|error| format!("无法保存下载重试次数：{error}"))?;
        let message = match transfer(asset, version, tx, root, policy.client, stop) {
            Ok(Some(path)) => {
                let _ = std::fs::remove_file(&attempts_path);
                return Ok(Some(path));
            }
            Ok(None) => return Ok(None),
            Err(message) if attempts > MAX_RETRIES => return Err(message),
            Err(message) => message,
        };
        let wait = (policy.delay)(attempts - 1);
        log::warn!(target: "tramoing::update", "{message}；{} 秒后自动重试（{attempts}/{MAX_RETRIES}）", wait.as_secs());
        let _ = tx.send(Event::DownloadRetrying { delay: wait });
        match stop.recv_timeout(wait) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return Ok(None),
        }
    }
}

fn transfer(
    asset: &Asset,
    version: &str,
    tx: &Sender<Event>,
    root: &Path,
    client: &ureq::Agent,
    stop: &Receiver<()>,
) -> Result<Option<PathBuf>, String> {
    let mut download = PartialDownload::open(asset, version, root)?;
    loop {
        if stopped(Some(stop)) {
            break;
        }
        let before = download.len()?;
        download.progress(tx)?;
        match download.attempt(client, &asset.url, tx, Some(stop)) {
            Ok(true) if !stopped(Some(stop)) => return download.finish().map(Some),
            Ok(true) | Err(AttemptError::Paused) => break,
            Ok(false) if download.len()? > before => continue,
            Ok(false) => return Err("续传范围已重置，将从头下载".into()),
            Err(error) => {
                download.sync()?;
                download.progress(tx)?;
                return Err(error.message());
            }
        }
    }
    if let Err(error) = download.sync() {
        log::warn!(target: "tramoing::update", "暂停下载时无法保存进度：{error}");
    }
    Ok(None)
}

#[derive(Debug)]
enum AttemptError {
    Retry(String),
    Failed(String),
    ChecksumMismatch,
    ChecksumMismatchWithInheritedPrefix,
    Paused,
}

impl AttemptError {
    fn message(self) -> String {
        match self {
            Self::Retry(message) | Self::Failed(message) => message,
            Self::ChecksumMismatch | Self::ChecksumMismatchWithInheritedPrefix => {
                "sha256 不匹配，已丢弃损坏的下载内容，请重试".into()
            }
            Self::Paused => "下载已暂停，进度已保存".into(),
        }
    }

    fn network(error: ureq::Error) -> Self {
        if matches!(
            error,
            ureq::Error::Timeout(_)
                | ureq::Error::Io(_)
                | ureq::Error::HostNotFound
                | ureq::Error::ConnectionFailed
                | ureq::Error::ConnectProxyFailed(_)
        ) {
            Self::Retry(error.to_string())
        } else {
            Self::Failed(error.to_string())
        }
    }
}

impl From<String> for AttemptError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

#[derive(Default, Deserialize, Serialize)]
struct PartialState {
    total: Option<u64>,
    etag: Option<String>,
    #[serde(default)]
    source: Option<String>,
}

struct PartialDownload {
    file: File,
    path: PathBuf,
    state_path: PathBuf,
    dest: PathBuf,
    state: PartialState,
    has_inherited_prefix: bool,
    size: u64,
    hash: String,
}

impl PartialDownload {
    fn open(asset: &Asset, version: &str, root: &Path) -> Result<Self, String> {
        if !valid_sha256(&asset.sha256) {
            return Err("清单没有提供有效的 sha256，已跳过下载".into());
        }
        let hash = asset.sha256.trim().to_ascii_lowercase();
        let version = parse_version(version)?.to_string();
        http_url(&asset.url)?;
        let dir = root.join(&version);
        std::fs::create_dir_all(&dir).map_err(|e| format!("建目录失败：{e}"))?;
        // Identity is the expected content, not a possibly expiring signed URL.
        // A replacement asset (even under the same version/URL) cannot reuse old bytes.
        let path = dir.join(format!("{PLATFORM}-{hash}.part"));
        let state_path = path.with_extension("json");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| format!("打开下载缓存失败：{e}"))?;
        file.try_lock()
            .map_err(|e| format!("下载缓存正在使用或无法锁定：{e}"))?;
        let state = File::open(&state_path)
            .ok()
            .and_then(|file| serde_json::from_reader::<_, PartialState>(file.take(8192)).ok())
            .unwrap_or_default();
        let mut download = Self {
            file,
            path,
            state_path,
            dest: installer_path(&version, root)?,
            state,
            has_inherited_prefix: false,
            size: asset.size,
            hash,
        };
        let len = download.len()?;
        if len == 0
            || download.total().is_some_and(|total| len > total)
            || (asset.size > 0
                && download
                    .state
                    .total
                    .is_some_and(|total| total != asset.size))
        {
            download.reset()?;
        }
        Ok(download)
    }

    fn len(&self) -> Result<u64, String> {
        self.file
            .metadata()
            .map(|m| m.len())
            .map_err(|e| format!("读取下载进度失败：{e}"))
    }

    fn total(&self) -> Option<u64> {
        if self.size > 0 {
            Some(self.size)
        } else {
            self.state.total
        }
    }

    fn sync(&self) -> Result<(), String> {
        self.file
            .sync_all()
            .map_err(|e| format!("保存下载进度失败：{e}"))
    }

    fn save_state(&self) -> Result<(), String> {
        let bytes = serde_json::to_vec(&self.state).map_err(|e| e.to_string())?;
        tm_store::files::atomic_write(&self.state_path, &bytes)
            .map_err(|e| format!("保存续传信息失败：{e}"))
    }

    fn reset(&mut self) -> Result<(), String> {
        // Clear metadata first. A process exit between these writes can lose an ETag,
        // but must never label old bytes with a new representation's metadata.
        self.state = PartialState::default();
        self.save_state()?;
        self.file
            .set_len(0)
            .map_err(|e| format!("重置下载缓存失败：{e}"))?;
        self.has_inherited_prefix = false;
        self.file
            .rewind()
            .map_err(|e| format!("重置下载位置失败：{e}"))?;
        self.sync()
    }

    fn discard_checksum_mismatch(&mut self) -> Result<AttemptError, String> {
        let mismatch = if self.has_inherited_prefix {
            AttemptError::ChecksumMismatchWithInheritedPrefix
        } else {
            AttemptError::ChecksumMismatch
        };
        self.reset()?;
        Ok(mismatch)
    }

    fn progress(&self, tx: &Sender<Event>) -> Result<(), String> {
        let pct = self
            .total()
            .filter(|total| *total > 0)
            .map(|total| {
                self.len()
                    .map(|len| (len as f32 / total as f32).clamp(0.0, 1.0))
            })
            .transpose()?
            .unwrap_or(0.0);
        let _ = tx.send(Event::Progress(pct));
        Ok(())
    }

    fn matches_hash(&mut self) -> Result<bool, String> {
        // Windows locks also reject reads through a second handle; hash through our own.
        self.file
            .rewind()
            .map_err(|e| format!("校验定位失败：{e}"))?;
        sha256_reader(&mut self.file)
            .map(|hash| hash == self.hash)
            .map_err(|e| format!("校验失败：{e}"))
    }

    /// Returns true only for a complete, verified installer; false requests another range.
    fn attempt(
        &mut self,
        client: &ureq::Agent,
        url: &str,
        tx: &Sender<Event>,
        stop: Option<&Receiver<()>>,
    ) -> Result<bool, AttemptError> {
        let mut offset = self.len()?;
        // A disconnect/exit can happen after the last byte but before verification.
        if offset > 0 && self.total() == Some(offset) {
            if self.matches_hash()? {
                return Ok(true);
            }
            self.reset()?;
            offset = 0;
        }
        let started = Instant::now();
        let mut request = client
            .get(url)
            .header("Accept-Encoding", "identity")
            .config()
            .http_status_as_error(false)
            .build();
        if offset > 0 {
            request = request.header("Range", format!("bytes={offset}-"));
            if let Some(etag) = self.state.etag.as_deref().filter(|etag| strong_etag(etag)) {
                request = request.header("If-Range", etag);
            }
        }
        let mut response = request.call().map_err(AttemptError::network)?;
        if stopped(stop) {
            return Err(AttemptError::Paused);
        }
        let status = response.status().as_u16();
        if matches!(status, 408 | 429 | 500 | 502 | 503 | 504) {
            return Err(AttemptError::Retry(format!(
                "下载服务暂时不可用（HTTP {status}）"
            )));
        }
        if status == 416 && offset > 0 {
            // Unknown-length sources can report completion using bytes */N.
            let total = header(&response, "content-range")
                .and_then(|v| v.strip_prefix("bytes */"))
                .and_then(|v| v.parse::<u64>().ok());
            if total == Some(offset) && (self.size == 0 || self.size == offset) {
                if self.matches_hash()? {
                    return Ok(true);
                }
                return Err(self.discard_checksum_mismatch()?);
            }
            // The range is no longer satisfiable. Retry once from zero; a 416 at zero is fatal.
            self.reset()?;
            self.progress(tx)?;
            return Ok(false);
        }
        if !matches!(status, 200 | 206) {
            return Err(AttemptError::Failed(format!(
                "请求安装包失败（HTTP {status}）"
            )));
        }
        if header(&response, "content-encoding")
            .is_some_and(|v| !v.eq_ignore_ascii_case("identity"))
        {
            return Err(AttemptError::Failed(
                "服务器返回压缩编码，无法安全续传安装包".into(),
            ));
        }
        let length = header(&response, "content-length")
            .map(|v| v.parse::<u64>())
            .transpose()
            .map_err(|_| AttemptError::Failed("安装包 Content-Length 无效".into()))?;
        let etag = header(&response, "etag")
            .filter(|v| strong_etag(v))
            .map(str::to_owned);
        let response_end;
        if status == 206 {
            let (start, end, total) = header(&response, "content-range")
                .and_then(parse_range)
                .ok_or_else(|| AttemptError::Failed("服务器续传范围无效".into()))?;
            if start != offset || length.is_some_and(|len| len != end - start + 1) {
                return Err(AttemptError::Failed(
                    "服务器续传范围无效，请重试或手动下载".into(),
                ));
            }
            if self.total().is_some_and(|expected| total != expected)
                || (offset > 0
                    && self.state.etag.is_some()
                    && etag.is_some()
                    && self.state.etag != etag)
            {
                // Discard stale bytes before offering retry so this cannot get stuck forever.
                self.reset()?;
                return Err(AttemptError::Failed(
                    "安装包标识发生变化，已重置下载，请重试或重新检查更新".into(),
                ));
            }
            response_end = Some(end + 1);
            self.state.total = Some(total);
        } else {
            if self.size > 0 && length.is_some_and(|length| length != self.size) {
                return Err(AttemptError::Failed("安装包大小与清单不符".into()));
            }
            // Range ignored / If-Range failed: this is a full file, never append it.
            if offset > 0 {
                self.reset()?;
                offset = 0;
            }
            self.state.total = length;
            response_end = self.total();
        }
        if status == 200 || etag.is_some() {
            self.state.etag = etag;
        }
        self.save_state()?;
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|e| format!("定位下载位置失败：{e}"))?;
        self.progress(tx)?;
        let mut reader = response.body_mut().as_reader();
        let mut buf = [0u8; 64 * 1024];
        let mut written = offset;
        let mut last_report = offset;
        loop {
            if stopped(stop) {
                return Err(AttemptError::Paused);
            }
            let n = reader.read(&mut buf).map_err(|e| {
                let total = self
                    .total()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "未知".into());
                let context = format!(
                    "读流出错（已接收 {written}/{total} 字节，本次用时 {} 秒）",
                    started.elapsed().as_secs()
                );
                match AttemptError::network(ureq::Error::from(e)) {
                    AttemptError::Retry(error) => {
                        AttemptError::Retry(format!("{context}：{error}"))
                    }
                    error => AttemptError::Failed(format!("{context}：{}", error.message())),
                }
            })?;
            if n == 0 {
                break;
            }
            let next = written
                .checked_add(n as u64)
                .ok_or_else(|| AttemptError::Failed("下载大小溢出".into()))?;
            if response_end.is_some_and(|end| next > end) {
                return Err(AttemptError::Failed("下载大小超过声明范围".into()));
            }
            self.file
                .write_all(&buf[..n])
                .map_err(|e| format!("写文件失败：{e}"))?;
            written = next;
            let step = self
                .total()
                .map(|total| (total / 100).max(1))
                .unwrap_or(64 * 1024);
            if written - last_report >= step {
                self.progress(tx)?;
                last_report = written;
            }
        }
        if response_end.is_some_and(|end| written != end) {
            return Err(AttemptError::Retry(format!(
                "下载连接中断（已接收 {written} 字节）"
            )));
        }
        if self.total().is_some_and(|total| written < total) {
            return Ok(false);
        }
        self.sync()?;
        if !self.matches_hash()? {
            // A connection-close body has no length: EOF may just be a dropped connection.
            if self.total().is_none() {
                return Err(AttemptError::Retry(format!(
                    "下载尚未完成（已保留 {written} 字节）"
                )));
            }
            return Err(self.discard_checksum_mismatch()?);
        }
        self.progress(tx)?;
        Ok(true)
    }

    fn finish(self) -> Result<PathBuf, String> {
        self.sync()?;
        // Keep atomic replacement and preserve the partial if committing fails.
        let path = tempfile::TempPath::try_from_path(&self.path).map_err(|e| e.to_string())?;
        let file = tempfile::NamedTempFile::from_parts(self.file, path);
        if let Err(error) = file.persist(&self.dest) {
            let message = format!("保存安装包失败：{}", error.error);
            let _ = error.file.keep();
            return Err(message);
        }
        let _ = std::fs::remove_file(&self.state_path);
        Ok(self.dest)
    }
}

fn header<'a>(response: &'a ureq::http::Response<ureq::Body>, name: &str) -> Option<&'a str> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

fn strong_etag(etag: &str) -> bool {
    etag.len() >= 2
        && etag.len() <= 1024
        && etag.starts_with('"')
        && etag.ends_with('"')
        && etag[1..etag.len() - 1]
            .bytes()
            .all(|b| b == 0x21 || (0x23..=0x7e).contains(&b))
}

fn parse_range(value: &str) -> Option<(u64, u64, u64)> {
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end, total) = (
        start.parse::<u64>().ok()?,
        end.parse::<u64>().ok()?,
        total.parse::<u64>().ok()?,
    );
    (start <= end && end < total).then_some((start, end, total))
}

#[cfg(test)]
pub(super) fn download_to(
    asset: &Asset,
    version: &str,
    tx: &Sender<Event>,
    root: &Path,
) -> Result<PathBuf, String> {
    download_to_with_agent(asset, version, tx, root, &download_agent(IDLE_TIMEOUT))
}

#[cfg(test)]
pub(super) fn download_to_with_agent(
    asset: &Asset,
    version: &str,
    tx: &Sender<Event>,
    root: &Path,
    client: &ureq::Agent,
) -> Result<PathBuf, String> {
    let mut download = PartialDownload::open(asset, version, root)?;
    loop {
        match download.attempt(client, &asset.url, tx, None) {
            Ok(true) => return download.finish(),
            Ok(false) => {}
            Err(error) => {
                download.sync()?;
                return Err(error.message());
            }
        }
    }
}

#[cfg(test)]
mod tests;
