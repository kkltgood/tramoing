# Runs without elevation. It only reads local machine state; never downloads.
# Verify the complete installed payload before reporting it as reusable. --probe
# intentionally avoids hardware libraries, so its success alone cannot detect a
# deleted or damaged transitive DLL. This function is also tested without Windows
# registry/driver operations by scripts/test-sensor-payload.ps1.
function Test-SensorPayload([string]$SensorRoot) {
    try {
        $root = [IO.Path]::GetFullPath($SensorRoot).TrimEnd([char[]]'\\/')
        $directory = Get-Item -LiteralPath $root -Force
        if (-not $directory.PSIsContainer -or ($directory.Attributes -band [IO.FileAttributes]::ReparsePoint)) { return $false }
        $manifestPath = Join-Path $root 'payload-files.json'
        $manifestFile = Get-Item -LiteralPath $manifestPath -Force
        if ($manifestFile.PSIsContainer -or ($manifestFile.Attributes -band [IO.FileAttributes]::ReparsePoint) -or $manifestFile.Length -gt 4194304) { return $false }
        $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
        $files = @($manifest.files)
        if ($manifest.schema -ne 1 -or $files.Count -lt 6 -or $files.Count -gt 4096) { return $false }
        $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($file in $files) {
            $relative = [string]$file.path
            if ([string]::IsNullOrWhiteSpace($relative) -or $relative.Length -gt 1024 -or
                [IO.Path]::IsPathRooted($relative) -or $relative -match '[\\:<>"|?*]' -or
                $relative -eq 'payload-files.json' -or -not $seen.Add($relative)) { return $false }
            $parts = $relative.Split('/')
            if (@($parts | Where-Object { -not $_ -or $_ -in @('.', '..') -or $_.EndsWith('.') -or $_.EndsWith(' ') }).Count) { return $false }
            if ([string]$file.sha256 -notmatch '^[0-9a-fA-F]{64}$') { return $false }
            $expectedSize = [long]$file.size
            if ($expectedSize -lt 0 -or $expectedSize -gt 200000000) { return $false }
            $path = [IO.Path]::GetFullPath((Join-Path $root $relative))
            if (-not $path.StartsWith($root + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { return $false }
            $current = $root
            foreach ($part in $parts) {
                $current = Join-Path $current $part
                $entry = Get-Item -LiteralPath $current -Force
                if ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) { return $false }
            }
            if ($entry.PSIsContainer -or $entry.Length -ne $expectedSize -or
                (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash -ne [string]$file.sha256) { return $false }
        }
        foreach ($required in @('tramoing-sensors.exe', 'tramoing-sensors.dll', 'tramoing-sensors.deps.json', 'tramoing-sensors.runtimeconfig.json', 'LibreHardwareMonitorLib.dll', 'component.json')) {
            if (-not $seen.Contains($required)) { return $false }
        }
        # Do not accept stale/unlisted DLLs left by an interrupted manual repair.
        $pending = [Collections.Generic.Stack[string]]::new()
        $pending.Push($root)
        while ($pending.Count) {
            foreach ($entry in Get-ChildItem -LiteralPath $pending.Pop() -Force) {
                if ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) { return $false }
                if ($entry.PSIsContainer) { $pending.Push($entry.FullName); continue }
                $relative = $entry.FullName.Substring($root.Length + 1).Replace('\', '/')
                if ($relative -ne 'payload-files.json' -and -not $seen.Contains($relative)) { return $false }
            }
        }
        return $true
    } catch { return $false }
}

function Test-LegacySensorPayload([string]$SensorRoot) {
    try {
        $runtimeConfig = Get-Content -LiteralPath (Join-Path $SensorRoot 'tramoing-sensors.runtimeconfig.json') -Raw | ConvertFrom-Json
        $included = @($runtimeConfig.runtimeOptions.includedFrameworks | Where-Object { $_.name -eq 'Microsoft.NETCore.App' -and $_.version -like '10.0.*' })
        if (-not $included.Count) { return $false }
        # The old self-contained publish has no payload-files.json. Its .deps.json
        # lists the managed and native files copied to the publish directory.
        $deps = Get-Content -LiteralPath (Join-Path $SensorRoot 'tramoing-sensors.deps.json') -Raw | ConvertFrom-Json
        if ($deps.runtimeTarget.name -notlike '*/win-x64') { return $false }
        $target = $deps.targets.PSObject.Properties[$deps.runtimeTarget.name].Value
        $names = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($library in $target.PSObject.Properties) {
            foreach ($group in @('runtime', 'native')) {
                foreach ($asset in $library.Value.$group.PSObject.Properties) {
                    $name = [IO.Path]::GetFileName($asset.Name)
                    if ($name -ne '_._') { [void]$names.Add($name) }
                }
            }
        }
        foreach ($required in @('tramoing-sensors.dll', 'hostfxr.dll', 'hostpolicy.dll', 'coreclr.dll', 'System.Private.CoreLib.dll', 'LibreHardwareMonitorLib.dll')) {
            if (-not $names.Contains($required)) { return $false }
        }
        foreach ($name in $names) {
            if (-not [IO.File]::Exists([IO.Path]::Combine($SensorRoot, $name))) { return $false }
        }
        return $true
    } catch {
        # Damaged legacy metadata is a missing component, so settings can offer
        # a repair download instead of getting stuck on an inspection error.
        return $false
    }
}

function Test-X64DotNetRuntime([string]$Location, [version]$Minimum) {
    function Get-RuntimeVersion($Directory) {
        $pattern = '\A(?<core>(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*))' +
            '(?:-(?<pre>[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?\z'
        if ($Directory.Name -cnotmatch $pattern) { return }
        $core = $null
        if (-not [version]::TryParse($Matches.core, [ref]$core)) { return }
        $labels = @()
        if ($Matches.pre) {
            $labels = $Matches.pre.Split('.')
            if ($labels -match '^0[0-9]+$') { return }
        }
        return [pscustomobject]@{ Core=$core; Labels=$labels; Path=$Directory.FullName }
    }

    function Compare-RuntimeVersion($Left, $Right) {
        $order = $Left.Core.CompareTo($Right.Core)
        if ($order -ne 0) { return $order }
        if (-not $Left.Labels.Count -or -not $Right.Labels.Count) {
            return $Right.Labels.Count.CompareTo($Left.Labels.Count)
        }
        for ($index = 0; $index -lt [Math]::Min($Left.Labels.Count, $Right.Labels.Count); $index++) {
            $leftLabel = $Left.Labels[$index]
            $rightLabel = $Right.Labels[$index]
            $leftNumeric = $leftLabel -match '^[0-9]+$'
            $rightNumeric = $rightLabel -match '^[0-9]+$'
            if ($leftNumeric -ne $rightNumeric) { return $rightNumeric.CompareTo($leftNumeric) }
            if ($leftNumeric -and $leftLabel.Length -ne $rightLabel.Length) { return $leftLabel.Length.CompareTo($rightLabel.Length) }
            $order = [string]::CompareOrdinal($leftLabel, $rightLabel)
            if ($order -ne 0) { return $order }
        }
        return $Left.Labels.Count.CompareTo($Right.Labels.Count)
    }

    function Test-X64Library([string]$Path) {
        try {
            $reader = [IO.BinaryReader]::new([IO.File]::OpenRead($Path))
            try {
                if ($reader.ReadUInt16() -ne 0x5a4d) { return $false }
                $reader.BaseStream.Position = 0x3c
                $reader.BaseStream.Position = $reader.ReadUInt32()
                return $reader.ReadUInt32() -eq 0x4550 -and $reader.ReadUInt16() -eq 0x8664
            } finally { $reader.Dispose() }
        } catch { return $false }
    }

    $latestHost = $null
    foreach ($entry in Get-ChildItem -LiteralPath (Join-Path $Location 'host\fxr') -Directory -Force -ErrorAction SilentlyContinue) {
        $candidate = Get-RuntimeVersion $entry
        if ($candidate -and (-not $latestHost -or (Compare-RuntimeVersion $candidate $latestHost) -gt 0)) {
            $latestHost = $candidate
        }
    }
    if (-not $latestHost -or -not (Test-X64Library (Join-Path $latestHost.Path 'hostfxr.dll'))) { return $false }

    $framework = $null
    foreach ($entry in Get-ChildItem -LiteralPath (Join-Path $Location 'shared\Microsoft.NETCore.App') -Directory -Force -ErrorAction SilentlyContinue) {
        $candidate = Get-RuntimeVersion $entry
        if (-not $candidate -or $candidate.Labels.Count -or $candidate.Core.Major -ne $Minimum.Major -or
            $candidate.Core.Minor -ne $Minimum.Minor -or $candidate.Core -lt $Minimum) { continue }
        if ((-not $framework -or $candidate.Core -gt $framework.Core) -and
            [IO.File]::Exists((Join-Path $candidate.Path 'Microsoft.NETCore.App.deps.json'))) {
            $framework = $candidate
        }
    }
    return $null -ne $framework -and [IO.File]::Exists((Join-Path $framework.Path 'System.Private.CoreLib.dll')) -and
        [IO.File]::Exists((Join-Path $framework.Path 'hostpolicy.dll')) -and (Test-X64Library (Join-Path $framework.Path 'coreclr.dll'))
}

$runtime = @($cfg.manifest.dependencies | Where-Object { $_.id -eq 'dotnet' })[0]
$driver = @($cfg.manifest.dependencies | Where-Object { $_.id -eq 'pawnio' })[0]
# .NET's Windows host reads its registration from the 32-bit view even for
# x64 runtimes. PawnIO's uninstall registration uses the native 64-bit view.
$location = $null
$dotnetRegistry = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine, [Microsoft.Win32.RegistryView]::Registry32)
try {
    $installed = $dotnetRegistry.OpenSubKey('SOFTWARE\dotnet\Setup\InstalledVersions\x64')
    if ($installed) {
        try { $location = [string]$installed.GetValue('InstallLocation') } finally { $installed.Dispose() }
    }
} finally { $dotnetRegistry.Dispose() }
# Match AppHostDotNetSearch=Global, including global installs without MSI
# sharedfx inventory. Per-user SDKs and DOTNET_ROOT overrides are not consulted.
if (-not $location) { $location = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'dotnet' }
$runtimeOk = Test-X64DotNetRuntime $location ([version]$runtime.minimum_version)
$base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine, [Microsoft.Win32.RegistryView]::Registry64)
try {
    $driverOk = $false
    $key = $base.OpenSubKey('SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO')
    if ($key) {
        try {
            $version = $null
            $driverOk = [version]::TryParse([string]$key.GetValue('DisplayVersion'), [ref]$version) -and $version -ge [version]$driver.minimum_version
        } finally { $key.Dispose() }
    }
} finally { $base.Dispose() }
$worker = Join-Path $cfg.root 'sensors\tramoing-sensors.exe'
$metadata = Join-Path $cfg.root 'sensors\component.json'
$configFile = Join-Path $cfg.root 'sensors\tramoing-sensors.runtimeconfig.json'
$componentOk = $false
$legacy = $false
if ((Test-Path -LiteralPath $worker -PathType Leaf) -and (Test-Path -LiteralPath $configFile -PathType Leaf)) {
    if (-not (Test-Path -LiteralPath $metadata)) {
        # Older self-contained workers have no --probe command. Preserve their
        # private runtime instead of demanding a system-wide runtime migration.
        $legacy = Test-LegacySensorPayload (Join-Path $cfg.root 'sensors')
        $componentOk = $legacy
    } else {
        try {
            $info = Get-Content -LiteralPath $metadata -Raw | ConvertFrom-Json
            $expected = if ($cfg.manifest.component) { [version]$cfg.manifest.component.component_version } else { [version]'1.0.0' }
            $componentOk = $info.protocol -eq 1 -and ([version]$info.version).Major -eq $expected.Major -and ([version]$info.version) -ge $expected -and (Test-SensorPayload (Join-Path $cfg.root 'sensors'))
            if ($componentOk -and $runtimeOk) {
                $start = [Diagnostics.ProcessStartInfo]::new($worker, '--probe')
                $start.UseShellExecute = $false
                $start.CreateNoWindow = $true
                $start.RedirectStandardOutput = $true
                $start.RedirectStandardError = $true
                $start.RedirectStandardInput = $true
                $start.WorkingDirectory = Split-Path $worker
                $process = [Diagnostics.Process]::Start($start)
                try {
                    $process.StandardInput.Close()
                    $stdout = $process.StandardOutput.ReadToEndAsync()
                    $stderr = $process.StandardError.ReadToEndAsync()
                    if (-not $process.WaitForExit(10000)) { $process.Kill(); $componentOk = $false }
                    else {
                        $probe = $stdout.Result | ConvertFrom-Json
                        $componentOk = $process.ExitCode -eq 0 -and $probe.protocol -eq 1 -and $probe.architecture -eq 'X64' -and ([version]$probe.component_version).Major -eq $expected.Major -and ([version]$probe.component_version) -ge $expected
                    }
                } finally { $process.Dispose() }
            }
        } catch { $componentOk = $false }
    }
}
$service = Get-Service -Name PawnIO -ErrorAction SilentlyContinue
$restart = $driverOk -and (-not $service -or $service.Status -ne 'Running')
[pscustomobject]@{ component_ok=$componentOk; legacy_component=$legacy; runtime_ok=$runtimeOk; driver_ok=$driverOk; needs_restart=$restart } | ConvertTo-Json -Compress
