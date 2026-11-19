# 2026-11-20 任务栏背景与箭头定位修复

用户反馈：任务栏网速背景是一块与系统底色不一致的实色矩形；网速变化时，上下箭头会左右移动。

本记录对应透明背景初版提交 `7d8b663` 的验证与安装包。后续已将任务栏宽度从 60 缩到 44 个逻辑像素，并优化两页界面，最新安装包与验证见 [界面优化记录](ui-polish-2026-11-20.md)。文末截图已更新为紧凑布局。

## 改动

- 移除深灰/浅灰背景填充，改用 32bpp 预乘 BGRA 位图和 `UpdateLayeredWindow` 逐像素合成，透出真实任务栏背景。控件仍是 `Shell_TrayWnd` 下的 `WS_CHILD`，没有 TOPMOST 浮窗。
- 字形使用灰度抗锯齿，转换为对应前景色的 alpha 覆盖率，避免 ClearType 彩边或颜色键黑边。
- 空白像素保留 `1/255` 的最小 alpha，用于 Windows 原生命中测试；视觉上与真实背景最多相差一个色阶。完全为零的 alpha 会让空白位置的点击穿透到任务栏。
- 箭头绘制在固定列，数值在另一列独立右对齐。单位、整数和小数位宽变化都不会改变箭头位置。
- 重绘随内容、尺寸和 DPI 变化提交；GDI 位图、字体和绘图上下文在绘制结束时释放。合成失败时显示状态并隐藏控件，允许后续更新重试。

## 验证

- `cargo test -p tm-windows -p tray-app --locked --offline`：60 项通过，1 项交互桌面测试默认忽略。
- 单独运行 `native_child_lifecycle_and_clicks --ignored`：通过，包含真实父窗口、分层子窗口样式、空白区域命中、点击、窗口重建和清理。
- 新增栅格回归：100%、150%、200% DPI × 明暗主题 × 彩色/单色，在 `0B/9B`、`1.6K/875B`、`999M/1.5G` 间切换，箭头区域逐像素一致；背景覆盖率、文字覆盖率与预乘颜色均符合要求。
- `cargo clippy -p tm-windows -p tray-app --all-targets --locked --offline -- -D warnings`、`cargo fmt --all -- --check`、`git diff --check`：通过。
- Windows debug 和最终 release 程序实际运行验证通过：逐像素合成、空白区域点击命中、单实例、mock 数据隔离、正常退出和子窗口移除。
- 运行脚本会短暂隐藏测试程序自己的网速子窗口，对比显示前后的背景采样点。本机 150% DPI 下 RGB 最大差值为 **0**。

变更仅涉及 Windows 网速控件及其验证、文档。macOS 的原生构建和运行验收状态沿用前次记录。

## 新安装包

- 路径：`dist/Tramoing-0.1.0-setup.exe`，版本仍为 `0.1.0`，包含本次透明背景和箭头定位修复。
- 两个 Windows x64 程序通过 `cargo build --release -p tray-app -p tm-windows --locked --offline` 构建，复制到打包目录并校验一致后，由 NSIS 3.13 生成安装包。
- 大小：**3,283,039 字节（约 3.13 MiB）**。
- 安装包 SHA256：`B0BEB9C85B7DD2175DBC8BED1B86B216968E4064C20073B1846843181E026835`。
- 主程序 SHA256：`B1773274DDD5CDE37878F3A8DF7C571896B8553624C117602CA5CD516FD4DCEC`。
- 采集助手 SHA256：`C1645B437E9AAF1601C16E07794B38F0001DF78C244978411A64A21117771093`。

该文件替换同路径的前次安装包，旁边的 `.sha256` 文件也已更新。未运行安装或替换当前已安装的程序。

实屏裁剪（150% DPI，演示流量）：

![透明任务栏网速](screenshot-transparent-taskbar.png)
