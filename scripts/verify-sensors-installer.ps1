# Exercise component replacement/rollback without elevation, real processes or drivers.
param([string]$MakeNsis = '')
$ErrorActionPreference = 'Stop'
if (-not $MakeNsis) {
    $command = Get-Command makensis -ErrorAction SilentlyContinue
    if ($command) { $MakeNsis = $command.Source }
    foreach ($candidate in @("${env:ProgramFiles(x86)}\NSIS\makensis.exe", "$env:ProgramFiles\NSIS\makensis.exe")) {
        if (-not $MakeNsis -and (Test-Path -LiteralPath $candidate)) { $MakeNsis = $candidate }
    }
}
if (-not $MakeNsis) { throw 'NSIS 3 is required to verify the component installer.' }
$root = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$testRoot = Join-Path $root ('target/sensor-installer-test-' + [guid]::NewGuid().ToString('N'))
$payload = Join-Path $testRoot 'payload'
New-Item -ItemType Directory -Path $payload -Force | Out-Null
[IO.File]::WriteAllText((Join-Path $payload 'tramoing-sensors.exe'), 'new sensor')
[IO.File]::WriteAllText((Join-Path $payload 'component.json'), '{"version":"1.0.0","protocol":1}')
$installer = Join-Path $testRoot 'fixture.exe'
& $MakeNsis '/DSENSOR_TEST_MODE' '/DSENSOR_VERSION=1.0.0' "/DSENSOR_PAYLOAD=$payload" "/DSENSOR_OUTPUT=$installer" (Join-Path $root 'packaging/windows/sensors.nsi')
if ($LASTEXITCODE -ne 0) { throw 'Component installer fixture compilation failed' }
function Assert-That([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}
function New-Installed([string]$Name, [bool]$WithSensors) {
    $directory = Join-Path $testRoot $Name
    New-Item -ItemType Directory -Path $directory | Out-Null
    [IO.File]::WriteAllText((Join-Path $directory 'tramoing.exe'), 'unchanged main')
    [IO.File]::WriteAllText((Join-Path $directory 'tramoing-helper.exe'), 'unchanged helper')
    if ($WithSensors) {
        New-Item -ItemType Directory -Path (Join-Path $directory 'sensors') | Out-Null
        [IO.File]::WriteAllText((Join-Path $directory 'sensors/tramoing-sensors.exe'), 'old working sensor')
        [IO.File]::WriteAllText((Join-Path $directory 'sensors/old-runtime.dll'), 'old private runtime')
    }
    return $directory
}
function Invoke-Case([string]$Directory, [int]$ExpectedExit) {
    $process = Start-Process -FilePath $installer -ArgumentList "/S /D=$Directory" -PassThru
    try {
        if (-not $process.WaitForExit(20000)) { throw 'Component installer did not finish' }
        Assert-That ($process.ExitCode -eq $ExpectedExit) "Unexpected installer exit: $($process.ExitCode), expected $ExpectedExit"
    } finally {
        if (-not $process.HasExited) { $process.Kill() }
        $process.Dispose()
    }
    Assert-That ([IO.File]::ReadAllText((Join-Path $Directory 'tramoing.exe')) -eq 'unchanged main') 'Component modified main executable'
    Assert-That ([IO.File]::ReadAllText((Join-Path $Directory 'tramoing-helper.exe')) -eq 'unchanged helper') 'Component modified helper executable'
}
$fresh = New-Installed 'fresh install with spaces' $false
Invoke-Case $fresh 0
Assert-That ([IO.File]::ReadAllText((Join-Path $fresh 'sensors/tramoing-sensors.exe')) -eq 'new sensor') 'Fresh component was not installed'
$upgrade = New-Installed 'existing component' $true
Invoke-Case $upgrade 0
Assert-That ([IO.File]::ReadAllText((Join-Path $upgrade 'sensors/tramoing-sensors.exe')) -eq 'new sensor') 'Component was not replaced'
Assert-That (-not (Test-Path -LiteralPath (Join-Path $upgrade 'sensors/old-runtime.dll'))) 'Obsolete private runtime survived component replacement'
$rollback = New-Installed 'rollback' $true
[IO.File]::WriteAllText((Join-Path $rollback 'fail-restart'), 'simulate helper restart failure')
Invoke-Case $rollback 1
Assert-That ([IO.File]::ReadAllText((Join-Path $rollback 'sensors/tramoing-sensors.exe')) -eq 'old working sensor') 'Failed replacement did not restore old sensor'
Assert-That (Test-Path -LiteralPath (Join-Path $rollback 'sensors/old-runtime.dll')) 'Rollback discarded old runtime'
Assert-That (-not (Test-Path -LiteralPath (Join-Path $rollback 'sensors.next'))) 'Failed replacement left staging files'
$recovery = New-Installed 'interrupted installation' $true
New-Item -ItemType Directory -Path (Join-Path $recovery 'sensors.previous') | Out-Null
[IO.File]::WriteAllText((Join-Path $recovery 'sensors.previous/recover-me'), 'preserve this backup')
Invoke-Case $recovery 1
Assert-That (Test-Path -LiteralPath (Join-Path $recovery 'sensors.previous/recover-me')) 'Interrupted installation backup was deleted'
$junction = New-Installed 'junction rejection' $false
$outside = Join-Path $testRoot 'outside component directory'
New-Item -ItemType Directory -Path $outside | Out-Null
[IO.File]::WriteAllText((Join-Path $outside 'keep-me'), 'unrelated data')
New-Item -ItemType Junction -Path (Join-Path $junction 'sensors') -Value $outside | Out-Null
Invoke-Case $junction 1
Assert-That ([IO.File]::ReadAllText((Join-Path $outside 'keep-me')) -eq 'unrelated data') 'Component installer followed a directory junction'
Write-Host 'Sensor component installer: fresh install, replacement, rollback, interrupted-install recovery and junction rejection passed.'
