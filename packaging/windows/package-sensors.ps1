# Build/reuse the independently versioned sensor installer before compiling Rust.
param(
    [string]$DotNet = 'dotnet',
    [Parameter(Mandatory=$true)][string]$MakeNsis,
    [string]$Repository = 'https://github.com/kkltgood/tramoing'
)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$component = Get-Content -LiteralPath (Join-Path $root 'sensors/component.json') -Raw | ConvertFrom-Json
$version = [string]$component.version
if ($version -notmatch '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' -or $component.protocol -ne 1) {
    throw 'Invalid sensor component version or protocol'
}
foreach ($part in $version.Split('.')) {
    if ([int]$part -gt 65535) { throw 'Sensor component version parts must be <= 65535' }
}
if ($Repository -notmatch '^https://github\.com/[^/]+/[^/]+$') { throw 'Invalid component repository' }
& (Join-Path $PSScriptRoot 'build-sensors.ps1') -Architecture x64 -DotNet $DotNet
if ($LASTEXITCODE -ne 0) { throw 'Sensor publish failed' }

# A published component version is immutable, including its source inputs. A new
# app version can reuse the existing binary even when SDK/NSIS versions changed.
$inputs = @(
    Get-ChildItem -LiteralPath (Join-Path $root 'sensors/Tramoing.Sensors') -File -Recurse |
        Where-Object { $_.FullName -notmatch '[\\/](bin|obj)[\\/]' }
    Get-ChildItem -LiteralPath (Join-Path $root 'sensors/licenses') -File -Recurse
    Get-Item -LiteralPath (Join-Path $root 'sensors/component.json')
    Get-Item -LiteralPath (Join-Path $root 'sensors/THIRD-PARTY-NOTICES.md')
    Get-Item -LiteralPath (Join-Path $PSScriptRoot 'build-sensors.ps1')
    Get-Item -LiteralPath (Join-Path $PSScriptRoot 'package-sensors.ps1')
    Get-Item -LiteralPath (Join-Path $PSScriptRoot 'sensors.nsi')
)
$entries = @($inputs | ForEach-Object {
    $relative = $_.FullName.Substring($root.Length + 1).Replace('\', '/')
    $digest = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    "$relative $digest"
} | Sort-Object)
$hasher = [Security.Cryptography.SHA256]::Create()
try {
    $sourceHash = [BitConverter]::ToString($hasher.ComputeHash([Text.Encoding]::UTF8.GetBytes(($entries -join "`n") + "`n"))).Replace('-', '').ToLowerInvariant()
} finally { $hasher.Dispose() }

$name = "Tramoing-Sensors-$version-windows-x64-setup.exe"
$installer = Join-Path $root "dist/$name"
$manifestPath = Join-Path $root 'dist/sensors-component.json'
$baseUrl = "$Repository/releases/download/sensors-v$version"
$url = "$baseUrl/$name"
$cache = Join-Path $root 'target/sensor-component-release'
New-Item -ItemType Directory -Force $cache | Out-Null
$publishedPath = Join-Path $cache "sensors-$version.json"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$published = $null
try {
    Invoke-WebRequest -UseBasicParsing -Uri "$baseUrl/sensors-component.json" -OutFile $publishedPath -TimeoutSec 90
    $published = Get-Content -LiteralPath $publishedPath -Raw | ConvertFrom-Json
} catch {
    if (-not $_.Exception.Response -or [int]$_.Exception.Response.StatusCode -ne 404) { throw }
}
if ($published) {
    if ($published.component_version -ne $version -or $published.protocol -ne $component.protocol -or
        $published.architecture -ne 'x64' -or $published.url -ne $url -or $published.source_sha256 -ne $sourceHash -or
        $published.sha256 -notmatch '^[a-f0-9]{64}$' -or $published.size -le 0) {
        throw 'Published sensor component does not match these sources. Increment sensors/component.json version.'
    }
    $cached = (Test-Path -LiteralPath $installer) -and
        (Get-Item -LiteralPath $installer).Length -eq $published.size -and
        (Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash.ToLowerInvariant() -eq $published.sha256
    if (-not $cached) {
        Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $installer -TimeoutSec 180
    }
    $hash = (Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($hash -ne $published.sha256 -or (Get-Item -LiteralPath $installer).Length -ne $published.size) {
        throw 'Published sensor component checksum or size mismatch'
    }
    Copy-Item -LiteralPath $publishedPath -Destination $manifestPath -Force
    Write-Host "Reused immutable sensor component $version"
} else {
    & $MakeNsis "/DSENSOR_VERSION=$version" (Join-Path $PSScriptRoot 'sensors.nsi')
    if ($LASTEXITCODE -ne 0) { throw 'Sensor component installer build failed' }
    $hash = (Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash.ToLowerInvariant()
    $metadata = [ordered]@{
        component_version = $version
        protocol = [int]$component.protocol
        architecture = 'x64'
        url = $url
        sha256 = $hash
        size = (Get-Item -LiteralPath $installer).Length
        source_sha256 = $sourceHash
    }
    [IO.File]::WriteAllText($manifestPath, ($metadata | ConvertTo-Json) + "`n", [Text.UTF8Encoding]::new($false))
}
[IO.File]::WriteAllText("$installer.sha256", "$hash  $name`n", [Text.Encoding]::ASCII)
Write-Host "Sensor component manifest: $manifestPath"
