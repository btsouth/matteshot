# Self-contained checks for scripts/sandbox-smoke-result.ps1 (SBS-901).
# No Pester: CI cannot Install-Module, and check-workflow-pins.ps1 rejects
# unpinned Install-Module. Exit 0 only if every assertion passes.
# Does not launch WindowsSandbox.exe.

$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'sandbox-smoke-result.ps1')

$script:failures = 0
$script:passes = 0

function New-SandboxSmokeTestDir {
    $dir = Join-Path ([System.IO.Path]::GetTempPath()) (
        'sbs-901-' + [guid]::NewGuid().ToString('N')
    )
    New-Item -ItemType Directory -Path $dir | Out-Null
    return $dir
}

function Assert-SandboxSmoke {
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

function Write-SandboxSmokeFixture {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Directory,

        [AllowEmptyString()]
        [Parameter(Mandatory = $true)]
        [string]$Json
    )

    Set-Content -Encoding UTF8 -Path (Join-Path $Directory 'result.json') -Value $Json
}

# SBS-901: guest never wrote result.json must not look like a pass.
$dir = New-SandboxSmokeTestDir
try {
    $outcome = Wait-SandboxSmokeResult `
        -ResultsDirectory $dir `
        -TimeoutSeconds 1 `
        -PollIntervalMilliseconds 50
    Assert-SandboxSmoke `
        -Name 'missing-result-after-timeout' `
        -Condition ($outcome.Kind -eq 'Unknown' -and $outcome.ExitCode -eq 2) `
        -Detail "Kind=$($outcome.Kind) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

# SBS-901: guest passed=false must not look like a pass.
$dir = New-SandboxSmokeTestDir
try {
    Write-SandboxSmokeFixture -Directory $dir -Json (@'
{"passed": false, "error": "capture failed", "completed_at": "2026-01-01T00:00:00Z"}
'@)
    $outcome = Wait-SandboxSmokeResult `
        -ResultsDirectory $dir `
        -TimeoutSeconds 5 `
        -PollIntervalMilliseconds 50
    Assert-SandboxSmoke `
        -Name 'guest-passed-false' `
        -Condition ($outcome.Kind -eq 'Fail' -and $outcome.ExitCode -eq 1) `
        -Detail "Kind=$($outcome.Kind) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

# SBS-901: only passed=true is a host pass (exit 0).
$dir = New-SandboxSmokeTestDir
try {
    Write-SandboxSmokeFixture -Directory $dir -Json (@'
{"passed": true, "error": null, "completed_at": "2026-01-01T00:00:00Z"}
'@)
    $outcome = Wait-SandboxSmokeResult `
        -ResultsDirectory $dir `
        -TimeoutSeconds 5 `
        -PollIntervalMilliseconds 50
    Assert-SandboxSmoke `
        -Name 'guest-passed-true' `
        -Condition ($outcome.Kind -eq 'Pass' -and $outcome.ExitCode -eq 0) `
        -Detail "Kind=$($outcome.Kind) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

# SBS-901: empty / garbage result.json after timeout is Unknown, not a pass.
$dir = New-SandboxSmokeTestDir
try {
    Write-SandboxSmokeFixture -Directory $dir -Json ''
    $outcome = Wait-SandboxSmokeResult `
        -ResultsDirectory $dir `
        -TimeoutSeconds 1 `
        -PollIntervalMilliseconds 50
    Assert-SandboxSmoke `
        -Name 'unparseable-result-after-timeout' `
        -Condition ($outcome.Kind -eq 'Unknown' -and $outcome.ExitCode -eq 2) `
        -Detail "Kind=$($outcome.Kind) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

# SBS-901: parseable JSON without a passed property is Unknown, not guest FAIL.
$dir = New-SandboxSmokeTestDir
try {
    Write-SandboxSmokeFixture -Directory $dir -Json (@'
{"error": "no passed field", "completed_at": "2026-01-01T00:00:00Z"}
'@)
    $outcome = Wait-SandboxSmokeResult `
        -ResultsDirectory $dir `
        -TimeoutSeconds 5 `
        -PollIntervalMilliseconds 50
    Assert-SandboxSmoke `
        -Name 'json-missing-passed-property' `
        -Condition ($outcome.Kind -eq 'Unknown' -and $outcome.ExitCode -eq 2) `
        -Detail "Kind=$($outcome.Kind) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

# SBS-901: passed as the string "true" is Unknown, not a pass.
$dir = New-SandboxSmokeTestDir
try {
    Write-SandboxSmokeFixture -Directory $dir -Json (@'
{"passed": "true", "error": null, "completed_at": "2026-01-01T00:00:00Z"}
'@)
    $outcome = Wait-SandboxSmokeResult `
        -ResultsDirectory $dir `
        -TimeoutSeconds 5 `
        -PollIntervalMilliseconds 50
    Assert-SandboxSmoke `
        -Name 'passed-not-boolean' `
        -Condition ($outcome.Kind -eq 'Unknown' -and $outcome.ExitCode -eq 2) `
        -Detail "Kind=$($outcome.Kind) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

# SBS-901: a leftover passed:true from a previous guest must not make this run pass.
$dir = New-SandboxSmokeTestDir
try {
    Write-SandboxSmokeFixture -Directory $dir -Json (@'
{"passed": true, "error": null, "completed_at": "2026-01-01T00:00:00Z"}
'@)
    Clear-SandboxSmokePriorResult -ResultsDirectory $dir
    $outcome = Wait-SandboxSmokeResult `
        -ResultsDirectory $dir `
        -TimeoutSeconds 1 `
        -PollIntervalMilliseconds 50
    Assert-SandboxSmoke `
        -Name 'prior-result-json-is-wiped' `
        -Condition (
            $outcome.Kind -eq 'Unknown' -and
            $outcome.Reason -eq 'Missing' -and
            $outcome.ExitCode -eq 2
        ) `
        -Detail "Kind=$($outcome.Kind) Reason=$($outcome.Reason) ExitCode=$($outcome.ExitCode)"
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

# SBS-901: launcher already gone and no sandbox process seen is not SandboxExited at 3s.
$dir = New-SandboxSmokeTestDir
$exited = $null
try {
    $exited = Start-Process -FilePath $env:ComSpec -ArgumentList '/c', 'exit' -PassThru -WindowStyle Hidden
    $null = $exited.WaitForExit(5000)
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $outcome = Wait-SandboxSmokeResult `
        -ResultsDirectory $dir `
        -TimeoutSeconds 5 `
        -PollIntervalMilliseconds 50 `
        -SandboxProcess $exited
    $sw.Stop()
    Assert-SandboxSmoke `
        -Name 'unseen-sandbox-waits-until-timeout' `
        -Condition (
            $outcome.Kind -eq 'Unknown' -and
            $outcome.Reason -eq 'Missing' -and
            $outcome.ExitCode -eq 2 -and
            $outcome.TimedOut -and
            $sw.Elapsed.TotalSeconds -ge 4
        ) `
        -Detail "Kind=$($outcome.Kind) Reason=$($outcome.Reason) ExitCode=$($outcome.ExitCode) TimedOut=$($outcome.TimedOut) Seconds=$([math]::Round($sw.Elapsed.TotalSeconds, 1))"
} finally {
    if ($exited) { $exited.Dispose() }
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}

if ($script:failures -gt 0) {
    Write-Host "$script:failures sandbox smoke result test(s) failed; $script:passes passed."
    exit 1
}

Write-Host "All $script:passes sandbox smoke result tests passed."
exit 0
