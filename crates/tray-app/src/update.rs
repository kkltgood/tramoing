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

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use serde::Deserialize;

pub const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/kkltgood/tramoing/releases/latest/download/latest.json";

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

#[derive(Debug, Clone, Deserialize, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
pub struct Available {
    pub version: String,
    pub notes: String,
    /// 发布日期。界面上不显示（页脚放不下），但日志里带上，
    /// 排查「为什么这台机器还提示旧版本」时有用。
    pub pub_date: String,
    pub asset: Option<Asset>,
    pub homepage: String,
}

/// 后台线程发回界面的事件。
pub enum Event {
    /// `None` = 已是最新
    Checked(Option<Available>),
    Failed(String),
    Progress(f32),
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
        let (tx, rx) = channel();
        Self {
            tx,
            rx,
            phase: Phase::Idle,
            phase_at: Instant::now(),
        }
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
                | Phase::Installing { .. }
                | Phase::Restarting
        )
    }

    /// 新版本提示和已下载的安装包一直保留到用户处理，不被每日检查覆盖。
    pub fn can_auto_check(&self) -> bool {
        matches!(self.phase, Phase::Idle | Phase::Latest | Phase::Error(_))
    }

    /// 起一个线程去拉清单。重复调用是安全的：正在查/正在下时直接忽略。
    pub fn check(&mut self) {
        self.check_source(manifest_url());
    }

    fn check_source(&mut self, source: Option<String>) {
        if self.is_busy() {
            return;
        }
        let Some(url) = source else {
            self.set(Phase::Error(
                "未配置更新源 / Update source not configured".into(),
            ));
            return;
        };
        self.set(Phase::Checking);
        let tx = self.tx.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("update-check".into())
            .spawn(move || {
                let ev = match fetch_manifest(&url)
                    .and_then(|m| available_update(m, CURRENT, PLATFORM))
                {
                    Ok(available) => Event::Checked(available),
                    Err(e) => Event::Failed(e),
                };
                let _ = tx.send(ev);
            })
        {
            self.set(Phase::Error(format!("无法启动更新检查：{e}")));
        }
    }

    /// 开始下载安装包。`asset` 为空（清单没给本平台的包）时什么也不做。
    pub fn download(&mut self, avail: &Available) {
        self.download_into(avail, tm_store::data_dir().join("update"));
    }

    fn download_into(&mut self, avail: &Available, root: PathBuf) {
        if self.is_busy() {
            return;
        }
        let Some(asset) = avail.asset.clone() else {
            return;
        };
        let version = avail.version.clone();
        self.set(Phase::Downloading {
            version: version.clone(),
            pct: 0.0,
        });
        let tx = self.tx.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("update-download".into())
            .spawn(move || match download_to(&asset, &version, &tx, &root) {
                Ok(path) => {
                    let _ = tx.send(Event::Downloaded { version, path });
                }
                Err(e) => {
                    let _ = tx.send(Event::DownloadFailed(e));
                }
            })
        {
            self.set(Phase::Error(format!("无法启动下载：{e}")));
        }
    }

    /// 只有确认启动成功，界面才能退出。失败时保留已校验的包并允许重试。
    pub fn install(&mut self) -> bool {
        #[cfg(target_os = "macos")]
        {
            let Phase::Ready { version, path, .. } = self.phase.clone() else {
                return false;
            };
            self.set(Phase::Installing {
                version: version.clone(),
                path: path.clone(),
            });
            let tx = self.tx.clone();
            if let Err(error) = std::thread::Builder::new()
                .name("update-install".into())
                .spawn(move || {
                    let result = tm_macos::installation::start_update(&path, &version);
                    let _ = tx.send(Event::InstallPrepared(result));
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
        let Phase::Ready {
            path, launch_error, ..
        } = &mut self.phase
        else {
            return false;
        };
        match launch(path) {
            Ok(()) => {
                *launch_error = None;
                true
            }
            Err(error) => {
                log::error!(target: "tramoing::update", "启动更新失败：{error}");
                *launch_error = Some(error);
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
                    log_line(&format!(
                        "发现新版本 {}（当前 {CURRENT}，发布于 {}）",
                        a.version, a.pub_date
                    ));
                    self.set(Phase::Available(a));
                }
                Event::Checked(None) => {
                    log_line("已是最新版本");
                    self.set(Phase::Latest);
                }
                Event::Failed(e) => {
                    // 失败必须落盘：release 是 GUI 子系统，没有 stderr，
                    // 线上「怎么从来不提示」只能靠这个文件查。
                    log::error!(target: "tramoing::update", "检查更新失败：{e}");
                    self.set(Phase::Error(e));
                }
                Event::Progress(pct) => {
                    if let Phase::Downloading { version, .. } = &self.phase {
                        let version = version.clone();
                        self.set(Phase::Downloading { version, pct });
                    }
                }
                Event::Downloaded { version, path } => {
                    log_line(&format!("已下载 {}：{}", version, path.display()));
                    self.set(Phase::Ready {
                        version,
                        path,
                        launch_error: None,
                    });
                }
                Event::DownloadFailed(e) => {
                    log::error!(target: "tramoing::update", "下载失败：{e}");
                    self.set(Phase::Error(e));
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
        asset: m.platforms.get(platform).cloned(),
        homepage: m.homepage,
    }))
}

fn http_url(raw: &str) -> Result<url::Url, String> {
    let url = url::Url::parse(raw).map_err(|e| format!("网址无效：{e}"))?;
    if !matches!(url.scheme(), "https" | "http") || url.host_str().is_none() {
        return Err("网址必须使用 http 或 https".into());
    }
    Ok(url)
}

fn agent(timeout: Duration) -> ureq::Agent {
    // native-tls + 系统证书库：企业环境里自签的根证书也能认，
    // 不用把根证书列表打进二进制。
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::NativeTls)
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .user_agent(concat!("Tramoing/", env!("CARGO_PKG_VERSION")))
        .tls_config(tls)
        .build()
        .into()
}

fn fetch_manifest(source: &str) -> Result<Manifest, String> {
    let url = http_url(source)?;
    let text = agent(Duration::from_secs(8))
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

fn download_to(
    asset: &Asset,
    version: &str,
    tx: &Sender<Event>,
    root: &Path,
) -> Result<PathBuf, String> {
    let hash = asset.sha256.trim();
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("清单没有提供有效的 sha256，已跳过下载".to_owned());
    }
    let version = parse_version(version)?.to_string();
    let url = http_url(&asset.url)?;
    // 每个版本单独一个目录：既不会和上个版本的残留包撞名，
    // 清理时也可以整目录删掉，不用猜文件名规则。
    let dir = root.join(version);
    std::fs::create_dir_all(&dir).map_err(|e| format!("建目录失败：{e}"))?;
    // 文件名由本平台决定。签名网址的查询参数、重定向端点和反斜杠
    // 不能变成本地文件名，也不能改变安装包扩展名或逃出版本目录。
    let dest = dir.join(if cfg!(windows) {
        "Tramoing-setup.exe"
    } else {
        "Tramoing.dmg"
    });

    // 按安装包大小给慢速网络留出总下载时间，最多等待半小时。
    let timeout_secs = (asset.size / 102_400).clamp(120, 1800);
    let mut resp = agent(Duration::from_secs(timeout_secs))
        .get(url.as_str())
        .call()
        .map_err(|e| format!("下载失败：{e}"))?;

    let total = if asset.size > 0 {
        asset.size
    } else {
        resp.headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    };

    // 每次下载有独立临时文件，所有错误路径由 Drop 清理；成功校验后才提交。
    let mut file = tempfile::NamedTempFile::new_in(&dir).map_err(|e| format!("写文件失败：{e}"))?;
    let mut reader = resp.body_mut().as_reader();
    let mut buf = vec![0u8; 64 * 1024];
    let mut written: u64 = 0;
    let mut last_pct = -1.0f32;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("读流出错：{e}"))?;
        if n == 0 {
            break;
        }
        written += n as u64;
        if total > 0 && written > total {
            return Err(format!("下载大小超过清单声明（{written}/{total} 字节）"));
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("写文件失败：{e}"))?;
        if total > 0 {
            let pct = (written as f32 / total as f32).clamp(0.0, 1.0);
            // 每 1% 发一次就够了，否则 channel 每 64KB 被塞满，界面白重画。
            if pct - last_pct >= 0.01 {
                last_pct = pct;
                let _ = tx.send(Event::Progress(pct));
            }
        }
    }
    if total > 0 && written != total {
        return Err(format!("下载不完整（{written}/{total} 字节）"));
    }

    file.as_file()
        .sync_all()
        .map_err(|e| format!("保存下载失败：{e}"))?;
    let actual = sha256_file(file.path()).map_err(|e| format!("校验失败：{e}"))?;
    if !actual.eq_ignore_ascii_case(hash) {
        return Err(format!("sha256 不匹配（得到 {actual}）"));
    }

    file.persist(&dest).map_err(|e| format!("落盘失败：{e}"))?;
    Ok(dest)
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut f = std::fs::File::open(path)?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
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
    use super::*;

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
        assert!(matches!(
            updater.phase,
            Phase::Ready {
                launch_error: None,
                ..
            }
        ));
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
    fn background_check_and_download_reach_ready_only_after_verification() {
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
            for (target, body) in [
                ("/latest.json", manifest.as_str()),
                ("/setup.exe", "abc"),
                ("/setup.exe", "bad"),
            ] {
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
        let wait_for_result = |updater: &mut Updater| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while updater.is_busy() {
                updater.poll();
                assert!(Instant::now() < deadline, "background update timed out");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let root = tempfile::tempdir().unwrap();
        let mut updater = Updater::new();
        updater.check_source(Some(format!("http://{address}/latest.json")));
        assert_eq!(updater.phase, Phase::Checking);
        wait_for_result(&mut updater);
        let Phase::Available(available) = updater.phase.clone() else {
            panic!("expected update, got {:?}", updater.phase);
        };
        assert!(!updater.can_auto_check());
        updater.download_into(&available, root.path().to_owned());
        assert!(matches!(updater.phase, Phase::Downloading { .. }));
        wait_for_result(&mut updater);
        let Phase::Ready { version, path, .. } = updater.phase.clone() else {
            panic!("expected verified download, got {:?}", updater.phase);
        };
        assert_eq!(version, "999.0.0");
        assert_eq!(path.parent().unwrap(), root.path().join("999.0.0"));
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
        assert!(!updater.can_auto_check());

        // A corrupt retry must never offer installation or replace the verified file.
        updater.download_into(&available, root.path().to_owned());
        wait_for_result(&mut updater);
        assert!(matches!(&updater.phase, Phase::Error(error) if error.contains("sha256")));
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
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
    fn downloads_use_safe_names_and_clean_up_all_failure_paths() {
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
            1
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
