$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$payload = "C:\MatteshotSmoke"
$results = "C:\MatteshotResults"
$log = Join-Path $results "smoke.log"
$result = Join-Path $results "result.json"
$installer = Get-ChildItem $payload -Filter "MatteshotSetup-*.exe" | Select-Object -First 1
$app = "$env:LOCALAPPDATA\Programs\Matteshot\matteshot.exe"
$uninstaller = "$env:LOCALAPPDATA\Programs\Matteshot\unins000.exe"

function Write-Step([string]$Message) {
    $line = "[$(Get-Date -Format o)] $Message"
    $line | Tee-Object -FilePath $log -Append
}

function Invoke-Matteshot(
    [string[]]$Arguments,
    [string]$Name
) {
    $stderr = Join-Path $results "$Name.stderr.log"
    $stdout = Join-Path $results "$Name.stdout.log"
    $params = @{
        FilePath = $app
        ArgumentList = $Arguments
        RedirectStandardError = $stderr
        RedirectStandardOutput = $stdout
        PassThru = $true
        Wait = $true
    }
    $process = Start-Process @params
    if ($process.ExitCode -ne 0) {
        $errorText = if (Test-Path $stderr) { Get-Content -Raw $stderr } else { "" }
        throw "$Name failed with exit code $($process.ExitCode): $errorText"
    }
    if (Test-Path $stderr) {
        return (Get-Content -Raw $stderr).Trim()
    }
    return ""
}

# State a paid or trial install of 0.20.0 or earlier leaves behind. The free
# build must ignore all of it and must not delete any of it.
$settingsDir = Join-Path $env:APPDATA "matteshot"
$legacyLicense = Join-Path $settingsDir "license.json"
$config = Join-Path $settingsDir "config.json"
$legacyRegistry = "HKCU:\Software\Southbound Software\Matteshot"

function Initialize-LegacyState {
    New-Item -ItemType Directory -Force $settingsDir | Out-Null
    # A trial that ended 30 days ago: every capture path used to refuse here.
    $started = [DateTimeOffset]::UtcNow.AddDays(-44).ToUnixTimeSeconds()
    @{ trial_started_at = $started; last_seen_at = $started } |
        ConvertTo-Json | Set-Content -Encoding UTF8 $legacyLicense
    New-Item -Force $legacyRegistry | Out-Null
    Set-ItemProperty $legacyRegistry -Name "TrialStartedAt" -Value ([string]$started)
    # A config written by 0.20.0, including the retired telemetry keys.
    @{
        onboarded = $true
        auto_update = $false
        telemetry = $true
        telemetry_id = "8f0c1c1e-8d8a-4a53-9b53-3d9c3f3b7a10"
        capture_delay_secs = 7
    } | ConvertTo-Json | Set-Content -Encoding UTF8 $config
}

function Assert-UserStateKept([string]$When) {
    if (-not (Test-Path $legacyLicense)) {
        throw "The old license.json was deleted $When."
    }
    if (-not (Test-Path $config)) {
        throw "config.json was deleted $When."
    }
    $settings = Get-Content -Raw $config | ConvertFrom-Json
    if ($settings.capture_delay_secs -ne 7 -or $settings.auto_update -ne $false) {
        throw "Existing settings were not kept $When."
    }
}

