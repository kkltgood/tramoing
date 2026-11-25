# Optional, read-only bridge to an ALREADY RUNNING hardware monitor's WMI provider.
# Bounded snapshot file; no driver installation, elevation, network or fan control.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
try {
$owner = Get-Process -Id ([int]$env:TM_TEMPERATURE_OWNER)
$ownerStarted = $owner.StartTime
$sequence = 0
while ($true) {
    try {
        $currentOwner = Get-Process -Id $owner.Id
        if ($currentOwner.StartTime -ne $ownerStarted) { break }
    } catch { break }
    $sequence++
    $cpu = $null
    $gpu = $null
    $provider = $null
    $cpuSource = 'unavailable'
    $gpuSource = 'unavailable'
    $failures = @()
    foreach ($namespace in @('root/LibreHardwareMonitor', 'root/OpenHardwareMonitor')) {
        try {
            $hardware = @(Get-CimInstance -Namespace $namespace -ClassName Hardware -OperationTimeoutSec 2)
            $sensors = @(Get-CimInstance -Namespace $namespace -ClassName Sensor -Filter "SensorType='Temperature'" -OperationTimeoutSec 2)
            if ($hardware.Count -eq 0) { throw 'No hardware monitor instances' }
            $types = @{}
            foreach ($device in $hardware) { $types[[string]$device.Identifier] = [string]$device.HardwareType }
            foreach ($sensor in $sensors) {
                if ($null -eq $sensor.Value) { continue }
                $value = [double]$sensor.Value
                if ($value -le 0 -or $value -gt 150 -or [double]::IsNaN($value)) { continue }
                $type = $types[[string]$sensor.Parent]
                if ($type -eq 'Cpu' -and $sensor.Name -notmatch 'Distance|TjMax') {
                    if ($null -eq $cpu -or $value -gt $cpu) { $cpu = $value; $cpuSource = $namespace }
                }
                if ($type -like 'Gpu*' -and $sensor.Name -match '^GPU (Core|Temperature)$') {
                    if ($null -eq $gpu -or $value -gt $gpu) { $gpu = $value; $gpuSource = $namespace }
                }
            }
            $provider = $namespace
            if ($null -ne $cpu -and $null -ne $gpu) { break }
        } catch {
            $failures += "$namespace ($($_.Exception.HResult))"
        }
    }
    $source = if ($provider) { "WMI CPU=$cpuSource; GPU=$gpuSource; hottest CPU / GPU core sensor" } else { "WMI provider unavailable: $($failures -join ', ')" }
    $frame = @{ sequence=$sequence; cpu=$cpu; gpu=$gpu; source=$source } | ConvertTo-Json -Compress
    [IO.File]::WriteAllText($env:TM_TEMPERATURE_OUTPUT, $frame, [Text.UTF8Encoding]::new($false))
    if (-not $provider) { break }
    Start-Sleep -Seconds 2
}

} catch {
    $frame = @{ sequence=([long]$sequence + 1); cpu=$null; gpu=$null; source="Temperature WMI bridge failed: $($_.Exception.Message) ($($_.Exception.HResult))" } | ConvertTo-Json -Compress
    try { [IO.File]::WriteAllText($env:TM_TEMPERATURE_OUTPUT, $frame, [Text.UTF8Encoding]::new($false)) } catch { }
    exit 1
}
