# Locked RustSec audit gate (SBS-763).
# Functions are safe to dotsource. Running the file is the PR/release gate.
#
# Missing cargo-audit is not "no advisories". A wrong version is not the pin.
# An unreadable exceptions file is not "no exceptions". An expired exception
# is not a live ignore.

$ErrorActionPreference = 'Stop'

function Get-CargoAuditPin {
    # crates.io cargo-audit 0.22.2, published 2026-06-05.
    # Install: cargo install cargo-audit --version 0.22.2 --locked
    return '0.22.2'
}

function Get-CargoAuditCrateSource {
    return 'https://crates.io/crates/cargo-audit'
}

function New-RustsecAuditOutcome {
    param(
        [Parameter(Mandatory = $true)]
        [ValidateSet('Pass', 'Fail', 'Unknown')]
        [string]$Kind,

        [Parameter(Mandatory = $true)]
        [string]$Reason,

        [string]$Detail
    )

    $exitCode = switch ($Kind) {
        'Pass' { 0 }
        'Fail' { 1 }
        default { 2 }
    }

    $message = switch ($Reason) {
        'Clean' {
            'PASS: locked RustSec audit reported no actionable advisories.'
        }
        'Advisories' {
            if ($Detail) {
                "FAIL: cargo audit reported actionable advisories.`n$Detail"
            } else {
                'FAIL: cargo audit reported actionable advisories.'
            }
        }
        'MissingLockfile' {
            'FAIL: Cargo.lock is missing; a locked RustSec audit cannot run.'
        }
        'MissingAuditToml' {
            'UNKNOWN: .cargo/audit.toml is missing.'
        }
        'MissingExceptions' {
            'UNKNOWN: .cargo/rustsec-exceptions.json is missing.'
        }
        'UnparseableExceptions' {
            'UNKNOWN: .cargo/rustsec-exceptions.json is unreadable.'
        }
        'InvalidExceptions' {
            if ($Detail) {
                "FAIL: RustSec exception file is invalid: $Detail"
            } else {
                'FAIL: RustSec exception file is invalid.'
            }
        }
        'ExpiredException' {
            if ($Detail) {
                "FAIL: RustSec exception has expired: $Detail"
            } else {
                'FAIL: a RustSec exception has expired.'
            }
        }
        'IgnoreDrift' {
            if ($Detail) {
                "FAIL: .cargo/audit.toml ignore list does not match documented exceptions: $Detail"
            } else {
                'FAIL: .cargo/audit.toml ignore list does not match documented exceptions.'
            }
        }
        'MissingTool' {
            'UNKNOWN: cargo-audit is not installed; that is not a clean audit.'
        }
        'WrongVersion' {
            if ($Detail) {
                "FAIL: cargo-audit is not the pinned version: $Detail"
            } else {
                'FAIL: cargo-audit is not the pinned version.'
            }
        }
        'InstallFailed' {
            if ($Detail) {
                "FAIL: pinned cargo-audit install failed: $Detail"
            } else {
                'FAIL: pinned cargo-audit install failed.'
            }
        }
        'AuditError' {
            if ($Detail) {
                "UNKNOWN: cargo audit did not complete: $Detail"
            } else {
                'UNKNOWN: cargo audit did not complete.'
            }
        }
        default {
            "UNKNOWN: RustSec audit is $Reason."
        }
    }

    [pscustomobject]@{
        Kind     = $Kind
        ExitCode = $exitCode
        Reason   = $Reason
        Detail   = $Detail
        Message  = $message
    }
}

function Get-RustsecIgnoreIdsFromToml {
    param(
        [Parameter(Mandatory = $true)]
        [AllowEmptyString()]
        [string]$Toml
    )

    $ids = [System.Collections.Generic.List[string]]::new()
    $inIgnore = $false
    foreach ($line in ($Toml -split '\r?\n')) {
        # Full-line comments and inline comments are not ignore entries.
        $code = $line
        if ($code -match '^([^#]*)') {
            $code = $Matches[1]
        }
        if ($code -match '^\s*ignore\s*=') {
            $inIgnore = $true
        }
        if ($inIgnore) {
            foreach ($match in [regex]::Matches($code, 'RUSTSEC-\d{4}-\d{4}')) {
                if (-not $ids.Contains($match.Value)) {
                    $ids.Add($match.Value)
                }
            }
            if ($code -match '\]') {
                $inIgnore = $false
            }
        }
    }
    # Unary comma keeps an empty array from collapsing to $null.
    return ,([string[]]$ids.ToArray())
}

