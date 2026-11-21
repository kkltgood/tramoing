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

$line = Select-String -Path "Cargo.toml" -Pattern '^version = "(.+)"' | Select-Object -First 1
if (-not $line) { throw "读不到 Cargo.toml 里的 version" }
$cargoVersion = $line.Matches[0].Groups[1].Value
if ($Version -and $Version -ne $cargoVersion) {
    throw "请先修改 Cargo.toml 版本，安装包版本必须与程序一致（当前 $cargoVersion）"
}
$Version = $cargoVersion
Write-Host "==> 版本 $Version"

Write-Host "==> 编译 release"
# Cargo 报告本次构建的确切路径，兼容 CARGO_TARGET_DIR、.cargo 配置及目标平台子目录。
# 不能从固定 target/release 收集：那里可能还留着另一次构建的旧程序。
$buildOutput = & cargo build --release --locked -p tray-app -p tm-windows --bins --message-format=json-render-diagnostics
if ($LASTEXITCODE -ne 0) { throw "编译失败，未收集安装包输入" }
$artifacts = @{}
foreach ($line in $buildOutput) {
    $record = $line | ConvertFrom-Json
    if ($record.reason -eq 'compiler-artifact' -and $record.executable -and
        $record.target.name -in @('tramoing', 'tramoing-helper') -and
        $record.target.kind -contains 'bin') {
        $artifacts[$record.target.name] = $record.executable
    }
}
foreach ($binary in @('tramoing', 'tramoing-helper')) {
    if (-not $artifacts.ContainsKey($binary) -or
        -not (Test-Path -LiteralPath $artifacts[$binary] -PathType Leaf) -or
        [IO.Path]::GetFileName($artifacts[$binary]) -ne "$binary.exe") {
        throw "Cargo 未提供有效的 Windows 产物：$binary.exe"
    }
}

$stage = Join-Path $root "dist\windows"
New-Item -ItemType Directory -Force $stage | Out-Null
foreach ($binary in @('tramoing', 'tramoing-helper')) {
    $destination = Join-Path $stage "$binary.exe"
    Copy-Item -LiteralPath $artifacts[$binary] -Destination $destination -Force
    if ((Get-FileHash -LiteralPath $artifacts[$binary]).Hash -ne
        (Get-FileHash -LiteralPath $destination).Hash) {
        throw "打包输入校验失败：$binary.exe"
    }
    Write-Host "    $binary <- $($artifacts[$binary])"
}
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

$installerPath = Join-Path $root "dist\Tramoing-$Version-setup.exe"
$checksum = (Get-FileHash -Algorithm SHA256 -LiteralPath $installerPath).Hash.ToLowerInvariant()
[IO.File]::WriteAllText(
    "$installerPath.sha256",
    "$checksum  $([IO.Path]::GetFileName($installerPath))`n",
    [Text.Encoding]::ASCII
)

Write-Host ""
Write-Host "完成：dist\Tramoing-$Version-setup.exe"
Write-Host "校验：dist\Tramoing-$Version-setup.exe.sha256"
