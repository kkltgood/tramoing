# Tests only local payload validation; does not inspect drivers, elevate, or run executables.
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$source = Join-Path $root 'crates/tray-app/src/component_install/probe.ps1'
$tokens = $null
$parseErrors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($source, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw ($parseErrors | Out-String) }
foreach ($name in @('Test-SensorPayload', 'Test-LegacySensorPayload', 'Test-X64DotNetRuntime')) {
    $definition = $ast.Find({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name }, $true)
    if (-not $definition) { throw "Missing production validator: $name" }
    . ([scriptblock]::Create($definition.Extent.Text))
}
$directory = Join-Path ([IO.Path]::GetTempPath()) ('tramoing-payload-test-' + [guid]::NewGuid().ToString('N'))
$encoding = [Text.UTF8Encoding]::new($false)
$manifestPath = Join-Path $directory 'payload-files.json'
$names = @('tramoing-sensors.exe', 'tramoing-sensors.dll', 'tramoing-sensors.deps.json', 'tramoing-sensors.runtimeconfig.json', 'LibreHardwareMonitorLib.dll', 'component.json', 'licenses/License text.txt')
function Write-Manifest($Files) {
    [IO.File]::WriteAllText($manifestPath, ([ordered]@{schema=1;files=@($Files)} | ConvertTo-Json -Depth 5), $encoding)
}
function Assert-Payload([bool]$Expected, [string]$Case) {
    $actual = Test-SensorPayload $directory
    if ($actual -ne $Expected) { throw "Unexpected payload result for $Case : $actual" }
}
try {
    New-Item -ItemType Directory -Path (Join-Path $directory 'licenses') -Force | Out-Null
    $original = @($names | ForEach-Object {
        $path = Join-Path $directory $_
        [IO.File]::WriteAllText($path, 'abc', $encoding)
        [ordered]@{path=$_;size=3;sha256=(Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()}
    })
    Write-Manifest $original
    Assert-Payload $true 'complete payload including a nested license'
    $library = Join-Path $directory 'LibreHardwareMonitorLib.dll'
    [IO.File]::WriteAllText($library, 'bad', $encoding)
    Assert-Payload $false 'same-length corrupted library'
    Remove-Item -LiteralPath $library
    Assert-Payload $false 'missing library'
    [IO.File]::WriteAllText($library, 'abc', $encoding)
    [IO.File]::WriteAllText((Join-Path $directory 'unexpected.dll'), 'abc', $encoding)
    Assert-Payload $false 'unlisted stale library'
    Remove-Item -LiteralPath (Join-Path $directory 'unexpected.dll')
    foreach ($unsafe in @('../outside.dll', '/outside.dll', 'C:/outside.dll', 'a/../../outside.dll', 'a\\outside.dll', 'a:stream', 'a//empty.dll', 'a/./file.dll', 'payload-files.json')) {
        $entry = [ordered]@{path=$unsafe;size=3;sha256=$original[0].sha256}
        Write-Manifest (@($original) + @($entry))
        Assert-Payload $false ('unsafe manifest path ' + $unsafe)
    }
    Write-Manifest (@($original) + @([ordered]@{path='TRAMOING-SENSORS.EXE';size=3;sha256=$original[0].sha256}))
    Assert-Payload $false 'case-insensitive duplicate'
    Write-Manifest @($original | Where-Object { $_.path -ne 'tramoing-sensors.dll' })
    Assert-Payload $false 'mandatory assembly absent from manifest'
    [IO.File]::WriteAllText($manifestPath, '{broken', $encoding)
    Assert-Payload $false 'malformed file manifest'
    Write-Manifest $original
    Assert-Payload $true 'repaired payload'
    $runtimeConfig = Join-Path $directory 'tramoing-sensors.runtimeconfig.json'
    [IO.File]::WriteAllText($runtimeConfig, '{broken', $encoding)
    if (Test-LegacySensorPayload $directory) { throw 'Malformed legacy config was accepted' }
    [IO.File]::WriteAllText($runtimeConfig, '{"runtimeOptions":{"includedFrameworks":[{"name":"Microsoft.NETCore.App","version":"10.0.1"}]}}', $encoding)
    $legacyAssets = @('tramoing-sensors.dll', 'hostfxr.dll', 'hostpolicy.dll', 'coreclr.dll', 'System.Private.CoreLib.dll', 'LibreHardwareMonitorLib.dll', 'lib/net10.0/HidSharp.dll')
    $target = '.NETCoreApp,Version=v10.0/win-x64'
    $assets = @{}
    foreach ($asset in $legacyAssets) {
        $assets[$asset] = @{}
        [IO.File]::WriteAllText((Join-Path $directory ([IO.Path]::GetFileName($asset))), 'abc', $encoding)
    }
    $deps = @{runtimeTarget=@{name=$target};targets=@{$target=@{'worker/1.0.0'=@{runtime=$assets}}}}
    [IO.File]::WriteAllText((Join-Path $directory 'tramoing-sensors.deps.json'), ($deps | ConvertTo-Json -Depth 8), $encoding)
    if (-not (Test-LegacySensorPayload $directory)) { throw 'Compatible legacy runtime was not reused' }
    foreach ($name in @('coreclr.dll', 'tramoing-sensors.dll', 'HidSharp.dll', 'tramoing-sensors.deps.json')) {
        $path = Join-Path $directory $name
        $bytes = [IO.File]::ReadAllBytes($path)
        Remove-Item -LiteralPath $path
        if (Test-LegacySensorPayload $directory) { throw "Missing legacy file was accepted: $name" }
        [IO.File]::WriteAllBytes($path, $bytes)
    }

    $hostDir = New-Item -ItemType Directory -Path (Join-Path $directory 'host/fxr/10.0.9') -Force
    $olderHost = Join-Path $hostDir.FullName 'hostfxr.dll'
    New-Item -ItemType Directory -Path (Join-Path $directory 'host/fxr/backup') -Force | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $directory 'host/fxr/99.0.0.1') -Force | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $directory 'host/fxr/099.0.0') -Force | Out-Null
    $framework = New-Item -ItemType Directory -Path (Join-Path $directory 'shared/Microsoft.NETCore.App/10.0.2') -Force
    [IO.File]::WriteAllText((Join-Path $framework.FullName 'System.Private.CoreLib.dll'), 'library', $encoding)
    $frameworkDeps = Join-Path $framework.FullName 'Microsoft.NETCore.App.deps.json'
    [IO.File]::WriteAllText($frameworkDeps, '{"runtimeTarget":{"name":".NETCoreApp,Version=v10.0/win-x64"},"targets":{},"libraries":{}}', $encoding)
    $core = Join-Path $framework.FullName 'coreclr.dll'
    $header = New-Object byte[] 72
    $header[0] = 0x4d; $header[1] = 0x5a; $header[0x3c] = 64
    $header[64] = 0x50; $header[65] = 0x45
    $header[68] = 0x64; $header[69] = 0x86
    [IO.File]::WriteAllBytes($core, $header)
    [IO.File]::WriteAllBytes($olderHost, $header)
    [IO.File]::WriteAllBytes((Join-Path $framework.FullName 'hostpolicy.dll'), $header)
    if (-not (Test-X64DotNetRuntime $directory ([version]'10.0.0'))) { throw 'Compatible global runtime without MSI inventory was missed' }
    $depsBytes = [IO.File]::ReadAllBytes($frameworkDeps)
    Remove-Item -LiteralPath $frameworkDeps
    if (Test-X64DotNetRuntime $directory ([version]'10.0.0')) { throw 'Framework missing its .deps.json was accepted' }
    [IO.File]::WriteAllBytes($frameworkDeps, $depsBytes)
    if (-not (Test-X64DotNetRuntime $directory ([version]'10.0.0'))) { throw 'Restored framework manifest was not recognized' }

    $newerDir = New-Item -ItemType Directory -Path (Join-Path $directory 'host/fxr/10.0.10') -Force
    if (Test-X64DotNetRuntime $directory ([version]'10.0.0')) { throw 'Working 10.0.9 host masked a missing 10.0.10 host' }
    $newerHost = Join-Path $newerDir.FullName 'hostfxr.dll'
    $armHeader = [byte[]]$header.Clone()
    $armHeader[69] = 0xaa
    [IO.File]::WriteAllBytes($newerHost, $armHeader)
    if (Test-X64DotNetRuntime $directory ([version]'10.0.0')) { throw 'ARM64 hostfxr was reported as x64' }
    [IO.File]::WriteAllBytes($newerHost, $header)
    Remove-Item -LiteralPath $olderHost
    if (-not (Test-X64DotNetRuntime $directory ([version]'10.0.0'))) { throw 'Incomplete 10.0.9 host masked a working 10.0.10 host' }

    $firstPreview = New-Item -ItemType Directory -Path (Join-Path $directory 'host/fxr/11.0.0-preview.9') -Force
    [IO.File]::WriteAllBytes((Join-Path $firstPreview.FullName 'hostfxr.dll'), $header)
    New-Item -ItemType Directory -Path (Join-Path $directory 'host/fxr/11.0.0-preview.10') -Force | Out-Null
    if (Test-X64DotNetRuntime $directory ([version]'10.0.0')) { throw 'An older host masked a missing 11.0.0-preview.10 host' }
    $releaseHostDir = New-Item -ItemType Directory -Path (Join-Path $directory 'host/fxr/11.0.0') -Force
    [IO.File]::WriteAllBytes((Join-Path $releaseHostDir.FullName 'hostfxr.dll'), $header)
    if (-not (Test-X64DotNetRuntime $directory ([version]'10.0.0'))) { throw 'An incomplete preview masked a working final host' }
    foreach ($minimum in @('10.0.3', '11.0.0')) {
        if (Test-X64DotNetRuntime $directory ([version]$minimum)) { throw "Incompatible runtime accepted for $minimum" }
    }
    [IO.File]::WriteAllBytes($core, $armHeader)
    if (Test-X64DotNetRuntime $directory ([version]'10.0.0')) { throw 'ARM64 coreclr was reported as x64' }
    [IO.File]::WriteAllBytes($core, $header)

    $newerFramework = New-Item -ItemType Directory -Path (Join-Path $directory 'shared/Microsoft.NETCore.App/10.0.10') -Force
    $newerCore = Join-Path $newerFramework.FullName 'coreclr.dll'
    Copy-Item -LiteralPath (Join-Path $framework.FullName 'System.Private.CoreLib.dll') -Destination $newerFramework.FullName
    [IO.File]::WriteAllBytes((Join-Path $newerFramework.FullName 'hostpolicy.dll'), $header)
    [IO.File]::WriteAllBytes($newerCore, $armHeader)
    if (-not (Test-X64DotNetRuntime $directory ([version]'10.0.0'))) { throw 'Unlisted framework patch masked a usable older patch' }
    Copy-Item -LiteralPath $frameworkDeps -Destination $newerFramework.FullName
    if (Test-X64DotNetRuntime $directory ([version]'10.0.0')) { throw 'Working older patch masked ARM64 coreclr in the selected newer framework' }
    [IO.File]::WriteAllBytes($newerCore, $header)
    if (-not (Test-X64DotNetRuntime $directory ([version]'10.0.0'))) { throw 'Repaired newer framework was not recognized' }
    Write-Host 'Sensor payload validation passed, including legacy dependencies and global runtime version/architecture checks.'
} finally {
    if (Test-Path -LiteralPath $directory) { Remove-Item -LiteralPath $directory -Recurse -Force }
}
