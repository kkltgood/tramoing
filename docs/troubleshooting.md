# 配置与故障排查

## 配置和数据

默认目录为 Windows 的 `%LOCALAPPDATA%\TrafficMonitor\` 和 macOS 的 `~/Library/Application Support/TrafficMonitor/`。开发环境可以用 `TM_DATA_DIR` 覆盖；`TM_DB_PATH` 可单独覆盖数据库位置。

| 文件 | 内容 |
| --- | --- |
| `settings.conf` | 界面和采样设置，`key=value` 纯文本 |
| `history.db` | SQLite 流量历史 |
| `ui.log` | 采样连接、导出、更新等诊断信息 |
| `helper.log` | Windows 采集助手日志 |
| `etw-dump.log` | Windows `--dump-events` 事件诊断日志 |
| `update/<version>/` | 校验通过的更新安装包 |

界面修改设置后立即保存。手动编辑时先退出程序，避免运行中的设置覆盖文件；未知键和无效值会被忽略。常用配置：

| 键 | 取值 | 默认 |
| --- | --- | --- |
| `theme` | `system` / `light` / `dark` | `system` |
| `lang` | `auto` / `zh` / `en` | `auto` |
| `tray_mode` | `both` / `speed` / `icon`，macOS | `both` |
| `tray_badge` | `on` / `off`，Windows 任务栏网速 | `on` |
| `tray_color` | `on` / `off` | `on` |
| `performance_enabled` | `on` / `off` | `off` |
| `performance_interval` | 1 / 2 / 5 秒 | 2 |
| `performance_color` | `on` / `off` | `on` |
| `taskbar_order` | Windows 统计项顺序，任务栏拖拽后自动保存 | `speed,performance` |
| `cpu_color` / `gpu_color` / `download_color` / `upload_color` | `auto` / `#RRGGBB` | `auto` |
| `max_apps` | 5 / 10 / 20 | 10 |
| `retention_days` | 1–365 天 | 30 |
| `alert_mb` | 近 10 分钟单应用流量阈值，0 关闭 | 0 |
| `update_check` | `on` / `off`，自动检查 GitHub Releases | `on`（保留旧配置中的 `off`） |

CSV 导出优先保存到当前用户的 Downloads，失败时退回数据目录下的 `exports/`。演示模式不产生持久历史；命令行导出读取已有数据库。

Windows 任务栏上的统计项可直接按住拖拽排序，松开后自动保存，重启后保留；设置页不提供排序选项。拖出统计区域后松开会取消换位。暂时关闭某项显示不会清除它的排序位置。旧版 `taskbar_performance_first` 会自动迁移为项目顺序。

## Windows 无流量

24 小时累计来自历史库，不能证明当前助手正在采集。先查看面板的具体错误：

| 提示 | 检查方法 |
| --- | --- |
| 未连接到采集助手，错误 2 | 确认助手已安装、计划任务正在运行，必要时重新运行安装器 |
| 无法访问采集助手，错误 5 | 确认界面和助手属于同一 Windows 账户；用当前账户重新安装 |
| 采集连接被占用，错误 231 | 检查其他账户或旧版本是否仍连接助手 |
| 暂时中断、正在重新连接 | 查看 `helper.log` 和 `ui.log`；程序会退避重试 |

正常使用建议通过安装器部署。源码开发时，管理员终端运行 `tramoing-helper.exe --install` 会将该可执行文件的当前路径写入 `TrafficMonitorHelper` 计划任务；移动或删除它会导致下次启动失败。`--uninstall` 取消助手的登录任务，界面程序的 `--autostart` 是另一个独立开关。

```powershell
schtasks /Query /TN TrafficMonitorHelper /FO LIST
powershell -ExecutionPolicy Bypass -File scripts\diagnose-windows.ps1
```

诊断脚本兼容 PowerShell 5.1，只读检查系统、安装文件、任务、进程和最近日志，将报告写到桌面，不修改助手或系统任务。报告可能包含本机路径和进程名，提交 Issue 前请检查内容。

当前任务定义允许电池供电并取消默认运行时限。旧安装需要重新注册或覆盖安装，重新编译源码不会自动更新已经注册的系统任务。任务状态 `0x00041301` 表示正在运行。

## 界面和性能显示

- **修改源码后行为未变**：正在运行的程序不会自动加载新二进制。先从界面退出，再启动新构建；第二次启动通常只会唤起原实例。
- **任务栏数字未出现**：确认开启了对应显示选项。Windows 会检查任务栏空间和布局，无法安全挂接时在面板中提示；第三方任务栏工具和 Explorer 重建可能影响显示。
- **macOS 菜单栏不可见**：优先使用打包的 `.app` 验证，检查菜单栏可用空间及当前 Space。单独的面板预览不等同于菜单栏验收。
- **GPU 显示 `--%`**：系统或驱动没有提供可用计数器。它与网络采集助手是否连接无关。
- **空列表或速率为零**：确认存在外网流量，并等待采样建立基线；本机回环访问不计入。
- **没有自动检查更新**：在设置 → 软件更新中开启自动检查，旧配置中的关闭选择会保留。默认启动 10 秒后检查，24 小时内不重复；也可点击“检查更新”立即检查。
- **更新清单尚未发布**：当前 GitHub Release 尚无 `latest.json` 附件，需要维护者通过新版发布流程发布新版本；不是“已是最新版”。
- **未配置更新源**：检查 `TRAMOING_UPDATE_URL` 是否被设置为空值；移除运行时覆盖后恢复构建配置或默认 GitHub 地址，见[开发指南](development.md#更新源)。
- **更新下载失败**：确认可以访问 GitHub Releases 及其附件下载域名。程序会拒绝大小或 SHA-256 不匹配的文件，详细原因保存在数据目录中的 `ui.log`。

## 统计限制

本工具用于观察应用流量趋势，不应作为计费或精确审计依据。

- **统计口径**：macOS 采用 nettop socket 字节数，Windows 采用 ETW 网络事件；它们与网卡包含协议开销的计数器可能不同。本机回环转发被排除，以减少代理流量重复计算。
- **Windows 代理**：外网连接的流量通常归到代理进程，暂不保证回填到原始客户端。
- **macOS 代理**：尝试按连接匹配客户端与代理，但采样先后顺序、短连接结束和并发请求会影响归属。已有原生代理测试在部分环境未达到 75% 客户端归属阈值；未宣称完整解决这一限制。
- **TUN**：通过流量关系推断转发进程，属于启发式归属；无法可靠识别时保留代理归属，不保证所有 VPN 或隧道组合都能映射到客户端。
- **短连接与采样间隔**：两次快照之间结束的连接可能遗漏尾部统计。第一帧通常只建立基线，不能据此判断流量丢失。
- **多 GPU**：显示最忙设备或引擎的使用率，不是把所有 GPU 百分比相加。

报告统计问题时，请说明系统版本、直连 / 系统代理 / TUN 模式、预期流量来源及复现步骤，避免仅凭历史累计值判断实时采集状态。
