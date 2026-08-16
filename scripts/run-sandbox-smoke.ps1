param(
    [Parameter(Mandatory = $true)]
    [string]$InstallerPath,

    [Parameter(Mandatory = $true)]
    [string]$LicenseKeyPath,

    [Parameter(Mandatory = $true)]
    [string]$WorkDirectory,

    # How long the host waits for the guest result.json (SBS-901).
    # Default 20 minutes covers install + capture + license + uninstall.
    [ValidateRange(1, [int]::MaxValue)]
    [int]$TimeoutSeconds = 1200
)

$ErrorActionPreference = "Stop"

try {
    . (Join-Path $PSScriptRoot "sandbox-smoke-result.ps1")
} catch {
    Write-Host "FAIL: sandbox smoke helper could not be loaded: $($_.Exception.Message)"
    exit 1
}

$sandbox = (Get-Command "WindowsSandbox.exe" -ErrorAction Stop).Source
$installer = (Resolve-Path $InstallerPath).Path
$licenseKey = (Resolve-Path $LicenseKeyPath).Path
$work = [System.IO.Path]::GetFullPath($WorkDirectory)
$payload = Join-Path $work "payload"
$results = Join-Path $work "results"

New-Item -ItemType Directory -Force $payload, $results | Out-Null
Clear-SandboxSmokePriorResult -ResultsDirectory $results
Copy-Item $installer (Join-Path $payload (Split-Path $installer -Leaf))
Copy-Item $licenseKey (Join-Path $payload "license-key.txt")
Copy-Item `
    (Join-Path $PSScriptRoot "sandbox-smoke-guest.ps1") `
    (Join-Path $payload "sandbox-smoke-guest.ps1")

$payloadXml = [System.Security.SecurityElement]::Escape($payload)
$resultsXml = [System.Security.SecurityElement]::Escape($results)
$configuration = @"
<Configuration>
  <VGpu>Disable</VGpu>
  <Networking>Enable</Networking>
  <ClipboardRedirection>Disable</ClipboardRedirection>
  <PrinterRedirection>Disable</PrinterRedirection>
  <MappedFolders>
    <MappedFolder>
      <HostFolder>$payloadXml</HostFolder>
      <SandboxFolder>C:\MatteshotSmoke</SandboxFolder>
      <ReadOnly>true</ReadOnly>
    </MappedFolder>
    <MappedFolder>
      <HostFolder>$resultsXml</HostFolder>
      <SandboxFolder>C:\MatteshotResults</SandboxFolder>
      <ReadOnly>false</ReadOnly>
    </MappedFolder>
  </MappedFolders>
  <LogonCommand>
    <Command>powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\MatteshotSmoke\sandbox-smoke-guest.ps1</Command>
  </LogonCommand>
</Configuration>
"@
$configPath = Join-Path $work "matteshot-smoke.wsb"
$configuration | Set-Content -Encoding UTF8 $configPath

Write-Host "Starting isolated Matteshot acceptance test."
try {
    $sandboxProcess = Start-Process `
        -FilePath $sandbox `
        -ArgumentList "`"$configPath`"" `
        -PassThru
} catch {
    Write-Host "FAIL: Windows Sandbox could not be started: $($_.Exception.Message)"
    exit 1
}
if (-not $sandboxProcess) {
    Write-Host "FAIL: Windows Sandbox could not be started."
    exit 1
}

Write-Host "Waiting up to $TimeoutSeconds seconds for guest result.json under $results"
$outcome = Wait-SandboxSmokeResult `
    -ResultsDirectory $results `
    -TimeoutSeconds $TimeoutSeconds `
    -SandboxProcess $sandboxProcess `
    -StopLeftoverSandbox
Write-Host $outcome.Message
exit $outcome.ExitCode
