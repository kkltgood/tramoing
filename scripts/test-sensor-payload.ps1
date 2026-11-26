# Tests only local payload validation; does not inspect drivers, elevate, or run executables.
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$source = Join-Path $root 'crates/tray-app/src/component_install/probe.ps1'
$tokens = $null
$parseErrors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($source, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw ($parseErrors | Out-String) }
foreach ($name in @('Test-SensorPayload', 'Test-LegacySensorPayload')) {
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
    foreach ($name in @('coreclr.dll', 'System.Private.CoreLib.dll')) {
        [IO.File]::WriteAllText((Join-Path $directory $name), 'abc', $encoding)
    }
    if (-not (Test-LegacySensorPayload $directory)) { throw 'Compatible legacy runtime was not reused' }
    Remove-Item -LiteralPath (Join-Path $directory 'coreclr.dll')
    if (Test-LegacySensorPayload $directory) { throw 'Missing legacy runtime was accepted' }
    Write-Host 'Sensor payload validation passed: complete/corrupt/missing files, unsafe paths, stale DLLs, legacy repair.'
} finally {
    if (Test-Path -LiteralPath $directory) { Remove-Item -LiteralPath $directory -Recurse -Force }
}
