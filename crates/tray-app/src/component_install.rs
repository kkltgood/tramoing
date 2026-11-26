//! Trusted, optional sensor dependencies. Only embedded metadata selects code to
//! download/install; user-writable cache files never supply URLs, hashes or commands.
#![cfg_attr(not(windows), allow(dead_code))]

use crate::update::{Asset, Event};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::handle_install_command;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Dependency {
    id: String,
    name: String,
    version: String,
    architecture: String,
    minimum_version: String,
    url: String,
    fallbacks: Vec<String>,
    relative_path: String,
    size: u64,
    sha256: String,
    #[serde(default)]
    official_sha512: Option<String>,
    publisher: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Component {
    component_version: String,
    protocol: u32,
    architecture: String,
    url: String,
    sha256: String,
    size: u64,
    source_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Configuration {
    schema: u32,
    dependencies: Vec<Dependency>,
    #[serde(default)]
    component: Option<Component>,
}

#[derive(Clone, Debug)]
pub struct DownloadItem {
    id: String,
    name: String,
    version: String,
    asset: Asset,
    fallbacks: Vec<String>,
}

impl DownloadItem {
    fn verified_in(&self, cache: &Path) -> Result<bool, String> {
        let path = crate::update::download::component_path(&self.version, &cache.join(&self.id))?;
        Ok(
            std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() == self.asset.size)
                && crate::update::sha256_file(&path)
                    .is_ok_and(|hash| hash.eq_ignore_ascii_case(&self.asset.sha256)),
        )
    }
}

#[derive(Clone, Debug)]
pub struct Inspection {
    pub supported: bool,
    pub ready: bool,
    pub needs_restart: bool,
    pub total_bytes: u64,
    pub reused: Vec<String>,
    pub items: Vec<DownloadItem>,
}

#[derive(Clone, Debug)]
pub struct Progress {
    pub name: String,
    pub pct: f32,
}

#[derive(Clone, Debug)]
pub struct Prepared {
    root: PathBuf,
}

#[derive(Debug)]
pub enum CachedDownload {
    Empty,
    Partial(f32),
    Ready(Prepared),
}

impl Inspection {
    pub fn cached_download(&self, root: &Path) -> Result<CachedDownload, String> {
        if self.items.is_empty() {
            return Ok(CachedDownload::Empty);
        }
        let mut all_verified = true;
        let mut saved_bytes = 0.0;
        for item in &self.items {
            let fraction = if item.verified_in(root)? {
                1.0
            } else {
                all_verified = false;
                crate::update::download::saved_progress(
                    &item.asset,
                    &item.version,
                    &root.join(&item.id),
                )
                .unwrap_or(0.0)
            };
            saved_bytes += f64::from(fraction) * item.asset.size as f64;
        }
        if all_verified {
            Ok(CachedDownload::Ready(Prepared {
                root: root.to_owned(),
            }))
        } else if saved_bytes > 0.0 {
            Ok(CachedDownload::Partial(
                (saved_bytes / self.total_bytes as f64) as f32,
            ))
        } else {
            Ok(CachedDownload::Empty)
        }
    }
}

#[derive(Clone, Debug)]
pub struct InstallOutcome {
    pub needs_restart: bool,
}

#[derive(Default, Deserialize)]
struct SystemState {
    component_ok: bool,
    legacy_component: bool,
    runtime_ok: bool,
    driver_ok: bool,
    needs_restart: bool,
}

fn configuration() -> Result<Configuration, String> {
    let mut config: Configuration =
        serde_json::from_str(include_str!("../../../static/dependencies/manifest.json"))
            .map_err(|e| e.to_string())?;
    config.component = serde_json::from_str(include_str!(concat!(
        env!("OUT_DIR"),
        "/sensors-component.json"
    )))
    .map_err(|e| e.to_string())?;
    validate_configuration(&config)?;
    Ok(config)
}

fn valid_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|u| {
        u.scheme() == "https"
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.fragment().is_none()
    })
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit())
}

