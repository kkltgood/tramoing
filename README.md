# Tramoing

跨平台（macOS / Windows）菜单栏流量监控工具。状态栏一个图标，点一下弹出小面板，实时显示「哪些程序在用流量、各占多少下行 / 上行带宽」。

> **命名说明（2026-11-18）**：产品对外名称改为 **Tramoing**（仓库目录也从 `traffic-monitoring` 改成 `tramoing`）。
> 只改「用户看得到的名字」——安装目录、快捷方式、控制面板条目、窗口标题、安装包文件名；
> 可执行文件也随之改成 `tramoing.exe` / `tramoing-helper.exe`（Cargo 的 `[[bin]]` 名一起改）。
> **真正没动的只有内部标识**：数据目录 `%LOCALAPPDATA%\TrafficMonitor`、计划任务 `TrafficMonitorHelper`、
> ETW 会话 `TrafficMonitor-Etw`、管道 `\\.\pipe\traffic-monitor-v1`、HKCU Run 值 ——
> 这些一旦改，旧版的历史数据和已注册任务就变成孤儿了，测试时分不清是程序坏了还是迁移没做。
> macOS 的 `.app` 名与 `CFBundleExecutable` 也一起改成了 `Tramoing.app` / `tramoing`，
> 但 **bundle id 与数据目录仍是 `dev.guolongkang.trafficmonitor` / `~/Library/Application Support/TrafficMonitor`**，
> 免得升级后历史记录找不到。

