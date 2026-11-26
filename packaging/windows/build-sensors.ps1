# Builds the optional x64 worker without bundling .NET or the PawnIO installer.
param(
    [ValidateSet('x64')][string]$Architecture = 'x64',
    [string]$DotNet = 'dotnet'
)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$project = Join-Path $root 'sensors/Tramoing.Sensors/Tramoing.Sensors.csproj'
$component = Get-Content -LiteralPath (Join-Path $root 'sensors/component.json') -Raw | ConvertFrom-Json
if ($component.version -notmatch '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' -or $component.protocol -ne 1) {
    throw 'Invalid sensor component version or protocol'
}
$destination = Join-Path $root "dist/windows-$Architecture/sensors"
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
New-Item -ItemType Directory -Force $destination | Out-Null

# Both RIDs are in the committed NuGet lock file. Do not silently update it in CI.
& $DotNet restore $project --locked-mode
if ($LASTEXITCODE -ne 0) { throw 'Sensor dependency restore failed' }
& $DotNet publish $project -c Release -r "win-$Architecture" --self-contained false --no-restore "-p:Version=$($component.version)" "-p:AssemblyVersion=$($component.version).0" -o $destination
if ($LASTEXITCODE -ne 0) { throw 'Sensor build failed' }

# Preserve upstream license texts in the shipped package, including all transitive
# NuGet dependencies. The runtime is downloaded separately from Microsoft.
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
# A framework-dependent component must never silently grow a private runtime.
foreach ($runtimeFile in @('coreclr.dll', 'hostfxr.dll', 'System.Private.CoreLib.dll')) {
    if (Test-Path -LiteralPath (Join-Path $destination $runtimeFile)) {
        throw "Unexpected bundled .NET runtime: $runtimeFile"
    }
}
Copy-Item -LiteralPath (Join-Path $root 'sensors/component.json') -Destination $destination -Force
$payload = @(Get-ChildItem -LiteralPath $destination -File -Recurse | Sort-Object FullName | ForEach-Object {
        [ordered]@{
            path = $_.FullName.Substring($destination.Length + 1).Replace('\', '/')
            size = $_.Length
            sha256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        }
    }
)
[IO.File]::WriteAllText((Join-Path $destination 'payload-files.json'), (ConvertTo-Json -InputObject ([ordered]@{ schema = 1; files = $payload }) -Depth 4) + "`n", [Text.UTF8Encoding]::new($false))
Write-Host "Built optional sensor component: $destination"
