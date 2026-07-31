param(
    [Parameter(Mandatory = $true)]
    [string]$InstallerPath,

    [Parameter(Mandatory = $true)]
    [string]$LicenseKeyPath,

    [Parameter(Mandatory = $true)]
    [string]$WorkDirectory
)

$ErrorActionPreference = "Stop"

$sandbox = (Get-Command "WindowsSandbox.exe" -ErrorAction Stop).Source
$installer = (Resolve-Path $InstallerPath).Path
$licenseKey = (Resolve-Path $LicenseKeyPath).Path
$work = [System.IO.Path]::GetFullPath($WorkDirectory)
$payload = Join-Path $work "payload"
$results = Join-Path $work "results"

New-Item -ItemType Directory -Force $payload, $results | Out-Null
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
Start-Process -FilePath $sandbox -ArgumentList "`"$configPath`"" | Out-Null
Write-Host "Results will be written to $results"
