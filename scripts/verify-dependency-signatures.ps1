# Verify cached original installers without installing them or changing the machine.
param(
    [ValidateSet('all', 'dotnet', 'pawnio')][string]$Dependency = 'all',
    [string]$CacheDirectory = ''
)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $CacheDirectory) { $CacheDirectory = Join-Path $root 'target/dependency-cache' }
$cache = [IO.Path]::GetFullPath($CacheDirectory).TrimEnd('\', '/')
$manifest = Get-Content -LiteralPath (Join-Path $root 'static/dependencies/manifest.json') -Raw | ConvertFrom-Json
$selected = @($manifest.dependencies | Where-Object { $Dependency -eq 'all' -or $_.id -eq $Dependency })
if (-not $selected.Count) { throw 'No dependency selected for signature verification' }
foreach ($item in $selected) {
    $path = [IO.Path]::GetFullPath((Join-Path $cache $item.relative_path))
    if (-not $path.StartsWith($cache + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw 'Dependency file escapes cache directory' }
    $file = Get-Item -LiteralPath $path
    if ($file.PSIsContainer -or ($file.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
        $file.Length -ne $item.size -or (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash -ne $item.sha256) {
        throw "Dependency cache integrity failed: $($item.id)"
    }
    if ($item.official_sha512 -and (Get-FileHash -LiteralPath $path -Algorithm SHA512).Hash -ne $item.official_sha512) {
        throw "Official SHA-512 verification failed: $($item.id)"
    }
    $signature = Get-AuthenticodeSignature -LiteralPath $path
    $publisher = '(^|,\s*)CN=' + [regex]::Escape($item.publisher) + '(,|$)'
    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch $publisher) {
        throw "Official signature verification failed: $($item.id)"
    }
    Write-Host "Verified official publisher and pinned bytes: $($item.id) $($item.version)"
}
