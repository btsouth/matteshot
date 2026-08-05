<#
.SYNOPSIS
    Run every headless Matteshot probe and fail loudly if one breaks.

.DESCRIPTION
    Matteshot ships a good set of diagnostic flags, but they were only ever run
    by hand, one at a time, when somebody already suspected a problem. That is
    how --video-edit-test sat broken without anyone noticing. This runs the lot
    and reports a table.

    Deliberately safe to run while you are working:
      * nothing here writes the clipboard
      * nothing here injects keyboard or mouse input (--scroll-test does, so it
        is opt-in behind -IncludeScroll)
      * nothing here opens a window, unless it has to record a fixture

    Covers the capture path, recording and video export, and the auto-update
    trust gates. The interactive parts (picker, editors, overlay) cannot be
    driven safely from a script and still need a human.

    release-candidate.yml runs this against the signed candidate with -Strict,
    so a probe that cannot run is a red build rather than a quiet SKIP.

.EXAMPLE
    .\scripts\run-probes.ps1
.EXAMPLE
    .\scripts\run-probes.ps1 -WindowTitle Calculator -IncludeScroll
.EXAMPLE
    .\scripts\run-probes.ps1 -Strict -Offline -SignedFile .\MatteshotSetup-1.0.0.exe
#>
param(
    # Window to capture from. Defaults to any suitable visible window.
    [string]$WindowTitle,
    # Recording to exercise the video probes against. Defaults to the newest in
    # the videos folder, or records one.
    [string]$Fixture,
    # --scroll-test drives the target with synthetic wheel input. Off by default
    # so this is safe to run mid-work.
    [switch]$IncludeScroll,
    # Skip the probes that need the network.
    [switch]$Offline,
    [string]$Exe = "target\release\matteshot.exe",
    # File the Authenticode gate must accept. Defaults to the installed build,
    # because a local cargo build is unsigned. Release CI points this at the
    # signed installer it just produced, which is the artifact auto-update
    # actually downloads and runs.
    [string]$SignedFile,
    # Turn every SKIP into a failure. CI runs strict: a probe that quietly did
    # not run is how --video-edit-test stayed broken for weeks.
    [switch]$Strict,
    # Leave the capture and OCR probes out of the run entirely. They need a
    # visible window, which a GitHub-hosted runner does not have.
    [switch]$NoCapture
)

$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)

