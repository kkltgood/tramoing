# Shared by release verification and the embedded elevated installer.
function Assert-DependencySignature {
    param(
        [Parameter(Mandatory = $true)][string]$Dependency,
        [AllowNull()]$Signature
    )
    $status = if ($null -ne $Signature) { [string]$Signature.Status } else { '<none>' }
    $certificate = if ($null -ne $Signature) { $Signature.SignerCertificate } else { $null }
    $subject = if ($null -ne $certificate) { [string]$certificate.Subject } else { '<none>' }
    $detail = 'dependency={0}; status={1}; subject={2}' -f $Dependency, $status, $subject
    if ($Dependency -notin @('dotnet', 'pawnio')) { throw ('Unknown dependency signature identity: ' + $detail) }
    if ($status -ne 'Valid' -or $null -eq $certificate) { throw ('Invalid official publisher signature: ' + $detail) }
    try {
        # Decode one attribute per line so commas inside another attribute cannot
        # impersonate a CN or O. Reject missing or duplicate identity attributes.
        $attributes = @($certificate.SubjectName.Decode([Security.Cryptography.X509Certificates.X500DistinguishedNameFlags]::UseNewLines) -split '\r?\n')
        $commonNames = @($attributes | Where-Object { $_ -match '^CN=' } | ForEach-Object { $_.Substring(3) })
        $organizations = @($attributes | Where-Object { $_ -match '^O=' } | ForEach-Object { $_.Substring(2) })
        $validPublisher = switch ($Dependency) {
            'dotnet' { $commonNames.Count -eq 1 -and $commonNames[0] -eq '.NET' -and $organizations.Count -eq 1 -and $organizations[0] -eq 'Microsoft Corporation' }
            'pawnio' { $commonNames.Count -eq 1 -and $commonNames[0] -eq 'namazso.eu' }
        }
    } catch {
        throw ('Cannot decode official publisher certificate: ' + $detail)
    }
    if (-not $validPublisher) { throw ('Invalid official publisher signature: ' + $detail) }
}
