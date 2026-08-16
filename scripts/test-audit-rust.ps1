# Self-contained checks for scripts/audit-rust.ps1 (SBS-763).
# No Pester: CI cannot Install-Module, and check-workflow-pins.ps1 rejects
# unpinned Install-Module. Exit 0 only if every assertion passes.
# Does not install cargo-audit and does not fetch the advisory-db.

$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'audit-rust.ps1')

$script:failures = 0
$script:passes = 0

function Assert-Rustsec {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Name,

        [Parameter(Mandatory = $true)]
        [bool]$Condition,

        [string]$Detail
    )

    if ($Condition) {
        $script:passes++
        Write-Host "PASS $Name"
        return
    }

    $script:failures++
    if ($Detail) {
        Write-Host "FAIL ${Name}: $Detail"
    } else {
        Write-Host "FAIL $Name"
    }
}

function New-RustsecTestDir {
    $dir = Join-Path ([System.IO.Path]::GetTempPath()) (
        'sbs-763-' + [guid]::NewGuid().ToString('N')
    )
    New-Item -ItemType Directory -Path $dir | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $dir '.cargo') | Out-Null
    return $dir
}

function Write-RustsecFixture {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Directory,

        [switch]$OmitLock,
        [switch]$OmitToml,
        [switch]$OmitExceptions,

        [string]$LockText = '# lock',

        [string]$Toml = @"
[advisories]
ignore = []
"@,

        [string]$Exceptions = '{"exceptions":[]}'
    )

    if (-not $OmitLock) {
        Set-Content -Encoding UTF8 -Path (Join-Path $Directory 'Cargo.lock') -Value $LockText
    }
    if (-not $OmitToml) {
        Set-Content -Encoding UTF8 -Path (Join-Path $Directory '.cargo/audit.toml') -Value $Toml
    }
    if (-not $OmitExceptions) {
        Set-Content -Encoding UTF8 -Path (Join-Path $Directory '.cargo/rustsec-exceptions.json') -Value $Exceptions
    }
}

function Use-FakeCargoAudit {
    param(
        [scriptblock]$Invoker
    )

    $script:CargoAuditInvoker = $Invoker
}

