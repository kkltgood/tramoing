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

CI 在 Windows 和 macOS 执行这些检查，并检查 Windows ARM64 以及 macOS Apple Silicon / Intel 目标。发布脚本的测试需要 Node.js 20+，验证四个架构安装包的清单、校验和、草稿重试和上传失败处理，不会访问 GitHub。纯逻辑测试跨平台运行；真实桌面、性能计数器、网络采集等验收默认忽略，需要在对应系统上单独执行。

`Cargo.lock` 随仓库提交。常规构建使用 `--locked`，依赖升级时单独评估并提交锁文件变化。各 crate 是应用内部模块，设置了 `publish = false`。

## 代码结构

| 目录 | 职责 |
| --- | --- |
| `crates/tm-core` | 累计值差分、速率平滑、时间窗口聚合与共享颜色 |
| `crates/tm-diagnostics` | 主程序与助手共用的限量文件日志、UTC 时间和 panic 记录 |
| `crates/tm-sampler` | 采样接口与演示数据 |
| `crates/tm-macos` | nettop、应用归属、AppKit 菜单栏及性能采集 |
| `crates/tm-windows` | ETW 助手、命名管道、任务栏控件及性能采集 |
| `crates/tm-store` | SQLite 历史、恢复日志与 CSV 导出 |
| `crates/tray-app` | egui 界面、后台引擎、设置、自启与更新 |
| `assets` | 图标源文件与生成器 |
| `packaging` | Windows / macOS 打包和更新清单脚本 |
| `scripts` | macOS 打包、Windows 诊断及原生运行验证 |

诊断日志在打开窗口和数据库之前初始化；主程序和 Windows 助手分别写 `ui.log`、`helper.log`，每份最多 1 MiB 并保留三份备份。日志不依赖 SQLite，后台线程 panic 也经过同一个 hook。`--export-diagnostics` 在迁移、单实例锁和窗口创建之前执行；UI 导出运行在后台线程，系统检查有超时，Windows 检查脚本通过 `include_str!` 编入程序，不要求用户另行下载脚本。回归测试覆盖轮换、并发、子进程 panic、数据库损坏时导出及报告内容限制。ERROR 记录进入有界通知队列，5 分钟内相同来源和内容去重；界面线程展示确认弹框，导出失败仅更新当前界面，避免递归通知。启动失败使用原生提示；GUI 进程的 panic hook 在同步日志后启动 `--report-error` 独立报告进程，绕过安装、迁移和单实例锁，启动失败则保留崩溃标记供下次打开提示。命令行导出/自启操作不弹框。

采样器向核心层提供累计值，核心层计算速率并按应用归并。采样线程保留最新快照供界面读取，每 5 秒将增量写入历史库。导出和更新下载在独立后台任务中执行。

时间窗口保留原始秒桶，并为 1 分钟、5 分钟、10 分钟提醒和 1 小时维护滚动累计，避免每秒反复扫描历史；累计使用 `u128`，对外仍按 `u64` 饱和，保证极端流量过期后的统计正确。休眠超过完整窗口时直接释放失效应用历史。图标缓存将纹理、失败记录和待加载请求合计限制为 256 项，满额时淘汰最久未使用的已完成记录。

可运行 `cargo bench -p tm-core --bench window_tracker --locked` 检查窗口统计开销。基准预填 64 个应用的一小时历史，每轮推入一秒数据并查询上述四个窗口，报告五次运行的中位耗时；它衡量窗口计算，不代表整机 CPU 占用。

Windows 使用独立的提权采集助手，主程序保持普通权限。两个可执行文件通过 `packaging/windows/resources.rs` 共享图标、版本信息和 manifest 构建逻辑。

