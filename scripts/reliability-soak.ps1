param(
    [ValidateRange(1, 100)]
    [int]$Iterations = 20
)

$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)

for ($iteration = 1; $iteration -le $Iterations; $iteration++) {
    Write-Host "Reliability test pass $iteration of $Iterations"
    & cargo test --quiet
    if ($LASTEXITCODE -ne 0) {
        throw "cargo test failed on pass $iteration"
    }
}

& cargo clippy --all-targets --all-features -- -D warnings
if ($LASTEXITCODE -ne 0) {
    throw 'strict Clippy validation failed'
}

# Same reason as verify-code.ps1: audit-rust.ps1 wants pwsh 7's JSON parser.
$pwsh = (Get-Command pwsh -ErrorAction SilentlyContinue).Source
if (-not $pwsh) {
    $pwsh = (Get-Process -Id $PID).Path
}
& $pwsh -NoProfile -File (Join-Path $PSScriptRoot 'audit-rust.ps1')
if ($LASTEXITCODE -ne 0) {
    throw 'RustSec audit failed'
}

Write-Host "Reliability soak passed: $Iterations test passes, strict Clippy, and pinned RustSec audit."
