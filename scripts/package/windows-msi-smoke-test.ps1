<#
.SYNOPSIS
Installs flux's Windows installer on this machine (a CI runner: it changes the
machine), and checks it. -Earlier, an installer of the same version built
separately (so Windows takes it for another one), is installed first: the
flux service runs as LocalSystem and answers on port 8096 as the version
expected, from C:\Program Files\flux, marked "msi", with the bundled FFmpeg;
its data and settings are in C:\ProgramData\flux, which only administrators
and the service can read, and which commands run in a terminal find by
themselves; port 8097 is let through the firewall. Then flux updates itself
to -Msi, as to a newer version: it finds it in a stand-in for GitHub's
releases (with SHA256SUMS), downloads it and hands it to Windows Installer,
which replaces the earlier one without a restart of Windows; flux runs again
from a new service, the settings stay as they were, and Windows lists one
flux. Uninstalling removes the program, the service and the firewall rule,
and keeps the data and settings. Needs an administrator.

.EXAMPLE
scripts/package/windows-msi-smoke-test.ps1 -Msi dist/flux-0.1.0-x86_64-pc-windows-msvc.msi -Earlier $env:RUNNER_TEMP/msi-earlier/flux-0.1.0-x86_64-pc-windows-msvc.msi -Version 0.1.0
#>
param(
    [Parameter(Mandatory)] [string] $Msi,
    [Parameter(Mandatory)] [string] $Earlier,
    [Parameter(Mandatory)] [string] $Version
)
$ErrorActionPreference = 'Stop'
$Msi = (Resolve-Path $Msi).Path
$Earlier = (Resolve-Path $Earlier).Path
$program = Join-Path $env:ProgramFiles 'flux'
$server = Join-Path $program 'flux-server.exe'
$data = Join-Path $env:ProgramData 'flux'
$settings = Join-Path $data 'flux.toml'
$rule = 'flux (LAN)'
$logs = Join-Path ([IO.Path]::GetTempPath()) 'flux-msi-logs'
New-Item -ItemType Directory -Force -Path $logs | Out-Null