Windows 网络控制使用独立的账户 SID 命名管道；拒绝远程客户端，DACL 限定当前账户和 SYSTEM，Medium 完整性标签允许普通 UI 写入，双方校验对端 exe 路径，客户端另外核验助手的提升状态。消息有长度上限和总超时，只有 List / Set 两类请求。统计管道协议保持不变。WFP 的专用持久 sublayer 包含按 ALE_APP_ID 匹配的四条 block 规则，增加/移除使用事务；没有关联 provider，避免缺少 auto-start 服务的 provider 在 BFE 重启后使规则禁用。规则状态从系统读取，缓存仅用于保留恢复入口，不能作为成功依据。

当前标识统一为：数据目录及 Windows Run 值 `Tramoing`、计划任务 `TramoingHelper`、统计管道 `tramoing-v1`、ETW 会话 `Tramoing-Etw`、macOS bundle / LaunchAgent id `dev.guolongkang.tramoing`。Windows 安装脚本为 `packaging/windows/tramoing.nsi`。Rust 共用标识集中在 `tm-core::identity`；旧名称仅出现在明确的 legacy 识别、迁移、清理代码及其回归测试中。

数据迁移由主程序在打开设置、数据库和实例锁之前执行：锁住旧 UI，使用 SQLite 快照导入数据库（包括 WAL）、按缺失文件导入设置与 pending，最后写迁移标记；不覆盖新文件，保留旧目录备份。自启仅在旧项存在时迁移，禁用时同时清理旧项。Windows 安装器先停旧进程，注册新任务后清理旧任务、ETW、快捷方式、执行文件及注册项，主程序/助手必须成套升级。macOS 安装器接受旧 bundle 作为替换/回滚目标，新安装包只接受当前 id；旧程序内置更新器会拒绝新 id，首次跨标识升级需手动打开新版 DMG，之后恢复应用内更新。

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
# Apple Silicon；Intel 将 arm64 改为 x64
./scripts/package-macos.sh --arch arm64 --no-dmg
TM_DATA_DIR="$PWD/target/demo-data" \
  ./dist/macos-arm64/Tramoing.app/Contents/MacOS/tramoing --mock --show
```

常用环境变量：

| 变量 | 用途 |
| --- | --- |
| `TM_DATA_DIR` | 覆盖配置、历史、日志和实例锁目录 |
| `TM_DB_PATH` | 单独覆盖历史数据库路径 |
| `TM_LOG=1` | 在默认持久错误日志之外，启用依赖库 INFO 日志并同步输出到 stderr |
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
| `TM_CAPTURE_STATE=network-add\|network-add-invalid\|network-add-offline` | Windows 添加应用面板：默认表单、路径错误、联网助手离线；仅注入演示状态，不修改系统网络规则 |
| `TM_CAPTURE_STATE=settings-general\|settings-statusbar\|settings-statistics` | 设置页三个页签；可配合 `TM_SETTINGS_SCROLL` 检查长内容，Windows 截图实例不创建任务栏统计项 |
| `TM_CAPTURE_STATE=error-prompt` | 隔离演示实例注入可恢复错误，验证弹框及导出交互 |
| `TM_CAPTURE_STATE=settings-color-download\|settings-color-gpu` | 打开下载或 GPU 选色面板，检查深浅主题和中英文布局 |

截图来自应用自身渲染，不代表真实桌面点击或焦点行为已通过验证。

## 原生验收

Windows 的独立命名管道测试包含在常规测试中，不连接用户正在使用的采集助手。以下项目按需运行：

```powershell
# 验证任务 XML，不注册或修改系统任务
cargo test -p tm-windows task_definition_passes_windows_scheduler_validation --locked -- --ignored --nocapture

# 读取真实 CPU / GPU 性能计数器
cargo test -p tm-windows native_cpu_and_gpu_return_percentages --locked -- --ignored --nocapture

# 管理员终端：独立 ETW 会话启动、启用内核网络 provider、停止后再次启动。
# 仅使用 Tramoing-Etw-Verification，不连接或清理正在使用的采集助手。
cargo test -p tm-windows native_realtime_session_restarts_after_control_overwrites_properties --locked -- --ignored --nocapture