fn validate_configuration(config: &Configuration) -> Result<(), String> {
    if config.schema != 1 || config.dependencies.len() != 2 {
        return Err("Invalid embedded dependency manifest".into());
    }
    for (id, publisher, host) in [
        (
            "dotnet",
            "Microsoft Corporation",
            "builds.dotnet.microsoft.com",
        ),
        ("pawnio", "namazso.eu", "github.com"),
    ] {
        let Some(dep) = config.dependencies.iter().find(|d| d.id == id) else {
            return Err("Missing embedded dependency".into());
        };
        if dep.architecture != "x64"
            || dep.publisher != publisher
            || semver::Version::parse(&dep.version).is_err()
            || semver::Version::parse(&dep.minimum_version).is_err()
            || !valid_url(&dep.url)
            || url::Url::parse(&dep.url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned))
                .as_deref()
                != Some(host)
            || !valid_hash(&dep.sha256)
            || dep.size == 0
            || dep.size > 200_000_000
            || dep.fallbacks.iter().any(|u| !valid_url(u))
        {
            return Err("Invalid embedded dependency metadata".into());
        }
    }
    if let Some(c) = &config.component {
        if c.protocol != 1
            || c.architecture != "x64"
            || !valid_url(&c.url)
            || semver::Version::parse(&c.component_version).is_err()
            || !valid_hash(&c.sha256)
            || !valid_hash(&c.source_sha256)
            || c.size == 0
            || c.size > 200_000_000
        {
            return Err("Invalid embedded component metadata".into());
        }
    }
    Ok(())
}

fn plan(
    config: &Configuration,
    state: &SystemState,
    supported: bool,
) -> Result<Inspection, String> {
    let mut result = Inspection {
        supported,
        ready: false,
        needs_restart: state.needs_restart,
        total_bytes: 0,
        reused: Vec::new(),
        items: Vec::new(),
    };
    if !supported {
        return Ok(result);
    }
    for dep in &config.dependencies {
        let usable = match dep.id.as_str() {
            "dotnet" => state.legacy_component || state.runtime_ok,
            "pawnio" => state.driver_ok,
            _ => false,
        };
        if usable {
            result.reused.push(dep.name.clone());
        } else {
            result.items.push(DownloadItem {
                id: dep.id.clone(),
                name: dep.name.clone(),
                version: dep.version.clone(),
                asset: Asset {
                    url: dep.url.clone(),
                    sha256: dep.sha256.clone(),
                    size: dep.size,
                },
                fallbacks: dep.fallbacks.clone(),
            });
        }
    }
    if state.component_ok {
        result.reused.push("Tramoing Sensors".into());
    } else {
        let c = config.component.as_ref().ok_or("此开发构建未配置温度组件下载信息，请使用正式安装包 / Sensor download metadata is unavailable in this development build")?;
        result.items.push(DownloadItem {
            id: "sensors".into(),
            name: "Tramoing Sensors".into(),
            version: c.component_version.clone(),
            asset: Asset {
                url: c.url.clone(),
                sha256: c.sha256.clone(),
                size: c.size,
            },
            fallbacks: Vec::new(),
        });
    }
    result.total_bytes = result.items.iter().map(|i| i.asset.size).sum();
    result.ready = result.items.is_empty();
    Ok(result)
}

pub fn inspect() -> Result<Inspection, String> {
    #[cfg(windows)]
    {
        let supported = tm_windows::temperature_service::supported_platform();
        let config = configuration()?;
        let state = if supported {
            windows::inspect_system(&config)?
        } else {
            SystemState::default()
        };
        plan(&config, &state, supported)
    }
    #[cfg(not(windows))]
    plan(&configuration()?, &SystemState::default(), false)
}

pub fn download(
    inspection: &Inspection,
    root: &Path,
    progress: &Sender<Progress>,
    stop: &Receiver<()>,
) -> Result<Option<Prepared>, String> {
    if !inspection.supported {
        return Err("Unsupported sensor platform".into());
    }
    let total = inspection.total_bytes.max(1) as f64;
    let mut completed = 0u64;
    for item in &inspection.items {
        if stop.try_recv() != Err(mpsc::TryRecvError::Empty) {
            return Ok(None);
        }
        let item_root = root.join(&item.id);
        let (tx, rx) = mpsc::channel();
        let result = std::thread::scope(|scope| {
            let bridge = scope.spawn(move || {
                while let Ok(event) = rx.recv() {
                    if let Event::Progress(pct) = event {
                        let overall =
                            (completed as f64 + item.asset.size as f64 * pct as f64) / total;
                        let _ = progress.send(Progress {
                            name: item.name.clone(),
                            pct: overall.clamp(0.0, 1.0) as f32,
                        });
                    }
                }
            });
            let result = crate::update::download::component(
                &item.asset,
                &item.version,
                &tx,
                &item_root,
                stop,
                &item.fallbacks,
            );
            drop(tx);
            let _ = bridge.join();
            result
        })?;
        if result.is_none() {
            return Ok(None);
        }
        completed += item.asset.size;
        let _ = progress.send(Progress {
            name: item.name.clone(),
            pct: (completed as f64 / total) as f32,
        });
    }
    Ok(Some(Prepared {
        root: root.to_path_buf(),
    }))
}

