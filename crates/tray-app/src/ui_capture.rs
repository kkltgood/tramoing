//! 开发用渲染截图：只在隔离的 debug 演示实例中启用，不读取桌面。
//! TM_CAPTURE_PPM 指定输出路径，TM_CAPTURE_AFTER 指定等待采样的秒数。

use std::io::Write;
use std::time::Instant;

use eframe::egui;

pub fn enabled() -> bool {
    std::env::var_os("TM_FORCE_MOCK").is_some()
        && std::env::var_os("TM_DATA_DIR").is_some()
        && std::env::var_os("TM_CAPTURE_PPM").is_some()
}

/// 诊断卡片的静态预览；不导出文件，也不伪造真实实例的任务结果。
pub fn diagnostics_result(lang: crate::theme::Lang) -> Option<Result<std::path::PathBuf, String>> {
    if !enabled() {
        return None;
    }
    match std::env::var("TM_CAPTURE_STATE").as_deref() {
        Ok("settings-diagnostics-saved") => Some(Ok(tm_store::data_dir()
            .join("diagnostics")
            .join("Tramoing-diagnostics-20261125T095231Z-preview.txt"))),
        Ok("settings-diagnostics-error") => Some(Err(lang
            .t(
                "无法写入报告文件，请检查目标文件夹的存储权限（系统错误 5）。",
                "Could not write the report. Check the destination folder's permissions (OS error 5).",
            )
            .into())),
        _ => None,
    }
}

pub fn diagnostics_busy() -> bool {
    enabled() && std::env::var("TM_CAPTURE_STATE").as_deref() == Ok("settings-diagnostics-busy")
}

/// 仅在隔离预览中打开一次指定的颜色面板。
pub fn open_color_picker(ctx: &egui::Context, label: &str, lang: crate::theme::Lang) -> bool {
    if !enabled() {
        return false;
    }
    let target = match std::env::var("TM_CAPTURE_STATE").as_deref() {
        Ok("settings-color-download") => lang.t("下载", "Download"),
        Ok("settings-color-gpu") => "GPU",
        _ => return false,
    };
    if label != target {
        return false;
    }
    let id = egui::Id::new("capture-color-opened");
    ctx.data_mut(|data| {
        if data.get_temp::<bool>(id).unwrap_or(false) {
            false
        } else {
            data.insert_temp(id, true);
            true
        }
    })
}

#[cfg(windows)]
pub fn prepare_network_panel(
    panel: &mut crate::network_panel::NetworkPanel,
    access: &mut crate::network_access::NetworkAccess,
    lang: crate::theme::Lang,
) {
    if !enabled() {
        return;
    }
    let state = std::env::var("TM_CAPTURE_STATE").unwrap_or_default();
    if state.starts_with("network-add") {
        panel.open();
        if state == "network-add-invalid" {
            panel.preview_invalid_path(lang);
        }
        if state == "network-add-offline" {
            access.error = Some("未连接到网络控制助手，请重新运行安装程序以更新助手。".into());
        }
    }
}

/// 只给隔离的渲染验证注入状态；不会断开或启动系统的真实采集助手。
pub fn preview_snapshot(snapshot: crate::engine::Snapshot) -> crate::engine::Snapshot {
    if !enabled() {
        return snapshot;
    }
    let error = match std::env::var("TM_CAPTURE_STATE").as_deref() {
        // 验证有数据、无底部提示的布局；采样和持久化仍使用隔离的演示引擎。
        Ok("list" | "network-add" | "network-add-invalid" | "network-add-offline") => {
            return crate::engine::Snapshot {
                mock: false,
                ..snapshot
            }
        }
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