# 需要交互式桌面，会临时创建任务栏控件
cargo test -p tm-windows native_child_lifecycle_and_clicks --locked -- --ignored --nocapture
powershell -ExecutionPolicy Bypass -File scripts\verify-windows-runtime.ps1 -Executable .\target\release\tramoing.exe

# 需要 NSIS 3；独立测试安装器回调，不提权、不操作正在使用的程序或采集任务
powershell -ExecutionPolicy Bypass -File scripts\verify-windows-upgrade.ps1

# 管理员终端：只阻断临时目录中的测试 exe 副本，失败时也尝试清理规则。
# 覆盖 TCP/UDP、IPv4/IPv6、本机回环代理路径、已有连接、同名不同路径、恢复和已删除 exe 清理。
cargo test -p tm-windows --test network_blocking wfp_blocks --locked -- --ignored --nocapture
```

断网功能的发布验收还应在安装后的普通权限 UI 上覆盖：断网/恢复、助手错误提示、电脑重启后规则生效、应用退出重启、系统代理/TUN、卸载清理。管理员原生测试不替代这些环境验收，也不修改用户正在使用的软件规则。

控制管道的两端权限验收入口也在 `network_blocking` 集成测试中：在隔离目录复制测试二进制为 `tramoing-helper.exe`、`tramoing.exe` 和 `probe.exe`，设置 `TM_CONTROL_VERIFY=1`；管理员运行前者的 `--exact control_pipe_server_child --ignored --nocapture`，普通权限运行后者的 `--exact control_pipe_client_child --ignored --nocapture`。服务端建立 `ready` 文件后客户端才连接；结束时创建同目录 `stop` 文件。服务端最多运行 90 秒，退出前清理测试目标规则；不应与正式新版助手同时运行，因为使用同一账户控制管道。

macOS：

```sh
cargo test -p tm-macos --test native_runtime live_nettop_waits_between_samples_without_spinning_on_stdin --locked -- --ignored --nocapture --test-threads=1
cargo test -p tm-macos --test native_runtime live_loopback --locked -- --ignored --nocapture --test-threads=1
cargo test -p tray-app live_macos_engine_recovers --locked -- --ignored --nocapture --test-threads=1
```

`live_nettop_waits_between_samples_without_spinning_on_stdin` 检查真实采集进程在每秒采样之间等待，并在回收子进程后核对 CPU 时间，捕获 stdin EOF 引起的忙循环。子进程 CPU 记账是进程级的，需单独运行并使用 `--test-threads=1`；允许 25% 单核占用以容纳连接数量和调度差异。

代理归属测试需要另行设置 `TM_TEST_PROXY`，会从 `speed.cloudflare.com` 下载 4 MiB 公共测试数据；它是诊断用例，已知部分代理环境达不到其客户端归属阈值，见[统计限制](troubleshooting.md#统计限制)。不要把全部 `--ignored` 测试作为通用 CI 门槛。

手动回归应覆盖：网速与 CPU/GPU 点击打开主展示页、设置后收起并重新打开、失焦收起、任务栏拖动排序、应用置顶/取消置顶及重启保留、空闲置顶项与时间窗口切换、图钉点击和行详情互不干扰、深浅主题、触控板滚动、高 DPI、多显示器和正常退出。

## 打包

版本统一读取根 `Cargo.toml` 的 `[workspace.package].version`。发布时修改该值、更新锁文件，再运行测试和打包。构建产物放在 `dist/`，通过 GitHub Releases 分发，不提交二进制到源码仓库。

Windows：

```powershell
powershell -ExecutionPolicy Bypass -File packaging\windows\build.ps1

