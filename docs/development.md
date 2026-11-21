# 开发指南

## 环境与验证

使用 Rust stable。Windows 需要 MSVC 和 Windows SDK；macOS 需要 Xcode Command Line Tools。仅构建 Windows 安装包时需要 NSIS 3，修改 Windows 图标时需要 Python 3 标准库。

在仓库根目录运行：

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --release --locked -p tray-app -p tm-windows --bins
node --test packaging/update/release.test.cjs
```

CI 在 Windows 和 macOS 执行这些检查，并在 macOS 检查 Apple Silicon / Intel 两个目标。发布脚本的测试需要 Node.js 20+，验证清单、草稿重试和上传失败处理，不会访问 GitHub。纯逻辑测试跨平台运行；真实桌面、性能计数器、网络采集等验收默认忽略，需要在对应系统上单独执行。

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
| `TM_CAPTURE_STATE=list\|empty\|timeout\|disconnected` | 配合截图模式验证无底部提示的列表、空状态和错误布局 |

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

# 需要 NSIS 3；独立测试安装器回调，不提权、不操作正在使用的程序或采集任务
powershell -ExecutionPolicy Bypass -File scripts\verify-windows-upgrade.ps1
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

升级逻辑位于 `packaging/windows/upgrade.nsh`，同样保存为 UTF-8 BOM。`verify-windows-upgrade.ps1` 编译并运行使用这些回调的独立安装器，验证首次安装保留向导、旧客户端升级跳过向导、自定义目录、等待旧进程退出、失败时不重启和旧产品名迁移。测试仅写入 `target/upgrade-test-<id>` 和独立的 HKCU 测试键，结束时清理测试键；发布工作流在 Windows 打包前运行。

macOS：

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
./scripts/package-macos.sh
```

两个目标都安装时生成通用二进制；只安装一个时生成对应架构的包，支持 `CARGO_TARGET_DIR`。产物为 `dist/Tramoing.app`、`dist/Tramoing-<version>.dmg` 和对应的 `.sha256`。生成 DMG 需要 Python 3.10+；打包脚本在 `target/dmgbuild-venv` 中安装固定版本的 `dmgbuild`，也可用 `DMGBUILD_PYTHON` 指向预先准备好的 Python 环境。`--no-dmg` 不需要此工具。

DMG 的中英文背景、Retina 图片和图标位置由 `packaging/macos/dmg-settings.py` 固定写入 `.DS_Store`，不需要 Finder 自动化授权或交互式桌面。背景源为 `dmg-background.svg`，修改设计后同步生成 700×460 的 PNG 和 1400×920 的 `@2x` PNG。安装器支持拖入 Applications，也支持从 DMG 双击应用后确认“安装并打开”；普通账户无法写入 `/Applications` 时，首次安装使用 `~/Applications`。已有安装优先沿用其路径，不会因权限不足悄悄另装一份。安装、更新或第一次启动后展开主面板一次，失焦收起仍正常生效。

打包结束自动运行 `scripts/verify-macos-package.sh`：只读挂载并检查 DMG 完整性、签名、包内版本和二进制、Applications 链接、图标位置及 Retina 背景。原生安装测试随 macOS 的 `cargo test --workspace --locked` 执行，用隔离的临时签名应用检查暂存、版本核对和拒绝降级。手工验收应额外覆盖：DMG 双击安装和取消、拖拽后首次启动、旧版到新版自动更新、没有目录写权限、Gatekeeper 和安装后主面板。

默认仍为 ad-hoc 签名。正式发布可设置 `MACOS_SIGNING_IDENTITY` 为钥匙串中的 Developer ID Application 身份，启用 hardened runtime 和时间戳；设置 `MACOS_NOTARY_PROFILE` 为 `xcrun notarytool store-credentials` 已保存的凭据名称后，脚本提交应用和 DMG 公证并 stapler 附加票据。应用公证在制作 DMG 之前完成，SHA-256 在最后的签名和公证之后生成。密钥和凭据应配置在发布机器或 CI 的钥匙串中，不提交到源码；没有配置这些变量时不会宣称通过 Developer ID 签名或公证。

