# Wait/read/exit mapping for the sandbox smoke host (SBS-901).
# Functions only: safe to dotsource. Does not launch Windows Sandbox.
#
# Unknown is its own state. Missing, unparseable, and a missing/non-boolean
# `passed` property are not guest FAIL. Only `passed: true` is Pass.

function New-SandboxSmokeOutcome {
    param(
        [Parameter(Mandatory = $true)]
        [ValidateSet('Pass', 'Fail', 'Unknown')]
        [string]$Kind,

        [Parameter(Mandatory = $true)]
        [string]$Reason,

        [string]$ResultPath,

        [string]$ErrorText,

        [switch]$TimedOut
    )

    $exitCode = switch ($Kind) {
        'Pass' { 0 }
        'Fail' { 1 }
        default { 2 }
    }

    $message = switch ($Reason) {
        'GuestPassed' {
            'PASS: sandbox smoke guest reported passed.'
        }
        'GuestFailed' {
            if ($ErrorText) {
                "FAIL: sandbox smoke guest failed: $ErrorText"
            } else {
                'FAIL: sandbox smoke guest reported passed=false.'
            }
        }
        'Missing' {
            if ($TimedOut) {
                'UNKNOWN: guest did not write result.json before timeout.'
            } else {
                'UNKNOWN: result.json is missing.'
            }
        }
        'Unparseable' {
            if ($TimedOut) {
                'UNKNOWN: result.json was unreadable after timeout.'
            } else {
                'UNKNOWN: result.json is unreadable.'
            }
        }
        'InvalidPassed' {
            'UNKNOWN: result.json has no boolean passed property.'
        }
        'SandboxExited' {
            'UNKNOWN: Windows Sandbox exited without a readable guest result.'
        }
        default {
            "UNKNOWN: sandbox smoke result is $Reason."
        }
    }

    [pscustomobject]@{
        Kind       = $Kind
        ExitCode   = $exitCode
        Reason     = $Reason
        Error      = $ErrorText
        Message    = $message
        TimedOut   = [bool]$TimedOut
        ResultPath = $ResultPath
    }
}

function Get-SandboxSmokeResultPath {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ResultsDirectory
    )

    return Join-Path $ResultsDirectory 'result.json'
}

function Test-SandboxSmokeOutcomeTerminal {
    param(
        [Parameter(Mandatory = $true)]
        $Outcome
    )

    # Missing / unparseable stay non-terminal so a concurrent writer can finish.
    return $Outcome.Reason -in @('GuestPassed', 'GuestFailed', 'InvalidPassed')
}

function Read-SandboxSmokeResult {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ResultsDirectory
    )

    $path = Get-SandboxSmokeResultPath -ResultsDirectory $ResultsDirectory
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        return New-SandboxSmokeOutcome -Kind Unknown -Reason Missing -ResultPath $path
    }

    try {
        $raw = Get-Content -LiteralPath $path -Raw -ErrorAction Stop
    } catch {
        return New-SandboxSmokeOutcome -Kind Unknown -Reason Unparseable -ResultPath $path
    }

    if ([string]::IsNullOrWhiteSpace($raw)) {
        return New-SandboxSmokeOutcome -Kind Unknown -Reason Unparseable -ResultPath $path
    }

    try {
        $obj = $raw | ConvertFrom-Json -ErrorAction Stop
    } catch {
        return New-SandboxSmokeOutcome -Kind Unknown -Reason Unparseable -ResultPath $path
    }

    if ($null -eq $obj) {
        return New-SandboxSmokeOutcome -Kind Unknown -Reason Unparseable -ResultPath $path
    }

    $passedProperty = $obj.PSObject.Properties['passed']
    if ($null -eq $passedProperty) {
        return New-SandboxSmokeOutcome -Kind Unknown -Reason InvalidPassed -ResultPath $path
    }

    $passed = $passedProperty.Value
    if ($passed -isnot [bool]) {
        return New-SandboxSmokeOutcome -Kind Unknown -Reason InvalidPassed -ResultPath $path
    }

    $errorText = $null
    if ($obj.PSObject.Properties['error'] -and $null -ne $obj.error) {
        $text = [string]$obj.error
        if (-not [string]::IsNullOrWhiteSpace($text)) {
            $errorText = $text
        }
    }

    if ($passed) {
        return New-SandboxSmokeOutcome `
            -Kind Pass `
            -Reason GuestPassed `
            -ResultPath $path `
            -ErrorText $errorText
    }

    return New-SandboxSmokeOutcome `
        -Kind Fail `
        -Reason GuestFailed `
        -ResultPath $path `
        -ErrorText $errorText
}