# Windows ARM64；交叉编译时还需安装 Visual Studio 的 C++ ARM64 生成工具
rustup target add aarch64-pc-windows-msvc
powershell -ExecutionPolicy Bypass -File packaging\windows\build.ps1 -Architecture arm64
```

`-Architecture` 支持 `x64`（默认）和 `arm64`，分别指定 `x86_64-pc-windows-msvc` 和 `aarch64-pc-windows-msvc` 编译目标。脚本从 Cargo 返回的实际产物路径收集主程序和助手，兼容自定义 `CARGO_TARGET_DIR`，并校验两个 PE 文件的真实架构。暂存目录为 `dist/windows-<arch>/`，生成 `dist/Tramoing-<version>-windows-<arch>-setup.exe` 和对应的 `.sha256`，安装目录包含项目许可证。安装器在修改现有安装前拒绝不兼容的架构；已有的 x64 程序在 ARM64 Windows 的模拟环境下仍可继续升级 x64 包。`.ps1` 和 `.nsi` 中的中文使用 UTF-8 BOM，以兼容 Windows PowerShell 5.1 和 NSIS。

升级逻辑位于 `packaging/windows/upgrade.nsh`，同样保存为 UTF-8 BOM。`verify-windows-upgrade.ps1` 编译并运行使用这些回调的独立安装器，验证首次安装保留向导、旧客户端升级跳过向导、自定义目录、等待旧进程退出、失败时不重启和旧产品名迁移。等待退出的用例用 Rust 编译同架构的临时父进程，并核对其就绪信号；需要安装对应 Rust target 和 MSVC 生成工具。测试仅写入 `target/upgrade-test-<id>` 和独立的 HKCU 测试键，结束时清理测试键；发布工作流在 Windows 打包前运行。也可在 Actions 的 `Windows installer verification` 中选择架构单独运行，失败时保留安装器诊断文件。

macOS：

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
./scripts/package-macos.sh --arch arm64
./scripts/package-macos.sh --arch x64
```

省略 `--arch` 时只构建本机架构，不受已安装 target 数量影响；显式 `--arch universal` 仍可生成本地通用包，自动发布只使用 `arm64` 和 `x64`。缺少指定的 target 时会报错并提示安装命令，不会悄悄改用其他架构。支持 `CARGO_TARGET_DIR`。产物为 `dist/macos-<arch>/Tramoing.app`、`dist/Tramoing-<version>-macos-<arch>.dmg` 和对应的 `.sha256`；包内应用仍名为 `Tramoing.app`。打包时校验 Mach-O 架构，部署目标默认为 macOS 11.0。生成 DMG 需要 Python 3.10+；打包脚本在 `target/dmgbuild-venv` 中安装固定版本的 `dmgbuild`，也可用 `DMGBUILD_PYTHON` 指向预先准备好的 Python 环境。`--no-dmg` 不需要此工具。

DMG 的中英文背景、Retina 图片和图标位置由 `packaging/macos/dmg-settings.py` 固定写入 `.DS_Store`，不需要 Finder 自动化授权或交互式桌面。背景源为 `dmg-background.svg`，修改设计后同步生成 700×460 的 PNG 和 1400×920 的 `@2x` PNG。安装器支持拖入 Applications，也支持从 DMG 双击应用后确认“安装并打开”；普通账户无法写入 `/Applications` 时，首次安装使用 `~/Applications`。已有安装优先沿用其路径，不会因权限不足悄悄另装一份。安装、更新或第一次启动后展开主面板一次，失焦收起仍正常生效。

打包结束自动运行 `scripts/verify-macos-package.sh`：只读挂载并检查 DMG 完整性、签名、包内版本和二进制、Applications 链接、图标位置及 Retina 背景。原生安装测试随 macOS 的 `cargo test --workspace --locked` 执行，用隔离的临时签名应用检查暂存、版本核对和拒绝降级。手工验收应额外覆盖：DMG 双击安装和取消、拖拽后首次启动、旧版到新版自动更新、没有目录写权限、Gatekeeper 和安装后主面板。

