# Validate downloaded official installers and the real framework-dependent probe.
# Does not install runtimes, drivers, or change machine configuration.
param([switch]$ExpectMissingRuntime)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$manifest = Get-Content -LiteralPath (Join-Path $root 'static/dependencies/manifest.json') -Raw | ConvertFrom-Json
& (Join-Path $PSScriptRoot 'verify-dependency-signatures.ps1')
$fixture = Join-Path $root ('target/component-probe-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $fixture | Out-Null
$component = Get-Content -LiteralPath (Join-Path $root 'sensors/component.json') -Raw | ConvertFrom-Json
$manifest | Add-Member -NotePropertyName component -NotePropertyValue ([pscustomobject]@{ component_version=$component.version; protocol=$component.protocol })
$cfg = [pscustomobject]@{ root=$fixture; manifest=$manifest }
$script = Join-Path $root 'crates/tray-app/src/component_install/probe.ps1'
function Inspect-Fixture {
    $output = & $script
    return ($output | ConvertFrom-Json)
}
$original = Join-Path $root 'dist/windows-x64/sensors'
$sensorRoot = Join-Path $fixture 'sensors'
try {
    $fresh = Inspect-Fixture
    if ($fresh.component_ok -or $fresh.legacy_component) { throw 'An empty installation was treated as a working component' }
    $expectedRuntime = -not $ExpectMissingRuntime.IsPresent
    if ($fresh.runtime_ok -ne $expectedRuntime) {
        throw "Unexpected x64 .NET 10 runtime detection (expected: $expectedRuntime, detected: $($fresh.runtime_ok))"
    }
    Copy-Item -LiteralPath $original -Destination $sensorRoot -Recurse
    $installed = Inspect-Fixture
    if (-not $installed.component_ok -or $installed.legacy_component) { throw 'Published framework-dependent component did not pass its probe' }
    $worker = Join-Path $sensorRoot 'tramoing-sensors.exe'
    [IO.File]::WriteAllText($worker, 'not an executable')
    if ((Inspect-Fixture).component_ok) { throw 'A damaged executable was treated as usable' }
    Copy-Item -LiteralPath (Join-Path $original 'tramoing-sensors.exe') -Destination $worker -Force
    if (-not (Inspect-Fixture).component_ok) { throw 'Restored executable did not pass its probe' }
    $library = Join-Path $sensorRoot 'LibreHardwareMonitorLib.dll'
    Remove-Item -LiteralPath $library
    if ((Inspect-Fixture).component_ok) { throw 'Missing hardware library was treated as usable' }
    Copy-Item -LiteralPath (Join-Path $original 'LibreHardwareMonitorLib.dll') -Destination $library -Force
    if (-not (Inspect-Fixture).component_ok) { throw 'Restored hardware library did not pass its probe' }
    # Keep the original size: this must fail SHA-256 validation, not only length.
    $bytes = [IO.File]::ReadAllBytes($library)
    $bytes[0] = $bytes[0] -bxor 1
    [IO.File]::WriteAllBytes($library, $bytes)
    if ((Inspect-Fixture).component_ok) { throw 'Corrupt hardware library was treated as usable' }
    Copy-Item -LiteralPath (Join-Path $original 'LibreHardwareMonitorLib.dll') -Destination $library -Force
    if (-not (Inspect-Fixture).component_ok) { throw 'Repaired hardware library did not pass its probe' }
    # An older package has no component metadata. Invalid legacy JSON must yield
    # a missing-component plan, rather than aborting the entire local inspection.
    $metadata = Join-Path $sensorRoot 'component.json'
    $runtimeConfig = Join-Path $sensorRoot 'tramoing-sensors.runtimeconfig.json'
    Remove-Item -LiteralPath $metadata
    [IO.File]::WriteAllText($runtimeConfig, '{broken')
    $brokenLegacy = Inspect-Fixture
    if ($brokenLegacy.component_ok -or $brokenLegacy.legacy_component) { throw 'Malformed legacy runtime config was treated as usable' }
    Copy-Item -LiteralPath (Join-Path $original 'component.json') -Destination $metadata -Force
    Copy-Item -LiteralPath (Join-Path $original 'tramoing-sensors.runtimeconfig.json') -Destination $runtimeConfig -Force
    if (-not (Inspect-Fixture).component_ok) { throw 'Component was not reusable after restoring its metadata' }
    Write-Host "Official signatures, fresh install and component repair checks passed. Global runtime detected: $($fresh.runtime_ok)."
} finally {
    if (Test-Path -LiteralPath $fixture) { Remove-Item -LiteralPath $fixture -Recurse -Force }
}