pub fn install(prepared: &Prepared) -> Result<InstallOutcome, String> {
    #[cfg(windows)]
    {
        windows::install(prepared)
    }
    #[cfg(not(windows))]
    {
        let _ = prepared;
        Err("Unsupported sensor platform".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> Configuration {
        let mut config = configuration().unwrap();
        config.component = Some(Component {
            component_version: "1.0.0".into(),
            protocol: 1,
            architecture: "x64".into(),
            url: "https://example.com/sensors.exe".into(),
            sha256: "a".repeat(64),
            size: 123,
            source_sha256: "b".repeat(64),
        });
        config
    }
    #[test]
    fn existing_compatible_dependencies_are_not_downloaded() {
        let config = config();
        let result = plan(
            &config,
            &SystemState {
                runtime_ok: true,
                driver_ok: true,
                ..Default::default()
            },
            true,
        )
        .unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].id, "sensors");
        assert_eq!(result.total_bytes, 123);
    }
    #[test]
    fn legacy_self_contained_install_does_not_require_system_dotnet() {
        let result = plan(
            &config(),
            &SystemState {
                component_ok: true,
                legacy_component: true,
                driver_ok: true,
                ..Default::default()
            },
            true,
        )
        .unwrap();
        assert!(result.ready);
        assert!(result.items.is_empty());
    }
    #[test]
    fn arm64_never_requires_download_metadata_or_dependencies() {
        let mut config = config();
        config.component = None;
        let result = plan(&config, &SystemState::default(), false).unwrap();
        assert!(!result.supported);
        assert!(result.items.is_empty());
    }
    #[test]
    fn invalid_download_metadata_is_rejected() {
        let mut bad = config();
        bad.dependencies[0].url = "http://example.com/runtime.exe".into();
        assert!(validate_configuration(&bad).is_err());
        let mut config = config();
        config.component.as_mut().unwrap().sha256.clear();
        assert!(validate_configuration(&config).is_err());
    }

    #[test]
    fn inspection_restores_progress_and_only_offers_verified_installers() {
        use crate::update::download::component_path;
        use sha2::{Digest, Sha256};

        let mut config = config();
        let runtime = &mut config.dependencies[0];
        runtime.size = 4;
        runtime.sha256 = format!("{:x}", Sha256::digest(b"abcd"));
        let sensor = config.component.as_mut().unwrap();
        sensor.size = 4;
        sensor.sha256 = format!("{:x}", Sha256::digest(b"efgh"));
        let state = SystemState {
            driver_ok: true,
            ..Default::default()
        };
        let inspection = plan(&config, &state, true).unwrap();
        let root = tempfile::tempdir().unwrap();
        let cached = || inspection.cached_download(root.path()).unwrap();
        assert!(matches!(cached(), CachedDownload::Empty));
        assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());

        let path = |item: &DownloadItem| {
            let path = component_path(&item.version, &root.path().join(&item.id)).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            path
        };
        let runtime = path(&inspection.items[0]);
        let sensor = &inspection.items[1];
        let final_path = path(sensor);
        let partial = final_path.with_file_name(format!(
            "{}-{}.part",
            crate::update::PLATFORM,
            sensor.asset.sha256
        ));
        std::fs::write(runtime, b"abcd").unwrap();
        std::fs::write(&partial, b"ef").unwrap();
        assert!(matches!(cached(), CachedDownload::Partial(pct) if pct == 0.75));
        std::fs::write(&partial, b"efgh").unwrap();
        assert!(matches!(cached(), CachedDownload::Partial(pct) if pct == 1.0));
        std::fs::remove_file(partial).unwrap();
        std::fs::write(&final_path, b"oops").unwrap();
        assert!(matches!(cached(), CachedDownload::Partial(pct) if pct == 0.5));
        std::fs::write(final_path, b"efgh").unwrap();
        assert!(
            matches!(cached(), CachedDownload::Ready(prepared) if prepared.root == root.path())
        );
    }

    #[test]
    fn installed_driver_needing_restart_is_not_reinstalled() {
        let result = plan(
            &config(),
            &SystemState {
                component_ok: true,
                runtime_ok: true,
                driver_ok: true,
                needs_restart: true,
                ..Default::default()
            },
            true,
        )
        .unwrap();
        assert!(result.ready && result.needs_restart);
        assert!(result.items.is_empty());
    }
}
