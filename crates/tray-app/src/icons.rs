//! 应用图标缓存。
//!
//! 取图标要走 LaunchServices，冷启动一次可能几十毫秒。放在界面线程里会让
//! 弹窗打开时卡一下，所以拆成「后台取图 + 每帧 poll」：拿不到先用首字母色块
//! 顶上，到了再替换。

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use eframe::egui;

/// 列表里图标的显示边长（pt）。位图按 2x 渲染，见 `tm_macos::ICON_SIDE`。
pub const ICON_PT: f32 = 18.0;

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
    /// 确认取不到的键，不用每次重试
    missing: HashSet<String>,
    /// 每帧自增，配合 `seen` 做过期淘汰
    frame: u64,
    /// 键 → 最后一次被列表请求的帧号
    seen: HashMap<String, u64>,
    req_tx: Sender<String>,
    res_rx: Receiver<(String, Option<egui::ColorImage>)>,
}

/// 纹理上限。一个应用一张 48×48 图 ≈ 9 KB，256 张不到 3 MB；
/// 超过就按「多久没被用到」淘汰。长会话里进程开了又关，
/// 不做这件事纹理表只增不减（虽然慢，但确实是泄漏）。
const MAX_TEXTURES: usize = 256;

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
                    let tex = self
                        .ctx
                        .load_texture(format!("app-icon:{key}"), img, egui::TextureOptions::LINEAR);
                    self.textures.insert(key, tex);
                }
                None => {
                    self.missing.insert(key);
                }
            }
        }

        if self.textures.len() > MAX_TEXTURES {
            let now = self.frame;
            // 面板每秒至少重绘一次，600 帧≈10 分钟没上过榜的图标可以丢掉
            self.textures
                .retain(|k, _| now.saturating_sub(*self.seen.get(k).unwrap_or(&0)) < 600);
            self.seen.retain(|k, _| self.textures.contains_key(k) || self.pending.contains(k));
        }
    }

    /// 取图标纹理；没有就顺手排进后台队列，本帧返回 `None`。
    pub fn get(&mut self, key: &str) -> Option<egui::TextureHandle> {
        // 记一次「本帧还在用」，供过期淘汰参考
        self.seen.insert(key.to_owned(), self.frame);
        if let Some(t) = self.textures.get(key) {
            return Some(t.clone());
        }
        if !self.pending.contains(key) && !self.missing.contains(key) {
            if self.req_tx.send(key.to_owned()).is_ok() {
                self.pending.insert(key.to_owned());
            }
        }
        None
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
        return Some(egui::ColorImage::from_rgba_unmultiplied(
            [bmp.width as usize, bmp.height as usize],
            &bmp.rgba,
        ));
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
