//! 给 `tramoing-helper.exe` 塞图标（只有 Windows 需要）。
//!
//! 助手没有界面，但用户会在安装目录和任务管理器里看到它 —— 同一个目录里
//! 主程序有图标、助手是个空白方块，看起来就像装坏了。图标和主程序共用
//! `assets/tramoing.ico`，逻辑与 `crates/tray-app/build.rs` 一致（那边多一段
//! VERSIONINFO）；两份都留着是因为 build 脚本没法跨 crate 复用。

fn main() {
    println!("cargo:rerun-if-changed=../../assets/tramoing.ico");
    println!("cargo:rerun-if-changed=../../packaging/windows/app.manifest");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // rc.exe 自己找（原因见 tray-app/build.rs：winresource 默认查注册表）。
    // 要在 `new()` 之前设：构造时就会解析 SDK 路径。
    if std::env::var_os("RC_PATH").is_none() {
        if let Some(rc) = find_rc() {
            std::env::set_var("RC_PATH", rc);
        }
    }

    let mut res = winresource::WindowsResource::new();
    // 测试宿主同样需要 Windows 8+ 兼容声明，才能创建合成子窗口。
    res.set_manifest_file("../../packaging/windows/app.manifest");
    res.set_icon("../../assets/tramoing.ico");
    res.set_language(0x0804);
    res.set("ProductName", "Tramoing");
    res.set("FileDescription", "Tramoing 流量采集助手");
    res.set("InternalName", "tramoing-helper");
    res.set("OriginalFilename", "tramoing-helper.exe");
    res.set("LegalCopyright", "MIT");

    if let Err(e) = res.compile() {
        let release = std::env::var("DEBUG").as_deref() == Ok("false");
        let msg = format!("嵌入助手图标失败（安装目录里会是个空白方块）：{e}");
        if release {
            panic!("{msg}");
        }
        println!("cargo:warning={msg}");
    }
}

fn find_rc() -> Option<std::path::PathBuf> {
    let kits = std::env::var_os("ProgramFiles(x86)")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Program Files (x86)"))
        .join("Windows Kits")
        .join("10")
        .join("bin");

    let mut versions: Vec<_> = std::fs::read_dir(&kits)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("10."))
        })
        .collect();
    versions.sort_by_key(|a| version_key(a));
    for v in versions.iter().rev() {
        let rc = v.join("x64").join("rc.exe");
        if rc.exists() {
            return Some(rc);
        }
    }
    None
}

fn version_key(p: &std::path::Path) -> Vec<u32> {
    p.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .split('.')
        .map(|s| s.parse::<u32>().unwrap_or(0))
        .collect()
}
