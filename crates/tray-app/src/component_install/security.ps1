function Assert-ProtectedItem([string]$Path) {
    $item = Get-Item -LiteralPath $Path -Force
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Component paths must not contain junctions or symbolic links' }
    $acl = Get-Acl -LiteralPath $Path
    $trusted = @('S-1-5-18', 'S-1-5-32-544', 'S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464')
    $owner = $acl.GetOwner([Security.Principal.SecurityIdentifier]).Value
    if ($owner -notin $trusted) { throw 'Component files must be owned by Administrators, SYSTEM or TrustedInstaller' }
    $write = [Security.AccessControl.FileSystemRights]::Write -bor [Security.AccessControl.FileSystemRights]::Delete -bor [Security.AccessControl.FileSystemRights]::DeleteSubdirectoriesAndFiles -bor [Security.AccessControl.FileSystemRights]::ChangePermissions -bor [Security.AccessControl.FileSystemRights]::TakeOwnership
    foreach ($rule in $acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])) {
        if ($rule.AccessControlType -eq [Security.AccessControl.AccessControlType]::Allow -and
            -not ($rule.PropagationFlags -band [Security.AccessControl.PropagationFlags]::InheritOnly) -and
            ($rule.FileSystemRights -band $write) -and $rule.IdentityReference.Value -notin $trusted) {
            throw 'Component files permit non-administrator writes; repair the installation permissions first'
        }
    }
}

function Assert-ProtectedTree([string]$Path) {
    Assert-ProtectedItem $Path
    # Enumerate one level at a time and reject reparse points before descending.
    # Get-ChildItem -Recurse alone can traverse an attacker-created junction.
    foreach ($item in Get-ChildItem -LiteralPath $Path -Force) {
        if ($item.PSIsContainer) { Assert-ProtectedTree $item.FullName }
        else { Assert-ProtectedItem $item.FullName }
    }
}

function Assert-InstallRoot([string]$Path) {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    try {
        $principal = [Security.Principal.WindowsPrincipal]::new($identity)
        if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Administrator authorization is required' }
    } finally { $identity.Dispose() }
    # A writable ancestor can replace even a protected child directory. Require
    # the native Program Files tree rather than trusting an environment variable.
    $registry = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine, [Microsoft.Win32.RegistryView]::Registry64)
    try {
        $key = $registry.OpenSubKey('SOFTWARE\Microsoft\Windows\CurrentVersion')
        try { $programFiles = [string]$key.GetValue('ProgramFilesDir') } finally { if ($key) { $key.Dispose() } }
    } finally { $registry.Dispose() }
    if (-not $programFiles -or -not [IO.Path]::IsPathRooted($Path)) { throw 'Cannot locate the protected Program Files directory' }
    $programFiles = [IO.Path]::GetFullPath($programFiles).TrimEnd('\')
    $resolved = [IO.Path]::GetFullPath($Path).TrimEnd('\')
    if (-not $resolved.StartsWith($programFiles + '\', [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Reinstall Tramoing inside Program Files before installing temperature components'
    }
    $directory = [IO.DirectoryInfo]::new($resolved)
    while ($directory) {
        Assert-ProtectedItem $directory.FullName
        if ($directory.FullName.TrimEnd('\') -eq $programFiles) { break }
        $directory = $directory.Parent
    }
    foreach ($executable in @('tramoing.exe','tramoing-helper.exe')) {
        Assert-ProtectedItem (Join-Path $resolved $executable)
    }
    $sensors = Join-Path $resolved 'sensors'
    if (Test-Path -LiteralPath $sensors) { Assert-ProtectedTree $sensors }
}
