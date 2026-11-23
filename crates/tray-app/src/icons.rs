//! 应用图标缓存。
//!
//! 取图标要走 LaunchServices，冷启动一次可能几十毫秒。放在界面线程里会让
//! 弹窗打开时卡一下，所以拆成「后台取图 + 每帧 poll」：拿不到先用首字母色块
//! 顶上，到了再替换。
//!
//! 系统服务（`mDNSResponder` / `netbiosd` 这类没有 `.app` 的守护进程）压根
//! 没有图标可取，走 [`IconSet::service_icon`] 的齿轮，不占后台队列。

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use eframe::egui;

/// 列表里图标的显示边长（pt）。位图按 2x 渲染，见 `tm_macos::ICON_SIDE`。
pub const ICON_PT: f32 = 23.0;

/// 齿轮纹理按显示尺寸的 2x 渲染。
const SERVICE_SIDE: u32 = (ICON_PT as u32) * 2;

/// 首字母色块的底色。挑的都是和面板配色不冲突的中低饱和色。
const TILE_COLORS: [egui::Color32; 8] = [
    egui::Color32::from_rgb(0x4C, 0x8B, 0xF5), // 蓝
    egui::Color32::from_rgb(0x8B, 0x6C, 0xF2), // 紫
    egui::Color32::from_rgb(0x3F, 0xA9, 0x8C), // 青绿
    egui::Color32::from_rgb(0xD4, 0x8A, 0x3B), // 橙
    egui::Color32::from_rgb(0xC4, 0x5F, 0x7E), // 玫红
    egui::Color32::from_rgb(0x5A, 0x9E, 0xC8), // 钢蓝
    egui::Color32::from_rgb(0x7A, 0xA5, 0x4E), // 橄榄
    egui::Color32::from_rgb(0x9A, 0x6B, 0xB0), // 紫红
];

pub struct IconSet {
    ctx: egui::Context,
    textures: HashMap<String, egui::TextureHandle>,
    /// 已经发给后台、还没回来的键
    pending: HashSet<String>,
    /// 确认取不到的键，在缓存淘汰前不用每次重试
    missing: HashSet<String>,
    /// 每帧自增，配合 `seen` 淘汰最久未使用的缓存
    frame: u64,
    /// 键 → 最后一次被列表请求的帧号
    seen: HashMap<String, u64>,
    /// 系统服务的齿轮，进程一生只烘一次
    service: Option<egui::TextureHandle>,
    req_tx: Sender<String>,
    res_rx: Receiver<(String, Option<egui::ColorImage>)>,
}

/// 图标缓存和待加载请求的总上限，包括取不到图标的应用。
/// 一个应用一张 48×48 图 ≈ 9 KB，256 张不到 3 MB。
const MAX_ICONS: usize = 256;

impl IconSet {
    pub fn new(ctx: egui::Context) -> Self {
        let (req_tx, req_rx) = mpsc::channel::<String>();
        let (res_tx, res_rx) = mpsc::channel();

        let loader_ctx = ctx.clone();
        // 后台失败不能 panic：图标只是装饰，取不到就用色块
        let _ = thread::Builder::new()
            .name("icons".to_owned())
            .spawn(move || {
                while let Ok(key) = req_rx.recv() {
                    let img = load(&key);
                    if res_tx.send((key, img)).is_err() {
                        break;
                    }
                    loader_ctx.request_repaint();
                }
            });

        Self {
            ctx,
            textures: HashMap::new(),
            pending: HashSet::new(),
            missing: HashSet::new(),
            frame: 0,
            seen: HashMap::new(),
            service: None,
            req_tx,
            res_rx,
        }
    }

    /// 把后台已完成的图标转成纹理。每帧调用一次，非阻塞。
    pub fn poll(&mut self) {
        self.frame += 1;
        while let Ok((key, img)) = self.res_rx.try_recv() {
            self.pending.remove(&key);
            match img {
                Some(img) => {
                    let tex = self.ctx.load_texture(
                        format!("app-icon:{key}"),
                        img,
                        egui::TextureOptions::LINEAR,
                    );
                    self.textures.insert(key, tex);
                }
                None => {
                    self.missing.insert(key);
                }
            }
        }
    }

