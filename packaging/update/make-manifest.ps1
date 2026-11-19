# 生成更新清单 latest.json。
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File packaging\update\make-manifest.ps1 `
#       -BaseUrl "https://your-bucket.oss-cn-hangzhou.aliyuncs.com/tramoing"
#
# 产出的 JSON 要和应用一起上传：客户端只认 platforms 里和它同名的那个键。
# 上传完**先自己用浏览器打开一次清单地址**，确认返回的是 JSON 而不是
# 「AccessDenied」——403/404 在客户端是静默失败，只留一行 ui.log。

param(
    # 安装包路径。默认取 dist 里当前版本的那个。
    [string]$Installer = "",
    # 安装包所在目录的公开地址，末尾不要带斜杠
    [string]$BaseUrl = "",
    # 发布说明，会原样显示不了（客户端只记日志），但它是清单里唯一能写更新内容的地方
    [string]$Notes = "",
    [string]$OutFile = ""
)

$ErrorActionPreference = "Stop"

# 版本从 Cargo.toml 读，和 build.ps1 同一个来源，避免两处写不一样
$line = Select-String -Path "Cargo.toml" -Pattern '^version = "(.+)"' | Select-Object -First 1
if (-not $line) { throw "读不到 Cargo.toml 里的 version" }
$Version = $line.Matches[0].Groups[1].Value

if (-not $Installer) {
    $Installer = "dist\Tramoing-$Version-setup.exe"
}
if (-not (Test-Path $Installer)) {
    throw "找不到安装包：$Installer（先跑 build.ps1）"
}
if (-not $BaseUrl) {
    throw "必须给 -BaseUrl，否则清单里的下载地址是空的"
}
$BaseUrl = $BaseUrl.TrimEnd('/')
if (-not $OutFile) {
    $OutFile = "dist\latest.json"
}

$file = Get-Item $Installer
$hash = (Get-FileHash -Algorithm SHA256 -Path $file.FullName).Hash.ToLowerInvariant()

$manifest = [ordered]@{
    version   = $Version
    notes     = $Notes
    pub_date  = (Get-Date).ToString("yyyy-MM-dd")
    homepage  = "$BaseUrl"
    platforms = [ordered]@{
        "windows-x86_64" = [ordered]@{
            url    = "$BaseUrl/$($file.Name)"
            sha256 = $hash
            size   = $file.Length
        }
    }
}

$json = $manifest | ConvertTo-Json -Depth 6

# 必须写**无 BOM** 的 UTF-8：PowerShell 5.1 的 `Out-File -Encoding utf8`
# 会加 BOM，客户端虽然已经兼容，但别的工具（jq、部分 CDN 校验）会不认。
$dir = Split-Path -Parent $OutFile
if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null }
[System.IO.File]::WriteAllText(
    (Resolve-Path -LiteralPath (Split-Path -Parent $OutFile)).Path + "\" + (Split-Path -Leaf $OutFile),
    $json,
    (New-Object System.Text.UTF8Encoding($false))
)

Write-Host "==> 版本 $Version"
Write-Host "==> sha256 $hash"
Write-Host "==> 清单已写到 $OutFile"
Write-Host ""
Write-Host "接下来把这两个文件上传到同一个目录："
Write-Host "  $($file.Name)"
Write-Host "  latest.json"
Write-Host ""
Write-Host "示例（装了 ossutil 的话）："
Write-Host "  ossutil cp `"$($file.FullName)`" oss://<bucket>/tramoing/"
Write-Host "  ossutil cp `"$OutFile`" oss://<bucket>/tramoing/latest.json"
