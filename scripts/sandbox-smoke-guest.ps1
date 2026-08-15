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

function Remove-TestActivation {
    $licenseStatePath = Join-Path $env:APPDATA "matteshot\license.json"
    if (-not (Test-Path $licenseStatePath)) {
        return $false
    }

    $licenseState = Get-Content -Raw $licenseStatePath | ConvertFrom-Json
    # Current builds store the refresh token as a DPAPI current-user blob;
    # the plaintext field only exists in pre-migration state. The sandbox
    # user is the one who activated, so it can unprotect its own blob.
    $storedToken = $licenseState.license.refresh_token
    if (-not $storedToken -and $licenseState.license.refresh_token_protected) {
        Add-Type -AssemblyName System.Security
        $protectedBytes = [Convert]::FromBase64String($licenseState.license.refresh_token_protected)
        $tokenBytes = [System.Security.Cryptography.ProtectedData]::Unprotect(
            $protectedBytes,
            $null,
            [System.Security.Cryptography.DataProtectionScope]::CurrentUser
        )
        $storedToken = [Text.Encoding]::UTF8.GetString($tokenBytes)
    }
    if (-not $storedToken -or -not $licenseState.license.certificate) {
        return $false
    }

    $certificateBody = $licenseState.license.certificate.Replace("-", "+").Replace("_", "/")
    switch ($certificateBody.Length % 4) {
        2 { $certificateBody += "==" }
        3 { $certificateBody += "=" }
    }
    $certificateJson = [Text.Encoding]::UTF8.GetString(
        [Convert]::FromBase64String($certificateBody)
    )
    $certificate = $certificateJson | ConvertFrom-Json
    $deactivationBody = @{
        refresh_token = $storedToken
        device_id = $certificate.device_id
    } | ConvertTo-Json
    $deactivation = Invoke-RestMethod `
        -Method Post `
        -Uri "https://license.matteshot.app/v1/license/deactivate" `
        -ContentType "application/json" `
        -Body $deactivationBody
    if (-not $deactivation.deactivated) {
        throw "License service did not release the sandbox activation."
    }
    Remove-Item $licenseStatePath -Force
    return $true
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

    Write-Step "Checking untouched trial state"
    $initialStatus = Invoke-Matteshot @("--license-status") "license-before"
    if ($initialStatus -notmatch "14-day trial ready") {
        throw "Unexpected initial license state: $initialStatus"
    }

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

    Write-Step "Checking that the first capture started the trial"
    $trialStatus = Invoke-Matteshot @("--license-status") "license-trial"
    if ($trialStatus -notmatch "^Trial: 14 days left") {
        throw "Unexpected trial state after capture: $trialStatus"
    }

    $licenseKey = Join-Path $payload "license-key.txt"
    if (-not (Test-Path $licenseKey)) {
        throw "Test license key is missing from the sandbox payload."
    }

    Write-Step "Activating the test license"
    $activationInfo = New-Object System.Diagnostics.ProcessStartInfo
    $activationInfo.FileName = $app
    $activationInfo.Arguments = "--activate-stdin"
    $activationInfo.UseShellExecute = $false
    $activationInfo.CreateNoWindow = $true
    $activationInfo.RedirectStandardInput = $true
    $activationInfo.RedirectStandardOutput = $true
    $activationInfo.RedirectStandardError = $true
    $activationProcess = New-Object System.Diagnostics.Process
    $activationProcess.StartInfo = $activationInfo
    $activationProcess.Start() | Out-Null
    $activationProcess.StandardInput.Write((Get-Content -Raw $licenseKey))
    $activationProcess.StandardInput.Close()
    $activationStatus = $activationProcess.StandardError.ReadToEnd().Trim()
    $activationProcess.StandardOutput.ReadToEnd() | Out-Null
    $activationProcess.WaitForExit()
    $activationExitCode = $activationProcess.ExitCode
    if ($activationExitCode -ne 0) {
        throw "license-activate failed with exit code ${activationExitCode}: $activationStatus"
    }
    if ($activationStatus -notmatch "^Licensed") {
        throw "Activation did not produce a licensed state."
    }
    $licensedStatus = Invoke-Matteshot @("--license-status") "license-licensed"
    if ($licensedStatus -notmatch "^Licensed") {
        throw "License did not survive a new process."
    }

    Write-Step "Restarting the resident app"
    $resident = Start-Process $app -PassThru
    Start-Sleep -Seconds 3
    $resident.Refresh()
    if ($resident.HasExited -or -not $resident.Responding) {
        throw "Resident app did not remain responsive."
    }
    Stop-Process -Id $resident.Id -Force

    Write-Step "Releasing the sandbox activation"
    if (-not (Remove-TestActivation)) {
        throw "Activated license state was not stored."
    }
    $deactivatedStatus = Invoke-Matteshot @("--license-status") "license-deactivated"
    if ($deactivatedStatus -notmatch "^Trial ended") {
        throw "Paid activation was not released cleanly."
    }

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
    try {
        if (Remove-TestActivation) {
            Write-Step "Released a remaining sandbox activation during cleanup"
        }
    }
    catch {
        Write-Step "Cleanup warning: sandbox activation could not be released"
    }
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