function Read-RustsecExceptions {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return New-RustsecAuditOutcome -Kind Unknown -Reason MissingExceptions
    }

    try {
        $raw = Get-Content -LiteralPath $Path -Raw -ErrorAction Stop
    } catch {
        return New-RustsecAuditOutcome -Kind Unknown -Reason UnparseableExceptions
    }

    if ([string]::IsNullOrWhiteSpace($raw)) {
        return New-RustsecAuditOutcome -Kind Unknown -Reason UnparseableExceptions
    }

    try {
        $obj = $raw | ConvertFrom-Json -ErrorAction Stop
    } catch {
        return New-RustsecAuditOutcome -Kind Unknown -Reason UnparseableExceptions
    }

    if ($null -eq $obj) {
        return New-RustsecAuditOutcome -Kind Unknown -Reason UnparseableExceptions
    }

    $prop = $obj.PSObject.Properties['exceptions']
    if ($null -eq $prop) {
        return New-RustsecAuditOutcome -Kind Unknown -Reason UnparseableExceptions
    }

    if ($null -eq $prop.Value) {
        # ConvertFrom-Json can turn JSON [] into $null on Windows PowerShell 5.
        # A present exceptions key that is empty is "no exceptions", not missing.
        return [pscustomobject]@{
            Kind       = 'Parsed'
            Exceptions = @()
        }
    }

    # ConvertFrom-Json turns a JSON array into Object[] and a single object
    # into a PSCustomObject. A hashtable/object is not an exception list.
    if ($prop.Value -is [string] -or $prop.Value -is [bool] -or $prop.Value -is [ValueType]) {
        return New-RustsecAuditOutcome `
            -Kind Fail `
            -Reason InvalidExceptions `
            -Detail 'exceptions must be an array (empty is allowed).'
    }

    if ($prop.Value -isnot [System.Array] -and $prop.Value -isnot [System.Collections.IEnumerable]) {
        return New-RustsecAuditOutcome `
            -Kind Fail `
            -Reason InvalidExceptions `
            -Detail 'exceptions must be an array (empty is allowed).'
    }

    if ($prop.Value -is [System.Management.Automation.PSCustomObject]) {
        return New-RustsecAuditOutcome `
            -Kind Fail `
            -Reason InvalidExceptions `
            -Detail 'exceptions must be an array (empty is allowed).'
    }

    $items = @($prop.Value)
    $parsed = [System.Collections.Generic.List[object]]::new()
    foreach ($item in $items) {
        if ($null -eq $item) {
            return New-RustsecAuditOutcome `
                -Kind Fail `
                -Reason InvalidExceptions `
                -Detail 'exceptions array contains a null entry.'
        }

        $id = $null
        $owner = $null
        $justification = $null
        $expires = $null
        if ($item.PSObject.Properties['id']) { $id = [string]$item.id }
        if ($item.PSObject.Properties['owner']) { $owner = [string]$item.owner }
        if ($item.PSObject.Properties['justification']) { $justification = [string]$item.justification }
        if ($item.PSObject.Properties['expires']) { $expires = [string]$item.expires }

        if ([string]::IsNullOrWhiteSpace($id) -or $id -notmatch '^RUSTSEC-\d{4}-\d{4}$') {
            return New-RustsecAuditOutcome `
                -Kind Fail `
                -Reason InvalidExceptions `
                -Detail 'each exception needs a RUSTSEC-YYYY-NNNN id.'
        }
        if ([string]::IsNullOrWhiteSpace($owner)) {
            return New-RustsecAuditOutcome `
                -Kind Fail `
                -Reason InvalidExceptions `
                -Detail "$id is missing owner."
        }
        if ([string]::IsNullOrWhiteSpace($justification)) {
            return New-RustsecAuditOutcome `
                -Kind Fail `
                -Reason InvalidExceptions `
                -Detail "$id is missing justification."
        }
        if ([string]::IsNullOrWhiteSpace($expires) -or $expires -notmatch '^\d{4}-\d{2}-\d{2}$') {
            return New-RustsecAuditOutcome `
                -Kind Fail `
                -Reason InvalidExceptions `
                -Detail "$id is missing expires (YYYY-MM-DD)."
        }

        try {
            $expiryDate = [datetime]::ParseExact($expires, 'yyyy-MM-dd', [cultureinfo]::InvariantCulture)
        } catch {
            return New-RustsecAuditOutcome `
                -Kind Fail `
                -Reason InvalidExceptions `
                -Detail "$id expires is not a calendar date."
        }

        $parsed.Add([pscustomobject]@{
            Id            = $id
            Owner         = $owner.Trim()
            Justification = $justification.Trim()
            Expires       = $expires
            ExpiryDate    = $expiryDate.Date
        })
    }

    return [pscustomobject]@{
        Kind       = 'Parsed'
        Exceptions = @($parsed)
    }
}

function Test-RustsecExceptionSet {
    param(
        [AllowEmptyCollection()]
        [AllowNull()]
        [string[]]$IgnoreIds,

        [Parameter(Mandatory = $true)]
        $ParsedExceptions,

        [datetime]$NowUtc = [datetime]::UtcNow
    )

    if ($ParsedExceptions.Kind -ne 'Parsed') {
        return $ParsedExceptions
    }

    $today = $NowUtc.ToUniversalTime().Date
    $exceptionIds = [System.Collections.Generic.List[string]]::new()
    foreach ($item in @($ParsedExceptions.Exceptions)) {
        if ($item.ExpiryDate -lt $today) {
            return New-RustsecAuditOutcome `
                -Kind Fail `
                -Reason ExpiredException `
                -Detail "$($item.Id) expired $($item.Expires) (owner $($item.Owner))."
        }
        if ($exceptionIds.Contains($item.Id)) {
            return New-RustsecAuditOutcome `
                -Kind Fail `
                -Reason InvalidExceptions `
                -Detail "$($item.Id) is documented more than once."
        }
        $exceptionIds.Add($item.Id)
    }

    $ignore = @()
    if ($null -ne $IgnoreIds) {
        $ignore = @($IgnoreIds | Where-Object { $_ } | Sort-Object -Unique)
    }
    $documented = @($exceptionIds | Sort-Object)
    $missingDocs = @($ignore | Where-Object { $documented -notcontains $_ })
    $extraDocs = @($documented | Where-Object { $ignore -notcontains $_ })
    if ($missingDocs.Count -gt 0 -or $extraDocs.Count -gt 0) {
        $bits = @()
        if ($missingDocs.Count -gt 0) {
            $bits += "ignored without docs: $($missingDocs -join ', ')"
        }
        if ($extraDocs.Count -gt 0) {
            $bits += "documented but not ignored: $($extraDocs -join ', ')"
        }
        return New-RustsecAuditOutcome `
            -Kind Fail `
            -Reason IgnoreDrift `
            -Detail ($bits -join '; ')
    }

    return New-RustsecAuditOutcome -Kind Pass -Reason Clean
}