默认仍为 ad-hoc 签名。正式发布可设置 `MACOS_SIGNING_IDENTITY` 为钥匙串中的 Developer ID Application 身份，启用 hardened runtime 和时间戳；设置 `MACOS_NOTARY_PROFILE` 为 `xcrun notarytool store-credentials` 已保存的凭据名称后，脚本提交应用和 DMG 公证并 stapler 附加票据。应用公证在制作 DMG 之前完成，SHA-256 在最后的签名和公证之后生成。密钥和凭据应配置在发布机器或 CI 的钥匙串中，不提交到源码；没有配置这些变量时不会宣称通过 Developer ID 签名或公证。

图标修改后保留源文件和生成产物：`assets/gen-icon.rs` 生成源图，macOS 脚本可生成 `AppIcon.icns`，`python assets/gen-ico.py` 生成 Windows `.ico`。普通构建直接使用仓库中的图标。

## 自动发布到 GitHub Releases

[Release packages](../.github/workflows/release.yml) 仅在 `kkltgood/tramoing` 运行。日常 `git push` 仍推送原仓库 `origin`；只有明确推送到 `opensource` 的版本标签才会自动发布。

| 操作 | 结果 |
| --- | --- |
| 推送分支代码 | 执行 CI 检查，不创建 Release |
| 向 `opensource` 推送 `v0.1.9` 这样的标签 | 检查版本、运行测试、打包四个系统/架构组合并发布 Release 和更新清单 |
| Actions → Release packages → Run workflow，`tag` 留空 | 试打包所选分支，安装包保存在该次运行的 Artifacts 中 14 天 |
| Run workflow 填入已存在的版本标签 | 重试该标签的发布；也可对失败的运行使用 Re-run failed jobs |

