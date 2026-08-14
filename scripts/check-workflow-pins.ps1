# Fails when a workflow or CI script references a mutable third-party
# dependency. Every workflow here either signs, publishes, or runs on the
# persistent self-hosted runner, so a retargetable action tag, an unpinned
# `npx` package, or an unpinned `Install-Module` is code execution with
# credentials: the check demands full commit SHAs for `uses:`, exact x.y.z
# versions for anything npx resolves, and -RequiredVersion on every
# Install-Module. Dependency updates then always arrive as a reviewable diff
# of the pinned revision.
$ErrorActionPreference = 'Stop'

$root = Join-Path $PSScriptRoot '..'
$failures = @()

$workflows = Get-ChildItem (Join-Path $root '.github/workflows') -Filter '*.yml'
foreach ($workflow in $workflows) {
    $lines = Get-Content $workflow.FullName
    for ($i = 0; $i -lt $lines.Count; $i++) {
        $line = $lines[$i]
        $location = "$($workflow.Name):$($i + 1)"

        if ($line -match '^\s*(?:-\s+)?uses:\s*(\S+)') {
            $ref = $Matches[1].Trim("'`"")
            # Local composite actions carry no ref to pin.
            if (-not $ref.StartsWith('./') -and $ref -notmatch '@[0-9a-f]{40}$') {
                $failures += "${location}: '$ref' must be pinned to a full commit SHA."
            }
        }

        # The first non-flag token after npx is the package it resolves. A
        # bare `npx wrangler` means `latest`, so absence of @x.y.z is exactly
        # as mutable as writing @latest — both must fail here.
        if ($line -match '\bnpx\s+((?:--?[\w-]+\s+)*)(\S+)') {
            $package = $Matches[2].Trim("'`"")
            if ($package -notmatch '^@?[^@]+@\d+\.\d+\.\d+$') {
                $failures += "${location}: npx package '$package' must pin an exact x.y.z version."
            }
        }
    }
}

# The same standard for PowerShell modules the CI scripts install (PSGallery
# tags are as mutable as npm's).
$scripts = Get-ChildItem (Join-Path $root 'scripts') -Filter '*.ps1'
foreach ($script in $scripts) {
    $lines = Get-Content $script.FullName
    for ($i = 0; $i -lt $lines.Count; $i++) {
        # Comments don't install anything — without this the checker flags
        # its own header (and any script that documents the rule).
        if ($lines[$i] -match '^\s*#') {
            continue
        }
        if ($lines[$i] -match '\bInstall-Module\b') {
            # The call may wrap across backtick continuations; look at the
            # whole statement before deciding the version is missing.
            $statement = $lines[$i]
            for ($j = $i; $j -lt $lines.Count - 1 -and $lines[$j].TrimEnd().EndsWith('`'); $j++) {
                $statement += ' ' + $lines[$j + 1]
            }
            if ($statement -notmatch '-RequiredVersion\b') {
                $failures += "$($script.Name):$($i + 1): Install-Module must pin -RequiredVersion."
            }
        }
    }
}

if ($failures.Count -gt 0) {
    $failures | ForEach-Object { Write-Host "PIN VIOLATION $_" }
    Write-Error ("{0} mutable workflow dependenc{1} found." -f
        $failures.Count, $(if ($failures.Count -eq 1) { 'y' } else { 'ies' }))
    exit 1
}
Write-Host "All workflow dependencies are pinned."