    /// 取图标纹理；没有就顺手排进后台队列，本帧返回 `None`。
    pub fn get(&mut self, key: &str) -> Option<egui::TextureHandle> {
        if let Some(seen) = self.seen.get_mut(key) {
            // 命中缓存时直接更新时间，避免每帧为每行重复分配路径字符串。
            *seen = self.frame;
        } else {
            if self.seen.len() >= MAX_ICONS && !self.evict_oldest() {
                // 全部都还在加载时等待结果，不继续往后台堆积请求。
                return None;
            }
            self.seen.insert(key.to_owned(), self.frame);
        }
        if let Some(t) = self.textures.get(key) {
            return Some(t.clone());
        }
        if !self.pending.contains(key)
            && !self.missing.contains(key)
            && self.req_tx.send(key.to_owned()).is_ok()
        {
            self.pending.insert(key.to_owned());
        }
        None
    }

    fn evict_oldest(&mut self) -> bool {
        let oldest = self
            .seen
            .iter()
            .filter(|(key, _)| !self.pending.contains(*key))
            .min_by_key(|(_, frame)| *frame)
            .map(|(key, _)| key.clone());
        let Some(key) = oldest else { return false };
        self.textures.remove(&key);
        self.missing.remove(&key);
        self.seen.remove(&key);
        true
    }

    /// 系统服务的齿轮纹理。形状见 [`crate::icon::gear`]，只烘一次、所有服务共用。
    ///
    /// 图是纯白 + 透明底：颜色交给 `painter.image` 的 tint，明暗两套主题共用
    /// 同一张纹理，切换主题不用重新烘。
    pub fn service_icon(&mut self) -> egui::TextureHandle {
        if let Some(t) = &self.service {
            return t.clone();
        }
        let (rgba, w, h) = crate::icon::gear(SERVICE_SIDE);
        let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
        let tex = self
            .ctx
            .load_texture("service-gear", img, egui::TextureOptions::LINEAR);
        self.service = Some(tex.clone());
        tex
    }
}

/// 首字母色块的颜色，按名字稳定散列，同一个应用每次运行颜色一致。
pub fn tile_color(name: &str) -> egui::Color32 {
    // FNV-1a：短字符串上分布够均匀，用不着引入哈希库
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    TILE_COLORS[(h % TILE_COLORS.len() as u64) as usize]
}

