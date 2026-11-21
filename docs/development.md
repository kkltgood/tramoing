# 开发指南

## 环境与验证

使用 Rust stable。Windows 需要 MSVC 和 Windows SDK；macOS 需要 Xcode Command Line Tools。仅构建 Windows 安装包时需要 NSIS 3，修改 Windows 图标时需要 Python 3 标准库。

在仓库根目录运行：

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --release --locked -p tray-app -p tm-windows --bins
```

CI 在 Windows 和 macOS 执行这些检查，并在 macOS 检查 Apple Silicon / Intel 两个目标。纯逻辑测试跨平台运行；真实桌面、性能计数器、网络采集等验收默认忽略，需要在对应系统上单独执行。

`Cargo.lock` 随仓库提交。常规构建使用 `--locked`，依赖升级时单独评估并提交锁文件变化。各 crate 是应用内部模块，设置了 `publish = false`。

## 代码结构

| 目录 | 职责 |
| --- | --- |
| `crates/tm-core` | 累计值差分、速率平滑、时间窗口聚合与共享颜色 |
| `crates/tm-sampler` | 采样接口与演示数据 |
| `crates/tm-macos` | nettop、应用归属、AppKit 菜单栏及性能采集 |
| `crates/tm-windows` | ETW 助手、命名管道、任务栏控件及性能采集 |
| `crates/tm-store` | SQLite 历史、恢复日志与 CSV 导出 |
| `crates/tray-app` | egui 界面、后台引擎、设置、自启与更新 |
| `assets` | 图标源文件与生成器 |
| `packaging` | Windows / macOS 打包和更新清单脚本 |
| `scripts` | macOS 打包、Windows 诊断及原生运行验证 |

采样器向核心层提供累计值，核心层计算速率并按应用归并。采样线程保留最新快照供界面读取，每 5 秒将增量写入历史库。导出和更新下载在独立后台任务中执行。

Windows 使用独立的提权采集助手，主程序保持普通权限。两个可执行文件通过 `packaging/windows/resources.rs` 共享图标、版本信息和 manifest 构建逻辑。

保持数据目录、计划任务 `TrafficMonitorHelper`、管道 `traffic-monitor-v1`、ETW 会话 `TrafficMonitor-Etw` 和 macOS bundle id `dev.guolongkang.trafficmonitor` 的兼容性；修改它们需要提供迁移方案。

## 隔离调试

`--mock` 不写持久流量历史，但界面设置仍会保存。使用 `TM_DATA_DIR` 隔离设置、日志和实例锁。

Windows PowerShell：

```powershell
$env:TM_DATA_DIR = Join-Path $PWD 'target\demo-data'
cargo run -p tray-app --locked -- --mock --show
Remove-Item Env:TM_DATA_DIR
```

macOS：

```sh
TM_DATA_DIR="$PWD/target/demo-data" cargo run -p tray-app --locked -- --mock --show
```

直接运行二进制可调试面板。验证 macOS 菜单栏位置、焦点、Space 和登录自启时，应打包 `.app`，再运行包内程序：

```sh
./scripts/package-macos.sh --no-dmg
TM_DATA_DIR="$PWD/target/demo-data" \
  ./dist/Tramoing.app/Contents/MacOS/tramoing --mock --show
```

常用环境变量：

| 变量 | 用途 |
| --- | --- |
| `TM_DATA_DIR` | 覆盖配置、历史、日志和实例锁目录 |
| `TM_DB_PATH` | 单独覆盖历史数据库路径 |
| `TM_LOG=1` | 将 eframe / winit 日志写入 stderr |
| `TM_TRACE_FRAMES=1` | 查看窗口收起后的重绘情况 |
| `TM_TRACE_ICONS=1` | 查看图标加载情况 |
| `TM_NETTOP_BIN` | 覆盖 macOS nettop 可执行文件 |
| `TM_WINDOW=1m\|5m\|1h` | 设置初始统计窗口 |
| `TM_PANEL_Y` | 调整预览面板的纵向位置 |
| `TM_SCROLL` / `TM_SETTINGS_SCROLL` | 设置初始滚动位置 |
| `TM_TOAST` | 注入预览提示文案 |
| `TM_TRACE_LIST=1` | 查看列表布局尺寸 |
| `TM_TRACE=1` | 输出 macOS 菜单栏诊断信息 |
| `TM_TRAY_PNG` / `TM_PERFORMANCE_PNG` | 配合 `TM_TRACE` 保存 macOS 菜单栏离屏渲染 |
| `TM_CAPTURE_PPM` / `TM_CAPTURE_AFTER` | debug 演示实例保存面板渲染并退出；需设置 `TM_DATA_DIR` |
| `TM_CAPTURE_STATE=empty\|timeout\|disconnected` | 配合截图模式验证空状态和错误布局 |

截图来自应用自身渲染，不代表真实桌面点击或焦点行为已通过验证。

## 原生验收

Windows 的独立命名管道测试包含在常规测试中，不连接用户正在使用的采集助手。以下项目按需运行：

```powershell
# 验证任务 XML，不注册或修改系统任务
cargo test -p tm-windows task_definition_passes_windows_scheduler_validation --locked -- --ignored --nocapture

