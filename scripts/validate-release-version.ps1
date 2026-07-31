param(
    [Parameter(Mandatory = $true)]
    [string]$Tag
)

$ErrorActionPreference = "Stop"

if ($Tag -notmatch '^v(?<Version>(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-[0-9A-Za-z][0-9A-Za-z.-]*)?)$') {
    throw "Release tag '$Tag' must use vMAJOR.MINOR.PATCH or a SemVer prerelease."
}
$tagVersion = $Matches.Version

$cargo = Get-Content -Raw "Cargo.toml"
$package = [regex]::Match($cargo, '(?ms)^\[package\]\s*(.*?)(?=^\[|\z)')
if (-not $package.Success) {
    throw "Cargo.toml has no [package] section."
}
$cargoVersionMatch = [regex]::Match(
    $package.Groups[1].Value,
    '(?m)^\s*version\s*=\s*"(?<Version>[^"]+)"\s*$'
)
if (-not $cargoVersionMatch.Success) {
    throw "Cargo.toml [package] has no version."
}
$cargoVersion = $cargoVersionMatch.Groups["Version"].Value

$installer = Get-Content -Raw "installer\matteshot.iss"
$installerVersionMatch = [regex]::Match(
    $installer,
    '(?m)^\s*#define\s+AppVersion\s+"(?<Version>[^"]+)"\s*$'
)
if (-not $installerVersionMatch.Success) {
    throw "installer/matteshot.iss has no fallback AppVersion."
}
$installerVersion = $installerVersionMatch.Groups["Version"].Value

$mismatches = @()
if ($cargoVersion -ne $tagVersion) {
    $mismatches += "Cargo.toml=$cargoVersion"
}
if ($installerVersion -ne $tagVersion) {
    $mismatches += "installer/matteshot.iss=$installerVersion"
}
if ($mismatches.Count -gt 0) {
    throw "Release tag $Tag does not match: $($mismatches -join ', ')."
}

Write-Host "Release versions match: $tagVersion"
if ($env:GITHUB_OUTPUT) {
    "version=$tagVersion" >> $env:GITHUB_OUTPUT
}

