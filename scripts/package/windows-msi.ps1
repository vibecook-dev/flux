<#
.SYNOPSIS
Builds flux's Windows installer, flux-<version>-x86_64-pc-windows-msvc.msi,
with WiX 7 (scripts/package/windows/flux.wxs), in two steps so that the files
can be signed in between (build.yml):

  -Step prepare   unpacks the release zip (package.sh's) into -Stage, marks it
                  "msi", adds the icon and the service's flux.toml, and the
                  FFmpeg that ffmpeg-windows.env pins (downloaded and checked
                  against its SHA-256) in ffmpeg\, with its licence and where
                  its source is;
  -Step build     builds the installer from -Stage into -Out.

.EXAMPLE
scripts/package/windows-msi.ps1 -Step prepare -Zip dist/flux-0.1.0-x86_64-pc-windows-msvc.zip -Version 0.1.0 -Stage $env:RUNNER_TEMP/msi
scripts/package/windows-msi.ps1 -Step build -Version 0.1.0 -Stage $env:RUNNER_TEMP/msi -Out dist
#>
param(
    [Parameter(Mandatory)] [ValidateSet('prepare', 'build')] [string] $Step,
    [Parameter(Mandatory)] [string] $Version,
    [Parameter(Mandatory)] [string] $Stage,
    [string] $Zip = '',
    [string] $Out = 'dist'
)
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot

function Read-Pin {
    $pin = @{}
    foreach ($line in Get-Content (Join-Path $here 'ffmpeg-windows.env')) {
        if ($line -match '^\s*([A-Z0-9_]+)=(.*)$') { $pin[$Matches[1]] = $Matches[2].Trim() }
    }
    $pin
}

if ($Step -eq 'prepare') {
    if (-not (Test-Path $Zip)) { throw "-Zip: no such file: $Zip" }
    $work = Join-Path ([IO.Path]::GetTempPath()) ("flux-msi-" + [Guid]::NewGuid())
    New-Item -ItemType Directory -Path $work | Out-Null
    try {
        Expand-Archive -Path $Zip -DestinationPath (Join-Path $work 'zip')
        $top = @(Get-ChildItem (Join-Path $work 'zip') -Directory)
        if ($top.Count -ne 1) { throw "$Zip should hold one folder" }
        if (Test-Path $Stage) { Remove-Item -Recurse -Force $Stage }
        New-Item -ItemType Directory -Path $Stage | Out-Null
        Copy-Item -Recurse -Path (Join-Path $top[0].FullName '*') -Destination $Stage
        foreach ($f in 'flux-server.exe', 'flux.exe', 'LICENSE.txt', 'README.txt', 'THIRD_PARTY_LICENSES.txt', 'web\index.html') {
            if (-not (Test-Path (Join-Path $Stage $f))) { throw "$Zip has no $f" }
        }
        # How this flux updates: with the next installer, not in place.
        [IO.File]::WriteAllText((Join-Path $Stage 'install-kind'), "msi`n")
        Copy-Item (Join-Path $here 'windows\flux.ico') (Join-Path $Stage 'flux.ico')
        # The service's settings, C:\ProgramData\flux\flux.toml.
        Copy-Item (Join-Path $here 'windows\flux.toml') (Join-Path $Stage 'flux.toml')

        $pin = Read-Pin
        $download = Join-Path $work $pin.BTBN_ASSET
        $url = "https://github.com/BtbN/FFmpeg-Builds/releases/download/$($pin.BTBN_TAG)/$($pin.BTBN_ASSET)"
        Write-Host "FFmpeg: $url"
        Invoke-WebRequest -Uri $url -OutFile $download -UseBasicParsing
        $sha = (Get-FileHash -Algorithm SHA256 $download).Hash.ToLowerInvariant()
        if ($sha -ne $pin.BTBN_SHA256) { throw "$($pin.BTBN_ASSET): SHA-256 $sha, not $($pin.BTBN_SHA256)" }
        Expand-Archive -Path $download -DestinationPath (Join-Path $work 'ffmpeg')
        $root = @(Get-ChildItem (Join-Path $work 'ffmpeg') -Directory)[0].FullName
        $ffmpeg = New-Item -ItemType Directory -Path (Join-Path $Stage 'ffmpeg')
        Copy-Item (Join-Path $root 'bin\ffmpeg.exe'), (Join-Path $root 'bin\ffprobe.exe') $ffmpeg
        Copy-Item (Join-Path $root 'bin\*.dll') $ffmpeg
        Copy-Item (Join-Path $root 'LICENSE.txt') (Join-Path $ffmpeg 'LICENSE.txt')
        $source = "ffmpeg-$($pin.FFMPEG_VERSION)-windows-source.tar"
        $readme = @"
FFmpeg $($pin.FFMPEG_VERSION), built by BtbN/FFmpeg-Builds
($($pin.BTBN_TAG), $($pin.BTBN_ASSET)), licensed under the GNU General
Public License version 3: LICENSE.txt.

Its complete source, with every library compiled into it and the scripts that
built it, is attached to the flux release this installer comes from, as
$source (and $($source -replace '\.tar$', '-2.tar') and on, in parts
under GitHub's 2 GiB limit for a file):
https://github.com/vibecook-dev/flux/releases/tag/v$Version

flux runs ffmpeg.exe and ffprobe.exe as separate programs and links none of
FFmpeg's libraries.
"@
        [IO.File]::WriteAllText((Join-Path $ffmpeg 'SOURCE.txt'), $readme.Replace("`r`n", "`n").Replace("`n", "`r`n"))
        Get-ChildItem $ffmpeg | Format-Table Name, Length
    }
    finally {
        Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
    }
    exit 0
}

# build
if (-not (Test-Path (Join-Path $Stage 'install-kind'))) { throw "-Stage $Stage isn't prepared (run -Step prepare first)" }
New-Item -ItemType Directory -Force -Path $Out | Out-Null
# Windows Installer compares three numeric fields; a pre-release's number goes
# in the fourth, which it shows but doesn't compare (the .wxs allows the same
# version to replace itself).
$release, $pre = $Version -split '-', 2
if ($release -notmatch '^\d+\.\d+\.\d+$') { throw "-Version $Version isn't a release version" }
$build = if ($pre -and $pre -match '(\d+)$') { $Matches[1] } else { '0' }
$productVersion = "$release.$build"
$hasSidecar = if (Test-Path (Join-Path $Stage 'sidecar-slim.exe')) { '1' } else { '0' }
$msi = Join-Path $Out "flux-$Version-x86_64-pc-windows-msvc.msi"
& wix build (Join-Path $here 'windows\flux.wxs') -arch x64 -ext WixToolset.Util.wixext -ext WixToolset.Firewall.wixext `
    -d "ProductVersion=$productVersion" -d "FluxVersion=$Version" -d "HasSidecar=$hasSidecar" `
    -bindpath "pkg=$Stage" -o $msi
if ($LASTEXITCODE -ne 0) { throw "wix build failed ($LASTEXITCODE)" }
Get-Item $msi | Format-Table Name, Length
