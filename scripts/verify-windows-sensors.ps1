# Read-only smoke test of the real worker. Does not install a driver or elevate.
param([ValidateSet('x64', 'arm64')][string]$Architecture = 'x64')
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$path = Join-Path $root "dist/windows-$Architecture/sensors/tramoing-sensors.exe"
$start = [Diagnostics.ProcessStartInfo]::new()
$start.FileName = $path
$start.UseShellExecute = $false
$start.CreateNoWindow = $true
$start.RedirectStandardInput = $true
$start.RedirectStandardOutput = $true
$start.RedirectStandardError = $true
$process = [Diagnostics.Process]::new()
$process.StartInfo = $start
try {
    if (-not $process.Start()) { throw 'Cannot start sensor worker' }
    # The worker must wait for the supervisor handshake.
    $stream = $process.StandardOutput.BaseStream
    $header = New-Object byte[] 4
    $read = $stream.ReadAsync($header, 0, 4)
    if ($read.Wait(500)) { throw 'Worker emitted data before the start handshake' }
    $process.StandardInput.WriteLine('start')
    $process.StandardInput.Flush()
    if (-not $read.Wait(15000)) { throw 'Sensor worker did not respond in 15 seconds' }
    $count = $read.Result
    while ($count -lt 4) {
        $read = $stream.ReadAsync($header, $count, 4 - $count)
        if (-not $read.Wait(2000) -or $read.Result -eq 0) { throw 'Truncated sensor header' }
        $count += $read.Result
    }
    $size = [BitConverter]::ToUInt32($header, 0)
    if ($size -lt 1 -or $size -gt 8192) { throw 'Invalid sensor frame length' }
    $body = New-Object byte[] $size
    $offset = 0
    while ($offset -lt $size) {
        $read = $stream.ReadAsync($body, $offset, $size - $offset)
        if (-not $read.Wait(2000) -or $read.Result -eq 0) { throw 'Truncated sensor frame' }
        $offset += $read.Result
    }
    $frame = [Text.Encoding]::UTF8.GetString($body) | ConvertFrom-Json
    $allowed = @('ready','starting','permission_denied','unsupported_platform','driver_missing','driver_unavailable','sensor_unavailable')
    if ($frame.status -notin $allowed -or -not $frame.source) { throw "Unexpected sensor response: $($frame | ConvertTo-Json -Compress)" }
    if ($frame.status -ne 'ready' -and ($null -ne $frame.cpu -or $null -ne $frame.gpu)) { throw 'Failure returned a temperature' }
    if ($frame.status -eq 'ready' -and ($frame.cpu -le 0 -or $frame.cpu -gt 150)) { throw 'Invalid CPU temperature' }
    Write-Host "Sensor worker protocol passed: $($frame.status) ($($frame.source))"
} finally {
    if ($process.Id -and -not $process.HasExited) { $process.Kill(); $process.WaitForExit() }
    $process.Dispose()
}
