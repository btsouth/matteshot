# Signs one file with Azure Trusted Signing, using the Azure CLI login already
# established in the calling workflow. Invoked directly by CI steps and by Inno
# Setup's SignTool hook, which is how the generated uninstaller gets signed:
# ISCC has to do that signing itself because unins000.exe only exists inside
# the compiler, so a post-build signing pass can never reach it.
#
# Endpoint and identity come from the environment rather than parameters so the
# Inno /S command line stays free of values that vary per repository:
#   ARTIFACT_SIGNING_ENDPOINT
#   ARTIFACT_SIGNING_ACCOUNT_NAME
#   ARTIFACT_SIGNING_CERTIFICATE_PROFILE_NAME
param(
    [Parameter(Mandatory = $true)]
    [string]$Path
)

$ErrorActionPreference = 'Stop'

foreach ($name in @(
    'ARTIFACT_SIGNING_ENDPOINT',
    'ARTIFACT_SIGNING_ACCOUNT_NAME',
    'ARTIFACT_SIGNING_CERTIFICATE_PROFILE_NAME'
)) {
    if ([string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($name))) {
        Write-Error "$name is required to sign $Path."
        exit 1
    }
}

# Version-pinned like every other dependency this pipeline executes with
# credentials in scope; check-workflow-pins.ps1 enforces the pin's presence.
$trustedSigningVersion = '0.5.8'
$installed = Get-Module -ListAvailable -Name TrustedSigning |
    Where-Object { $_.Version -eq $trustedSigningVersion }
if (-not $installed) {
    Install-Module -Name TrustedSigning -RequiredVersion $trustedSigningVersion `
        -Force -Scope CurrentUser -Repository PSGallery
}
Import-Module -Name TrustedSigning -RequiredVersion $trustedSigningVersion

# Invoke-TrustedSigning 0.5.8 has no single-file parameter — it signs a
# folder's contents (FilesFolder + FilesFolderFilter). This script is handed
# exactly one file at a time (the setup exe, or the uninstaller's temp file,
# which is not even named *.exe), so stage a lone copy as .exe in a scratch
# folder, sign that, and copy the signed bytes back: Authenticode lives inside
# the file, so the copy carries the signature with it.
$resolved = (Resolve-Path -LiteralPath $Path).Path
$scratchRoot = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
$scratch = New-Item -ItemType Directory `
    -Path (Join-Path $scratchRoot ("sign-" + [guid]::NewGuid().ToString('N')))
try {
    $staged = Join-Path $scratch.FullName 'staged.exe'
    Copy-Item -LiteralPath $resolved -Destination $staged
    Invoke-TrustedSigning `
        -Endpoint $env:ARTIFACT_SIGNING_ENDPOINT `
        -CodeSigningAccountName $env:ARTIFACT_SIGNING_ACCOUNT_NAME `
        -CertificateProfileName $env:ARTIFACT_SIGNING_CERTIFICATE_PROFILE_NAME `
        -FilesFolder $scratch.FullName `
        -FilesFolderFilter 'exe' `
        -FileDigest SHA256 `
        -TimestampRfc3161 'http://timestamp.acs.microsoft.com' `
        -TimestampDigest SHA256
    Copy-Item -LiteralPath $staged -Destination $resolved -Force
} finally {
    Remove-Item -Recurse -Force $scratch
}

$signature = Get-AuthenticodeSignature -FilePath $Path
if ($signature.Status -ne 'Valid') {
    Write-Error "Signature on $Path is $($signature.Status) after signing."
    exit 1
}
# Exact simple display name, the same value the installed app's updater gate
# compares (CERT_NAME_SIMPLE_DISPLAY_TYPE in src/installer.rs). A substring
# match would accept 'CN=Brandon South Evil'.
$signer = $signature.SignerCertificate.GetNameInfo(
    [System.Security.Cryptography.X509Certificates.X509NameType]::SimpleName, $false)
if ($signer -cne 'Brandon South') {
    Write-Error "Unexpected signer on $Path`: '$signer' ($($signature.SignerCertificate.Subject))"
    exit 1
}
Write-Host "Signed $Path as $($signature.SignerCertificate.Subject)."
