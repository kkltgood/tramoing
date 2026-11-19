//! 给 `tramoing.exe` 塞图标和版本信息（只有 Windows 需要）。
//!
//! 没有资源段的 exe 在资源管理器、开始菜单、任务管理器里都是一个**空白方块**
//! （`ExtractIconExW` 返回 0 个图标），用户只会觉得「装坏了」。这里在链接前
//! 生成一个 `.res`：
//!
//! - 图标来自 `assets/tramoing.ico`（9 个尺寸，16px 起还能认出波形，
//!   由 `assets/gen-ico.py` 从 `icon-1024.png` 生成，圆角外面是透明而不是黑）；
//! - VERSIONINFO 让文件属性 / 任务管理器显示「Tramoing」和中文说明，
//!   而不是小写的 `tramoing.exe`。
//!
//! macOS 不走这条路：那边图标在 `.app` 包的 `AppIcon.icns` 里
//! （见 `packaging/macos`），所以这里按目标平台直接跳过。

fn main() {
    println!("cargo:rerun-if-changed=../../assets/tramoing.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // rc.exe 自己找：winresource 默认去查注册表问 Visual Studio 装在哪，
    // 而 SDK 在默认位置时根本不用问 —— 受限环境（注册表被策略挡住）反而会失败。
    // 必须在 `new()` 之前设好：它构造时就会解析 SDK 路径。
    if std::env::var_os("RC_PATH").is_none() {
        if let Some(rc) = find_rc() {
            std::env::set_var("RC_PATH", rc);
        }
    }

    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../assets/tramoing.ico");
    // 0x0804 = 中文（简体）。语言 ID 决定文件属性里那几行的语言归属，
    // 不影响程序本身。
    res.set_language(0x0804);
    res.set("ProductName", "Tramoing");
    res.set("FileDescription", "Tramoing 托盘流量监控");
    res.set("InternalName", "tramoing");
    res.set("OriginalFilename", "tramoing.exe");
    res.set("LegalCopyright", "MIT");

    if let Err(e) = res.compile() {
        // 开发构建只吼一声，release 直接失败：**发出去的包不能没有图标**。
        let release = std::env::var("DEBUG").as_deref() == Ok("false");
        let msg = format!("嵌入 exe 图标/版本信息失败（打出来的包会没有图标）：{e}");
        if release {
            panic!("{msg}");
        }
        println!("cargo:warning={msg}");
    }
}

/// 在 `%ProgramFiles(x86)%\Windows Kits\10\bin\<版本>\x64\rc.exe` 里找最新的一个。
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
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("10.")))
        .collect();
    // 目录名是 SDK 版本号，逐段按数字比，取最新的那个
    versions.sort_by(|a, b| version_key(a).cmp(&version_key(b)));
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
