# Read-only diagnostics for a Windows collector that cannot connect.
# Compatible with Windows PowerShell 5.1; no task/process/service changes.
param(
    [string]$OutputPath = (Join-Path ([Environment]::GetFolderPath('Desktop')) ('Tramoing-diagnostics-' + (Get-Date -Format 'yyyyMMdd-HHmmss') + '.txt'))
)

$ErrorActionPreference = 'Stop'
$report = New-Object 'System.Collections.Generic.List[string]'
$report.Add('Tramoing Windows collector diagnostics')
$report.Add('Collected: ' + (Get-Date -Format 'o'))

function Add-Section {
    param([string]$Title, [scriptblock]$Read)
    $report.Add('')
    $report.Add('[' + $Title + ']')
    try {
        $text = (& $Read | Out-String -Width 240).TrimEnd()
        if ($text) { $report.Add($text) } else { $report.Add('(no entries)') }
    } catch {
        $report.Add('Unavailable: ' + $_.Exception.Message)
    }
}

Add-Section 'Windows' {
    $os = Get-CimInstance Win32_OperatingSystem
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    [pscustomobject]@{
        Caption = $os.Caption
        Version = $os.Version
        Build = $os.BuildNumber
        Architecture = $os.OSArchitecture
        Elevated = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    } | Format-List
}

Add-Section 'Installed binaries' {
    $installPath = $null
    foreach ($key in @('HKLM:\Software\Tramoing', 'HKLM:\Software\WOW6432Node\Tramoing')) {
        $candidate = (Get-ItemProperty $key -ErrorAction SilentlyContinue).InstallDir
        if ($candidate) { $installPath = $candidate; break }
    }
    if (-not $installPath) { $installPath = Join-Path $env:ProgramFiles 'Tramoing' }
    foreach ($binary in @('tramoing.exe', 'tramoing-helper.exe')) {
        $path = Join-Path $installPath $binary
        if (Test-Path -LiteralPath $path -PathType Leaf) {
            $file = Get-Item -LiteralPath $path
            [pscustomobject]@{
                Path = $file.FullName
                Version = $file.VersionInfo.ProductVersion
                Modified = $file.LastWriteTime
                SHA256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
            } | Format-List
        } else { 'Missing: ' + $path }
    }
}

Add-Section 'Collector task' {
    $task = Get-ScheduledTask -TaskName 'TramoingHelper'
    $info = $task | Get-ScheduledTaskInfo
    $currentSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $ownerSid = $task.Principal.UserId
    if ($ownerSid -notmatch '^S-1-') {
        try { $ownerSid = ([Security.Principal.NTAccount]::new($ownerSid)).Translate([Security.Principal.SecurityIdentifier]).Value }
        catch { $ownerSid = 'unresolved' }
    }
    [pscustomobject]@{
        TaskAccountMatchesCurrentUser = $(if ($ownerSid -eq 'unresolved') { 'unknown' } else { $ownerSid -eq $currentSid })
        State = $task.State
        LastRunTime = $info.LastRunTime
        LastTaskResult = ('0x{0:X8}' -f $info.LastTaskResult)
        RunLevel = $task.Principal.RunLevel
        LogonType = $task.Principal.LogonType
        DisallowStartIfOnBatteries = $task.Settings.DisallowStartIfOnBatteries
        StopIfGoingOnBatteries = $task.Settings.StopIfGoingOnBatteries
        ExecutionTimeLimit = $task.Settings.ExecutionTimeLimit
        RestartCount = $task.Settings.RestartCount
        Command = ($task.Actions.Execute -join '; ')
        CommandExists = (($task.Actions | ForEach-Object { Test-Path -LiteralPath $_.Execute -PathType Leaf }) -join '; ')
    } | Format-List
}

Add-Section 'Processes' {
    Get-Process -Name 'tramoing','tramoing-helper' -ErrorAction SilentlyContinue |
        Select-Object ProcessName,Id,SessionId,Path | Format-Table -AutoSize
}

Add-Section 'Task Scheduler service' {
    Get-Service -Name Schedule | Select-Object Name,Status,StartType | Format-List
}

foreach ($name in @('ui.log.3', 'ui.log.2', 'ui.log.1', 'ui.log', 'helper.log.3', 'helper.log.2', 'helper.log.1', 'helper.log')) {
    Add-Section $name {
        $path = Join-Path (Join-Path $env:LOCALAPPDATA 'Tramoing') $name
        'Path: ' + $path
        if (Test-Path -LiteralPath $path -PathType Leaf) {
            Get-Content -LiteralPath $path -Encoding UTF8 -Tail 100
        } else { '(log not found for this Windows account)' }
    }
}

$report.Add('')
$report.Add('If the collector was installed using a different administrator account, its helper.log may be in that account. No processes or scheduled tasks were modified.')
$resolvedOutput = [IO.Path]::GetFullPath($OutputPath)
[IO.File]::WriteAllLines($resolvedOutput, $report, [Text.UTF8Encoding]::new($true))
Write-Output ('Saved: ' + $resolvedOutput)
