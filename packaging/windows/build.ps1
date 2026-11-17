# Windows 打包：编译两个 exe → 调 NSIS 出安装包。
#
#   powershell -ExecutionPolicy Bypass -File packaging\windows\build.ps1
#
# 前置条件：
#   1. Visual Studio Build Tools（MSVC 链接器 + Windows SDK）
#   2. Rust 工具链：rustup target add x86_64-pc-windows-msvc
#   3. NSIS 3：winget install NSIS.NSIS
#
# 产物：dist\TrafficMonitor-<version>-setup.exe

param(
    [string]$Version = ""
)

$ErrorActionPreference = "Stop"

# 切到仓库根目录（脚本放在 packaging\windows 下）
$root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Set-Location $root

if (-not $Version) {
    $line = Select-String -Path "Cargo.toml" -Pattern '^version = "(.+)"' | Select-Object -First 1
    if (-not $line) { throw "读不到 Cargo.toml 里的 version" }
    $Version = $line.Matches[0].Groups[1].Value
}
Write-Host "==> 版本 $Version"

Write-Host "==> 编译 release"
cargo build --release -p tray-app
if ($LASTEXITCODE -ne 0) { throw "编译 tray-app 失败" }
cargo build --release -p tm-windows --bin tm-helper
if ($LASTEXITCODE -ne 0) { throw "编译 tm-helper 失败" }

$stage = Join-Path $root "dist\windows"
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item "target\release\traffic-monitor.exe" $stage -Force
Copy-Item "target\release\tm-helper.exe" $stage -Force
Write-Host "    已收集到 $stage"

$makensis = Get-Command makensis -ErrorAction SilentlyContinue
if (-not $makensis) {
    throw "找不到 makensis。先安装 NSIS：winget install NSIS.NSIS，或到 nsis.sourceforge.io 下载"
}

Write-Host "==> 生成安装包"
& $makensis.Source "/DAPP_VERSION=$Version" "packaging\windows\traffic-monitor.nsi"
if ($LASTEXITCODE -ne 0) { throw "makensis 失败" }

Write-Host ""
Write-Host "完成：dist\TrafficMonitor-$Version-setup.exe"
