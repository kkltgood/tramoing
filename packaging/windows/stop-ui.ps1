# Stop GUI processes politely so they can remove their notification icons and
# flush history. Forced termination is only a bounded fallback for a hung UI.
param(
    [Parameter(Mandatory = $true)]
    [string]$ProcessName,
    [ValidateRange(100, 30000)]
    [int]$TimeoutMilliseconds = 10000
)

$ErrorActionPreference = 'Stop'
$ProcessName = [IO.Path]::GetFileNameWithoutExtension($ProcessName)
$processes = @(Get-Process -Name $ProcessName -ErrorAction SilentlyContinue)
try {
    foreach ($process in $processes) {
        # Keep the process handle open across shutdown; it identifies this
        # instance even if Windows later reuses its PID.
        try { $null = $process.Handle } catch {
            if ($process.HasExited) { continue }
            throw
        }
        if ($process.HasExited) { continue }
        Write-Output "Requesting normal exit: $ProcessName ($($process.Id))"
        # Tramoing keeps its main window WS_VISIBLE while parked offscreen.
        # Post WM_CLOSE to that window; do not close the tray library's hidden
        # callback window before the UI has removed its notification icon.
        try { $null = $process.CloseMainWindow() } catch {
            if ($process.HasExited) { continue }
            throw
        }
        if ($process.WaitForExit($TimeoutMilliseconds)) { continue }

        Write-Output "Normal exit timed out; terminating: $ProcessName ($($process.Id))"
        try { $process.Kill() } catch {
            if (-not $process.HasExited) { throw }
        }
        if (-not $process.WaitForExit(5000)) {
            throw "Process did not exit: $ProcessName ($($process.Id))"
        }
    }
} finally {
    foreach ($process in $processes) { $process.Dispose() }
}

# A missing/already exited process is a successful shutdown.
exit 0
