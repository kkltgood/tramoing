# Compile a non-elevated NSIS fixture against the real upgrade callbacks.
# All payloads and registry entries belong to this test run only.
param([string]$MakeNsis = '')

$ErrorActionPreference = 'Stop'
if (-not $MakeNsis) {
    $command = Get-Command makensis -ErrorAction SilentlyContinue
    if ($command) { $MakeNsis = $command.Source }
    foreach ($candidate in @("${env:ProgramFiles(x86)}\NSIS\makensis.exe", "$env:ProgramFiles\NSIS\makensis.exe")) {
        if (-not $MakeNsis -and (Test-Path -LiteralPath $candidate)) { $MakeNsis = $candidate }
    }
}
if (-not $MakeNsis) { throw 'NSIS 3 is required to verify the upgrade flow.' }

$repository = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$testId = [guid]::NewGuid().ToString('N')
$testRoot = Join-Path $repository "target\upgrade-test-$testId"
$bundleId = "TramoingUpgradeTest-$testId"
$registryPath = "HKCU:\Software\$bundleId"
$legacyRegistryPath = "$registryPath-legacy"
New-Item -ItemType Directory -Path $testRoot -Force | Out-Null
$fixture = Join-Path $testRoot 'fixture.exe'

function Assert-That([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

function New-Case([string]$Name) {
    $caseRoot = Join-Path $testRoot $Name
    New-Item -ItemType Directory -Path $caseRoot | Out-Null
    Copy-Item -LiteralPath $fixture -Destination (Join-Path $caseRoot 'setup.exe')
    return $caseRoot
}

function Set-Installed([string]$Directory, [string]$Registry = $registryPath, [string]$App = 'tramoing.exe') {
    New-Item -ItemType Directory -Path $Directory -Force | Out-Null
    [IO.File]::WriteAllText((Join-Path $Directory $App), 'old payload')
    [IO.File]::WriteAllText((Join-Path $Directory 'settings.conf'), 'existing settings')
    [IO.File]::WriteAllText((Join-Path $Directory 'history.db'), 'existing history')
    New-Item -Path $Registry -Force | Out-Null
    New-ItemProperty -LiteralPath $Registry -Name InstallDir -Value $Directory -PropertyType String -Force | Out-Null
}

function Run-Case([string]$CaseRoot, [string]$Arguments = '', [int]$ExpectedExit = 0) {
    # WritePrivateProfileString uses ANSI unless the existing INI has a UTF-16 BOM.
    $result = Join-Path $CaseRoot 'result.ini'
    [IO.File]::WriteAllBytes($result, [Text.Encoding]::Unicode.GetPreamble())
    $options = @{
        FilePath = (Join-Path $CaseRoot 'setup.exe')
        WindowStyle = 'Hidden'
        PassThru = $true
    }
    if ($Arguments) { $options.ArgumentList = $Arguments }
    $process = Start-Process @options
    try {
        if (-not $process.WaitForExit(25000)) {
            throw "Installer stalled on an unexpected page: $CaseRoot"
        }
        Assert-That ($process.ExitCode -eq $ExpectedExit) "Unexpected exit $($process.ExitCode): $CaseRoot"
    } finally {
        if (-not $process.HasExited) { Stop-Process -Id $process.Id -Force }
        $process.Dispose()
    }
    if (Test-Path -LiteralPath $result) { return [IO.File]::ReadAllText($result) }
    return ''
}

function Assert-Upgraded([string]$Result, [string]$Directory) {
    Assert-That ($Result.Contains('installed=1')) 'Upgrade did not install the payload.'
    Assert-That ($Result.Contains('restarted=1')) 'Upgrade did not invoke the restart callback.'
    Assert-That (-not $Result.Contains('page=')) 'Upgrade displayed a setup wizard page.'
    Assert-That ($Result.Contains("directory=$Directory")) 'Upgrade changed the installation directory.'
    Assert-That ([IO.File]::ReadAllText((Join-Path $Directory 'tramoing.exe')) -eq 'new payload') 'Payload was not replaced.'
    Assert-That ([IO.File]::ReadAllText((Join-Path $Directory 'settings.conf')) -eq 'existing settings') 'Settings changed.'
    Assert-That ([IO.File]::ReadAllText((Join-Path $Directory 'history.db')) -eq 'existing history') 'History changed.'
}

try {
    & $MakeNsis '/V2' "/DTEST_BUNDLE_ID=$bundleId" "/DTEST_OUTPUT=$fixture" (Join-Path $repository 'packaging\windows\tests\upgrade-harness.nsi')
    if ($LASTEXITCODE -ne 0) { throw 'Failed to compile the NSIS upgrade harness.' }

    $caseRoot = New-Case 'fresh'
    $result = Run-Case $caseRoot
    Assert-That ($result.Contains('page=directory') -and -not $result.Contains('restarted=1')) 'Fresh installation skipped the setup wizard.'
    Write-Output 'PASS fresh installation keeps the setup wizard'

    $caseRoot = New-Case 'old-client'
    $installedDir = Join-Path $caseRoot 'custom apps & tools\安装目录'
    Set-Installed $installedDir
    $result = Run-Case $caseRoot
    Assert-Upgraded $result $installedDir
    Write-Output 'PASS old client without flags upgrades in place and restarts'

    $caseRoot = New-Case 'explicit'
    $installedDir = Join-Path $caseRoot 'another directory\中文 & spaces'
    # Leave InstallDir pointing at the previous case; /D must override it.
    Set-Installed $installedDir $legacyRegistryPath
    $result = Run-Case $caseRoot "/UPDATE /D=$installedDir"
    Assert-Upgraded $result $installedDir
    Write-Output 'PASS explicit update preserves its actual path, including spaces and Unicode'

    $caseRoot = New-Case 'parent-exit'
    $installedDir = Join-Path $caseRoot 'app'
    Set-Installed $installedDir
    $sleeper = Join-Path $testRoot 'parent.ps1'
    [IO.File]::WriteAllText($sleeper, 'Start-Sleep -Seconds 4')
    $parent = Start-Process -FilePath powershell.exe -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$sleeper`"" -WindowStyle Hidden -PassThru
    try {
        $result = Run-Case $caseRoot "/UPDATE /APP_PID=$($parent.Id) /D=$installedDir"
        Assert-That $parent.HasExited 'Installer did not wait for the old application to exit.'
        Assert-Upgraded $result $installedDir
        # Reuse the already exited PID to cover the other side of the handoff race.
        $result = Run-Case $caseRoot "/UPDATE /APP_PID=$($parent.Id) /D=$installedDir"
        Assert-Upgraded $result $installedDir
    } finally {
        if (-not $parent.HasExited) { Stop-Process -Id $parent.Id -Force }
        $parent.Dispose()
    }
    Write-Output 'PASS installer waits for parent exit and accepts an already exited parent'

    $caseRoot = New-Case 'parent-timeout'
    $installedDir = Join-Path $caseRoot 'app'
    Set-Installed $installedDir
    # The test runner stays alive; a timeout must leave the old payload untouched.
    $result = Run-Case $caseRoot "/S /UPDATE /APP_PID=$PID /D=$installedDir" 1
    Assert-That ($result.Contains('failed=1') -and -not $result.Contains('restarted=1')) 'Parent timeout was treated as success.'
    Assert-That ([IO.File]::ReadAllText((Join-Path $installedDir 'tramoing.exe')) -eq 'old payload') 'Files changed before the parent exited.'
    Write-Output 'PASS parent timeout stops the update before changing files'

    $caseRoot = New-Case 'failed-install'
    $installedDir = Join-Path $caseRoot 'app'
    Set-Installed $installedDir
    [IO.File]::WriteAllText((Join-Path $caseRoot 'fail-install'), '')
    $result = Run-Case $caseRoot "/S /UPDATE /D=$installedDir" 1
    Assert-That ($result.Contains('failed=1') -and -not $result.Contains('restarted=1')) 'Failed install restarted the application.'
    Write-Output 'PASS failed installation does not run the success restart callback'

    $caseRoot = New-Case 'missing-update-target'
    $result = Run-Case $caseRoot "/S /UPDATE /D=$caseRoot\missing" 1
    Assert-That (-not $result.Contains('installed=1')) 'Missing update target fell back to a fresh installation.'
    Write-Output 'PASS missing update target is rejected'

    # Remove only the two unique, verified test keys; no system installation is touched.
    Remove-Item -LiteralPath $registryPath -Recurse -Force
    $caseRoot = New-Case 'legacy'
    $installedDir = Join-Path $caseRoot 'Traffic Monitor'
    Set-Installed $installedDir $legacyRegistryPath 'traffic-monitor.exe'
    $result = Run-Case $caseRoot
    Assert-Upgraded $result $installedDir
    Write-Output 'PASS legacy installation is detected and upgraded in its original directory'

    New-ItemProperty -LiteralPath $legacyRegistryPath -Name InstallDir -Value (Join-Path $testRoot 'missing') -Force | Out-Null
    $caseRoot = New-Case 'stale-registry'
    $result = Run-Case $caseRoot
    Assert-That ($result.Contains('page=directory')) 'Stale installation registry skipped fresh setup.'
    Write-Output 'PASS stale registry does not trigger upgrade mode'
} finally {
    foreach ($testRegistryPath in @($registryPath, $legacyRegistryPath)) {
        if ($testRegistryPath -notmatch '^HKCU:\\Software\\TramoingUpgradeTest-[a-f0-9]{32}(-legacy)?$') {
            throw "Refusing to remove an unexpected registry key: $testRegistryPath"
        }
        if (Test-Path -LiteralPath $testRegistryPath) { Remove-Item -LiteralPath $testRegistryPath -Recurse -Force }
    }
    Write-Output "Upgrade test artifacts: $testRoot"
}