- **版本**：0.1.0
- **技术栈**：纯 Rust + egui / eframe，无 Electron、无 Python、无外部运行时
- **当前状态**：macOS 已完成并打包验证；Windows 代码完成、`tray-app` 与 `tm-windows` 已通过交叉编译检查（0 警告），**等待真机功能测试与 ETW 事件表校准**（见 [第 6 节](#6-windows-真机测试指南)，本轮修复见 [6.8](#68-本轮修复windows-面板定位等)）

![主面板](docs/screenshot-panel.png)

---

## 1. 功能清单

### 1.1 菜单栏 / 任务栏

**macOS（状态栏文字）**

- 波形图标 + 速率文字；速率每秒刷新一次
- 上传 / 下载**上下两行**显示，上行绿箭头、下行蓝箭头，数字保持系统色（保证可读性）
- 三档显示模式：`图标+网速` / `仅网速` / `仅图标`，运行时切换即时生效
- 可选「单色」模式，全部文字跟随系统色
- 鼠标悬停显示完整数值（菜单栏里只放得下缩写）

**Windows（任务栏贴片）**

Windows 的托盘图标是定死的方形槽（150% DPI 下 24×24），塞不进 `↓1.2M`，
所以数字画在一个**独立的小窗口**里，位置落在任务栏条内、系统托盘区左侧：

- 两行缩写：`↓ 7.5M` / `↑ 841K`，长度恒为 4 个字符，数字列不会左右跳。
  方向图标是**手绘的细箭头**（竖杆 + 三角头），高度按数字字高对齐
- 背景是**抠出来的**（纯黑颜色键），所以看起来就是任务栏自己的一部分，
  不是浮在上面的小方块
- **点贴片等于点托盘图标**：面板没开就打开、开着就收起（抠出来的区域收不到
  鼠标消息，所以点击是自己轮询认的，见 [6.13](#613-windows-任务栏网速贴片)）
- 任务栏上有别的同类工具（TrafficMonitor 之类）时自动让到它左边，两边的字不会叠在一起
- 设置里可关（「任务栏网速」），也可以切「网速配色」彩色/单色

![任务栏贴片](docs/screenshot-tray-badge.png)

### 1.2 弹出面板

- 顶部两个大数字：下载 / 上行，等分两栏，直接显示本帧速率、不做数字缓动动画（下载绿、上传蓝）
- 应用列表：应用图标 + 名称 + 下行速率 + 上行速率，两列数字定宽右对齐
- 列表最多显示 8 个应用（设置里可改 5 / 8 / 12），超出可滚动
- 时间窗口四档：`实时` / `1分钟` / `5分钟` / `1小时`，决定列表按哪段时间聚合排序
- 标题栏显示最近 24 小时（或本次运行）累计流量
- 近 10 分钟单应用流量超阈值时给一行提醒
- 底部一条页脚：导出历史 / 退出两个图标按钮，导出结果就地显示在中间
- 失焦自动收起，点击菜单栏图标可开可关
- 深色 / 浅色 / 跟随系统三档外观，中文 / 英文 / 自动三档语言

### 1.3 数据与设置

- SQLite 历史库，按**分钟**存增量，默认保留 30 天
- CSV 导出（界面按钮或命令行 `--export`）
- 开机自启开关（界面或命令行）
- 设置写成纯文本 `settings.conf`，坏文件不会导致程序起不来

### 1.4 界面截图

| 状态 | 截图 |
|---|---|
| 数据面板 | ![面板](docs/screenshot-panel.png) |
| 加载骨架屏 | ![加载](docs/screenshot-loading.png) |
| 设置页 | ![设置](docs/screenshot-settings.png) |
| Windows 任务栏贴片 | ![任务栏](docs/screenshot-tray-badge.png) |

---

## 2. 目录结构

```text
tramoing/
├── Cargo.toml                     # workspace 定义，版本 0.1.0 / edition 2021
├── Cargo.lock
├── README.md                      # 本文档
├── assets/                        # 图标源（gen-icon.rs 生成 PNG / icns）
├── crates/
│   ├── tm-core/                   # 平台无关的速率/窗口聚合核心
│   ├── tm-sampler/                # Sampler trait + MockSampler
│   ├── tm-macos/                  # macOS：nettop 按连接采集、流量归属、应用归并
│   ├── tm-windows/                # Windows：ETW 助手、命名管道、进程路径、屏幕工作区
│   ├── tm-store/                  # SQLite 历史库（rusqlite bundled）
│   ├── tm-spike-mac/              # macOS 探针小程序（开发用）
│   └── tray-app/                  # egui 面板 + 设置 + 自启 + 采样线程
├── packaging/
│   ├── macos/Info.plist
│   └── windows/                   # NSIS 安装包 + build.ps1
├── scripts/package-macos.sh       # macOS 打包：.app + DMG
└── docs/screenshot-*.png
```

---

## 3. 架构

### 3.1 分层

| crate | 职责 | 是否依赖系统 API |
|---|---|---|
| `tm-core` | 累计值差分、EMA 平滑、按应用归并、1m/5m/1h 窗口聚合 | 否，纯逻辑 |
| `tm-sampler` | `Sampler` trait 与演示数据实现 | 否 |
| `tm-macos` | `nettop` 子进程 + pty、按连接解析与流量归属、pid→应用归并、AppKit 图标/状态栏 | 仅 macOS |
| `tm-windows` | ETW 会话、聚合、命名管道协议、进程路径解析、屏幕工作区与任务栏明暗 | 仅 Windows |
| `tm-store` | SQLite 建表/写入/查询/清理/CSV 导出 | 否（SQLite 已 bundled） |
| `tray-app` | 采样线程、界面、设置、自启、导出 | 平台相关部分用 cfg 隔离 |

**关键约束**：`tm-core`、`tm-sampler`、`tm-store` 以及 `tm-windows` 的 `proto` / `agg` 模块完全不碰系统 API，所以能在 macOS 上直接跑单测。这也是 114 个测试里有一半以上与平台无关的原因。

### 3.2 数据流

```text
macOS:
  nettop -x -n -L 0 -s 1          # CSV，按连接
      └─(pty，行缓冲)→ tm-macos::FlowParser → FlowAccounting（归属）→ 每秒一帧累计值

Windows:
  tramoing-helper.exe（管理员，登录自启）
      └─ ETW: Microsoft-Windows-Kernel-Network
      └─ Aggregator（按 PID 累加累计字节）
      └─ \\.\pipe\traffic-monitor-v1（每秒一帧，手写小端协议）
      └─ WindowsSampler（普通权限的界面进程只读管道）

两条支路汇合：
  Sampler → tm_core::RateCalculator（差分 + 平滑 + 按应用归并）
          ├─→ 面板：实时速率 + 1m/5m/1h 窗口
          ├─→ tm_store::Store（每 5 秒落一次增量，按分钟upsert）
          └─→ 24 小时合计（每 30 秒查一次库）
```

### 3.3 几个关键设计决策

| 决策 | 原因 |
|---|---|
| 采样器送**累计值**，统一由 `RateCalculator` 差分 | macOS 与 Windows 复用同一套速率链路；进程重启归零时按「重新计」处理，不产生负数 |
| 速率平滑 alpha=0.7，并按真实帧间隔换算 `1-(1-alpha)^dt` | 0.5 会让数字慢半拍；按 dt 换算后采样抖动不影响观感 |
| **第一帧快照不下发** | 第一帧只能建基线、速率必然全 0，下发会先闪一下「没有流量」；界面停在骨架屏等第二帧 |
| 按分钟存**增量**，主键 `(minute, app)` | 查询直接 `SUM`，不用处理进程重启归零；1 小时和 24 小时视图共用一张表 |
| 历史库开 WAL | 后台写盘与界面查询不互相阻塞 |
| 界面线程只渲染 | 起子进程、读流、syscall、写库全在采样线程，弹窗不会因为 IO 掉帧 |
| 管道协议手写小端编码 | 一帧只有几个数字加进程名，省掉 serde/bincode 依赖和编译时间 |
| macOS 给 `nettop` 开 pty | `nettop` 的 stdout 接管道时走 4KB 全缓冲，`-s 1` 实际 3 秒才吐一帧；接 tty 才是精确 1 秒 |
| macOS 按**连接**统计，不用 `-P` 进程汇总 | 系统代理下一份下载会出现在三个 socket 上（代理外网收、代理回环发、App 回环收）。汇总口径要么算两遍（实测 6 MB 变 12.5 MB，上下行接近），要么只能记到代理名下。按连接配对后可以把回环客户端侧回填给发起方，总量只取外网连接 |
| 回环两端按「监听端口」判服务端 | 服务端那一侧和客户端侧是同一份数据，用 `state=Listen` 的端口集合就能区分，不需要猜端口号大小 |
| 代理自己的流量 = 外网 − 已回填部分 | 代理的自更新、DNS 不该被减没，也不能重复计；链式代理的第一跳没有外网流量，按「同时在服务回环且连向下一跳」也识别为中继 |
| 本机互访不计入 | 浏览器 ↔ 本地 dev server 两端都不是中继，这份流量从没上网卡，计进去只会让数字虚高 |
| TUN 模式按「量守恒」认转发进程 | `utun` 和物理网卡之间没有端口可配对，唯一可靠的关系是：转发进程的物理流量 ≈ 隧道里所有 App 的流量（略多一点点是加密开销）。认不出就退回外网口径，保证总量不翻倍 |
| 系统服务用一只齿轮，不按首字母散列 | `mDNSResponder`、`netbiosd` 这类守护进程系统压根没给图标，首字母块会让它们看起来像「某个叫 M 的应用」；齿轮是唯一既不抢戏、又能说明「这不是你装的 app」的符号。判断只看路径前缀，不猜进程名 |
| 页脚整条用 painter 画，只占一次位 | 老版页脚是 `add_space + hairline + 两个 Button` 摊出来的：每个 `allocate_*` 都会再插一次 `item_spacing`，按钮高度又跟着字形走（中英文不一样），实测比预留的 `FOOTER_BAND` 高出 12pt，把 导出/退出 顶出了面板底边，换语言还会再变一次。现在整条带子按面板底边定位、一次占位，图标和提示都画在里面，高度恒等于常量 |
| 下载绿、上传蓝，且**定义只有一份** | 绿色表示「进来」，和多数网络工具的直觉一致。上下行色同时出现在面板、macOS 菜单栏两行小字、Windows 任务栏贴片三处，所以颜色定义放在 `tm_core::color`（`download(on_dark)` / `upload(on_dark)`），三处各自转成 `Color32` / `NSColor`。以前是各写各的，面板改了色菜单栏不跟着变 |
| Windows 拆成两个进程 | ETW 按进程统计必须提权；助手提权跑，托盘程序始终普通权限 |
| Windows 网速数字不画进托盘图标，另开一个贴片窗口 | `Shell_NotifyIcon` 只接受方形小图标，`↓1.2M` 塞进去会被系统缩成一团糊；图标槽的 rect 是壳定死的，撑开 HICON 也没用 |
| 贴片用**颜色键**抠背景，不用逐像素 alpha | eframe 的 GL 配置在这台机器上没有 alpha 通道（`Cannot create transparent window`），`UpdateLayeredWindow` 那套用不上；纯黑颜色键既能抠干净，又顺带让鼠标穿透。详见 [6.13](#613-windows-任务栏网速贴片) |

---

## 4. 通用开发命令

工具链：Rust 1.89.0（edition 2021），无需额外系统依赖即可跑单测。

```bash
cargo test                       # 全部 114 个单测
cargo build                      # debug 构建整个工作区
cargo build --release -p tray-app

# 交叉检查（在 macOS 上可检查 Windows 侧的纯逻辑部分）
cargo check --target x86_64-pc-windows-msvc -p tm-core -p tm-sampler -p tm-windows
```

### 4.1 命令行参数（`tramoing`）

| 参数 | 作用 |
|---|---|
| `--mock` | 用演示数据跑完整界面，不需要采样权限 |
| `--show` | 启动后立即展开面板（默认隐藏，只出状态栏图标） |
| `--settings` | 启动后直接打开设置页 |
| `--export` | 无界面导出全部历史为 CSV，打印结果后退出 |
| `--autostart [on\|off\|status]` | 查询 / 修改开机自启，不启动界面 |

macOS 上必须放进 `.app` 包运行，直接跑 `target/debug/tramoing` 会看到「图标不出现」的假象——裸二进制的状态栏窗口 `isOnActiveSpace = false`，系统不渲染它。

### 4.2 环境变量（调试用）

| 变量 | 作用 |
|---|---|
| `TM_FORCE_MOCK=1` | 等价于 `--mock` |
| `TM_SHOW_ON_START=1` | 等价于 `--show` |
| `TM_VIEW=settings` | 启动时显示设置页 |
| `TM_WINDOW=1m\|5m\|1h` | 固定初始时间窗口 |
| `TM_TRACE=1` | 打印采样帧间隔（macOS 还打印状态栏窗口诊断信息） |
| `TM_TRAY_PNG=/tmp/x.png` | macOS：把状态栏按钮离屏渲染成 PNG（状态项被折叠进 `«` 时唯一可靠的检查手段） |
| `TM_NETTOP_BIN` | 覆盖 `nettop` 路径 |
| `TM_DB_PATH` | 覆盖历史库路径 |
| `TM_PANEL_Y` / `TM_SCROLL` | 截图时调整面板位置 / 给列表一个初始滚动位置（之后照常按整行吸附） |
| `TM_TRACE_LIST` | 打印列表可用高度、能放几行、整行高度，以及设置页余量（调固定高度时用） |
| `TM_TOAST="已导出 12 行"` | 启动时先塞一条页脚提示，用来核对提示文案的排版 |

---

## 5. macOS

### 5.1 本地运行

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo run -p tray-app -- --show            # 开发调试
cargo run -p tray-app -- --show --mock     # 只看界面
open dist/Tramoing.app                     # 验证真实形态（推荐）
```

免 root：采集用的是系统自带的 `nettop`，不读 raw socket。

### 5.2 打包

```bash
./scripts/package-macos.sh           # .app + DMG（两个 target 都装了会做通用二进制）
./scripts/package-macos.sh --no-dmg  # 只出 .app
```

产物：`dist/Tramoing.app`（约 12 MB，arm64 + x86_64 通用二进制）、`dist/Tramoing-0.1.0.dmg`（约 6.6 MB）。

签名是 **ad-hoc**（`codesign -s -`）：本机跑没问题，发给别人会被 Gatekeeper 拦，需要右键「打开」或 `xattr -dr com.apple.quarantine`。正式分发需要 Developer ID + notarytool 公证。

### 5.3 已知现象

- 菜单栏被其他图标塞满时，系统会把本应用收进 `«` 折叠区，看起来像"图标没出现"。收掉一两个不常用图标即可，与程序无关。
- 应用图标取自 `NSWorkspace`，取不到时退化为首字母色块；系统服务（`mDNSResponder`、`netbiosd` 这类没有 `.app` 的守护进程）统一画齿轮，见 5.4。Chrome 等应用的 helper 进程会归并到最外层 `.app`，不会重复占位。
- 走系统代理（Clash / Surge / 公司代理）的流量会**回填给发起请求的 App**；代理进程只保留它自己的那部分流量。链式代理只记最外层一次。
- **TUN 模式**（Clash 增强模式 / WireGuard）靠「量守恒」识别转发进程：隧道侧（`utun*`）的字节回填给 App，转发进程的物理流量按隧道总量抵扣。判定条件是「没有隧道侧流量、物理流量与隧道总量最接近且比值在 0.7–3.0 之间」；认不出来时宁可不回填（总量仍然准，只是退化成只看到代理）。同一时刻只支持一个隧道转发进程。
- 采样器读的是 nettop 的 socket 字节数，不含 TCP/IP 头与 ACK，所以总量会比网卡计数器略低（实测下行 90% 左右，上行差得多一些，属正常）。
- 没有 `.app` 外壳的进程用**可执行文件全路径**当归并键（列表里显示路径里的文件名）；只有路径都读不到时（极少见）才退回 nettop 给的进程名，那种名字可能被截断到 20 字符。

### 5.4 应用图标（macOS）

三种来源，优先级从高到低：

| 行 | 图标 | 为什么 |
|---|---|---|
| 普通应用 | `.app` 的真实图标 | `NSWorkspace.iconForFile(bundle)` 拿到的就是 Finder / Dock 里那张，不用自己解 `.icns` |
| 系统服务 | 灰色齿轮 | `mDNSResponder`（`/usr/sbin`）、`netbiosd` 这类守护进程根本没有 `.app`，macOS 从来没给它们画过图标 —— Finder 和活动监视器里也是一片空白。统一发一只齿轮：既看得出「这是系统在跑的服务」，又不会和某个应用的品牌色混淆 |
| 其余 | 首字母色块 | 既没有 bundle、又不算系统服务（典型是 `/Library/Application Support/clash-verge-service/cores/verge-mihomo` 这种被服务模式复制出来的内核），按名字散列一个稳定色 |

判断依据是**归并键的路径前缀**（`tm_core::is_system_service`）：`/usr/sbin/`、`/usr/libexec/`、`/sbin/`、`/System/Library/` 算系统服务，`.app` 结尾的排除在外（`/System/Applications/Finder.app` 有自己的图标）。`/usr/bin/` 不算 —— 那是用户随手敲的 CLI（`curl`、`ssh`），全画成齿轮反而淹没了真正的后台服务。

这也正是「没有 `.app` 就用全路径当键」的原因：键退化成进程名，`/usr/sbin` 这层信息就丢了，图标层无从判断。

齿轮不是贴图，和托盘图标一样是画出来的（`crates/tray-app/src/icon.rs::gear`）：8 个齿、齿顶半径 13.9、齿根圆 9.4、中心孔 4.4（都在 36×36 的设计坐标里），4x 超采样再盒式降采样。输出纯白 + 透明底，明暗主题只改 `painter.image` 的 tint，不用烘两套图。

---

## 6. Windows 真机测试指南

> 本节是给今晚在 Windows 上做验证准备的。**Windows 侧尚未在真机跑过**，安装包脚本也未在本机执行（开发环境是 macOS，只能做交叉编译检查）。请按下面顺序测。

### 6.1 环境准备

1. Visual Studio Build Tools（MSVC 链接器 + Windows SDK）
2. Rust 工具链（安装时选 `x86_64-pc-windows-msvc` 默认目标）
   ```powershell
   rustup default stable-x86_64-pc-windows-msvc
   rustc --version
   ```
3. 仅打包安装包才需要 NSIS 3：`winget install NSIS.NSIS`

### 6.2 第一步：不开助手，先验证界面

```powershell
cargo run -p tray-app -- --show --mock
```

用演示数据跑通整条 UI 链路：面板弹出、两栏速率、应用列表、时间窗口切换、设置页、主题与语言、导出。这一步**不需要管理员权限**，如果这里就有问题，先不要往下走。

### 6.3 第二步：开启真实采集

Windows 拿「每进程流量」必须提权。仓库里把它拆成两个可执行文件：

- `tramoing.exe`：托盘 + 面板，普通权限
- `tramoing-helper.exe`：ETW 采集，登录时以最高权限静默运行

**手动开发验证**：

```powershell
# 普通 PowerShell：编译
cargo build --release -p tray-app
cargo build --release -p tm-windows --bin tramoing-helper

# 管理员 PowerShell：注册采集助手
# 先确定 tramoing-helper.exe 最终位置，--install 会把当前路径写进计划任务
# 装好之后不要再移动这个文件，否则任务会指向旧路径
.\target\release\tramoing-helper.exe --install

# 确认任务已注册并正在运行
schtasks /Query /TN TrafficMonitorHelper /FO LIST

# 普通 PowerShell：启动界面
.\target\release\tramoing.exe
```

日志位置：

- 助手运行日志：`%LOCALAPPDATA%\TrafficMonitor\helper.log`
- ETW 事件诊断：`%LOCALAPPDATA%\TrafficMonitor\etw-dump.log`（仅 `--dump-events` 时产生）

`--install` 注册的是**登录时以最高权限运行的计划任务**（`schtasks /SC ONLOGON /RL HIGHEST`），不是 Windows 服务；`--uninstall` 会停任务、删任务、杀掉助手进程。

### 6.4 第三步：功能测试用例

| # | 操作 | 预期结果 | 备注 |
|---|---|---|---|
| T1 | 启动 `tramoing.exe` | 托盘出现波形图标，**不自动弹出面板** | 见 6.6「启动就弹面板」 |
| T2 | 单击托盘图标 | 弹出面板；再点一次收起 | 失焦约 0.3 秒后也会自动收起 |
| T3 | 打开面板观察数值 | 每秒更新一次；顶部下载 / 上行两栏等宽 | 不需要等 2~3 秒 |
| T4 | 进行一个已知方向的传输（如下载一个大文件） | 下行数字明显上升，上行不应同步暴涨 | 若相反，执行 6.5 校准 |
| T5 | 打开 Chrome 等有多进程的程序 | 列表里合并为**一行**，不是五六个 `chrome.exe` | Windows 按 exe 全路径归并 |
| T6 | 依次点击 `实时 / 1分钟 / 5分钟 / 1小时` | 列表内容随之变化，滑块滑到对应档位 | |
| T7 | 设置页逐项切换：外观、语言、最多展示、保留天数、流量提醒 | 立即生效，关闭再打开设置仍保留 | 配置在 `%LOCALAPPDATA%\TrafficMonitor\settings.conf` |
| T8 | 设置里打开「开机自启」 | 注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\TrafficMonitor` 出现 | 命令行 `tramoing.exe --autostart status` 可交叉验证 |
| T9 | 等待 10 秒后查看数据目录 | 生成 `history.db`，且大小不再为 0 | 每 5 秒落一次盘 |
| T10 | 点击面板左下「导出」 | `Downloads` 下出现 `Tramoing-<时间戳>.csv` | 没有 Downloads 目录时退到应用数据目录 |
| T11 | 运行 `tramoing.exe --export` | 终端打印导出行数与路径，程序立即退出 | 无界面 |
| T12 | 注销并重新登录 | 托盘自动出现，助手任务自动以最高权限运行 | 验证计划任务 |
| T13 | `tramoing-helper.exe --uninstall` 后再启动界面 | 面板停在骨架屏、没有应用列表；控制台打印"连不上采集助手"，每 2 秒重连一次 | 助手是进程级数据的前提；助手恢复后界面会自动重连 |
| T14 | 运行 NSIS 安装包，再运行卸载程序 | 安装到 `Program Files\Tramoing`；开始菜单出现 `Tramoing`；卸载时先停任务再删文件；历史数据默认保留 | 只验证过「生成安装包」，安装/卸载流程待测 |

### 6.5 【重点】ETW 事件方向校准

这是 Windows 侧**唯一依赖记忆而非规范**的部分，请在真机上确认一次。

`Microsoft-Windows-Kernel-Network` 的事件数据前 8 字节是 `PID:u32, Size:u32`，代码靠事件 ID 判断方向：

```rust
// crates/tm-windows/src/etw.rs
10 | 12 | 14 | 16 => Tx   // 发送
11 | 13 | 15 | 17 => Rx   // 接收
```

**校准步骤**：

1. 先停掉正在运行的助手，避免同名 ETW 会话冲突：
   ```powershell
   schtasks /End /TN TrafficMonitorHelper
   taskkill /IM tramoing-helper.exe /F
   ```
2. 在**管理员** PowerShell 里运行诊断模式（窗口是隐藏子系统，Ctrl+C 不一定生效，停止用 `taskkill`）：
   ```powershell
   .\target\release\tramoing-helper.exe --dump-events
   ```
3. 制造**只有单一方向**的流量。推荐：在内网起一个 HTTP 服务，用 `curl -O` 只下载一个大文件；再用 `curl -T` 只上传一个大文件。
4. 每次动作后查看 `%LOCALAPPDATA%\TrafficMonitor\etw-dump.log`，形如：
   ```text
   id=14 pid=12345 size=1460 len=88 bytes=39 30 00 00 b4 05 00 00 ...
   ```
   记下「纯下载」时占多数的 `id`，以及「纯上传」时占多数的 `id`。
5. 把 `etw.rs` 里 `direction()` 的两组 ID 改成实测值，`cargo test -p tm-windows` 确认内部一致性，重新构建助手。

判定标准：改完之后，单独下载时下行数字涨、上行基本不动；单独上传时相反。若方向仍反了，说明 ID 分组需要整体对调。

> 日志里的 `size` 是**单次事件**的字节数，不是累计值；聚合由 `Aggregator` 完成。

### 6.6 故障排查

| 现象 | 可能原因 | 处理 |
|---|---|---|
| 托盘图标"不出现" | Win11 默认把新图标收进任务栏的 `«` 溢出区 | 点任务栏的 `«` 展开，把图标拖到常显区；确认 `tramoing.exe` 进程还活着 |
| 托盘图标看不见/几乎是空白 | 图标颜色跟任务栏底色撞了（深色任务栏要白线、浅色要黑线，代码已按注册表自适应，每 3 秒复查一次） | 若仍异常，记录 Windows 版本 + 任务栏明暗设置 |
| 启动就弹出面板 | eframe 首帧强制显示（macOS 上连发 3 帧隐藏命令抵消；Windows 改成开局停在屏幕外） | 若复现，请记录 Windows 版本和是否首次启动 |
| 点托盘图标后看不到面板 | ① 面板被摆到屏幕外（6.8）；② 收起时把窗口真隐藏，eframe 的 `update()` 停摆，点击没人处理（6.12） | 已在 6.8 / 6.12 修复；若仍复现，`TM_TRACE_FRAMES=1` 启动看收起后 `[ui] frame=` 还在不在涨 |
| 面板一直"等待数据"、没有应用列表 | 助手没装、没提权、或崩了 | 看 `helper.log`；确认计划任务状态；`tramoing-helper.exe --help` 会把用法写进 `helper.log` |
| 有总流量但没有进程级数据 | helper 未运行或管道连不上 | 管理员运行 `--install`；检查 `\\.\pipe\traffic-monitor-v1` 是否存在 |
| 上下行数字对调 | ETW 事件 ID 表不对 | 执行 6.5 |
| 进程名显示为 `pid 1234` | 进程刚退出，或即使提权也查不到路径 | 正常降级，不影响流量统计 |
| 数据不更新 | ETW 会话被同名会话占用 | `tramoing-helper.exe --uninstall` 后重装；检查是否有旧助手进程 |
| `makensis` 找不到 | 没装 NSIS | `winget install NSIS.NSIS` |

### 6.7 Windows 已知差异与未验证项

请测试时特别留意，这些**不是 bug 或已有明确说明**：

1. **ETW 事件 ID 表待校准**（6.5）——目前唯一未经真机确认的核心逻辑。
2. **安装包脚本只在真机跑通了「生成」这一步**（见 6.10）：`dist\Tramoing-0.1.0-setup.exe` 已产出，但安装 → 卸载全流程、UAC 提示和 SmartScreen 拦截尚未验证。
3. **任务栏没有速率文字**：Windows 托盘项不支持文字标题（`tray-icon` 的 `set_title` 只对 macOS 生效），
   所以面板之外的实时速率只有一个出口——**鼠标悬停**（tooltip 每秒更新，显示 `↓ 1.2 MB/s  ↑ 180 KB/s`）。
   设置页里「菜单栏」「菜单栏配色」两行因此只在 macOS 显示。
4. **少数应用仍显示首字母色块**：图标走 `SHGetFileInfoW` 从 exe 资源里取（见 6.11），
   取不到就退回首字母方块。已知取不到的一类：装在**受 ACL 保护的服务目录**里的核心程序
   （例如 `C:\ProgramData\clash-verge-service\cores\verge-mihomo.exe`），普通权限读不到文件，
   也就取不到图标。这是权限模型决定的，不是渲染 bug。
5. **未做代码签名**：安装包会触发 SmartScreen 警告。
6. 滚轮手感、极端长会话（>24h）的内存表现尚未验证。

### 6.8 本轮修复（面板定位 / 设置页高度）

之前 Windows 上有一批只在 macOS 验证过、Windows 分支从未真机跑过的逻辑，逐条修掉：

| 问题 | 原因 | 修法 |
|---|---|---|
| 面板打开位置/方向都不对 | `place_below()` 写死「在图标下方展开」——macOS 菜单栏在顶部成立，Windows 任务栏在底部就把面板顶到屏幕外 | 方向由「图标在工作区上半还是下半」决定；工作区取图标所在显示器的 `rcWork`（扣掉任务栏），副屏/竖屏也准 |
| 面板展开动画像在往下掉 | 窗口高度动画只钉了顶边 | 向上展开时每帧按当前高度重算顶边，底边始终贴着图标 |
| 悬停看不到实时速率 | `sync_tray_title()` 开头 `if !macos { return }`，把 tooltip 一起跳过了 | tooltip 提到平台分支之外，两平台每秒更新 |
| 托盘图标在深色任务栏上隐形 | 图标是给 macOS 模板图设计的纯黑 36px，Windows 照原样画 | 按注册表 `SystemUsesLightTheme` 出白线/黑线，32px；主题切换 3 秒内自动重画 |
| 面板一打开就自己关掉 | Windows 的 `SetForegroundWindow` 可能被前台规则拒绝，焦点一直为 false 就触发「失焦收起」 | 只有**曾经拿到过焦点**才允许失焦收起；没焦点时再点一次图标收起 |
| 界面全是英文 | 中文字体候选路径只有 macOS 的 | Windows 依次找 微软雅黑 / 等线 / 黑体 / 宋体，`.ttc` 用 `FontData::index` 取字面 |
| 设置里两个开关调了没反应 | 那两个选项只在 macOS 生效，却照样显示 | Windows 上不再渲染这两行 |
| 加载态只有三行灰条、下面一片空白 | 骨架屏写死画 3 行，而面板高度固定 | 骨架铺满可视行数（7 行）、逐行变淡，并画成同样的行盒；空状态文案在列表格里垂直居中 |
| 点开设置时"卡一下 + 面板变矮" | 窗口高度按内容逐帧算出来，切页/换数据都会让它变 | **面板改成固定高度 411pt**（`PANEL_H`，正好 7 行列表）：列表吃掉剩余空间并按整行裁剪，超出滚动；页脚与设置页的"开机自启"都贴底。窗口尺寸从此不再变化 |
| 列表滚动后上下各露半截行，底部还空一截 | ① 触控板停下时偏移量停在 12.3 这种半行值；② 列表下方无条件预留了提醒 22pt + 提示 18pt，没内容时就是一块死白 | ① 滚动停下后把偏移吸附到整行（`snap_scroll`）；② 只保留页脚带，提醒行真有内容时才占位；提示改画在页脚里，不再单占一行 |
| 设置页中间一大块空白 | 7 行设置按 32.5pt 排，面板比内容高 110pt，全堆在"流量提醒"和"开机自启"之间 | 设置行改成 37pt 一行（行高 32 + 间距 5），并把面板高度压到刚好装下：余量只剩 10pt |
| 列表看着像普通表格 | 行与行之间没有层级；`1.3 MB/s` 整串落字，单位占一半宽度；字母图标是一整块高饱和色块 | **每一行做成独立的小卡**：1px 淡描边 + 6pt 圆角 + 5pt 行距，条目边界一眼可见；数值用等宽字体、单位缩到 8.5pt 并收到固定单位列（小数点竖着对齐）；字母图标改成同色系淡底 + 亮字；悬停时底色提亮一档、描边渐变到强调色（120ms） |
| 滚动条把手压在行盒描边上 | 用的是 egui 自带的浮动把手，它画在内容矩形之内 | 关掉自带把手，改成自绘的 3pt 指示条，专门占行盒右侧的 9pt 过道；滚动仍然按整行吸附 |
| 菜单栏速率宽度来回跳 | 菜单栏项按标题宽度自适应，而 `compact()` 是"最短写法"：`9.9M`/`10M`、`56B`/`1.2M` 长度不同，`820K` 和 `1.2M` 长度相同但 K/M 字母宽度不同；空闲时 `0` 只有一个字符 | 速率固定成 **4 个字符**（`menu_speed`：`"  0B"`、`" 820K"`、`"1.2M"`），标题字体从"等宽数字"换成**整段等宽**（SF Mono）；实测连续 7 次刷新按钮宽度恒为 56×22（带图标 76×22） |

验证方式：`cargo test --workspace`（114 个用例，含定位几何、滚动吸附、速率定宽的单测）；
`cargo check --target x86_64-pc-windows-msvc`（`tray-app` + `tm-windows`，0 警告）。
真机行为仍需按 6.4 的用例表复测。

### 6.9 本轮修复（Windows 真机运行）

在 3840×2160 @150% 的 Windows 真机上把两条链路都跑通了（`--show --mock` 走假数据、
不带 `--mock` 走真实 ETW 助手，275 个进程、稳定 1.00s 一帧），修掉的是只有真机才暴露的问题：

| 问题 | 原因 | 修法 |
|---|---|---|
| `cargo check --workspace` 在 Windows 上直接编不过 | workspace 成员是无条件列出的，`tm-macos` / `tm-spike-mac` 没有 crate 级门控，Windows 上会去编 libc / objc2 | `tm-macos` 加 `#![cfg(target_os = "macos")]`；`tm-spike-mac` 给非 macOS 平台留一个空壳 `main` |
| 开机自启的查询/写入会闪一下黑框，环境禁掉 `reg.exe` 时整个程序起不来 | 在 `App::new()` 里 spawn `reg.exe query`，属于启动路径 | 改调 advapi32（`RegGetValueW` / `RegSetKeyValueW` / `RegDeleteKeyValueW`），落到新模块 `tm-windows::registry_run` |
| 助手被强杀过一次之后就永远起不来，日志还提示「请确认管理员权限」 | `StartTraceW` 返回 183（同名会话残留）被当成普通失败 | 183 时先探管道：通 → `AlreadyRunning` 安静退出；不通 → 按名字停掉僵尸会话再重试一次 |
| 列表底部堆一串「0 B/s / 0 B/s」的空行 | 每帧都出现的空转进程走 `a*0+(1-a)*prev`，EMA 永远到不了 0 | EMA 低于 1 B/s 一律归零并在快照里剔除；窗口视图同理 |
| 刚启动那几十秒切到「1分钟 / 5分钟 / 1小时」，数字明显偏小 | 分母写死窗口长度（60/300/3600），实际只攒了几秒 | 分母取 `min(窗口秒数, 已覆盖秒数)` |
| 进程路径含中文且超长时整帧被丢弃 | 名字按 512 字节硬切，正好切在多字节字符中间 → 对端 `from_utf8` 失败 | 截断退到最近的字符边界（`is_char_boundary`） |
| `--show` 起来一瞬间就自己收起，没法截图核对界面 | 失焦自动收起对调试模式同样生效 | `--show` 下不自动收起 |

验证：`cargo test --workspace` 97 个用例全过（Windows 上 `tm-macos` 被平台门控跳过，实跑 77）；
`tramoing.exe --autostart on/off/status`、`--export` 均实测通过。

### 6.10 打包（Windows）

仓库根目录已改成 `tramoing/`，**打包产物一律出在当前仓库根目录下的 `dist/`**，
不要再从旧路径（`traffic-monitoring/`）找安装包。

```powershell
# 需要一个能跑脚本的 PowerShell（或临时放开策略）
powershell -ExecutionPolicy Bypass -File packaging\windows\build.ps1
# 想手动指定版本
powershell -ExecutionPolicy Bypass -File packaging\windows\build.ps1 -Version 0.1.1
```

脚本做的事：读 `Cargo.toml` 的版本 → `cargo build --release` 两个 exe →
收集到 `dist\windows\` → 调 `makensis` 出 `dist\Tramoing-<version>-setup.exe`（约 2.5 MB）。

实操踩到的两个坑，已在脚本里修掉：

| 坑 | 现象 | 处理 |
|---|---|---|
| `build.ps1` / `traffic-monitor.nsi` 没存 BOM | PowerShell 5.1 按 GBK 解读，中文注释乱码 → `字符串缺少终止符`；NSIS 也会按 ANSI 读源码 | 两个文件都存成 **UTF-8 with BOM**（脚本头部有注释提醒；用编辑器改完记得补回） |
| `makensis` 不在 PATH | `winget install NSIS.NSIS` 不会写 PATH，脚本报「找不到 makensis」 | 脚本先查 `Get-Command`，再退到 `Program Files (x86)\NSIS\makensis.exe` |

已验证：安装包生成成功（0 warning），版本信息为 `Tramoing 0.1.0 / MIT`；
`dist\windows\tramoing.exe` 直接运行正常。**安装 → 卸载全流程尚未跑过**
（需要管理员权限，且会改动系统计划任务），首次安装时请留意 `tramoing-helper --install` 的返回码。

改名后安装包的行为：

- 安装目录 `%PROGRAMFILES%\Tramoing`，开始菜单文件夹 / 快捷方式 / 控制面板条目都叫 `Tramoing`，
  注册表标识 `HKLM\Software\Tramoing`（旧版的 `...\Traffic Monitor` 不会被自动卸载）。
- 装之前**先把旧版退掉**：改名前的 `traffic-monitor.exe`（或更早装的 `TrafficMonitor.exe`）如果还在跑，会和新包抢命名管道和
  ETW 会话（表现为面板一直骨架屏，日志里是 183 / 连接被拒）。
- 卸载新版时会顺手清掉旧版的残留：开始菜单 `Traffic Monitor` 文件夹、`HKCU\...\Run` 里的
  `TrafficMonitor` 值与 `HKLM\Software\TrafficMonitor*` 键。历史数据默认保留。

### 6.11 应用图标（Windows）

列表左侧的图标不是内置贴图，而是运行期从每个 exe 的资源段里取出来的真图标：

| 环节 | 实现 |
|---|---|
| 取图标 | `tm-windows::icon::app_icon(path)` → `SHGetFileInfoW`（`SHGFI_ICON \| SHGFI_LARGEICON`），150% 缩放下返回 48×48 |
| 解码 | `GetIconInfo` 拿到彩色位图 + AND mask → `GetDIBits` 读 32bpp BGRA；有 alpha 的按「预乘 → 非预乘」反算，没有 alpha 的老图标用 mask 补透明度 |
| 释放 | `GetIconInfo` 出门就把两张位图的所有权交给你，所以 `DeleteObject` 与 `DestroyIcon` 必须成对，否则每取一个图标漏两块 GDI 对象 |
| 线程 | 取图跑在 `tray-app::icons` 的后台线程里：`SHGetFileInfoW` 冷启动几十毫秒，放界面线程就是白帧 |
| 缓存 | 键直接用 `AppKey`（Windows 下就是 exe 全路径），纹理上限 256 张，超了按「10 分钟没上过榜」淘汰 |
| 兜底 | 取不到就画首字母色块（`tile_color` 按名字散列，颜色稳定） |

排查：`TM_TRACE_ICONS=1 tramoing.exe --show` 会把每个取不到的路径和原因打到 stderr。
单测在 `crates/tm-windows/src/icon.rs`（占位名/不存在路径/真实系统图标解码）。

### 6.12 本轮修复（点托盘图标没反应）

**现象**：启动后点托盘图标，面板不弹出来；托盘图标本身是在的，鼠标悬停也正常。

**原因**：收面板用的是 `ViewportCommand::Visible(false)`。窗口一旦真的隐藏，
winit 就不再派发 `RedrawRequested`，eframe 的 `update()` 整个停摆 ——
托盘点击虽然被 handler 塞进 channel（还顺手 `request_repaint()`），但再也没人消费。
更糟的是启动路径：`with_visible(false)` + 前 3 帧强制隐藏，导致 `update()`
在第一秒内就停了，连托盘图标都是靠"运气"建的。

**修法**：

| 位置 | 改动 |
|---|---|
| `main.rs` | Windows 改成开局就 `with_position([-32000, -32000])` + `with_visible(true)`：窗口一直"可见"，只是停在屏幕外，事件循环不会停 |
| `app.rs::park_window` | 收起时 Windows 用 `OuterPosition(-32000, -32000)` 挪走（`visible` 逻辑状态照旧置 false）；macOS 仍用 `Visible(false)` |
| `app.rs::App::new` | Windows 上同步 `ensure_tray()`，不再等 `update()` 里那 600ms 延迟（macOS 的 NSStatusBar 布局延迟保留） |
| `app.rs::update` | Win 专属：首帧兜底确认窗口是"可见 + 停在屏幕外"；`startup_hide` 只在 macOS 编译 |

**验证**：用 `PostMessage(hwnd, WM_USER_TRAYICON=6002, 0, WM_LBUTTONUP)`
向 `tray_icon_app` 窗口模拟点击，连续三轮「开 → 收 → 开」都正确：
打开时窗口在 `(2127, 973)`，收起时回到 `-32000` 屏幕外，面板内容完整渲染。
`TM_TRACE_FRAMES=1` 可以看到收起后 `[ui] frame=` 依旧每 30 帧打一次（说明 update 没停）。

### 6.13 Windows 任务栏网速贴片

**目标**：像 TrafficMonitor 那样，把上下行速率直接显示在**任务栏条里**，
而不是浮在任务栏上方的一块小卡片。

**为什么不能画进托盘图标**：`Shell_NotifyIcon` 只接受方形小图标（150% DPI
下 24×24），`↓1.2M` 这种宽内容会被系统等比缩成一团糊；图标槽的 rect 由壳
决定，撑大 HICON 也没用。所以数字必须由**另一个窗口**来画。

**位置**：`Shell_TrayWnd`（任务栏整条）→ `TrayNotifyWnd`（托盘区）→ 贴片
放在托盘区左缘再往左 `GAP` 处，纵向按任务栏高度居中。任务栏上如果已经插着
别的同类工具（`Shell_TrayWnd` 里不属于系统类的子窗口，TrafficMonitor 就是
一个 `#32770`），贴片继续往左让，避免两边文字叠在一起。

**方向图标**：手绘的细箭头（竖杆 + 三角头），不用 `↓`/`↑` 字符（字体缺字形
就是个方块）；箭头高度按数字的**字高**对齐，不要更高——11pt 那种高度试过，
两个箭头会把视觉重心全抢走，数字显得又小又灰。箭头比文字基线再上提 0.5pt，
因为 `layout_no_wrap` 量出来的中心含了下沉部。

**点击**：点贴片和点托盘图标完全等价（`toggle`），但**锚点不同**：点贴片时
面板以**贴片自己**为锚点（`TrayEvent::BadgeClick`），不是托盘图标。贴片和托盘
图标之间常常隔着一整个第三方插件（TrafficMonitor 那种），按托盘图标摆的话，
点最左边的数字、面板却从右边托盘上方冒出来，偏出去三百多像素。这里有个反直觉的坑：

| 现象 | 原因 | 修法 |
|---|---|---|
| 只有笔画本身能点，箭头/数字之间的留白点不到 | 颜色键分层窗口的命中测试是**按像素**走的，抠掉的像素在系统眼里根本不属于这个窗口；`WM_NCHITTEST` 返回 `HTCLIENT` 也没用（那一步在窗口过程之前就做完了，实测无效），`SetWindowRgn` 覆盖整块矩形也没用 | 后台线程每 12ms 读一次 `GetCursorPos` + `GetAsyncKeyState(VK_LBUTTON)`，按下和抬起都落在贴片矩形里就算一次点击（按住超过 900ms 或者中途拖出去不算）。贴片周围留 40px 的「快速区」也用 12ms 采样，否则一次 30ms 的快速点击可能整个落在两次慢轮询之间 |

不用全局鼠标钩子（`WH_MOUSE_LL`）：那会进到系统输入链路里，我们这条线程
一卡，整机的鼠标都跟着卡。也别想靠 `SetWindowSubclass` 做命中测试——它只在
comctl32 **v6** 里，没有应用清单的程序会被加载 5.82，静态导入的结果是整个
进程起不来（退出码 127 / `ERROR_PROC_NOT_FOUND`，一行日志都没有）。

矩形由轮询线程自己按窗口句柄 `GetWindowRect`，**不要**从主循环传过去：
`update()` 空闲时只有 1Hz，而贴片的落位是子 viewport 自己自校正的，刚启动
或任务栏重排那一两秒会点不中（踩过一次，点下去完全没反应）。

**渲染的四个坑（都踩过，勿回退）**

| 现象 | 原因 | 修法 |
|---|---|---|
| 窗口位置、样式、Z 序全对，就是一个像素都不显示 | 用 `show_viewport_immediate` 建子 viewport：它的 `swap_buffers` 走父窗口那一帧的 GL 上下文切换，画完屏幕上什么都没有（连不透明洋红测试块都看不见） | 换成 `show_viewport_deferred`（走和主窗口相同的渲染通道）。代价是回调必须 `'static + Send + Sync`，数据走 `Arc<Mutex<BadgeData>>` |
| 贴片是一块黑方块 | 窗口是 GL 画的，而 eframe 拿到的 GL 配置**没有 alpha 通道**（启动日志 `Cannot create transparent window: the GL config does not support it`），`glClear(0,0,0,0)` 的 alpha 被丢掉 | `WS_EX_LAYERED` + `SetLayeredWindowAttributes(hwnd, 0, 0, LWA_COLORKEY)`：把纯黑整片抠成透明。抗锯齿过渡像素是「文字色×覆盖率」不是纯黑，不会被误抠。顺带抠掉的区域鼠标也穿透 |
| 一开始完全不显示 / 后来又能显示 | 想用 `with_mouse_passthrough(true)` 做穿透，winit 那条路加的是 `WS_EX_TRANSPARENT \| WS_EX_LAYERED`，而**没有分层属性的分层窗口不会被合成** | 不碰 `with_mouse_passthrough`，穿透交给颜色键 |
| 贴片毫无征兆地消失，过一会儿又出现 | 任务栏自己也是 TOPMOST，它每次重绘（时间跳一分钟、托盘图标变化）都会把自己重新提到最前 | `tune()` 每次更新都 `SetWindowPos(HWND_TOPMOST, SWP_NOACTIVATE)` 重新申明一次；样式位也一样会被 winit 按 `WindowFlags` 重算掉，所以 `tune()` 是幂等的、每帧都调 |

**进不进 Alt+Tab / 抢不抢焦点**：`WS_EX_TOOLWINDOW`（不进 Alt+Tab）、
`WS_EX_NOACTIVATE`（点了不激活）。这两个位 winit 也会覆盖，同样靠幂等的
`tune()` 每秒补一次。

**排查**：`TM_TRACE_BADGE=1` 会打贴片的 `screen/inner/outer` 和实际缩放；
`TM_LOG=1` 把 eframe/winit 的内部日志接到 stderr（`swap_buffers failed`
这类错误平时是静默的）。`scripts/grab.py` 是 DPI 感知的屏幕抓图小工具，
截图核对时用它，别用 DPI 无感的截图（3840×2160@150% 下会差 1.5 倍）。



---

## 7. 配置与数据文件

### 7.1 `settings.conf`

位置：macOS `~/Library/Application Support/TrafficMonitor/`；Windows `%LOCALAPPDATA%\TrafficMonitor\`。

| 键 | 取值 | 默认 | 说明 |
|---|---|---|---|
| `tray_mode` | `both` / `speed` / `icon` | `both` | 旧值 `full`→`both`、`compact`→`speed` 自动兼容 |
| `theme` | `system` / `light` / `dark` | `system` | |
| `lang` | `auto` / `zh` / `en` | `auto` | auto 按是否有中文字体决定 |
| `max_apps` | 5 / 10 / 20 | `10` | 列表最多列出几个应用；装不下或超出可视行数时滚动查看。旧键 `row_limit` 仍可读，会吸附到最近的档位 |
| `retention_days` | 1~365 | `30` | 历史保留天数 |
| `alert_mb` | 0~1048576 | `0` | 近 10 分钟单应用超阈值提醒，0 关闭 |
| `tray_color` | `on` / `off` | `on` | 仅 macOS 有效 |

文件是 `key=value` 纯文本，解析永不失败：未知键和非法值直接忽略，用默认值兜底。

### 7.2 数据文件

| 文件 | 位置 | 说明 |
|---|---|---|
| `history.db` | 同 `settings.conf` 目录 | SQLite，WAL 模式，表 `samples`，主键 `(minute, app)` |
| `settings.conf` | 同上 | 用户设置 |
| `helper.log` | 同上（Windows） | 采集助手运行日志 |
| `etw-dump.log` | 同上（Windows） | 事件诊断日志，仅 `--dump-events` 时产生 |
| `Tramoing-<时间戳>.csv` | `Downloads`，失败退到应用目录下的 `exports/` | 导出结果 |

`history.db` 可以用任何 SQLite 工具打开，例如 `sqlite3 history.db "select * from samples limit 5;"`。

---

## 8. 测试与质量

- **121 个单元测试全部通过**
  - `tm-core` 28：速率差分、pid 复用、计数器回退、窗口补零、格式化、系统服务识别、上下行语义色
  - `tm-macos` 36：CSV 连接解析、回环配对与代理回填、TUN 隧道量守恒归属、应用归并（bundle / 全路径 / 名字三级）、pty 换行、状态栏渲染辅助
  - `tm-windows` 15：管道协议编解码、按 PID 聚合、方向表一致性
  - `tray-app` 31：设置解析、自启 plist、CSV 导出、名称截断、齿轮位图
  - `tm-store` 9：建表、upsert、范围查询、清理
  - `tm-sampler` 2：trait 与 Mock
- `cargo check --target x86_64-pc-windows-msvc -p tm-core -p tm-sampler -p tm-windows` 通过——注意这只覆盖纯逻辑 crate，**不代表 Windows 系统调用部分已经编译验证过**。

---

## 9. 提交前检查

- `.gitignore` 已忽略 `target/`、`dist/`、`.workbuddy/`、`.DS_Store`
- `docs/screenshot-*.png` 是随文档一起提交的截图
- `assets/AppIcon.icns` 已生成并提交，打包脚本在文件存在时不会重新生成
- 当前仓库还没有任何 commit，所有源码均为未跟踪状态；首次提交建议：
  ```bash
  git add .
  git commit -m "feat: cross-platform menu bar traffic monitor v0.1.0"
  ```

## 10. 许可

MIT（见根 `Cargo.toml` 的 `license` 字段）。
