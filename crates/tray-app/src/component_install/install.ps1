# Embedded in tramoing.exe. Input contains only files chosen from the embedded
# manifest; this script never executes anything directly from the user cache.
Assert-InstallRoot $cfg.root
$stage = Join-Path $cfg.root ('.component-install-' + [guid]::NewGuid().ToString('N'))
$log = Join-Path $cfg.root 'component-install.log'
if ((Test-Path -LiteralPath $log) -and ((Get-Item -LiteralPath $log).Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Invalid component log path' }
$mutex = [Threading.Mutex]::new($false, 'Global\TramoingDependencyInstall')
$locked = $false
$restart = $false
try {
    try { $locked = $mutex.WaitOne(0) } catch [Threading.AbandonedMutexException] { $locked = $true }
    if (-not $locked) { throw 'Another component installation is already running' }
    $security = [Security.AccessControl.DirectorySecurity]::new()
    $security.SetAccessRuleProtection($true, $false)
    foreach ($sid in @('S-1-5-18','S-1-5-32-544')) {
        $rule = [Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new($sid), 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')
        $security.AddAccessRule($rule)
    }
    [IO.Directory]::CreateDirectory($stage, $security) | Out-Null
    [IO.File]::WriteAllText($log, 'Verifying optional component installers.' + [Environment]::NewLine)
    $verified = @()
    foreach ($file in $cfg.files) {
        if ($file.id -notin @('dotnet','pawnio','sensors')) { throw 'Unknown component installer' }
        $target = Join-Path $stage ($file.id + '.exe')
        # Hold a read-only handle which denies concurrent writers while copying.
        $source = [IO.File]::Open([string]$file.path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
        try {
            if ($source.Length -ne [long]$file.size -or $source.Length -gt 200000000) { throw ('Installer size mismatch: ' + $file.id) }
            $destination = [IO.File]::Open($target, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
            try { $source.CopyTo($destination); $destination.Flush($true) } finally { $destination.Dispose() }
        } finally { $source.Dispose() }
        if ((Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash -ne [string]$file.sha256) { throw ('Installer checksum mismatch: ' + $file.id) }
        if ($file.publisher) {
            $signature = Get-AuthenticodeSignature -LiteralPath $target
            Assert-DependencySignature -Dependency $file.id -Signature $signature
        }
        $verified += [pscustomobject]@{ id=$file.id; path=$target }
    }
    # Validate every file before making the first system change.
    foreach ($file in $verified) {
        $arguments = switch ($file.id) {
            'dotnet' { '/install /quiet /norestart' }
            'pawnio' { '-install -silent' }
            # NSIS requires /D= last and unquoted, including paths with spaces.
            'sensors' { '/S /D=' + $cfg.root }
        }
        [IO.File]::AppendAllText($log, 'Installing ' + $file.id + [Environment]::NewLine)
        $start = [Diagnostics.ProcessStartInfo]::new($file.path, $arguments)
        $start.UseShellExecute = $false
        $start.WorkingDirectory = $stage
        $process = [Diagnostics.Process]::Start($start)
        try {
            $process.WaitForExit()
            $code = $process.ExitCode
        } finally { $process.Dispose() }
        if ($code -in @(3010,1641)) { $restart = $true }
        elseif ($code -ne 0) { throw ('Installer failed: ' + $file.id + ' (' + $code + ')') }
    }
    [IO.File]::AppendAllText($log, 'Installation completed.' + [Environment]::NewLine)
    if ($restart) { exit 3010 }
    exit 0
} catch {
    [IO.File]::AppendAllText($log, $_.Exception.Message + [Environment]::NewLine)
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
} finally {
    if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue }
    if ($locked) { $mutex.ReleaseMutex() }
    $mutex.Dispose()
}