fn load(key: &str) -> Option<egui::ColorImage> {
    #[cfg(target_os = "macos")]
    {
        let bmp = tm_macos::app_icon(key)?;
        Some(egui::ColorImage::from_rgba_unmultiplied(
            [bmp.width as usize, bmp.height as usize],
            &bmp.rgba,
        ))
    }

    #[cfg(windows)]
    {
        // Windows 的 AppKey 就是可执行文件全路径（见 tm-core::AppKey 的说明），
        // 所以这里不用再反查 pid：直接拿路径去 shell 要图标。
        // 拿不到（进程已退出、路径是 `pid N` 占位符）就返回 None，上层退化成色块。
        let trace = std::env::var_os("TM_TRACE_ICONS").is_some();
        if !std::path::Path::new(key).is_absolute() {
            if trace {
                eprintln!("[icons] 跳过（不是绝对路径）：{key}");
            }
            return None;
        }
        let bmp = match tm_windows::icon::app_icon(key) {
            Some(b) => b,
            None => {
                if trace {
                    eprintln!(
                        "[icons] 取不到：{key}（文件存在={}）",
                        std::path::Path::new(key).exists()
                    );
                }
                return None;
            }
        };
        if trace {
            eprintln!("[icons] ok {}x{} {key}", bmp.width, bmp.height);
        }
        Some(egui::ColorImage::from_rgba_unmultiplied(
            [bmp.width as usize, bmp.height as usize],
            &bmp.rgba,
        ))
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    {
        // 其他平台没有取图标的实现，统一走首字母色块
        let _ = key;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct IconHarness {
        icons: IconSet,
        requests: Receiver<String>,
        results: Sender<(String, Option<egui::ColorImage>)>,
    }

    impl IconHarness {
        fn new() -> Self {
            let (req_tx, requests) = mpsc::channel();
            let (results, res_rx) = mpsc::channel();
            Self {
                icons: IconSet {
                    ctx: egui::Context::default(),
                    textures: HashMap::new(),
                    pending: HashSet::new(),
                    missing: HashSet::new(),
                    frame: 0,
                    seen: HashMap::new(),
                    service: None,
                    req_tx,
                    res_rx,
                },
                requests,
                results,
            }
        }
    }

    #[test]
    fn icon_churn_bounds_successes_failures_and_access_records() {
        let mut harness = IconHarness::new();
        for index in 0..MAX_ICONS * 3 {
            if index >= 2 {
                assert!(harness.icons.get("app-0").is_some());
                assert!(harness.icons.get("app-1").is_none());
                assert!(
                    harness.requests.try_recv().is_err(),
                    "cached failures must not reload"
                );
            }
            let key = format!("app-{index}");
            assert!(harness.icons.get(&key).is_none());
            assert_eq!(harness.requests.try_recv().unwrap(), key);
            let image =
                (index % 2 == 0).then(|| egui::ColorImage::filled([2, 2], egui::Color32::WHITE));
            harness.results.send((key, image)).unwrap();
            harness.icons.poll();
            assert!(harness.icons.seen.len() <= MAX_ICONS);
            assert_eq!(
                harness.icons.textures.len() + harness.icons.missing.len(),
                harness.icons.seen.len(),
            );
            assert!(harness.icons.pending.is_empty());
        }
        assert!(harness.icons.textures.contains_key("app-0"));
        assert!(harness.icons.missing.contains("app-1"));
        assert!(!harness.icons.seen.contains_key("app-2"));
        assert!(!harness.icons.textures.contains_key("app-2"));
        assert!(!harness.icons.missing.contains("app-3"));
        assert!(harness.icons.get("app-3").is_none());
        assert_eq!(harness.requests.try_recv().unwrap(), "app-3");
        assert_eq!(harness.icons.seen.len(), MAX_ICONS);
    }

    #[test]
    fn slow_loader_bounds_requests_and_retries_when_capacity_is_available() {
        let mut harness = IconHarness::new();
        for index in 0..MAX_ICONS {
            let key = format!("app-{index}");
            assert!(harness.icons.get(&key).is_none());
            assert_eq!(harness.requests.try_recv().unwrap(), key);
        }
        for _ in 0..3 {
            assert!(harness.icons.get("app-0").is_none());
            assert!(harness.icons.get("waiting").is_none());
            assert!(harness.requests.try_recv().is_err());
        }
        assert_eq!(harness.icons.pending.len(), MAX_ICONS);
        assert_eq!(harness.icons.seen.len(), MAX_ICONS);

        harness.results.send(("app-0".into(), None)).unwrap();
        harness.icons.poll();
        assert!(harness.icons.get("waiting").is_none());
        assert_eq!(harness.requests.try_recv().unwrap(), "waiting");
        assert!(harness.icons.get("waiting").is_none());
        assert!(harness.requests.try_recv().is_err());
        assert_eq!(harness.icons.pending.len(), MAX_ICONS);
        assert_eq!(harness.icons.seen.len(), MAX_ICONS);
        assert!(!harness.icons.missing.contains("app-0"));
    }

    #[test]
    fn tile_color_is_stable_and_in_palette() {
        let a = tile_color("Google Chrome");
        let b = tile_color("Google Chrome");
        assert_eq!(a, b, "同一个名字必须得到同一个颜色");
        assert!(TILE_COLORS.contains(&a));
    }

    #[test]
    fn tile_color_spreads_across_palette() {
        let names = [
            "Google Chrome",
            "WeChat",
            "verge-mihomo",
            "Code Helper",
            "node",
            "mDNSResponder",
        ];
        let mut seen = HashSet::new();
        for n in names {
            seen.insert(tile_color(n));
        }
        assert!(seen.len() >= 4, "六个不同名字不该挤在两三个颜色上");
    }
}