function Get-CargoAuditVersionFromOutput {
    param(
        [AllowEmptyString()]
        [string]$Output
    )

    if ([string]::IsNullOrWhiteSpace($Output)) {
        return $null
    }

    # `cargo audit --version` prints cargo-audit-audit 0.22.2;
    # `cargo-audit --version` prints cargo-audit 0.22.2.
    $match = [regex]::Match($Output, 'cargo-audit(?:-audit)?\s+(\d+\.\d+\.\d+)')
    if (-not $match.Success) {
        return $null
    }
    return $match.Groups[1].Value
}

function Test-CargoAuditVersion {
    param(
        [string]$Version
    )

    $pin = Get-CargoAuditPin
    if ([string]::IsNullOrWhiteSpace($Version)) {
        return New-RustsecAuditOutcome -Kind Unknown -Reason MissingTool
    }
    if ($Version -ne $pin) {
        return New-RustsecAuditOutcome `
            -Kind Fail `
            -Reason WrongVersion `
            -Detail "have $Version, need $pin from $(Get-CargoAuditCrateSource)."
    }
    return New-RustsecAuditOutcome -Kind Pass -Reason Clean
}

function Invoke-CargoAuditRaw {
    param(
        [Parameter(Mandatory = $true)]
        [string[]]$Arguments
    )

    if ($null -ne $script:CargoAuditInvoker) {
        return & $script:CargoAuditInvoker $Arguments
    }

    try {
        $output = & cargo @Arguments 2>&1 | Out-String
        return [pscustomobject]@{
            ExitCode = $LASTEXITCODE
            Output   = $output
            Found    = $true
        }
    } catch {
        return [pscustomobject]@{
            ExitCode = 127
            Output   = [string]$_
            Found    = $false
        }
    }
}

function Resolve-CargoAuditTool {
    param(
        [switch]$InstallIfMissing
    )

    $probe = Invoke-CargoAuditRaw -Arguments @('audit', '--version')
    $version = $null
    if ($probe.Found) {
        $version = Get-CargoAuditVersionFromOutput -Output $probe.Output
    }
    $check = Test-CargoAuditVersion -Version $version
    if ($check.Kind -eq 'Pass') {
        return $check
    }

    if (-not $InstallIfMissing) {
        return $check
    }

    $pin = Get-CargoAuditPin
    $installArgs = @('install', 'cargo-audit', '--version', $pin, '--locked')
    if ($probe.Found -and $version) {
        $installArgs += '--force'
    }

    $install = Invoke-CargoAuditRaw -Arguments $installArgs
    if (-not $install.Found -or $install.ExitCode -ne 0) {
        return New-RustsecAuditOutcome `
            -Kind Fail `
            -Reason InstallFailed `
            -Detail $install.Output
    }

    $probe = Invoke-CargoAuditRaw -Arguments @('audit', '--version')
    $version = $null
    if ($probe.Found) {
        $version = Get-CargoAuditVersionFromOutput -Output $probe.Output
    }
    return Test-CargoAuditVersion -Version $version
}

