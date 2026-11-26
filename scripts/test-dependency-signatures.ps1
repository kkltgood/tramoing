# Exercise the same publisher assertion used by CI and the elevated installer.
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
. (Join-Path $root 'crates/tray-app/src/component_install/signature.ps1')

function New-TestSignature {
    param([string]$Subject, [string]$Status = 'Valid')
    $name = [Security.Cryptography.X509Certificates.X500DistinguishedName]::new($Subject)
    [pscustomobject]@{
        Status = $Status
        SignerCertificate = [pscustomobject]@{ Subject = $name.Name; SubjectName = $name }
    }
}

$dotnet = 'CN=.NET, O=Microsoft Corporation, L=Redmond, S=Washington, C=US'
$pawnio = 'CN=namazso.eu, O=namazso, L=Debrecen, C=HU'
$cases = @(
    @{ Name = 'official .NET publisher'; Dependency = 'dotnet'; Signature = (New-TestSignature $dotnet); Accept = $true }
    @{ Name = 'official PawnIO publisher'; Dependency = 'pawnio'; Signature = (New-TestSignature $pawnio); Accept = $true }
    @{ Name = 'incorrect .NET common name'; Dependency = 'dotnet'; Signature = (New-TestSignature 'CN=Microsoft Corporation, O=Microsoft Corporation'); Accept = $false }
    @{ Name = 'incorrect .NET organization'; Dependency = 'dotnet'; Signature = (New-TestSignature 'CN=.NET, O=Unrelated Publisher'); Accept = $false }
    @{ Name = 'missing .NET organization'; Dependency = 'dotnet'; Signature = (New-TestSignature 'CN=.NET'); Accept = $false }
    @{ Name = 'organization inside another attribute'; Dependency = 'dotnet'; Signature = (New-TestSignature 'CN=.NET, OU="Team, O=Microsoft Corporation"'); Accept = $false }
    @{ Name = 'incorrect PawnIO common name'; Dependency = 'pawnio'; Signature = (New-TestSignature 'CN=other.example, O=namazso'); Accept = $false }
    @{ Name = 'invalid signature status'; Dependency = 'dotnet'; Signature = (New-TestSignature $dotnet 'HashMismatch'); Accept = $false }
    @{ Name = 'untrusted certificate'; Dependency = 'dotnet'; Signature = (New-TestSignature $dotnet 'NotTrusted'); Accept = $false }
    @{ Name = 'missing certificate'; Dependency = 'dotnet'; Signature = [pscustomobject]@{ Status = 'Valid'; SignerCertificate = $null }; Accept = $false }
    @{ Name = 'missing signature'; Dependency = 'dotnet'; Signature = $null; Accept = $false }
    @{ Name = 'unknown dependency'; Dependency = 'unknown'; Signature = (New-TestSignature $dotnet); Accept = $false }
)
foreach ($case in $cases) {
    $failure = $null
    try { Assert-DependencySignature -Dependency $case.Dependency -Signature $case.Signature } catch { $failure = $_.Exception.Message }
    if ($case.Accept -and $null -ne $failure) { throw ('Expected acceptance: {0}: {1}' -f $case.Name, $failure) }
    if (-not $case.Accept) {
        if ($null -eq $failure) { throw ('Expected rejection: ' + $case.Name) }
        if ($failure -notmatch 'status=' -or $failure -notmatch 'subject=') { throw ('Missing signature diagnostics: ' + $case.Name) }
    }
    Write-Host ('Passed: ' + $case.Name)
}
Write-Host ('Passed {0} dependency signature checks.' -f $cases.Count)