$passed = $false
$failure = $null
try {
    if (-not $installer) {
        throw "Signed installer is missing from the sandbox payload."
    }

    Write-Step "Verifying installer signature"
    $installerSignature = Get-AuthenticodeSignature $installer.FullName
    if ($installerSignature.Status -ne "Valid") {
        throw "Installer signature is $($installerSignature.Status)."
    }
    $installerSigner = $installerSignature.SignerCertificate.GetNameInfo(
        [System.Security.Cryptography.X509Certificates.X509NameType]::SimpleName, $false)
    if ($installerSigner -cne "Brandon South") {
        throw "Unexpected installer signer."
    }

    Write-Step "Seeding state left by a paid-era install"
    Initialize-LegacyState

    Write-Step "Installing Matteshot silently"
    $install = Start-Process `
        -FilePath $installer.FullName `
        -ArgumentList '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP- /MERGETASKS="!autostart"' `
        -PassThru `
        -Wait
    if ($install.ExitCode -ne 0) {
        throw "Installer exited with code $($install.ExitCode)."
    }
    if (-not (Test-Path $app)) {
        throw "Installed app was not found."
    }

    Write-Step "Verifying installed binary signature"
    $appSignature = Get-AuthenticodeSignature $app
    if ($appSignature.Status -ne "Valid") {
        throw "Installed binary signature is $($appSignature.Status)."
    }
    $appSigner = $appSignature.SignerCertificate.GetNameInfo(
        [System.Security.Cryptography.X509Certificates.X509NameType]::SimpleName, $false)
    if ($appSigner -cne "Brandon South") {
        throw "Unexpected installed binary signer."
    }

    # Inno generates unins000.exe at install time from the payload the compiler
    # signed (SignedUninstaller). An unsigned one here means SmartScreen scares
    # customers at uninstall despite a trusted install.
    Write-Step "Verifying uninstaller signature"
    if (-not (Test-Path $uninstaller)) {
        throw "Uninstaller was not found after install."
    }
    $uninstallerSignature = Get-AuthenticodeSignature $uninstaller
    if ($uninstallerSignature.Status -ne "Valid") {
        throw "Uninstaller signature is $($uninstallerSignature.Status)."
    }
    $uninstallerSigner = $uninstallerSignature.SignerCertificate.GetNameInfo(
        [System.Security.Cryptography.X509Certificates.X509NameType]::SimpleName, $false)
    if ($uninstallerSigner -cne "Brandon South") {
        throw "Unexpected uninstaller signer."
    }

    Write-Step "Cutting Matteshot off from the network"
    # Nothing in capture, editing or the resident may need a connection. The
    # sandbox keeps its own networking so the install above and the signature
    # checks behave as they do for a real user; only matteshot.exe is blocked.
    New-NetFirewallRule `
        -DisplayName "Matteshot smoke offline" `
        -Direction Outbound `
        -Program $app `
        -Action Block | Out-Null

    Write-Step "Launching an isolated test window for capture"
    Start-Process "cmd.exe" -ArgumentList "/k title Matteshot-Smoke" | Out-Null
    $captureWindow = $null
    $deadline = (Get-Date).AddSeconds(20)
    do {
        Start-Sleep -Milliseconds 250
        $captureWindow = Get-Process -ErrorAction SilentlyContinue |
            Where-Object { $_.MainWindowTitle -like "*Matteshot-Smoke*" } |
            Select-Object -First 1
    } while (-not $captureWindow -and (Get-Date) -lt $deadline)
    if (-not $captureWindow) {
        throw "The isolated test window was not exposed."
    }

    Write-Step "Capturing the isolated test window without host clipboard access"
    Invoke-Matteshot @("--bench", "Matteshot-Smoke") "capture" | Out-Null
    $capture = Join-Path $env:TEMP "matteshot-bench.png"
    if (-not (Test-Path $capture) -or (Get-Item $capture).Length -eq 0) {
        throw "Capture output is missing or empty."
    }

    Write-Step "Starting the resident app offline"
    # A silent install relaunches the resident itself, before the firewall
    # rule existed. A second launch would only hand off to that copy and exit,
    # so stop it and start one that has never had a connection.
    Get-Process -Name "matteshot" -ErrorAction SilentlyContinue |
        Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 2
    $resident = Start-Process $app -PassThru
    Start-Sleep -Seconds 5
    $resident.Refresh()
    if ($resident.HasExited -or -not $resident.Responding) {
        throw "Resident app did not remain responsive offline."
    }
    Stop-Process -Id $resident.Id -Force
    Assert-UserStateKept "while running"

    Write-Step "Uninstalling Matteshot"
    if (-not (Test-Path $uninstaller)) {
        throw "Uninstaller was not found."
    }
    $uninstall = Start-Process `
        -FilePath $uninstaller `
        -ArgumentList "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART" `
        -PassThru `
        -Wait
    if ($uninstall.ExitCode -ne 0) {
        throw "Uninstaller exited with code $($uninstall.ExitCode)."
    }
    Start-Sleep -Seconds 2
    if (Test-Path $app) {
        throw "Installed binary remains after uninstall."
    }

    $runKey = Get-ItemProperty `
        "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" `
        -Name "Matteshot" `
        -ErrorAction SilentlyContinue
    if ($runKey) {
        throw "Autostart registry value remains after uninstall."
    }
    Assert-UserStateKept "by uninstall"

    $passed = $true
    Write-Step "PASS"
}
catch {
    $failure = $_.Exception.Message
    Write-Step "FAIL: $failure"
}
finally {
    Get-Process -Name "matteshot" -ErrorAction SilentlyContinue |
        Stop-Process -Force -ErrorAction SilentlyContinue
    Remove-NetFirewallRule -DisplayName "Matteshot smoke offline" -ErrorAction SilentlyContinue
    if ((Test-Path $app) -and (Test-Path $uninstaller)) {
        Start-Process `
            -FilePath $uninstaller `
            -ArgumentList "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART" `
            -Wait `
            -ErrorAction SilentlyContinue
    }

    # SBS-901: the host polls this file. Write temp + rename so a reader
    # never sees truncated JSON as a completed Fail.
    $resultBody = @{
        passed = $passed
        completed_at = (Get-Date).ToUniversalTime().ToString("o")
        error = $failure
    } | ConvertTo-Json
    $resultTemp = Join-Path $results "result.json.tmp"
    Set-Content -Encoding UTF8 -Path $resultTemp -Value $resultBody
    Move-Item -LiteralPath $resultTemp -Destination $result -Force

    Start-Sleep -Seconds 2
    Stop-Computer -Force
}