function Invoke-RustsecAuditGate {
    param(
        [string]$RepoRoot = (Split-Path -Parent $PSScriptRoot),
        [datetime]$NowUtc = [datetime]::UtcNow,
        [switch]$InstallIfMissing
    )

    $lockPath = Join-Path $RepoRoot 'Cargo.lock'
    if (-not (Test-Path -LiteralPath $lockPath -PathType Leaf)) {
        return New-RustsecAuditOutcome -Kind Fail -Reason MissingLockfile
    }

    $tomlPath = Join-Path $RepoRoot '.cargo/audit.toml'
    if (-not (Test-Path -LiteralPath $tomlPath -PathType Leaf)) {
        return New-RustsecAuditOutcome -Kind Unknown -Reason MissingAuditToml
    }

    $exceptionsPath = Join-Path $RepoRoot '.cargo/rustsec-exceptions.json'
    $parsed = Read-RustsecExceptions -Path $exceptionsPath
    if ($parsed.Kind -ne 'Parsed') {
        return $parsed
    }

    try {
        $toml = Get-Content -LiteralPath $tomlPath -Raw -ErrorAction Stop
    } catch {
        return New-RustsecAuditOutcome -Kind Unknown -Reason AuditError
    }
    $ignoreIds = Get-RustsecIgnoreIdsFromToml -Toml $toml
    $exceptions = Test-RustsecExceptionSet `
        -IgnoreIds $ignoreIds `
        -ParsedExceptions $parsed `
        -NowUtc $NowUtc
    if ($exceptions.Kind -ne 'Pass') {
        return $exceptions
    }

    $tool = Resolve-CargoAuditTool -InstallIfMissing:$InstallIfMissing
    if ($tool.Kind -ne 'Pass') {
        return $tool
    }

    # cargo-audit has no --locked flag. "Locked" here means Cargo.lock
    # (passed explicitly) plus a version-pinned cargo-audit binary.
    $audit = Invoke-CargoAuditRaw -Arguments @(
        'audit',
        '--file', $lockPath,
        '--deny', 'unsound'
    )
    if (-not $audit.Found) {
        return New-RustsecAuditOutcome -Kind Unknown -Reason MissingTool
    }
    if ($audit.ExitCode -eq 0) {
        return New-RustsecAuditOutcome -Kind Pass -Reason Clean
    }
    if ($audit.ExitCode -eq 1) {
        # cargo-audit uses 1 for vulns and also for some config/runtime
        # failures. Only a report that names an advisory is "advisories found".
        if ($audit.Output -match 'RUSTSEC-\d{4}-\d{4}|Advisory:') {
            return New-RustsecAuditOutcome `
                -Kind Fail `
                -Reason Advisories `
                -Detail $audit.Output
        }
        return New-RustsecAuditOutcome `
            -Kind Unknown `
            -Reason AuditError `
            -Detail $audit.Output
    }
    return New-RustsecAuditOutcome `
        -Kind Unknown `
        -Reason AuditError `
        -Detail $audit.Output
}

if ($MyInvocation.InvocationName -ne '.') {
    $outcome = Invoke-RustsecAuditGate -InstallIfMissing
    Write-Host $outcome.Message
    exit $outcome.ExitCode
}