function Msiexec([string] $what, [string[]] $arguments) {
    $log = Join-Path $logs "$what.log"
    $p = Start-Process -FilePath msiexec.exe -ArgumentList ($arguments + @('/qn', '/norestart', '/l*v', "`"$log`"")) -Wait -PassThru
    # 3010 would be a file in use: Windows would finish at its next restart.
    if ($p.ExitCode -ne 0) {
        Get-Content $log -Tail 80 | Write-Host
        throw "msiexec ($what) exited $($p.ExitCode)"
    }
    Write-Host "msiexec ($what): $($p.ExitCode)"
}

function Wait-Answers([string] $what, [int] $seconds = 60) {
    $deadline = (Get-Date).AddSeconds($seconds)
    while ((Get-Date) -lt $deadline) {
        try {
            $info = Invoke-RestMethod -Uri 'http://127.0.0.1:8096/api/v1/server' -TimeoutSec 3
            if ($info.version -eq $Version) { return $info }
        }
        catch { }
        Start-Sleep -Milliseconds 500
    }
    $log = Join-Path $data 'flux.log'
    if (Test-Path $log) { Get-Content $log -Tail 60 | Write-Host }
    throw "$what`: flux $Version didn't answer on 8096 within $seconds s"
}

# The flux products Windows lists (their product codes). Call it as
# @(Get-Installed): PowerShell hands back a lone code as a string.
function Get-Installed {
    @(Get-ItemProperty 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*' |
        Where-Object { $_.PSObject.Properties['DisplayName'] -and $_.DisplayName -eq 'flux' } |
        ForEach-Object { $_.PSChildName })
}

# `flux-server --check` in a terminal, with no flags: what it prints.
function Get-Check {
    $report = & $server --check
    $code = $LASTEXITCODE
    $report | Write-Host
    if ($code -ne 0) { throw "flux-server --check exited $code" }
    $report
}

Write-Host "== install"
Msiexec 'install' @('/i', "`"$Earlier`"")
$service = Get-Service flux
if ($service.Status -ne 'Running') { throw "the flux service is $($service.Status)" }
$cim = Get-CimInstance Win32_Service -Filter "Name='flux'"
Write-Host "service: $($cim.StartName), $($cim.StartMode), $($cim.PathName)"
if ($cim.StartName -ne 'LocalSystem') { throw "the service runs as $($cim.StartName)" }
if ($cim.PathName -notlike '*flux-server.exe" service run') { throw "the service runs $($cim.PathName)" }
$info = Wait-Answers 'after installing'
Write-Host "flux $($info.version) answers"
if ((Get-Content (Join-Path $program 'install-kind') -Raw).Trim() -ne 'msi') { throw 'install-kind is not msi' }
foreach ($f in 'flux.exe', 'web\index.html', 'ffmpeg\ffprobe.exe', 'ffmpeg\LICENSE.txt', 'ffmpeg\SOURCE.txt') {
    if (-not (Test-Path (Join-Path $program $f))) { throw "$f is missing from $program" }
}
$link = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'flux.url'
if (-not (Test-Path $link)) { throw "no Start menu link at $link" }
$path = [Environment]::GetEnvironmentVariable('Path', 'Machine')
if (-not ($path -split ';' | Where-Object { $_.TrimEnd('\') -eq $program })) { throw "$program isn't on the machine's PATH" }

# The data: SYSTEM and Administrators only, nothing inherited from ProgramData.
$acl = Get-Acl $data
$acl.Access | Format-Table IdentityReference, FileSystemRights, IsInherited | Out-String | Write-Host
if (-not $acl.AreAccessRulesProtected) { throw "$data inherits ProgramData's permissions" }
$who = @($acl.Access | ForEach-Object { $_.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value } | Sort-Object -Unique)
if (Compare-Object $who @('S-1-5-18', 'S-1-5-32-544')) { throw "$data is open to $($who -join ', ')" }
if (-not (Test-Path (Join-Path $data 'catalog.db'))) { throw "the service keeps no catalog in $data" }
if (-not (Test-Path $settings)) { throw "no $settings" }

# Port 8097 on private networks, for flux-server.exe only.
$fw = Get-NetFirewallRule -DisplayName $rule
$ports = $fw | Get-NetFirewallPortFilter
$app = $fw | Get-NetFirewallApplicationFilter
Write-Host "firewall: $($fw.Direction) $($fw.Action) $($fw.Profile) $($ports.Protocol) $($ports.LocalPort) $($app.Program)"
if ($fw.Direction -ne 'Inbound' -or $fw.Action -ne 'Allow' -or $fw.Profile -ne 'Private' -or $ports.LocalPort -ne '8097' -or $app.Program -ne $server) {
    throw "the firewall rule isn't flux-server.exe's port 8097 on private networks"
}

# A terminal's flux-server finds the bundled FFmpeg, and the service's data
# and settings, by itself.
$media = Join-Path ([IO.Path]::GetTempPath()) 'flux-msi-media'
New-Item -ItemType Directory -Force -Path $media | Out-Null
Add-Content -Path $settings -Value "`n[[library]]`nname = `"Smoke`"`nkind = `"videos`"`npath = '$media'"
$report = Get-Check
foreach ($want in "$program\ffmpeg\ffmpeg.exe", "data     $data", "library  Smoke (videos): $media") {
    if (-not ($report -match [regex]::Escape($want))) { throw "flux-server --check doesn't say: $want" }
}

Write-Host "== flux updates itself to the installer under test"
$before = @(Get-Installed)
if ($before.Count -ne 1) { throw "Windows lists $($before.Count) flux installs" }
# The release, as GitHub's API lists it, its installer and SHA256SUMS.
$site = Join-Path ([IO.Path]::GetTempPath()) 'flux-msi-release'
New-Item -ItemType Directory -Force -Path $site | Out-Null
$name = Split-Path $Msi -Leaf
Copy-Item $Msi (Join-Path $site $name)
$sums = Join-Path $site 'SHA256SUMS'
[IO.File]::WriteAllText($sums, "$((Get-FileHash -Algorithm SHA256 $Msi).Hash.ToLowerInvariant())  $name`n")
$base = 'http://127.0.0.1:18099'
$release = @(@{
        tag_name     = "v$Version"
        draft        = $false
        prerelease   = $true
        html_url     = "$base/release"
        published_at = '2026-01-01T00:00:00Z'
        body         = '* the smoke test''s release'
        assets       = @(
            @{ name = $name; browser_download_url = "$base/$name"; size = (Get-Item $Msi).Length },
            @{ name = 'SHA256SUMS'; browser_download_url = "$base/SHA256SUMS"; size = (Get-Item $sums).Length }
        )
    })
[IO.File]::WriteAllText((Join-Path $site 'releases.json'), (ConvertTo-Json -InputObject $release -Depth 5))
$web = Start-Process -FilePath python -ArgumentList '-m', 'http.server', '18099', '--bind', '127.0.0.1', '--directory', "`"$site`"" -PassThru -WindowStyle Hidden
try {
    $deadline = (Get-Date).AddSeconds(30)
    while ($true) {
        try { Invoke-WebRequest -Uri "$base/releases.json" -TimeoutSec 3 | Out-Null; break }
        catch { if ((Get-Date) -gt $deadline) { throw 'the stand-in for GitHub never answered' } }
        Start-Sleep -Milliseconds 500
    }
    # The service reads it there, and takes itself for an older flux.
    $key = 'HKLM:\SYSTEM\CurrentControlSet\Services\flux'
    New-ItemProperty -Path $key -Name Environment -PropertyType MultiString -Force -Value @(
        "FLUX_UPDATE_SOURCE=$base/releases.json", 'FLUX_UPDATE_CURRENT=0.0.1-alpha.1') | Out-Null
    Restart-Service flux
    Wait-Answers 'before updating' | Out-Null
    $request = @{ 'X-Flux-Request' = '1' }
    $u = Invoke-RestMethod -Method Post -Uri 'http://127.0.0.1:8096/api/v1/admin/update/apply' -Headers $request
    Write-Host "update: $($u.phase), from $($u.current)"
    $deadline = (Get-Date).AddMinutes(10)
    do {
        Start-Sleep -Seconds 3
        $now = @(Get-Installed)
        if ((Get-Date) -gt $deadline) {
            $msiLog = Join-Path $data "updates\msi-$Version.log"
            if (Test-Path $msiLog) { Get-Content $msiLog -Tail 80 | Write-Host }
            Get-Content (Join-Path $data 'flux.log') -Tail 60 | Write-Host
            throw 'flux did not update itself within 10 minutes'
        }
    } until ($now.Count -eq 1 -and $now[0] -ne $before[0])
    Write-Host "installed: $($now[0]), was $($before[0])"
}
finally {
    Stop-Process -Id $web.Id -Force -ErrorAction SilentlyContinue
}
Wait-Answers 'after updating itself' | Out-Null
$msiLog = Join-Path $data "updates\msi-$Version.log"
if (-not (Select-String -Path $msiLog -SimpleMatch 'Installation success or error status: 0' -Quiet)) {
    Get-Content $msiLog -Tail 80 | Write-Host
    throw "Windows Installer didn't finish cleanly: $msiLog"
}
$deadline = (Get-Date).AddSeconds(30)
do {
    Start-Sleep -Seconds 1
    $u = Invoke-RestMethod -Uri 'http://127.0.0.1:8096/api/v1/admin/update' -Headers $request
} until ($u.phase -eq 'idle' -or (Get-Date) -gt $deadline)
Write-Host "flux $($u.current) answers, $($u.phase)"
# A new service: no stand-in, no pretending.
if ($u.current -ne $Version -or $u.phase -ne 'idle' -or $u.error) { throw "after the update: $($u.current), $($u.phase), $($u.error)" }
if (-not (Select-String -Path $settings -SimpleMatch 'name = "Smoke"' -Quiet)) { throw "the update replaced $settings" }

Write-Host "== uninstall"
Msiexec 'uninstall' @('/x', "`"$Msi`"")
if (Get-Service flux -ErrorAction SilentlyContinue) { throw 'the flux service is still there' }
if (Test-Path $server) { throw "$server is still there" }
if (Get-NetFirewallRule -DisplayName $rule -ErrorAction SilentlyContinue) { throw "the firewall rule '$rule' is still there" }
if (-not (Test-Path (Join-Path $data 'catalog.db'))) { throw "the data in $data didn't stay" }
if (-not (Select-String -Path $settings -SimpleMatch 'name = "Smoke"' -Quiet)) { throw "uninstalling removed $settings" }
Write-Host "The installer installs flux $Version, flux updates itself with it, and it uninstalls."
