# Fails when a workflow references a mutable third-party dependency. Every
# workflow here either signs, publishes, or runs on the persistent self-hosted
# runner, so a retargetable action tag or an `npx pkg@latest` is code execution
# with credentials: the check demands full commit SHAs for `uses:` and exact
# x.y.z versions for anything npx installs. Dependency updates then always
# arrive as a reviewable diff of the pinned revision.
$ErrorActionPreference = 'Stop'

$root = Join-Path $PSScriptRoot '..'
$workflows = Get-ChildItem (Join-Path $root '.github/workflows') -Filter '*.yml'
$failures = @()

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

        if ($line -match '\bnpx\b') {
            foreach ($m in [regex]::Matches($line, '\bnpx\s+(?:--yes\s+)?(\S+?)@(\S+)')) {
                $spec = "$($m.Groups[1].Value)@$($m.Groups[2].Value)"
                if ($m.Groups[2].Value -notmatch '^\d+\.\d+\.\d+$') {
                    $failures += "${location}: '$spec' must pin an exact x.y.z version."
                }
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
