param(
    # Useful while the local resident owns target\release\matteshot.exe.
    # CI, candidate, and release workflows always run the release build.
    [switch]$SkipReleaseBuild
)

$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)

$pwsh = (Get-Process -Id $PID).Path

Write-Host 'Running sandbox smoke result tests'
& $pwsh -NoProfile -File (Join-Path $PSScriptRoot 'test-sandbox-smoke-result.ps1')
if ($LASTEXITCODE -ne 0) {
    throw "Sandbox smoke result tests failed with exit code $LASTEXITCODE"
}

Write-Host 'Running RustSec audit gate tests'
& $pwsh -NoProfile -File (Join-Path $PSScriptRoot 'test-audit-rust.ps1')
if ($LASTEXITCODE -ne 0) {
    throw "RustSec audit gate tests failed with exit code $LASTEXITCODE"
}

Write-Host 'Running locked RustSec audit'
& $pwsh -NoProfile -File (Join-Path $PSScriptRoot 'audit-rust.ps1')
if ($LASTEXITCODE -ne 0) {
    throw "RustSec audit failed with exit code $LASTEXITCODE"
}

function Invoke-CargoStep {
    param(
        [string]$Name,
        [string[]]$Arguments
    )

    Write-Host $Name
    & cargo @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Name failed with exit code $LASTEXITCODE"
    }
}

Invoke-CargoStep 'Running all tests' @(
    'test', '--all-targets', '--all-features'
)
Invoke-CargoStep 'Running strict Clippy' @(
    'clippy', '--all-targets', '--all-features', '--', '-D', 'warnings'
)

if (-not $SkipReleaseBuild) {
    Invoke-CargoStep 'Building release binary' @('build', '--release')
    Write-Host 'Code verification passed: RustSec audit, tests, strict Clippy, and release build.'
} else {
    Write-Host 'Code verification passed: RustSec audit, tests, and strict Clippy (release build skipped).'
}