if (-not (Test-Path $Exe)) {
    throw "$Exe not found. Build first: cargo build --release"
}
$Exe = (Resolve-Path $Exe).Path

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("matteshot-probes-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work -Force | Out-Null
$results = @()

function Invoke-Probe {
    param(
        [string]$Name,
        [string[]]$ProbeArgs,
        # Probe passes only if stderr matches this. Exit code alone is not
        # enough: some probes report failure in their output.
        [string]$Expect,
        # Probe is expected to fail; used for the signature rejection checks.
        [switch]$ExpectFailure
    )
    $log = Join-Path $work ("{0}.log" -f ($Name -replace '[^\w]', '-'))
    $timer = [Diagnostics.Stopwatch]::StartNew()
    $exit = 0
    try {
        $p = Start-Process -FilePath $script:Exe -ArgumentList $ProbeArgs `
            -RedirectStandardError $log -Wait -NoNewWindow -PassThru
        $exit = $p.ExitCode
    } catch {
        $exit = -1
    }
    $timer.Stop()
    $out = if (Test-Path $log) { (Get-Content $log -Raw) } else { '' }

    $ok = if ($ExpectFailure) { $exit -ne 0 } else { $exit -eq 0 }
    if ($ok -and $Expect) { $ok = $out -match $Expect }

    $detail = ($out -split "`r?`n" | Where-Object { $_.Trim() } | Select-Object -Last 1)
    $script:results += [pscustomobject]@{
        Probe   = $Name
        Result  = if ($ok) { 'PASS' } else { 'FAIL' }
        Seconds = [math]::Round($timer.Elapsed.TotalSeconds, 1)
        Detail  = if ($detail) { $detail.Trim() } else { "exit $exit" }
    }
    return $ok
}

# ---------------------------------------------------------------- capture ---
# -NoCapture puts these out of scope rather than skipping them, so -Strict keeps
# meaning "every probe this run promised actually ran". A GitHub-hosted runner
# has a desktop you can record, but no genuinely visible top-level window:
# PowerShell reports a MainWindowTitle for a console started there and
# find_by_title still cannot see it. Capture and OCR stay in the local run and
# in INTERACTIVE-REGRESSION.md.
if (-not $NoCapture -and -not $WindowTitle) {
    # Size matters: plenty of apps keep tiny hidden helper windows, and
    # capturing a 16x16 one makes the OCR probe "fail" for no real reason.
    Add-Type -Name ProbeWin -Namespace Matteshot -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool GetWindowRect(System.IntPtr h, out RECT r);
public struct RECT { public int L, T, R, B; }
'@ -ErrorAction SilentlyContinue
    $WindowTitle = Get-Process |
        Where-Object { $_.MainWindowTitle -and $_.ProcessName -ne 'matteshot' } |
        Where-Object {
            $r = New-Object Matteshot.ProbeWin+RECT
            [void][Matteshot.ProbeWin]::GetWindowRect($_.MainWindowHandle, [ref]$r)
            ($r.R - $r.L) -ge 600 -and ($r.B - $r.T) -ge 400
        } |
        Select-Object -First 1 -ExpandProperty MainWindowTitle
}

# Report an absent target rather than throwing: a bare terminating error loses
# the table for the probes that did run, and -Strict is what decides whether it
# was acceptable.
if ($NoCapture) {
    Write-Host 'Capture probes excluded by -NoCapture.' -ForegroundColor DarkGray
} elseif (-not $WindowTitle) {
    $results += [pscustomobject]@{
        Probe = 'capture probes'; Result = 'SKIP'; Seconds = 0
        Detail = 'no window big enough to capture; pass -WindowTitle'
    }
} else {
    Write-Host "Capturing from: $WindowTitle" -ForegroundColor DarkGray

    Invoke-Probe -Name 'capture (--bench)' -ProbeArgs @('--bench', $WindowTitle) -Expect 'bench 3:' | Out-Null
    Invoke-Probe -Name 'ocr text' -ProbeArgs @('--ocr', $WindowTitle) -Expect 'chars' | Out-Null

    $benchPng = Join-Path ([System.IO.Path]::GetTempPath()) 'matteshot-bench.png'
    if (Test-Path $benchPng) {
        Invoke-Probe -Name 'ocr word boxes' -ProbeArgs @('--ocr-words', $benchPng) -Expect 'words over' | Out-Null
    }

    if ($IncludeScroll) {
        Invoke-Probe -Name 'scrolling capture' -ProbeArgs @('--scroll-test', $WindowTitle) | Out-Null
    }
}

# ------------------------------------------------------------ tweak editor ---
# The editor sizes its working bitmap to the preview pane so a large capture
# stays sharp to annotate against, which means every rebuild composes more
# pixels than it used to. That trade only holds while a rebuild stays cheap, and
# nothing else here would notice it getting expensive. Synthetic input, so it
# needs no window and is safe to run anywhere.
Invoke-Probe -Name 'preview rebuild budget' -ProbeArgs @('--preview-bench', '2560') `
    -Expect 'preview rebuild within budget' | Out-Null

# ------------------------------------------------- recording and exporting ---
if (-not $Fixture) {
    $videos = Join-Path ([Environment]::GetFolderPath('MyVideos')) 'Matteshot'
    $Fixture = Get-ChildItem $videos -Filter *.mp4 -ErrorAction SilentlyContinue |
        Where-Object { $_.Length -gt 0 } |
        Sort-Object LastWriteTime -Descending |
        Select-Object -First 1 -ExpandProperty FullName
}
if (-not $Fixture -or -not (Test-Path $Fixture)) {
    Write-Host 'No recording to test against; recording one (a window will appear briefly).' -ForegroundColor Yellow
    Invoke-Probe -Name 'record (fixture)' -ProbeArgs @('--record-test', '3') | Out-Null
    $videos = Join-Path ([Environment]::GetFolderPath('MyVideos')) 'Matteshot'
    $Fixture = Get-ChildItem $videos -Filter *.mp4 -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1 -ExpandProperty FullName
}

if ($Fixture -and (Test-Path $Fixture)) {
    Write-Host "Video fixture: $(Split-Path -Leaf $Fixture)" -ForegroundColor DarkGray
    Invoke-Probe -Name 'playback decode' -ProbeArgs @('--playback-test', $Fixture) -Expect 'frames through' | Out-Null
    Invoke-Probe -Name 'trim export' -ProbeArgs @('--trim-test', $Fixture, '0', '20000000') -Expect 'exported ->' | Out-Null
    Invoke-Probe -Name 'trim + matte export' -ProbeArgs @('--trim-test', $Fixture, '0', '20000000', '3') -Expect 'exported ->' | Out-Null
    # Forces a 1:1 aspect, which is what used to compose past the H.264 frame
    # limit and die with an unexplained media-type error.
    Invoke-Probe -Name 'annotated edit export' -ProbeArgs @('--video-edit-test', $Fixture) -Expect 'video editor export' | Out-Null

    # Clean up only what this run produced.
    foreach ($suffix in @('.trim.mp4', '.matte.mp4', '.edit.mp4')) {
        $spawned = [IO.Path]::ChangeExtension($Fixture, $null).TrimEnd('.') + $suffix
        Remove-Item $spawned -Force -ErrorAction SilentlyContinue
    }
} else {
    $results += [pscustomobject]@{
        Probe = 'video probes'; Result = 'SKIP'; Seconds = 0; Detail = 'no recording available'
    }
}

# ------------------------------------------------------------ auto-update ---
# The signature gate is the whole security story for auto-update, so it is
# checked in both directions: ours accepted, somebody else's refused.
#
# It has to run against a signed binary, and only CI-built releases are signed
# — a local cargo build is not, and would be rejected for the right reason but
# the wrong test. Locally that means the installed build; in release CI it is
# the freshly signed installer, so the candidate proves it would pass its own
# auto-update gate before anyone can download it.
$signed = if ($SignedFile) {
    $SignedFile
} else {
    Join-Path $env:LOCALAPPDATA 'Programs\Matteshot\matteshot.exe'
}
if (Test-Path $signed) {
    Write-Host "Signature gate target: $signed" -ForegroundColor DarkGray
    Invoke-Probe -Name 'signature accepts ours' -ProbeArgs @('--verify-signature-test', $signed) -Expect 'ACCEPT' | Out-Null

    $tampered = Join-Path $work 'tampered.exe'
    Copy-Item $signed $tampered -Force
    $fs = [IO.File]::Open($tampered, 'Open', 'ReadWrite')
    $fs.Position = [math]::Min(150000, $fs.Length - 1)
    $b = $fs.ReadByte(); $fs.Position -= 1; $fs.WriteByte($b -bxor 0xFF)
    $fs.Close()
    Invoke-Probe -Name 'signature rejects tampering' -ProbeArgs @('--verify-signature-test', $tampered) -Expect 'REJECT' | Out-Null
} else {
    $results += [pscustomobject]@{
        Probe = 'signature gate'; Result = 'SKIP'; Seconds = 0
        Detail = 'no installed signed build to check against'
    }
}

$foreign = Join-Path $env:WINDIR 'System32\notepad.exe'
if (Test-Path $foreign) {
    Invoke-Probe -Name 'signature rejects foreign' -ProbeArgs @('--verify-signature-test', $foreign) -Expect 'REJECT' | Out-Null
}

if (-not $Offline) {
    Invoke-Probe -Name 'update manifest' -ProbeArgs @('--update-test') | Out-Null
    Invoke-Probe -Name 'update download + verify' `
        -ProbeArgs @('--update-stage-test', 'https://download.matteshot.app/MatteshotSetup.exe') `
        -Expect 'verified hash \+ signature' | Out-Null
}

# ----------------------------------------------------------------- report ---
Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
Write-Host ''
$results | Format-Table -AutoSize

$failed = @($results | Where-Object Result -eq 'FAIL')
$skipped = @($results | Where-Object Result -eq 'SKIP')
if ($failed.Count -gt 0) {
    Write-Host ("{0} probe(s) failed." -f $failed.Count) -ForegroundColor Red
    exit 1
}
if ($Strict -and $skipped.Count -gt 0) {
    Write-Host (
        "{0} probe(s) skipped and -Strict was requested: {1}" -f
        $skipped.Count, (($skipped | ForEach-Object { $_.Probe }) -join ', ')
    ) -ForegroundColor Red
    exit 1
}
Write-Host ("All {0} probes passed." -f @($results | Where-Object Result -eq 'PASS').Count) -ForegroundColor Green