function Get-SandboxSmokeProcessNames {
    # WindowsSandbox.exe is a short-lived launcher. On current Windows the
    # processes that stay up are Server / RemoteSession; Client is older.
    @('WindowsSandbox', 'WindowsSandboxClient', 'WindowsSandboxServer', 'WindowsSandboxRemoteSession', 'wsb')
}

function Get-SandboxSmokeProcesses {
    Get-Process -Name (Get-SandboxSmokeProcessNames) -ErrorAction SilentlyContinue
}

function Clear-SandboxSmokePriorResult {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ResultsDirectory
    )

    $path = Get-SandboxSmokeResultPath -ResultsDirectory $ResultsDirectory
    Remove-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath ($path + '.tmp') -Force -ErrorAction SilentlyContinue
}

function Stop-SandboxSmokeLeftovers {
    param(
        [System.Diagnostics.Process]$SandboxProcess
    )

    # Only the sandbox this run started. Killing every WindowsSandbox* process
    # would tear down an unrelated session on the same host.
    if (-not $SandboxProcess) {
        return
    }
    try {
        $SandboxProcess | Stop-Process -Force -ErrorAction SilentlyContinue
    } catch {
    }
}

function Test-SandboxSmokeSessionEnded {
    param(
        [System.Diagnostics.Process]$SandboxProcess,

        [switch]$SawSandbox
    )

    if (-not $SawSandbox) {
        return $false
    }

    if (-not $SandboxProcess) {
        return $false
    }

    try {
        $SandboxProcess.Refresh()
        if (-not $SandboxProcess.HasExited) {
            return $false
        }
    } catch {
        # Handle is unusable; fall through to the named-process check.
    }

    $running = Get-SandboxSmokeProcesses
    return -not $running
}

function Wait-SandboxSmokeResult {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ResultsDirectory,

        [ValidateRange(1, [int]::MaxValue)]
        [int]$TimeoutSeconds = 1200,

        [ValidateRange(1, 60000)]
        [int]$PollIntervalMilliseconds = 500,

        [System.Diagnostics.Process]$SandboxProcess,

        [switch]$StopLeftoverSandbox
    )

    $deadline = [datetime]::UtcNow.AddSeconds($TimeoutSeconds)
    $graceDeadline = $null
    $sawSandbox = $false

    while ($true) {
        $snapshot = Read-SandboxSmokeResult -ResultsDirectory $ResultsDirectory
        if (Test-SandboxSmokeOutcomeTerminal -Outcome $snapshot) {
            return $snapshot
        }

        if (Get-SandboxSmokeProcesses) {
            $sawSandbox = $true
        }

        $now = [datetime]::UtcNow
        if ($now -ge $deadline) {
            $snapshot = Read-SandboxSmokeResult -ResultsDirectory $ResultsDirectory
            if (Test-SandboxSmokeOutcomeTerminal -Outcome $snapshot) {
                return $snapshot
            }

            if ($StopLeftoverSandbox) {
                Stop-SandboxSmokeLeftovers -SandboxProcess $SandboxProcess
            }

            return New-SandboxSmokeOutcome `
                -Kind Unknown `
                -Reason $snapshot.Reason `
                -ResultPath $snapshot.ResultPath `
                -ErrorText $snapshot.Error `
                -TimedOut
        }

        if (Test-SandboxSmokeSessionEnded -SandboxProcess $SandboxProcess -SawSandbox:$sawSandbox) {
            if ($null -eq $graceDeadline) {
                $flushSeconds = 3
                $graceDeadline = $now.AddSeconds($flushSeconds)
                if ($graceDeadline -gt $deadline) {
                    $graceDeadline = $deadline
                }
            }
            if ($now -ge $graceDeadline) {
                $snapshot = Read-SandboxSmokeResult -ResultsDirectory $ResultsDirectory
                if (Test-SandboxSmokeOutcomeTerminal -Outcome $snapshot) {
                    return $snapshot
                }

                if ($StopLeftoverSandbox) {
                    Stop-SandboxSmokeLeftovers -SandboxProcess $SandboxProcess
                }

                return New-SandboxSmokeOutcome `
                    -Kind Unknown `
                    -Reason SandboxExited `
                    -ResultPath $snapshot.ResultPath `
                    -ErrorText $snapshot.Error
            }
        }

        Start-Sleep -Milliseconds $PollIntervalMilliseconds
    }
}