图标修改后保留源文件和生成产物：`assets/gen-icon.rs` 生成源图，macOS 脚本可生成 `AppIcon.icns`，`python assets/gen-ico.py` 生成 Windows `.ico`。普通构建直接使用仓库中的图标。

## 自动发布到 GitHub Releases

[Release packages](../.github/workflows/release.yml) 仅在 `kkltgood/tramoing` 运行。日常 `git push` 仍推送原仓库 `origin`；只有明确推送到 `opensource` 的版本标签才会自动发布。

| 操作 | 结果 |
| --- | --- |
| 推送分支代码 | 执行 CI 检查，不创建 Release |
| 向 `opensource` 推送 `v0.1.3` 这样的标签 | 检查版本、运行测试、打包两个平台并发布 Release 和更新清单 |
| Actions → Release packages → Run workflow，`tag` 留空 | 试打包所选分支，安装包保存在该次运行的 Artifacts 中 14 天 |
| Run workflow 填入已存在的版本标签 | 重试该标签的发布；也可对失败的运行使用 Re-run failed jobs |

首次启用时先把工作流推送到开源仓库。然后在 [Actions](https://github.com/kkltgood/tramoing/actions/workflows/release.yml) 手动运行一次，`tag` 留空，确认 Windows 和 macOS 都能完成打包。工作流使用 GitHub 自动提供的 `GITHUB_TOKEN`，无需添加个人令牌；发布任务已声明 `contents: write` 权限。如果仓库或组织禁用了 Actions，需要先由仓库管理员启用。

发布步骤（以当前 `0.1.3` 为例）：

1. 确认根 `Cargo.toml` 的 `[workspace.package].version` 是准备发布的版本。后续版本需要同步更新 `Cargo.lock` 中的工作区版本并提交；可运行一次 `cargo check --workspace` 更新锁文件，再执行常规检查。
2. 提交要发布的代码并推送分支和标签：

   ```sh
   git push opensource main
   git tag -a v0.1.3 -m "Release v0.1.3"
   git push opensource v0.1.3
   ```

3. 在 Actions 查看构建进度，成功后到 [Releases](https://github.com/kkltgood/tramoing/releases) 下载。后续发布请替换为新的版本号。

不要对 `opensource` 使用 `git push -u`，以免改变日常开发的上游。标签必须与 Cargo 版本完全一致，格式为 `v主版本.次版本.补丁版本`；当前 Windows 安装器要求三个版本分量均为不超过 65535 的非负整数，因此此流程暂不发布预览版本标签。

每次 Release 包含以下五个文件：

- `Tramoing-<version>-setup.exe`：Windows 10/11 x64，包含主程序和采集助手。
- `Tramoing-<version>-setup.exe.sha256`：Windows 安装包校验值。
- `Tramoing-<version>.dmg`：macOS 11+ 通用包，同一包内包含 Apple Silicon 和 Intel 可执行代码。
- `Tramoing-<version>.dmg.sha256`：macOS DMG 校验值。
- `latest.json`：应用内更新清单，包含版本、发布说明、日期和三个平台的下载地址、SHA-256、文件大小；两个 macOS 架构共用通用 DMG。

所有平台的格式、测试、Clippy 和打包都通过后，发布任务再次核对文件及 SHA-256，再创建草稿、生成清单、上传全部五个附件并公开 Release。清单中的安装包地址固定到本次版本标签，避免下一次发布时下载到不同文件。打包失败不会发布；上传失败会保留草稿供重试。已经公开的同版本 Release 不会被覆盖，需要修复时发布新版本。当前项目不支持 Linux，因此不生成 Linux 安装包。

Windows 安装程序暂未配置代码签名；macOS 的 ad-hoc 签名不等于 Developer ID 签名和公证，系统仍可能显示来源或安全提示。

## 更新源

默认更新源为 `https://github.com/kkltgood/tramoing/releases/latest/download/latest.json`，源码构建和官方安装包均可直接使用，不需要 GitHub 令牌，也不占用 GitHub API 的匿名请求配额。上面的发布流程自动提供该附件。首次发布带清单的版本前，检查会提示“更新清单尚未发布”，不会误报已是最新版。

新安装默认 `update_check=on`；旧配置已有的 `update_check=off` 会保留，可在设置 → 软件更新中开启。自动检查有 10 秒启动延迟和 24 小时间隔；macOS 收起后不驱动界面帧，检查可能延迟至面板展开。手动检查使用软件更新里的按钮、设置页版本号或 `--check-update`，不受自动检查开关和间隔限制。`--mock` 不自动联网，但仍可显式手动检查。

发现新版本后，点击提示下载本平台安装包。下载在后台进行，核对文件大小及 SHA-256 后才显示安装入口；检查间隔到期也不会清除待下载提示或已就绪的安装包。Windows 点击“更新并重启”后以 `/UPDATE /APP_PID=<当前进程> /D=<当前程序目录>` 启动安装器。NSIS 要求 `/D=` 为最后一个参数，含空格时也不加引号。UAC 取消或启动失败时保留当前程序和下载包，界面显示重试入口；启动成功后程序正常退出并提交历史，安装器最多等待 15 秒，超时不改动安装文件。随后更新主程序及助手，只显示进度页，成功后通过 Explorer 以普通权限重新启动，保留配置、历史和自启设置。

macOS 点击“更新并重启”后在后台只读挂载 DMG，不打开 Finder。安装模块校验 bundle ID、清单版本、代码签名及已签名安装的 Team ID，将完整应用复制到原安装目录旁的独立暂存目录，再次校验后启动独立助手。助手确认就绪后才让主程序正常退出；以 PID 和进程启动时间等待旧进程结束，20 秒超时则停止替换。旧 bundle 重命名为备份，再将新 bundle 放到原路径；替换或 `open` 命令失败时尝试恢复旧 bundle。新应用通过 LaunchServices 打开，原路径、自启设置、用户配置和历史不变。准备失败不退出旧程序；启动请求成功不等于应用不会稍后崩溃，不以此宣称覆盖所有运行故障。若恢复失败，错误中保留备份路径供恢复。

新安装包也会识别已有的安装目录及旧产品名：旧客户端没有传入 `/UPDATE` 时，首次升级到新安装包就能跳过目录、组件和完成页。全新安装仍保留原来的安装向导。升级需要替换 Program Files 内文件及更新采集助手计划任务，系统管理员授权提示仍然必要。

如需自建更新服务，可在**构建前**设置 `TRAMOING_UPDATE_URL`，将静态 JSON 地址写入程序；运行时同名环境变量优先于构建配置，构建配置优先于默认 GitHub 地址。显式空值禁用更新源，手动检查会显示“未配置更新源”。

自建服务的 Windows 清单生成示例（替换为维护者自己的 HTTPS 下载地址）：

```powershell
powershell -ExecutionPolicy Bypass -File packaging\update\make-manifest.ps1 `
  -BaseUrl 'https://example.com/releases/v0.1.3' -Notes '发布说明'
```

脚本计算安装包的 SHA-256 和文件大小，生成 `dist/latest.json`。客户端支持的平台键为 `windows-x86_64`、`macos-aarch64`、`macos-x86_64`；脚本默认生成 Windows 条目，其他平台按同一格式添加：

```json
{
  "version": "0.1.3",
  "notes": "发布说明",
  "homepage": "https://example.com/releases",
  "platforms": {
    "windows-x86_64": {
      "url": "https://example.com/releases/Tramoing-0.1.3-setup.exe",
      "sha256": "由脚本生成的64位十六进制摘要",
      "size": 123456
    }
  }
}
```

使用可信 HTTPS 服务托管清单和安装包；SHA-256 用于校验文件完整性，不能代替来源认证。macOS 已安装应用支持确认后自动替换 `.app`；开发时的裸二进制、磁盘映像中运行的副本或无写权限的目录会提示先完成安装或通过 Finder 手动更新。

## 提交改动

一个 Pull Request 聚焦一个问题，说明复现方式、修改后的行为和实际运行的检查。涉及平台功能时区分单元测试、编译检查与真机验证；平台限制应同步写入故障排查文档。不要提交本机数据库、诊断日志、临时截图、凭证或构建输出。