# 读取真实 CPU / GPU 性能计数器
cargo test -p tm-windows native_cpu_and_gpu_return_percentages --locked -- --ignored --nocapture

# 需要交互式桌面，会临时创建任务栏控件
cargo test -p tm-windows native_child_lifecycle_and_clicks --locked -- --ignored --nocapture
powershell -ExecutionPolicy Bypass -File scripts\verify-windows-runtime.ps1 -Executable .\target\release\tramoing.exe
```

macOS：

```sh
cargo test -p tm-macos --test native_runtime live_loopback --locked -- --ignored --nocapture --test-threads=1
cargo test -p tray-app live_macos_engine_recovers --locked -- --ignored --nocapture --test-threads=1
```

代理归属测试需要另行设置 `TM_TEST_PROXY`，会从 `speed.cloudflare.com` 下载 4 MiB 公共测试数据；它是诊断用例，已知部分代理环境达不到其客户端归属阈值，见[统计限制](troubleshooting.md#统计限制)。不要把全部 `--ignored` 测试作为通用 CI 门槛。

手动回归应覆盖：网速与 CPU/GPU 点击打开主展示页、设置后收起并重新打开、失焦收起、拖动排序、深浅主题、触控板滚动、高 DPI、多显示器和正常退出。

## 打包

版本统一读取根 `Cargo.toml` 的 `[workspace.package].version`。发布时修改该值、更新锁文件，再运行测试和打包。构建产物放在 `dist/`，通过 GitHub Releases 分发，不提交二进制到源码仓库。

Windows：

```powershell
powershell -ExecutionPolicy Bypass -File packaging\windows\build.ps1
```

脚本从 Cargo 返回的实际产物路径收集主程序和助手，兼容自定义 `CARGO_TARGET_DIR`。默认生成 `dist/Tramoing-<version>-setup.exe` 和对应的 `.sha256`，安装目录包含项目许可证。`.ps1` 和 `.nsi` 中的中文使用 UTF-8 BOM，以兼容 Windows PowerShell 5.1 和 NSIS。

macOS：

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
./scripts/package-macos.sh
```

两个目标都安装时生成通用二进制；只安装一个时生成对应架构的包，支持 `CARGO_TARGET_DIR`。产物为 `dist/Tramoing.app`、`dist/Tramoing-<version>.dmg` 和对应的 `.sha256`。本地构建采用 ad-hoc 签名；正式分发的 Developer ID 签名、公证及 GitHub Release 由维护者完成。

图标修改后保留源文件和生成产物：`assets/gen-icon.rs` 生成源图，macOS 脚本可生成 `AppIcon.icns`，`python assets/gen-ico.py` 生成 Windows `.ico`。普通构建直接使用仓库中的图标。

## 更新源

源码构建不预设更新服务，自动检查默认关闭。发布者可在**构建前**设置 `TRAMOING_UPDATE_URL`，将静态 JSON 地址写入程序；运行时同名环境变量优先，空值可禁用内置地址。未配置时手动检查会显示“未配置更新源”，不发起网络请求。

配置好更新源后，`settings.conf` 中的 `update_check=on` 启用自动检查，手动检查使用 `--check-update` 或设置页版本号。自动检查有启动延迟和 24 小时间隔；macOS 收起后不驱动界面帧，检查可能延迟至面板展开。

Windows 清单生成示例（替换为维护者自己的 HTTPS 下载地址）：

```powershell
powershell -ExecutionPolicy Bypass -File packaging\update\make-manifest.ps1 `
  -BaseUrl 'https://example.com/releases/v0.1.0' -Notes '发布说明'
```

脚本计算安装包的 SHA-256 和文件大小，生成 `dist/latest.json`。客户端支持的平台键为 `windows-x86_64`、`macos-aarch64`、`macos-x86_64`；脚本默认生成 Windows 条目，其他平台按同一格式添加：

```json
{
  "version": "0.1.0",
  "notes": "发布说明",
  "homepage": "https://example.com/releases",
  "platforms": {
    "windows-x86_64": {
      "url": "https://example.com/releases/Tramoing-0.1.0-setup.exe",
      "sha256": "由脚本生成的64位十六进制摘要",
      "size": 123456
    }
  }
}
```

使用可信 HTTPS 服务托管清单和安装包；SHA-256 用于校验文件完整性，不能代替来源认证。macOS 下载后由用户安装，不自动替换 `.app`。

## 提交改动

一个 Pull Request 聚焦一个问题，说明复现方式、修改后的行为和实际运行的检查。涉及平台功能时区分单元测试、编译检查与真机验证；平台限制应同步写入故障排查文档。不要提交本机数据库、诊断日志、临时截图、凭证或构建输出。