# SBS-763: a missing cargo-audit binary is not a clean audit.
Use-FakeCargoAudit {
    param($Arguments)
    [pscustomobject]@{ ExitCode = 127; Output = ''; Found = $false }
}
$dir = New-RustsecTestDir
try {
    Write-RustsecFixture -Directory $dir
    $outcome = Invoke-RustsecAuditGate -RepoRoot $dir
    Assert-Rustsec `
        -Name 'missing-cargo-audit-is-not-clean' `
        -Condition ($outcome.Kind -eq 'Unknown' -and $outcome.Reason -eq 'MissingTool' -and $outcome.ExitCode -ne 0) `
        -Detail "Kind=$($outcome.Kind) Reason=$($outcome.Reason) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

# SBS-763: a different cargo-audit version is not the pin.
Use-FakeCargoAudit {
    param($Arguments)
    [pscustomobject]@{ ExitCode = 0; Output = 'cargo-audit 0.21.2'; Found = $true }
}
$dir = New-RustsecTestDir
try {
    Write-RustsecFixture -Directory $dir
    $outcome = Invoke-RustsecAuditGate -RepoRoot $dir
    Assert-Rustsec `
        -Name 'wrong-cargo-audit-version-is-rejected' `
        -Condition ($outcome.Kind -eq 'Fail' -and $outcome.Reason -eq 'WrongVersion' -and $outcome.ExitCode -ne 0) `
        -Detail "Kind=$($outcome.Kind) Reason=$($outcome.Reason) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

# SBS-763: only the pinned crates.io version is accepted.
$versionCheck = Test-CargoAuditVersion -Version (Get-CargoAuditPin)
Assert-Rustsec `
    -Name 'pinned-version-is-accepted' `
    -Condition ($versionCheck.Kind -eq 'Pass') `
    -Detail "Kind=$($versionCheck.Kind) pin=$(Get-CargoAuditPin)"

$parsedVersion = Get-CargoAuditVersionFromOutput -Output 'cargo-audit 0.22.2'
Assert-Rustsec `
    -Name 'version-output-parses-pin' `
    -Condition ($parsedVersion -eq (Get-CargoAuditPin)) `
    -Detail "parsed=$parsedVersion"

$parsedSubcommand = Get-CargoAuditVersionFromOutput -Output 'cargo-audit-audit 0.22.2'
Assert-Rustsec `
    -Name 'subcommand-version-output-parses-pin' `
    -Condition ($parsedSubcommand -eq (Get-CargoAuditPin)) `
    -Detail "parsed=$parsedSubcommand"

# SBS-763: a missing exceptions file is Unknown, not "no exceptions".
Use-FakeCargoAudit {
    param($Arguments)
    [pscustomobject]@{ ExitCode = 0; Output = 'cargo-audit 0.22.2'; Found = $true }
}
$dir = New-RustsecTestDir
try {
    Write-RustsecFixture -Directory $dir -OmitExceptions
    $outcome = Invoke-RustsecAuditGate -RepoRoot $dir
    Assert-Rustsec `
        -Name 'missing-exceptions-file-is-unknown' `
        -Condition ($outcome.Kind -eq 'Unknown' -and $outcome.Reason -eq 'MissingExceptions' -and $outcome.ExitCode -ne 0) `
        -Detail "Kind=$($outcome.Kind) Reason=$($outcome.Reason) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

# SBS-763: garbage JSON is Unknown, not an empty ignore list.
$dir = New-RustsecTestDir
try {
    Write-RustsecFixture -Directory $dir -Exceptions 'not-json'
    $outcome = Invoke-RustsecAuditGate -RepoRoot $dir
    Assert-Rustsec `
        -Name 'unparseable-exceptions-is-unknown' `
        -Condition ($outcome.Kind -eq 'Unknown' -and $outcome.Reason -eq 'UnparseableExceptions' -and $outcome.ExitCode -ne 0) `
        -Detail "Kind=$($outcome.Kind) Reason=$($outcome.Reason) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

# SBS-763: exceptions must be an array; an object is not an empty list.
$dir = New-RustsecTestDir
try {
    Write-RustsecFixture -Directory $dir -Exceptions '{"exceptions":{"id":"RUSTSEC-2024-0001"}}'
    $outcome = Invoke-RustsecAuditGate -RepoRoot $dir
    Assert-Rustsec `
        -Name 'exceptions-object-is-not-a-list' `
        -Condition ($outcome.Kind -eq 'Fail' -and $outcome.Reason -eq 'InvalidExceptions' -and $outcome.ExitCode -ne 0) `
        -Detail "Kind=$($outcome.Kind) Reason=$($outcome.Reason) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

# SBS-763: owner / justification / expiry are required on every ignore.
$dir = New-RustsecTestDir
try {
    Write-RustsecFixture -Directory $dir -Exceptions '{"exceptions":[{"id":"RUSTSEC-2024-0001","justification":"x","expires":"2027-01-01"}]}'
    $outcome = Invoke-RustsecAuditGate -RepoRoot $dir
    Assert-Rustsec `
        -Name 'exception-missing-owner-is-rejected' `
        -Condition ($outcome.Kind -eq 'Fail' -and $outcome.Reason -eq 'InvalidExceptions' -and $outcome.ExitCode -ne 0) `
        -Detail "Kind=$($outcome.Kind) Reason=$($outcome.Reason) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

$dir = New-RustsecTestDir
try {
    Write-RustsecFixture -Directory $dir -Exceptions '{"exceptions":[{"id":"RUSTSEC-2024-0001","owner":"Tyler","expires":"2027-01-01"}]}'
    $outcome = Invoke-RustsecAuditGate -RepoRoot $dir
    Assert-Rustsec `
        -Name 'exception-missing-justification-is-rejected' `
        -Condition ($outcome.Kind -eq 'Fail' -and $outcome.Reason -eq 'InvalidExceptions' -and $outcome.ExitCode -ne 0) `
        -Detail "Kind=$($outcome.Kind) Reason=$($outcome.Reason) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

$dir = New-RustsecTestDir
try {
    Write-RustsecFixture -Directory $dir -Exceptions '{"exceptions":[{"id":"RUSTSEC-2024-0001","owner":"Tyler","justification":"x"}]}'
    $outcome = Invoke-RustsecAuditGate -RepoRoot $dir
    Assert-Rustsec `
        -Name 'exception-missing-expiry-is-rejected' `
        -Condition ($outcome.Kind -eq 'Fail' -and $outcome.Reason -eq 'InvalidExceptions' -and $outcome.ExitCode -ne 0) `
        -Detail "Kind=$($outcome.Kind) Reason=$($outcome.Reason) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

# SBS-763: an expired exception cannot keep ignoring an advisory.
$dir = New-RustsecTestDir
try {
    $toml = @"
[advisories]
ignore = ["RUSTSEC-2024-0001"]
"@
    $exceptions = '{"exceptions":[{"id":"RUSTSEC-2024-0001","owner":"Tyler","justification":"temporary","expires":"2026-01-01"}]}'
    Write-RustsecFixture -Directory $dir -Toml $toml -Exceptions $exceptions
    $outcome = Invoke-RustsecAuditGate -RepoRoot $dir -NowUtc ([datetime]::Parse('2026-08-16T00:00:00Z'))
    Assert-Rustsec `
        -Name 'expired-exception-is-rejected' `
        -Condition ($outcome.Kind -eq 'Fail' -and $outcome.Reason -eq 'ExpiredException' -and $outcome.ExitCode -ne 0) `
        -Detail "Kind=$($outcome.Kind) Reason=$($outcome.Reason) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

# SBS-763: audit.toml ignore IDs must be documented, and vice versa.
$dir = New-RustsecTestDir
try {
    $toml = @"
[advisories]
ignore = ["RUSTSEC-2024-0001"]
"@
    Write-RustsecFixture -Directory $dir -Toml $toml -Exceptions '{"exceptions":[]}'
    $outcome = Invoke-RustsecAuditGate -RepoRoot $dir
    Assert-Rustsec `
        -Name 'ignore-without-exception-doc-is-rejected' `
        -Condition ($outcome.Kind -eq 'Fail' -and $outcome.Reason -eq 'IgnoreDrift' -and $outcome.ExitCode -ne 0) `
        -Detail "Kind=$($outcome.Kind) Reason=$($outcome.Reason) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

$dir = New-RustsecTestDir
try {
    $exceptions = '{"exceptions":[{"id":"RUSTSEC-2024-0001","owner":"Tyler","justification":"x","expires":"2027-01-01"}]}'
    Write-RustsecFixture -Directory $dir -Exceptions $exceptions
    $outcome = Invoke-RustsecAuditGate -RepoRoot $dir
    Assert-Rustsec `
        -Name 'exception-without-ignore-is-rejected' `
        -Condition ($outcome.Kind -eq 'Fail' -and $outcome.Reason -eq 'IgnoreDrift' -and $outcome.ExitCode -ne 0) `
        -Detail "Kind=$($outcome.Kind) Reason=$($outcome.Reason) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

# SBS-763: empty ignore + empty docs is a valid exception set.
$dir = New-RustsecTestDir
try {
    Write-RustsecFixture -Directory $dir
    $toml = Get-Content -LiteralPath (Join-Path $dir '.cargo/audit.toml') -Raw
    $ids = Get-RustsecIgnoreIdsFromToml -Toml $toml
    $parsed = Read-RustsecExceptions -Path (Join-Path $dir '.cargo/rustsec-exceptions.json')
    $check = Test-RustsecExceptionSet -IgnoreIds $ids -ParsedExceptions $parsed
    Assert-Rustsec `
        -Name 'empty-exceptions-and-empty-ignore-is-ok' `
        -Condition ($check.Kind -eq 'Pass' -and $parsed.Kind -eq 'Parsed' -and @($parsed.Exceptions).Count -eq 0) `
        -Detail "Kind=$($check.Kind) parsed=$($parsed.Kind) count=$(@($parsed.Exceptions).Count)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

# SBS-763: a live, documented ignore is accepted (still requires a real audit).
$dir = New-RustsecTestDir
try {
    $toml = @"
[advisories]
ignore = ["RUSTSEC-2024-0001"]
"@
    $exceptions = '{"exceptions":[{"id":"RUSTSEC-2024-0001","owner":"Tyler","justification":"windows-only path","expires":"2027-01-01"}]}'
    Write-RustsecFixture -Directory $dir -Toml $toml -Exceptions $exceptions
    $parsed = Read-RustsecExceptions -Path (Join-Path $dir '.cargo/rustsec-exceptions.json')
    $ids = Get-RustsecIgnoreIdsFromToml -Toml $toml
    $check = Test-RustsecExceptionSet `
        -IgnoreIds $ids `
        -ParsedExceptions $parsed `
        -NowUtc ([datetime]::Parse('2026-08-16T00:00:00Z'))
    Assert-Rustsec `
        -Name 'valid-exception-matching-ignore-is-ok' `
        -Condition ($check.Kind -eq 'Pass') `
        -Detail "Kind=$($check.Kind) Reason=$($check.Reason)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

# SBS-763: cargo audit exit 1 is a failed gate, not a warning.
Use-FakeCargoAudit {
    param($Arguments)
    if ($Arguments -contains '--version') {
        return [pscustomobject]@{ ExitCode = 0; Output = 'cargo-audit 0.22.2'; Found = $true }
    }
    return [pscustomobject]@{ ExitCode = 1; Output = 'Crate: foo Advisory: RUSTSEC-2024-9999'; Found = $true }
}
$dir = New-RustsecTestDir
try {
    Write-RustsecFixture -Directory $dir
    $outcome = Invoke-RustsecAuditGate -RepoRoot $dir
    Assert-Rustsec `
        -Name 'cargo-audit-vuln-exit-is-fail' `
        -Condition ($outcome.Kind -eq 'Fail' -and $outcome.Reason -eq 'Advisories' -and $outcome.ExitCode -ne 0) `
        -Detail "Kind=$($outcome.Kind) Reason=$($outcome.Reason) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

# SBS-763: cargo audit exit 0 after a valid setup is the only Pass.
Use-FakeCargoAudit {
    param($Arguments)
    if ($Arguments -contains '--version') {
        return [pscustomobject]@{ ExitCode = 0; Output = 'cargo-audit 0.22.2'; Found = $true }
    }
    return [pscustomobject]@{ ExitCode = 0; Output = 'Success'; Found = $true }
}
$dir = New-RustsecTestDir
try {
    Write-RustsecFixture -Directory $dir
    $outcome = Invoke-RustsecAuditGate -RepoRoot $dir
    Assert-Rustsec `
        -Name 'cargo-audit-success-exit-is-pass' `
        -Condition ($outcome.Kind -eq 'Pass' -and $outcome.ExitCode -eq 0) `
        -Detail "Kind=$($outcome.Kind) Reason=$($outcome.Reason) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

# SBS-763: no Cargo.lock means the locked audit cannot run.
$dir = New-RustsecTestDir
try {
    Write-RustsecFixture -Directory $dir -OmitLock
    $outcome = Invoke-RustsecAuditGate -RepoRoot $dir
    Assert-Rustsec `
        -Name 'missing-lockfile-is-fail' `
        -Condition ($outcome.Kind -eq 'Fail' -and $outcome.Reason -eq 'MissingLockfile' -and $outcome.ExitCode -ne 0) `
        -Detail "Kind=$($outcome.Kind) Reason=$($outcome.Reason) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

# SBS-763: missing audit.toml is Unknown, not an implicit empty ignore list.
$dir = New-RustsecTestDir
try {
    Write-RustsecFixture -Directory $dir -OmitToml
    $outcome = Invoke-RustsecAuditGate -RepoRoot $dir
    Assert-Rustsec `
        -Name 'missing-audit-toml-is-unknown' `
        -Condition ($outcome.Kind -eq 'Unknown' -and $outcome.Reason -eq 'MissingAuditToml' -and $outcome.ExitCode -ne 0) `
        -Detail "Kind=$($outcome.Kind) Reason=$($outcome.Reason) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

# SBS-763: cargo audit process error (exit 2) is Unknown, not "no vulns".
Use-FakeCargoAudit {
    param($Arguments)
    if ($Arguments -contains '--version') {
        return [pscustomobject]@{ ExitCode = 0; Output = 'cargo-audit 0.22.2'; Found = $true }
    }
    return [pscustomobject]@{ ExitCode = 2; Output = "error: could not fetch advisory-db"; Found = $true }
}
$dir = New-RustsecTestDir
try {
    Write-RustsecFixture -Directory $dir
    $outcome = Invoke-RustsecAuditGate -RepoRoot $dir
    Assert-Rustsec `
        -Name 'cargo-audit-error-exit-is-unknown' `
        -Condition ($outcome.Kind -eq 'Unknown' -and $outcome.Reason -eq 'AuditError' -and $outcome.ExitCode -ne 0) `
        -Detail "Kind=$($outcome.Kind) Reason=$($outcome.Reason) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

# SBS-763: the required CI script must actually invoke the gate.
$verify = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'verify-code.ps1') -Raw
Assert-Rustsec `
    -Name 'verify-code-invokes-audit-gate' `
    -Condition (
        $verify -match 'test-audit-rust\.ps1' -and
        $verify -match 'audit-rust\.ps1'
    ) `
    -Detail 'verify-code.ps1 does not run the RustSec tests and gate'

$soak = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'reliability-soak.ps1') -Raw
Assert-Rustsec `
    -Name 'soak-uses-pinned-audit-gate' `
    -Condition ($soak -match 'audit-rust\.ps1' -and $soak -notmatch '(?m)^\s*& cargo audit\s*$') `
    -Detail 'reliability-soak.ps1 still calls unpinned cargo audit'

$script:CargoAuditInvoker = $null

if ($script:failures -gt 0) {
    Write-Host "$script:failures RustSec audit gate test(s) failed; $script:passes passed."
    exit 1
}

Write-Host "All $script:passes RustSec audit gate tests passed."
exit 0
