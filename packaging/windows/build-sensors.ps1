# Builds a self-contained worker; no machine-wide .NET installation is needed at runtime.
param(
    [ValidateSet('x64', 'arm64')][string]$Architecture = 'x64',
    [string]$DotNet = 'dotnet',
    [switch]$SkipDriverDownload
)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$project = Join-Path $root 'sensors/Tramoing.Sensors/Tramoing.Sensors.csproj'
$destination = Join-Path $root "dist/windows-$Architecture/sensors"
$cache = Join-Path $root 'target/sensor-downloads'
# Publish into a clean private staging directory so removed DLLs or runtime
# versions cannot accidentally enter a later installer.
if (Test-Path -LiteralPath $destination) {
    $resolved = (Resolve-Path -LiteralPath $destination).Path
    $expected = [IO.Path]::GetFullPath((Join-Path $root "dist/windows-$Architecture/sensors"))
    $stageParent = Get-Item -LiteralPath (Split-Path $resolved)
    $stageDirectory = Get-Item -LiteralPath $resolved
    if ($resolved -ne $expected -or -not $resolved.StartsWith($root + '\', [StringComparison]::OrdinalIgnoreCase) -or
        ($stageDirectory.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
        ($stageParent.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
        ((Get-Item -LiteralPath (Join-Path $root 'dist')).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw 'Sensor staging directory is outside the workspace or is a reparse point'
    }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
New-Item -ItemType Directory -Force $destination, $cache | Out-Null

# Both RIDs are in the committed NuGet lock file. Do not silently update it in CI.
& $DotNet restore $project --locked-mode
if ($LASTEXITCODE -ne 0) { throw 'Sensor dependency restore failed' }
& $DotNet publish $project -c Release -r "win-$Architecture" --self-contained true --no-restore -o $destination
if ($LASTEXITCODE -ne 0) { throw 'Sensor build failed' }

function Download-Verified([string]$Url, [string]$Path, [string]$Sha256) {
    if ((Test-Path -LiteralPath $Path) -and (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash -eq $Sha256) { return }
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $Path -TimeoutSec 180
    if ((Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash -ne $Sha256) {
        throw "Downloaded dependency checksum mismatch: $Url"
    }
}

# Preserve upstream license texts in the shipped package, including all transitive
# NuGet dependencies and the runtime. NuGet supplies the authoritative metadata.
$notices = Join-Path $destination 'licenses'
New-Item -ItemType Directory -Force $notices | Out-Null
Copy-Item -Path (Join-Path $root 'sensors/licenses/*.txt') -Destination $notices -Force
Copy-Item -LiteralPath (Join-Path $root 'sensors/THIRD-PARTY-NOTICES.md') -Destination $destination -Force
$assets = Get-Content -LiteralPath (Join-Path (Split-Path $project) 'obj/project.assets.json') -Raw | ConvertFrom-Json
$packageRoots = @($assets.packageFolders.PSObject.Properties.Name)
foreach ($library in $assets.libraries.PSObject.Properties) {
    if ($library.Value.type -ne 'package') { continue }
    $packagePath = $null
    foreach ($packageRoot in $packageRoots) {
        $candidate = Join-Path $packageRoot $library.Value.path
        if (Test-Path -LiteralPath $candidate) { $packagePath = $candidate; break }
    }
    if (-not $packagePath) { throw "Package metadata unavailable: $($library.Name)" }
    $noticeDirectory = Join-Path $notices ($library.Name -replace '/', '-')
    New-Item -ItemType Directory -Force $noticeDirectory | Out-Null
    Get-ChildItem -LiteralPath $packagePath -File -Recurse |
        Where-Object { $_.Name -match '(?i)license|copying|notice|\.nuspec$' } |
        ForEach-Object {
            $relative = $_.FullName.Substring($packagePath.Length).TrimStart('\', '/') -replace '[\\/]', '_'
            Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $noticeDirectory $relative) -Force
        }
}
# Runtime packs are downloaded by the SDK rather than listed as ordinary
# project dependencies. Include their own notices as well.
foreach ($packageRoot in $packageRoots) {
    $runtimeRoot = Join-Path $packageRoot "microsoft.netcore.app.runtime.win-$Architecture"
    if (Test-Path -LiteralPath $runtimeRoot) {
        Get-ChildItem -LiteralPath $runtimeRoot -Directory | ForEach-Object {
            $runtimeNotices = Join-Path $notices ("dotnet-runtime-" + $_.Name)
            New-Item -ItemType Directory -Force $runtimeNotices | Out-Null
            Get-ChildItem -LiteralPath $_.FullName -File |
                Where-Object { $_.Name -match '(?i)license|notice' } |
                Copy-Item -Destination $runtimeNotices -Force
        }
    }
}

# Official signed installer, pinned independently of the sensor library. Never
# install a driver while building or while sampling. ARM64 does not use PawnIO.
if ($Architecture -eq 'x64' -and -not $SkipDriverDownload) {
    $driver = Join-Path $cache 'PawnIO-2.2.0.exe'
    Download-Verified 'https://github.com/namazso/PawnIO.Setup/releases/download/2.2.0/PawnIO_setup.exe' $driver '1F519A22E47187F70A1379A48CA604981C4FCF694F4E65B734AAA74A9FBA3032'
    $signature = Get-AuthenticodeSignature -LiteralPath $driver
    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'CN=namazso\.eu(?:,|$)') {
        throw 'PawnIO installer signature is not valid or the publisher does not match'
    }
    Copy-Item -LiteralPath $driver -Destination (Join-Path (Split-Path $destination) 'PawnIO_setup.exe') -Force
}
Write-Host "Built sensor component: $destination"
