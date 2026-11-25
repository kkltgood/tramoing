# Embedded into the application. Read-only; never starts/stops a task or process.
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)

function Section([string]$Title, [scriptblock]$Read) {
    "`n[$Title]"
    try { (& $Read | Out-String -Width 240).TrimEnd() }
    catch { 'Unavailable: ' + $_.Exception.Message }
}

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$sid = $identity.User.Value
$executable = $env:TRAMOING_DIAGNOSTIC_EXE

Section 'Windows and account' {
    $os = Get-CimInstance Win32_OperatingSystem
    [pscustomobject]@{
        Version = $os.Version
        Build = $os.BuildNumber
        Architecture = $os.OSArchitecture
        Elevated = ([Security.Principal.WindowsPrincipal]::new($identity)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
        SessionId = (Get-Process -Id $PID).SessionId
    } | Format-List
}

Section 'Installed binaries beside this UI' {
    foreach ($name in @('tramoing.exe', 'tramoing-helper.exe')) {
        $path = Join-Path (Split-Path -LiteralPath $executable) $name
        if (Test-Path -LiteralPath $path -PathType Leaf) {
            $file = Get-Item -LiteralPath $path
            [pscustomobject]@{ Path = $path; Version = $file.VersionInfo.ProductVersion; Bytes = $file.Length; Modified = $file.LastWriteTimeUtc } | Format-List
        } else { 'MISSING: ' + $path }
    }
}

Section 'Collector scheduled task' {
    $task = Get-ScheduledTask -TaskName 'TramoingHelper'
    $info = $task | Get-ScheduledTaskInfo
    $ownerSid = $task.Principal.UserId
    if ($ownerSid -notmatch '^S-1-') {
        try { $ownerSid = ([Security.Principal.NTAccount]::new($ownerSid)).Translate([Security.Principal.SecurityIdentifier]).Value }
        catch { $ownerSid = 'unresolved' }
    }
    [pscustomobject]@{
        State = $task.State
        LastRunTime = $info.LastRunTime
        LastTaskResult = ('0x{0:X8}' -f $info.LastTaskResult)
        TaskAccountMatchesCurrentUser = $(if ($ownerSid -eq 'unresolved') { 'unknown' } else { $ownerSid -eq $sid })
        RunLevel = $task.Principal.RunLevel
        LogonType = $task.Principal.LogonType
        Command = ($task.Actions.Execute -join '; ')
        CommandExists = (($task.Actions | ForEach-Object { Test-Path -LiteralPath $_.Execute -PathType Leaf }) -join '; ')
        ExecutionTimeLimit = $task.Settings.ExecutionTimeLimit
        DisallowStartIfOnBatteries = $task.Settings.DisallowStartIfOnBatteries
        StopIfGoingOnBatteries = $task.Settings.StopIfGoingOnBatteries
    } | Format-List
}

Section 'Collector processes' {
    $processes = @(Get-CimInstance Win32_Process -Filter "Name='tramoing.exe' OR Name='tramoing-helper.exe'")
    if (-not $processes.Count) { '(none)' }
    foreach ($process in $processes) {
        $sameUser = 'unknown'
        try {
            $owner = Invoke-CimMethod -InputObject $process -MethodName GetOwnerSid
            if ($owner.ReturnValue -eq 0) { $sameUser = $owner.Sid -eq $sid }
        } catch {}
        [pscustomobject]@{ Name = $process.Name; PID = $process.ProcessId; SessionId = $process.SessionId; Path = $process.ExecutablePath; SameAccount = $sameUser } | Format-List
    }
}

Section 'Services' {
    Get-Service -Name Schedule,BFE | Select-Object Name,Status,StartType | Format-Table -AutoSize
}

Section 'Named pipe presence (no connection opened)' {
    try {
        $pipes = [IO.Directory]::GetFiles('\\.\pipe\')
        [pscustomobject]@{
            StatisticsPipePresent = $pipes -contains '\\.\pipe\tramoing-v1'
            CurrentAccountControlPipePresent = $pipes -contains ('\\.\pipe\Tramoing-Network-Control-' + $sid + '-v1')
        } | Format-List
    } catch { 'Unavailable: ' + $_.Exception.Message }
}

Section 'Recent task events (if enabled and readable)' {
    Get-WinEvent -FilterHashtable @{ LogName='Microsoft-Windows-TaskScheduler/Operational'; StartTime=(Get-Date).AddDays(-2) } -MaxEvents 200 |
        Where-Object { $_.Message -match 'TramoingHelper' } | Select-Object -First 10 TimeCreated,Id,LevelDisplayName,Message | Format-List
}

Section 'Recent application crashes (if readable)' {
    Get-WinEvent -FilterHashtable @{ LogName='Application'; Id=1000,1001; StartTime=(Get-Date).AddDays(-2) } -MaxEvents 100 |
        Where-Object { $_.Message -match 'tramoing(-helper)?\.exe' } | Select-Object -First 5 TimeCreated,Id,Message | Format-List
}
