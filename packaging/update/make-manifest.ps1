# 生成更新清单 latest.json。
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File packaging\update\make-manifest.ps1 `
#       -BaseUrl "https://example.com/releases/v0.1.0"
#
# 产出的 JSON 要和应用一起上传：客户端只认 platforms 里和它同名的那个键。
# 发布前确认公开地址返回的是清单 JSON 和对应的安装包。

param(
    # 安装包路径。默认取 dist 里当前版本的那个。
    [string]$Installer = "",
    # 安装包所在目录的公开地址，末尾不要带斜杠
    [string]$BaseUrl = "",
    # 发布说明，写入清单供客户端和发布工具读取
    [string]$Notes = "",
    [string]$OutFile = ""
)

$ErrorActionPreference = "Stop"
$root = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\..')).Path

# 版本从 Cargo.toml 读，和 build.ps1 同一个来源，避免两处写不一样
$line = Select-String -LiteralPath (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"' | Select-Object -First 1
if (-not $line) { throw "读不到 Cargo.toml 里的 version" }
$Version = $line.Matches[0].Groups[1].Value

if (-not $Installer) {
    $Installer = Join-Path $root "dist\Tramoing-$Version-setup.exe"
}
if (-not (Test-Path -LiteralPath $Installer -PathType Leaf)) {
    throw "找不到安装包：$Installer（先跑 build.ps1）"
}
if (-not $BaseUrl) {
    throw "必须给 -BaseUrl，否则清单里的下载地址是空的"
}
$BaseUrl = $BaseUrl.TrimEnd('/')
$baseUri = $null
if (-not [Uri]::TryCreate($BaseUrl, [UriKind]::Absolute, [ref]$baseUri) -or
    $baseUri.Scheme -notin @('http', 'https') -or $baseUri.Query -or $baseUri.Fragment) {
    throw 'BaseUrl 必须是无查询参数或片段的 HTTP(S) 目录地址'
}
if (-not $OutFile) {
    $OutFile = Join-Path $root 'dist\latest.json'
}

$file = Get-Item -LiteralPath $Installer
$hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $file.FullName).Hash.ToLowerInvariant()

$manifest = [ordered]@{
    version   = $Version
    notes     = $Notes
    pub_date  = (Get-Date).ToString("yyyy-MM-dd")
    homepage  = "$BaseUrl"
    platforms = [ordered]@{
        "windows-x86_64" = [ordered]@{
            url    = "$BaseUrl/$([Uri]::EscapeDataString($file.Name))"
            sha256 = $hash
            size   = $file.Length
        }
    }
}

$json = $manifest | ConvertTo-Json -Depth 6

# 必须写**无 BOM** 的 UTF-8：PowerShell 5.1 的 `Out-File -Encoding utf8`
# 会加 BOM，客户端虽然已经兼容，但别的工具（jq、部分 CDN 校验）会不认。
$resolvedOutput = [IO.Path]::GetFullPath($OutFile)
$dir = [IO.Path]::GetDirectoryName($resolvedOutput)
if (-not (Test-Path -LiteralPath $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null }
[System.IO.File]::WriteAllText(
    $resolvedOutput,
    $json,
    (New-Object System.Text.UTF8Encoding($false))
)

Write-Host "==> 版本 $Version"
Write-Host "==> sha256 $hash"
Write-Host "==> 清单已写到 $resolvedOutput"
Write-Host ""
Write-Host "接下来把这两个文件上传到同一个目录："
Write-Host "  $($file.Name)"
Write-Host "  latest.json"