首次启用时先把工作流推送到开源仓库。然后在 [Actions](https://github.com/kkltgood/tramoing/actions/workflows/release.yml) 手动运行一次，`tag` 留空，确认四个构建任务都能完成打包。Windows x64 使用 `windows-latest`，Windows ARM64 使用 `windows-11-arm`，macOS Apple Silicon 使用 `macos-latest`，macOS Intel 使用 `macos-15-intel`；每个任务在对应架构上执行测试和打包。macOS 部署目标仍为 11.0，构建脚本会核验产物架构。工作流使用 GitHub 自动提供的 `GITHUB_TOKEN`，无需添加个人令牌；发布任务已声明 `contents: write` 权限。如果仓库或组织禁用了 Actions，需要先由仓库管理员启用。

换电脑后，发布流程仍由 GitHub Actions 执行，发起发布的电脑可以是 Windows 或 macOS，本机不需要构建 Windows 安装包。Git 的远端配置不会随代码迁移；若缺少正式发布远端，执行 `git remote add opensource https://github.com/kkltgood/tramoing.git`，保留 `origin` 作为日常开发仓库。

推送账号需要对 `kkltgood/tramoing` 有写权限。macOS 钥匙串中的开发仓库账号不一定有正式仓库权限；可安装 GitHub CLI（`brew install gh`），运行 `gh auth login --hostname github.com --git-protocol https --web`，在浏览器中登录有发布权限的账号。随后用 `gh api repos/kkltgood/tramoing --jq '.permissions.push'` 确认结果为 `true`。使用不同账号时，登录过程中若询问是否配置 Git 凭证助手，选择 No，再将凭证助手仅配置给正式仓库：

```sh
git config --local credential.https://github.com/kkltgood/tramoing.git.helper ''
git config --local --add credential.https://github.com/kkltgood/tramoing.git.helper '!gh auth git-credential'
```

macOS 系统代理不一定会被 Git 或 GitHub CLI 使用。若终端无法连接而浏览器正常，可在确认本机代理端口后为 `opensource` 单独设置 `git config --local remote.opensource.proxy http://127.0.0.1:7897`，并在 `gh` 命令前添加 `HTTPS_PROXY=http://127.0.0.1:7897`；换了端口时同步修改，不再使用代理时通过 `git config --local --unset remote.opensource.proxy` 清除。

发布步骤（以当前 `0.1.9` 为例）：

1. 确认根 `Cargo.toml` 的 `[workspace.package].version` 是准备发布的版本。后续版本需要同步更新 `Cargo.lock` 中的工作区版本并提交；可运行一次 `cargo check --workspace` 更新锁文件，再执行常规检查。将本次更新说明保存为 `docs/releases/v0.1.9.md`，发布脚本会用它生成 GitHub Release 正文和 `latest.json` 中的更新说明；缺少或为空时回退到 GitHub 自动说明。
2. 提交要发布的代码并推送分支和标签：

   ```sh
   git push opensource main
   git tag -a v0.1.9 -m "Release v0.1.9"
   git push opensource v0.1.9
   ```

3. 在 Actions 查看构建进度，成功后到 [Releases](https://github.com/kkltgood/tramoing/releases) 下载。后续发布请替换为新的版本号。

不要对 `opensource` 使用 `git push -u`，以免改变日常开发的上游。标签必须与 Cargo 版本完全一致，格式为 `v主版本.次版本.补丁版本`；当前 Windows 安装器要求三个版本分量均为不超过 65535 的非负整数，因此此流程暂不发布预览版本标签。

每次 Release 包含四个安装包、各自的 `.sha256` 以及 `latest.json`，共九个附件：

| 安装包 | 适用设备 | 更新清单平台键 |
| --- | --- | --- |
| `Tramoing-<version>-windows-x64-setup.exe` | Windows 10/11，Intel / AMD 64 位 | `windows-x86_64` |
| `Tramoing-<version>-windows-arm64-setup.exe` | Windows 11，ARM64（如 Snapdragon） | `windows-aarch64` |
| `Tramoing-<version>-macos-arm64.dmg` | macOS 11+，Apple Silicon（M 系列） | `macos-aarch64` |
| `Tramoing-<version>-macos-x64.dmg` | macOS 11+，Intel | `macos-x86_64` |

Windows 包均包含同架构的主程序和采集助手。`latest.json` 包含版本、发布说明、日期以及四个平台各自的下载地址、SHA-256 和文件大小。已有客户端的平台键保持兼容，旧 macOS 通用程序升级后会下载与当前运行架构一致的专用 DMG；在 Rosetta 或 Windows x64 模拟环境下运行的程序继续选择 x64 包，需要切换到 ARM64 时手动安装对应包。

所有平台的格式、测试、Clippy 和打包都通过后，发布任务再次核对全部八个输入文件及 SHA-256，再创建草稿、生成清单、上传全部九个附件并公开 Release。缺少任意架构的安装包或校验文件都会停止发布。清单中的安装包地址固定到本次版本标签，避免下一次发布时下载到不同文件。打包失败不会发布；上传失败会保留草稿供重试。已经公开的同版本 Release 不会被覆盖，需要修复时发布新版本。历史 Release 保留原有文件名；新的拆分包从采用此流程的下一版本开始提供。当前项目不支持 Linux，因此不生成 Linux 安装包。

Windows 安装程序暂未配置代码签名；macOS 的 ad-hoc 签名不等于 Developer ID 签名和公证，系统仍可能显示来源或安全提示。

## 更新源

默认更新源为 `https://github.com/kkltgood/tramoing/releases/latest/download/latest.json`，源码构建和官方安装包均可直接使用，不需要 GitHub 令牌，也不占用 GitHub API 的匿名请求配额。上面的发布流程自动提供该附件。首次发布带清单的版本前，检查会提示“更新清单尚未发布”，不会误报已是最新版。

新安装默认 `update_check=on`；旧配置已有的 `update_check=off` 会保留，可在设置 → 通用 → 启动与更新中开启。自动检查有 10 秒启动延迟和 24 小时间隔；macOS 收起后不驱动界面帧，检查可能延迟至面板展开。手动检查使用该区域的“检查更新”按钮或 `--check-update`，不受自动检查开关和间隔限制。`--mock` 不自动联网，但仍可显式手动检查。

发现新版本后，点击提示下载本平台安装包。下载在后台进行，核对文件大小及 SHA-256 后才显示安装入口；检查间隔到期也不会清除待下载提示或已就绪的安装包。下载没有总时限，连接和读写分别设置超时；断网及临时 HTTP 错误按 2、4、8、16、30 秒退避重试。下载缓存按版本、平台和预期 SHA-256 隔离，使用独占文件锁、HTTP Range / If-Range 和强 ETag 续传；拒绝压缩响应和错误的范围，服务端返回完整文件时从零写入。用户可暂停/继续，已请求的任务通过 `update/pending.json` 恢复为暂停状态，重启不自动联网下载。完整校验后才原子替换可安装文件。

Windows 点击“更新并重启”后以 `/UPDATE /APP_PID=<当前进程> /D=<当前程序目录>` 启动安装器。NSIS 要求 `/D=` 为最后一个参数，含空格时也不加引号。UAC 取消或启动失败时保留当前程序和下载包，界面显示重试入口；启动成功后程序正常退出并提交历史，安装器最多等待 15 秒，超时不改动安装文件。随后更新主程序及助手，只显示进度页，成功后通过 Explorer 以普通权限重新启动，保留配置、历史和自启设置。

macOS 点击“更新并重启”后在后台只读挂载 DMG，不打开 Finder。安装模块校验 bundle ID、清单版本、代码签名及已签名安装的 Team ID，将完整应用复制到原安装目录旁的独立暂存目录，再次校验后启动独立助手。助手确认就绪后才让主程序正常退出；以 PID 和进程启动时间等待旧进程结束，20 秒超时则停止替换。旧 bundle 重命名为备份，再将新 bundle 放到原路径；替换或 `open` 命令失败时尝试恢复旧 bundle。新应用通过 LaunchServices 打开，原路径、自启设置、用户配置和历史不变。准备失败不退出旧程序；启动请求成功不等于应用不会稍后崩溃，不以此宣称覆盖所有运行故障。若恢复失败，错误中保留备份路径供恢复。

新安装包也会识别已有的安装目录及旧产品名：旧客户端没有传入 `/UPDATE` 时，首次升级到新安装包就能跳过目录、组件和完成页。全新安装仍保留原来的安装向导。升级需要替换 Program Files 内文件及更新采集助手计划任务，系统管理员授权提示仍然必要。

如需自建更新服务，可在**构建前**设置 `TRAMOING_UPDATE_URL`，将静态 JSON 地址写入程序；运行时同名环境变量优先于构建配置，构建配置优先于默认 GitHub 地址。显式空值禁用更新源，手动检查会显示“未配置更新源”。

自建服务的 Windows 清单生成示例（替换为维护者自己的 HTTPS 下载地址）：

```powershell
powershell -ExecutionPolicy Bypass -File packaging\update\make-manifest.ps1 `
  -BaseUrl 'https://example.com/releases/v0.1.3' -Notes '发布说明'
```

脚本计算安装包的 SHA-256 和文件大小，生成 `dist/latest.json`。客户端支持的平台键为 `windows-x86_64`、`windows-aarch64`、`macos-aarch64`、`macos-x86_64`；脚本默认生成 Windows x64 条目，使用 `-Architecture arm64` 时读取 ARM64 安装包并生成 `windows-aarch64` 条目。该脚本每次生成单个 Windows 架构的清单，多架构发布使用上面的自动发布流程，或按同一格式合并条目：

```json
{
  "version": "0.1.3",
  "notes": "发布说明",
  "homepage": "https://example.com/releases",
  "platforms": {
    "windows-x86_64": {
      "url": "https://example.com/releases/Tramoing-0.1.3-windows-x64-setup.exe",
      "sha256": "由脚本生成的64位十六进制摘要",
      "size": 123456
    }
  }
}
```

使用可信 HTTPS 服务托管清单和安装包；SHA-256 用于校验文件完整性，不能代替来源认证。macOS 已安装应用支持确认后自动替换 `.app`；开发时的裸二进制、磁盘映像中运行的副本或无写权限的目录会提示先完成安装或通过 Finder 手动更新。

## 提交改动

一个 Pull Request 聚焦一个问题，说明复现方式、修改后的行为和实际运行的检查。涉及平台功能时区分单元测试、编译检查与真机验证；平台限制应同步写入故障排查文档。不要提交本机数据库、诊断日志、临时截图、凭证或构建输出。

温度回归：`cargo test -p tm-macos temperature -- --include-ignored --nocapture` 可读取真实 AppleSMC（只读）。检查 CPU/GPU 使用率旁的 °C、独立“显示温度”开关和重启保留、无传感器时的 `--°C`、Windows 96–192 DPI 温度列。

Windows CPU 内置采集由 `sensors/Tramoing.Sensors` 实现，固定 LibreHardwareMonitor 0.9.6，NuGet 传递依赖与完整性记录在 `packages.lock.json`。构建需要 .NET 10 SDK，安装包发布 self-contained 的同架构组件，用户无需预装 .NET。执行 `dotnet run --project sensors/Tramoing.Sensors.Tests -c Release`、`packaging/windows/build-sensors.ps1 -Architecture x64`（或 arm64）及 `scripts/verify-windows-sensors.ps1` 验证；打包脚本的 `-DotNet` 可指定 SDK 可执行文件。只有完整 `build.ps1` 构建会生成可安装的主程序、助手和传感器组件组合。开发版助手旁也需要同架构 `sensors/` 子目录。

温度管道 `Tramoing-Temperature-<SID>-v1` 沿用网络控制的账户 ACL、远程连接拒绝、双端程序路径及助手提权验证，但请求只有 Read，不能传入路径或命令。助手在独立线程管理 worker，worker 先等待握手，再进入硬件库；Windows Job Object 确保助手终止时关闭子进程。协议是 4 字节长度加 UTF-8 JSON，worker 帧上限 8 KiB；采样约 2 秒，15 秒 watchdog、60 秒失败退避、12 秒 UI 请求租期。关闭显示不会保留最后一次有效温度。仅开启 LHM 的 CPU 硬件组，不扫描主板 EC、磁盘或风扇控制。

Windows GPU 继续使用 D3DKMT_ADAPTER_PERFDATA（0.1°C 单位）。CPU 内置读数不可用或 GPU WDDM 不可用时，可选 WMI 桥在独立 PowerShell 进程查询已有 LHM/OHM，使用限长快照与 15 秒超时。ARM64 及在 ARM64 上模拟运行的 x64 程序不进入 LHM 的 CPUID 路径；当前内置 CPU 适配覆盖上游支持的 Intel/AMD，老 AMD Family 10h–16h 暂无适配，必须区分构建覆盖与传感器实机覆盖。

x64 安装包包含官方 PawnIO 2.2.0，构建校验固定 SHA-256 及发布者 Authenticode 签名；构建和运行采集均不安装驱动。安装器默认选中的独立 CPU 温度组件使用官方 `-install -silent`，跳过已安装的较新版本，识别 3010 重启状态，错误时保留其他功能并说明原因。ARM64 包不携带或安装该驱动。卸载不删除共享 PawnIO。第三方说明和 NuGet 许可证随组件打包。

发布前实机矩阵应覆盖 Intel/AMD 台式机与笔记本、Windows 10/11 x64、Windows 11 ARM64、无驱动/旧驱动/驱动受策略限制、启停温度、助手被结束、硬件查询卡住、WMI 运行/退出与 GPU 驱动不支持。普通权限的 smoke test 只证明进程协议和权限状态，不代表真实温度读取成功。
