# Windows 打包：编译两个 exe → 调 NSIS 出安装包。
#
#   powershell -ExecutionPolicy Bypass -File packaging\windows\build.ps1
#
# 前置条件：
#   1. Visual Studio Build Tools（MSVC 链接器 + Windows SDK）
#   2. Rust 工具链：rustup target add x86_64-pc-windows-msvc
#   3. NSIS 3：winget install NSIS.NSIS
#
# 产物：dist\Tramoing-<version>-setup.exe
#
# 注意：本文件与 traffic-monitor.nsi 都必须保存成 **UTF-8 with BOM**。
# PowerShell 5.1 读无 BOM 的脚本会按系统 ANSI(GBK) 解析，中文注释直接变乱码、
# 引号配对错位，报「字符串缺少终止符」；NSIS 同理（无 BOM 会按 ANSI 读源码）。
# 编辑后用这条命令补 BOM：
#   $t=[IO.File]::ReadAllText($f,[Text.UTF8Encoding]::new($false));`
#   [IO.File]::WriteAllText($f,$t,[Text.UTF8Encoding]::new($true))

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
cargo build --release -p tm-windows --bin tramoing-helper
if ($LASTEXITCODE -ne 0) { throw "编译 tramoing-helper 失败" }

$stage = Join-Path $root "dist\windows"
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item "target\release\tramoing.exe" $stage -Force
Copy-Item "target\release\tramoing-helper.exe" $stage -Force
Write-Host "    已收集到 $stage"

# winget 装的 NSIS 不会把自己写进 PATH，所以先查命令、再退到默认安装目录
$makensis = $null
$cmd = Get-Command makensis -ErrorAction SilentlyContinue
if ($cmd) {
    $makensis = $cmd.Source
} else {
    $candidates = @(
        (Join-Path ${env:ProgramFiles(x86)} "NSIS\makensis.exe"),
        (Join-Path $env:ProgramFiles "NSIS\makensis.exe"),
        "C:\Program Files (x86)\NSIS\makensis.exe"
    )
    foreach ($c in $candidates) {
        if ($c -and (Test-Path $c)) { $makensis = $c; break }
    }
}
if (-not $makensis) {
    throw "找不到 makensis。先安装 NSIS：winget install NSIS.NSIS，或到 nsis.sourceforge.io 下载"
}

Write-Host "==> 生成安装包"
& $makensis "/DAPP_VERSION=$Version" "packaging\windows\traffic-monitor.nsi"
if ($LASTEXITCODE -ne 0) { throw "makensis 失败" }

Write-Host ""
Write-Host "完成：dist\Tramoing-$Version-setup.exe"
