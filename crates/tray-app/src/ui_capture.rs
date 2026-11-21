//! 开发用渲染截图：只在隔离的 debug 演示实例中启用，不读取桌面。
//! TM_CAPTURE_PPM 指定输出路径，TM_CAPTURE_AFTER 指定等待采样的秒数。

use std::io::Write;
use std::time::Instant;

use eframe::egui;

fn enabled() -> bool {
    std::env::var_os("TM_FORCE_MOCK").is_some()
        && std::env::var_os("TM_DATA_DIR").is_some()
        && std::env::var_os("TM_CAPTURE_PPM").is_some()
}

/// 只给隔离的渲染验证注入状态；不会断开或启动系统的真实采集助手。
pub fn preview_snapshot(snapshot: crate::engine::Snapshot) -> crate::engine::Snapshot {
    if !enabled() {
        return snapshot;
    }
    let error = match std::env::var("TM_CAPTURE_STATE").as_deref() {
        Ok("empty") => None,
        Ok("timeout") => Some("采集暂时中断，正在重新连接".into()),
        Ok("disconnected") => Some(
            "未连接到采集助手，请重新运行安装程序以修复采集助手；连接命名管道失败（Win32 错误 2）"
                .into(),
        ),
        _ => return snapshot,
    };
    crate::engine::Snapshot {
        error,
        mock: true,
        ..Default::default()
    }
}

pub fn capture(ctx: &egui::Context, started_at: Instant) {
    if !enabled() {
        return;
    }
    let Ok(path) = std::env::var("TM_CAPTURE_PPM") else {
        return;
    };
    let requested = egui::Id::new("render-capture-requested");
    let screenshot = ctx.input(|i| {
        i.events.iter().find_map(|event| match event {
            egui::Event::Screenshot { image, .. } => Some(image.clone()),
            _ => None,
        })
    });
    if let Some(image) = screenshot {
        let result = (|| -> std::io::Result<()> {
            let mut file = std::io::BufWriter::new(std::fs::File::create(&path)?);
            write!(file, "P6\n{} {}\n255\n", image.width(), image.height())?;
            for color in &image.pixels {
                file.write_all(&color.to_array()[..3])?;
            }
            file.flush()
        })();
        eprintln!("[capture] {path}: {result:?}");
        std::env::remove_var("TM_CAPTURE_PPM");
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    } else {
        let after = std::env::var("TM_CAPTURE_AFTER")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
            .unwrap_or(3.0);
        if started_at.elapsed().as_secs_f32() >= after
            && !ctx.data(|d| d.get_temp::<bool>(requested).unwrap_or(false))
        {
            ctx.data_mut(|d| d.insert_temp(requested, true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
    }
}
