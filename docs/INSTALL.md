# Installing flux

flux is one program, `flux-server`, that serves its own web client. It runs
`ffmpeg` and `ffprobe`, which you install separately unless the way you
install flux brings them (see [ffmpeg](#ffmpeg)). The ways to install it:

- [Homebrew](#homebrew-macos-and-linux), on macOS or Linux: `brew install
  vibecook-dev/tap/flux`, ffmpeg included;
- [a .deb or .rpm](#debian-ubuntu-fedora-and-rhel) for Debian and Ubuntu,
  or Fedora and RHEL, which runs flux as a system service;
- [the Windows installer](#the-windows-installer), an `.msi` that runs flux
  as a Windows service, FFmpeg included;
- [a release package](#release-packages) (a tarball) for macOS or Linux, or
  a zip for [Windows](#windows);
- [the Docker image](#docker), which brings its own ffmpeg;
- [building from source](#building-from-source).

Releases are on <https://github.com/vibecook-dev/flux/releases>. Until the
first stable release they are pre-releases (`0.1.0-alpha.N`); expect
changes between them. How releases are made is in [RELEASING.md](RELEASING.md).

## Requirements

<!-- The macOS versions here are checked: scripts/package/smoke-test.sh fails a package whose flux-server needs more than MACOS_FLUX_SERVER (11.0) or whose sidecar-slim needs more than MACOS_SIDECAR (13.0), and RELEASING.md's target table repeats them. Change all three together. -->

| | |
|---|---|
| macOS | 11 or later, Apple Silicon (`aarch64-apple-darwin`) or Intel (`x86_64-apple-darwin`). Embedded remote access (`--tailnet embedded`) runs the bundled `bin/sidecar-slim`, which needs macOS 13 or later. |
| Linux | x86_64 or aarch64 with glibc 2.35 or later: Debian 12+, Ubuntu 22.04+, Fedora 36+, Raspberry Pi OS (64-bit, bookworm or later), and others. Kernels with 4, 16 or 64 KiB pages all work. |
| Windows | 64-bit Windows 10 or 11, or Windows Server 2016 or later (`x86_64-pc-windows-msvc`); tested on Windows 11 and, in CI, Windows Server 2025. Windows on Arm has no package. See [Windows](#windows). |
| ffmpeg | `ffmpeg` and `ffprobe`, any recent version. See [ffmpeg](#ffmpeg). |

## ffmpeg

flux runs `ffmpeg` and `ffprobe` as separate programs, for transcodes, frame
posters and probe fallbacks. It never links FFmpeg's libraries, so any
ffmpeg build works, including GPL builds, and flux stays MIT-licensed.
Direct play and remuxing (most playback) don't use ffmpeg at all.

flux looks for them first in an `ffmpeg` folder next to `flux-server`
(where the [Windows installer](#the-windows-installer) puts its own), then
on `PATH`, then in `/usr/lib/jellyfin-ffmpeg`, `/opt/homebrew/bin`,
`/usr/local/bin` and `/usr/bin`. On Windows it looks only in that `ffmpeg`
folder and on `PATH`. (When neither has them, Windows' own program search
also finds an `ffmpeg.exe` and `ffprobe.exe` placed right next to
`flux-server.exe`, but `--check` then shows them without a folder; prefer
the `ffmpeg` folder, `PATH` or `--ffmpeg`.) `--ffmpeg PATH` (or
`FLUX_FFMPEG`) picks another, and `ffprobe` defaults to the one next to
`ffmpeg` (`--ffprobe`, `FLUX_FFPROBE`).

At startup flux asks ffmpeg which H.264 encoders it has and picks the best
one for video transcodes:

| Platform | Hardware encoders | Software fallback |
|---|---|---|
| macOS | VideoToolbox (`h264_videotoolbox`) | `libx264` |
| Linux, NVIDIA | NVENC (`h264_nvenc`) | `libx264` |
| Linux, Intel | Quick Sync (`h264_qsv`) or VAAPI (`h264_vaapi`) | `libx264` |
| Linux, AMD | VAAPI (`h264_vaapi`) | `libx264` |
| Windows, NVIDIA | NVENC (`h264_nvenc`) | `libx264` |
| Windows, Intel | Quick Sync (`h264_qsv`), detected but not yet tested on Windows | `libx264` |
| Windows, AMD | none: AMD's AMF (`h264_amf`) isn't used | `libx264` |

Where to get ffmpeg:

- **macOS:** `brew install ffmpeg` (has VideoToolbox and libx264).
- **Debian, Ubuntu, Raspberry Pi OS:** `sudo apt install ffmpeg` (has libx264
  and VAAPI). For Jellyfin's tuned build, install
  [jellyfin-ffmpeg](https://github.com/jellyfin/jellyfin-ffmpeg/releases)
  and pass `--ffmpeg /usr/lib/jellyfin-ffmpeg/ffmpeg`.
- **Fedora:** the `ffmpeg` package from RPM Fusion (Fedora's own
  `ffmpeg-free` has no libx264).
- **Windows:** the [installer](#the-windows-installer) brings its own. For
  the zip, Jellyfin's portable build,
  `jellyfin-ffmpeg_<version>_portable_win64-clang-gpl.zip` from
  [jellyfin-ffmpeg's releases](https://github.com/jellyfin/jellyfin-ffmpeg/releases)
  (has NVENC, Quick Sync, libx264 and `tonemapx`); see [Install](#install).
- **Docker:** the image includes jellyfin-ffmpeg.

To check what flux found and what it will use:

```sh
flux-server --check
```

It prints the ffmpeg and ffprobe it resolved, their versions and the
encoders it detected, and exits non-zero if something required is missing.

## Homebrew (macOS and Linux)

```sh
brew install vibecook-dev/tap/flux
brew services start vibecook-dev/tap/flux     # now, and at every login
```

Then open <http://localhost:8096> and follow *Set up flux*. Homebrew
installs ffmpeg with it, and `brew services` runs flux as your account, so
it reads your folders and drives; its log is `$(brew --prefix)/var/log/flux.log`.
Update with `brew upgrade vibecook-dev/tap/flux` (the Libraries screen says
so when a new flux is out); `brew services restart vibecook-dev/tap/flux`
then runs the new version.

Use the full name, `vibecook-dev/tap/flux`: Homebrew's own `flux` is
another program (InfluxData's), as is fluxcd's, and neither can be
installed beside this one, since each installs a `flux` command. Homebrew's
ffmpeg is a slim build without libass, so flux can't burn ASS subtitles
into a transcode with it; `brew install ffmpeg-full` and `--ffmpeg
"$(brew --prefix ffmpeg-full)/bin/ffmpeg"` can.

## Debian, Ubuntu, Fedora and RHEL

Each release has a `.deb` (Debian 12 and newer, Ubuntu 22.04 and newer) and
an `.rpm` (Fedora, RHEL and AlmaLinux 10), for x86_64 and arm64, from the
[releases page](https://github.com/vibecook-dev/flux/releases):

<!-- x-release-please-start-version -->
```sh
# Debian, Ubuntu
curl -fLO https://github.com/vibecook-dev/flux/releases/download/v0.1.0-alpha.4/flux-0.1.0-alpha.4-amd64.deb
sudo apt install ./flux-0.1.0-alpha.4-amd64.deb
# Fedora, RHEL
sudo dnf install https://github.com/vibecook-dev/flux/releases/download/v0.1.0-alpha.4/flux-0.1.0-alpha.4-x86_64.rpm
sudo systemctl enable --now flux
```
<!-- x-release-please-end -->

(`arm64.deb` and `aarch64.rpm` on arm64.) flux then runs as a system
service, as an account of its own, `flux`: its settings are in
`/etc/flux/flux.toml`, its data in `/var/lib/flux`, and its log in
`journalctl -u flux`. Debian and Ubuntu start it right away; Fedora and
RHEL leave a new service off until it is enabled, as above.

- **Other devices.** Out of the box flux listens on this machine only.
  Uncomment `lan = "0.0.0.0:8097"` in `/etc/flux/flux.toml` and `sudo
  systemctl restart flux`, then open the [owner link](#the-owner-link) from
  the log (`journalctl -u flux | grep own#`) on the device you set flux up
  from, or make one with `sudo -u flux flux-server owner-link --config
  /etc/flux/flux.toml`.
- **Your media.** The `flux` account needs to read the library folders:
  `sudo setfacl -R -m u:flux:rX /srv/media` (or group permissions). The
  package adds it to the `render` and `video` groups for hardware
  transcoding.
- **ffmpeg.** The package recommends `ffmpeg`, which Debian and Ubuntu
  install with it. Fedora's own `ffmpeg-free` can't decode H.264 or HEVC;
  install RPM Fusion's ffmpeg instead (`sudo dnf swap ffmpeg-free ffmpeg
  --allowerasing`, with [RPM Fusion](https://rpmfusion.org) enabled).
- **Updating.** Install the new release's package the same way; the
  service restarts as the new version. The Libraries screen shows the
  command when a new flux is out.
- **Removing.** `sudo apt remove flux` (or `sudo dnf remove flux`) keeps
  `/etc/flux` and `/var/lib/flux`; `sudo apt purge flux` removes them too.

## Release packages

Each release has a package per platform, named
`flux-<version>-<target>.tar.gz` (for Windows, a `.zip` laid out
differently: see [Windows](#windows)):

```
flux-<version>-<target>/
  bin/flux-server
  bin/flux                  the command line, for you and your agents (docs/AGENT-NATIVE.md)
  bin/sidecar-slim          Truffle's Tailscale sidecar (when the release bundles it)
  share/flux/web/           the web client
  LICENSE  README.md  THIRD_PARTY_LICENSES
```

`flux-server` finds the web client at `../share/flux/web` from its own
location and the sidecar next to itself, so keep the layout: unpack it
anywhere and run `bin/flux-server`, or put the directory in `/opt` and link
the binary onto your `PATH`.

On macOS the binaries are not notarized yet. Downloads made with `curl`, as
below, are not quarantined. If you download the tarball with a browser
instead, clear its quarantine flag before you unpack it
(`xattr -d com.apple.quarantine flux-<version>-<target>.tar.gz`), or clear
it from the installed copy afterwards
(`sudo xattr -dr com.apple.quarantine /opt/flux`).

To install a release under `/opt/flux` (to upgrade an existing install, see
[Upgrading](#upgrading)):

<!-- x-release-please-start-version -->
```sh
version=0.1.0-alpha.4
target=aarch64-apple-darwin      # or x86_64-apple-darwin, x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu
base=https://github.com/vibecook-dev/flux/releases/download/v$version
curl -fLO "$base/flux-$version-$target.tar.gz"
curl -fLO "$base/SHA256SUMS"
shasum -a 256 -c --ignore-missing SHA256SUMS      # Linux: sha256sum -c --ignore-missing SHA256SUMS
tar -xzf "flux-$version-$target.tar.gz"
sudo mv "flux-$version-$target" /opt/flux
sudo ln -sf /opt/flux/bin/flux-server /usr/local/bin/flux-server
sudo ln -sf /opt/flux/bin/flux /usr/local/bin/flux
flux-server --check
```
<!-- x-release-please-end -->

`flux` is the command line agents use (docs/AGENT-NATIVE.md). If another
`flux` is already on your `PATH` (FluxCD's, for Kubernetes), link this one
under another name, such as `flux-media`.

Every package also has a build-provenance attestation. This checks that the
package was built by this repository's release workflow from the tagged
commit:

```sh
gh attestation verify "flux-$version-$target.tar.gz" --repo vibecook-dev/flux \
  --signer-workflow vibecook-dev/flux/.github/workflows/release.yml \
  --source-digest "$(gh api "repos/vibecook-dev/flux/commits/v$version" --jq .sha)"
```

With `--repo` alone, `gh` checks only that the attestation comes from this
repository. Check the commit with `--source-digest`, not `--source-ref`:
Release Please runs the release workflow from the push to `main` that
merges the release PR, so the attestation records `refs/heads/main`, not the
tag. `SHA256SUMS` and the jellyfin-ffmpeg source tarball have no
attestation.

### Running it

```sh
flux-server
```

Open <http://127.0.0.1:8096> and follow *Set up flux* ([Setting flux
up](#setting-flux-up)). Or name the libraries when you start flux, and it
creates them on first run:

```sh
flux-server --library Movies:movies:/path/to/Movies --library TV:shows:/path/to/TV
```

The catalog, artwork and caches live in `~/Library/Application Support/flux` (macOS) or
`~/.local/share/flux` (Linux) unless you pass `--data-dir`; the first log
line and `flux-server --check` name the folder. While flux is stopped, the
folder can be moved or copied as a whole (point `--data-dir` at the new
place); its artwork moves with it. Every flag, its
environment variable and the TOML config file are in the
[README](../README.md#quickstart-build-from-source) and `flux-server --help`.

flux is private by default: it listens on 127.0.0.1 only, where callers are
the Owner. `--lan 0.0.0.0:8097` adds a LAN listener whose callers are
anonymous (they can browse and play, but not add libraries, scan or change
remote access). Remote access with identities goes through Tailscale
(`--tailnet system|embedded`, README §Remote access), and can add HTTPS with
the tailnet's certificate (`--tailnet-https`, README §HTTPS).

### Starting with your Mac or PC

`flux-server service install` makes flux start by itself, and come back if
it stops on its own:

```sh
flux-server service install      # with --config or --data-dir, if you use them
flux-server service status
```

- **macOS:** a LaunchAgent for your account
  (`~/Library/LaunchAgents/dev.vibecook.flux.plist`). flux starts when you
  log in and reads your folders and drives as it does in a terminal; the
  first time it reads an external or network volume, macOS asks you to
  allow it. Its log is `~/Library/Logs/flux/flux.log`.
- **Linux:** a systemd user unit (`~/.config/systemd/user/flux.service`),
  started when you log in; `loginctl enable-linger` starts it at boot
  instead. Its log: `journalctl --user -u flux`.

The service runs flux with the `--config`, `--data-dir`, `--port` and
`--bind` you give `service install`; anything else goes in the config
file. `service stop` stops it until the next login (or `service start`),
and `service uninstall` removes it; flux's data stays. After an update,
flux starts the new version in its own place, so the service carries on
with it.

For a server where flux runs under an account of its own from boot, use a
system unit instead:

### As a system service (Linux, systemd)

The unit below takes its settings from `/etc/flux.toml`, so create that
file first: without it `flux-server` exits with status 2 and the service
doesn't start. On a headless server, list the libraries in the file,
because LAN callers can't add them; each one is created at startup when no
library has its name yet. `lan` makes flux reachable from other machines:

```sh
sudo tee /etc/flux.toml >/dev/null <<'EOF'
lan = "0.0.0.0:8097"                    # leave out to listen on 127.0.0.1 only
# allow_host = ["media.example.com"]    # names other than this machine's own
# localhost_role = "none"               # if other people can log in here

[[library]]
name = "Movies"
kind = "movies"                         # movies | shows | mixed | videos
path = "/srv/media/movies"

[[library]]
name = "TV"
kind = "shows"
path = "/srv/media/tv"
EOF
flux-server --config /etc/flux.toml --check
```

The keys are the long flag names with `_` (see the
[README](../README.md#quickstart-build-from-source)), and top-level keys go
before the first `[[library]]` table. An unknown key or a missing file stops
`flux-server` with exit status 2; `--check` reads the file and shows the
error. flux already answers to this machine's host name, `<hostname>.local`
and its addresses; `allow_host` adds other names, such as a reverse proxy's.
The `flux` user needs read access to the library folders.

A folder can be in only one library. When a `--library` flag or
`[[library]]` entry creates a library whose folder another library already
has, or one inside or around it (`/media` beside `/media/kids`), flux-server
stops with exit status 1 and says which library has it. Setting up and the
Libraries screen refuse such a folder too. Earlier alphas allowed overlapping libraries;
libraries that already exist in a catalog are left as they are.

```ini
# /etc/systemd/system/flux.service
[Unit]
Description=flux media server
After=network-online.target
Wants=network-online.target

[Service]
User=flux
Group=flux
ExecStart=/opt/flux/bin/flux-server --config /etc/flux.toml
Environment=FLUX_DATA_DIR=/var/lib/flux
Environment=NO_COLOR=1
StateDirectory=flux
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

```sh
sudo useradd --system --home /var/lib/flux flux
sudo systemctl daemon-reload && sudo systemctl enable --now flux
journalctl -u flux -f        # "flux is ready" with its URLs
```

flux colours its log only when stderr is a terminal, so the journal gets
plain text. `NO_COLOR=1` turns colour off everywhere; v0.1.0-alpha.1 needs it
to keep colour codes out of the journal.

With the default `localhost_role`, the Owner is whoever connects on the
server's own 127.0.0.1. To set flux up from another machine, open the
[owner link](#the-owner-link) from the journal (`journalctl -u flux | grep
own#`), or make a new one with `sudo -u flux flux-server owner-link --config
/etc/flux.toml`. Forwarding the port over SSH (`ssh -L
8096:127.0.0.1:8096 <server>`, then <http://127.0.0.1:8096>) works too.

For hardware transcoding through VAAPI or Quick Sync, add the `flux` user to
the `render` (and `video`) group.

### Upgrading

`flux-server update` does this for you ([Updating](#updating)). By hand:
download, check and unpack the new version with the steps above (with the
new `version=`), but move the old install aside before you move the new
one in. A plain `mv` onto an existing `/opt/flux` puts the new directory
*inside* it, and the old binary keeps running:

```sh
sudo rm -rf /opt/flux.old
sudo mv /opt/flux /opt/flux.old
sudo mv "flux-$version-$target" /opt/flux
flux-server --version
```

The link in `/usr/local/bin` points into `/opt/flux`, so it needs no
change, and the data dir is not touched. Restart the service if you run one
(`sudo systemctl restart flux`). `/opt/flux.old` keeps the previous version
until the next upgrade.

## Windows

flux runs on 64-bit Windows 10 or 11 and Windows Server 2016 or later. Each
release has two ways to install it:

- **The installer**, `flux-<version>-x86_64-pc-windows-msvc.msi`: flux as a
  Windows service, FFmpeg included, set up in a few clicks
  ([below](#the-windows-installer)). The first release with it is the one
  after v0.1.0-alpha.4.
- **The zip**, `flux-<version>-x86_64-pc-windows-msvc.zip`: one folder to
  keep anywhere and run in a terminal, or as a service you install
  ([from Download and check on](#download-and-check)). The first release
  with it is the one after v0.1.0-alpha.2.

### The Windows installer

<!-- x-release-please-start-version -->
Download
[`flux-0.1.0-alpha.4-x86_64-pc-windows-msvc.msi`](https://github.com/vibecook-dev/flux/releases/download/v0.1.0-alpha.4/flux-0.1.0-alpha.4-x86_64-pc-windows-msvc.msi)
and open it, or in PowerShell:

```powershell
curl.exe -fLO https://github.com/vibecook-dev/flux/releases/download/v0.1.0-alpha.4/flux-0.1.0-alpha.4-x86_64-pc-windows-msvc.msi
msiexec /i flux-0.1.0-alpha.4-x86_64-pc-windows-msvc.msi
```
<!-- x-release-please-end -->

It asks for an administrator, then:

- installs flux in `C:\Program Files\flux`, with FFmpeg in `ffmpeg\` there,
  and puts the folder on the system `PATH`;
- runs flux as the Windows service `flux` ("flux media server" in
  Services), as LocalSystem: it starts right away and with Windows, and
  again if it stops on its own;
- keeps flux's data in `C:\ProgramData\flux`, with its log (`flux.log`)
  and its settings (`flux.toml`). Only administrators can open that folder:
  the log has [owner links](#the-owner-link) in it;
- lets port 8097 through Windows Firewall on private networks, for when
  you switch on access from other devices (below);
- adds *flux* to the Start menu, which opens <http://localhost:8096>.

Open <http://localhost:8096> and follow [*Set up flux*](#setting-flux-up).

- **Other devices.** Out of the box flux answers this computer only. Open
  `C:\ProgramData\flux\flux.toml` in Notepad run as administrator, remove
  the `#` before `lan = "0.0.0.0:8097"`, and restart flux in a terminal
  opened as administrator: `Restart-Service flux`. Phones and TVs at home
  then open flux on port 8097 of this computer's address (*Set up flux*
  lists it, with a QR code). To set flux up from another device, make an
  [owner link](#the-owner-link) in a terminal opened as administrator:
  `flux-server owner-link`.
- **Your media.** LocalSystem reads your user folders and local drives.
  Drive letters mapped in your session (`Z:`) don't exist for a service:
  use `\\server\share` paths; for a share LocalSystem can't read, see
  *Network drives* under [As a Windows service](#as-a-windows-service).
- **Settings.** Each key in `flux.toml` is a `flux-server` flag with `_`
  for `-` (`flux-server --help`); restart flux after a change. In a
  terminal opened as administrator, `flux-server` commands (`--check`,
  `owner-link`) find this flux's data and settings by themselves.
- **FFmpeg.** The installer's is BtbN's GPL build of FFmpeg
  ([FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds)), with its
  licence and `SOURCE.txt` in `ffmpeg\`; each release attaches its complete
  source, `ffmpeg-<version>-windows-source.tar`. flux uses it before any
  other ffmpeg on the machine: set `ffmpeg = '…'` in `flux.toml` for
  another. `flux-server --check` shows what it can do here. It has no
  `tonemapx`, so it can't tone-map Dolby Vision profile 5 videos: a
  transcode of one looks purple and green. For those, set `ffmpeg` to
  Jellyfin's portable build ([Install](#install) has the download).
- **Updating.** When a new flux is out, the Libraries screen says so:
  *Update now* there (or `flux-server update` in a terminal opened as
  administrator) downloads the new installer, checks it, and has Windows
  Installer put it in place; flux is back as the new version a minute
  later. Running the new installer yourself does the same. Either way the
  data and settings stay.
- **Removing.** *Settings → Apps → flux → Uninstall* removes the program,
  the service, the firewall rule and the Start menu entry.
  `C:\ProgramData\flux` stays; delete it to remove flux's data and
  settings too.
- **From the zip's service.** If you installed the service from the zip
  (`flux-server service install`), run `flux-server service uninstall`
  first. The installer's flux uses the same data dir,
  `C:\ProgramData\flux`, unless you gave the service another one.

The rest of this section is about the zip, one folder:

```
flux-<version>-x86_64-pc-windows-msvc\
  flux-server.exe
  flux.exe                  the command line, for you and your agents (docs/AGENT-NATIVE.md)
  sidecar-slim.exe          Truffle's Tailscale sidecar, for --tailnet embedded
  web\                      the web client
  LICENSE.txt  README.txt  THIRD_PARTY_LICENSES.txt
```

`flux-server.exe` finds `web\` and `sidecar-slim.exe` next to itself, so
keep the folder together. It has the C runtime built in, so it needs no
Visual C++ Redistributable. The commands below are for PowerShell.

### Download and check

<!-- x-release-please-start-version -->
```powershell
$version = '0.1.0-alpha.4'
$zip = "flux-$version-x86_64-pc-windows-msvc.zip"
$base = "https://github.com/vibecook-dev/flux/releases/download/v$version"
curl.exe -fLO "$base/$zip"
curl.exe -fLO "$base/SHA256SUMS"
$want = (Select-String -Path SHA256SUMS -SimpleMatch $zip).Line.Split(' ')[0]
if ((Get-FileHash $zip -Algorithm SHA256).Hash -ne $want) { throw "$zip does not match SHA256SUMS" }
gh attestation verify $zip --repo vibecook-dev/flux `
  --signer-workflow vibecook-dev/flux/.github/workflows/release.yml `
  --source-digest (gh api "repos/vibecook-dev/flux/commits/v$version" --jq .sha)
```
<!-- x-release-please-end -->

The attestation check needs the [GitHub CLI](https://cli.github.com/); see
[Release packages](#release-packages) for what it proves.

The same checks work for the installer: put its name in `$zip`.

**SmartScreen.** A release's Windows programs and installer are
code-signed once the project's signing is set up; until then they aren't
(*Properties → Digital Signatures* lists no signature). Files downloaded
with `curl.exe` aren't marked as coming from the internet, but a zip
downloaded with a browser is, and so is everything unpacked from it:
SmartScreen may then stop an unsigned `flux-server.exe` with "Windows
protected your PC". Check the zip as above, then clear the mark before
unpacking it with `Unblock-File $zip` (or choose *More info → Run
anyway*). On Windows 11 with Smart App Control turned on, unsigned programs
can be blocked outright.

### Install

Any folder works; this one needs no administrator rights:

```powershell
New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\Programs" | Out-Null
Expand-Archive $zip -DestinationPath .
Move-Item "flux-$version-x86_64-pc-windows-msvc" "$env:LOCALAPPDATA\Programs\flux"
```

flux doesn't include ffmpeg. Jellyfin's portable build is two programs,
`ffmpeg.exe` and `ffprobe.exe`, with NVENC, Quick Sync, libx264 and
`tonemapx`. Its release page lists each file's SHA-256; flux's CI tests
with 8.1.3-1:

```powershell
$ff = 'jellyfin-ffmpeg_8.1.3-1_portable_win64-clang-gpl.zip'
curl.exe -fLO "https://github.com/jellyfin/jellyfin-ffmpeg/releases/download/v8.1.3-1/$ff"
(Get-FileHash $ff).Hash   # 3691F8B2D4C39C511B5FAFF1C8C4687BD782DAC823E63BA69F0755F7CE1AF9CE
Expand-Archive $ff -DestinationPath "$env:LOCALAPPDATA\Programs\jellyfin-ffmpeg"
# Put it on your PATH (open a new terminal afterwards), or pass --ffmpeg instead.
$dir = "$env:LOCALAPPDATA\Programs\jellyfin-ffmpeg"
[Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path', 'User') + ";$dir", 'User')
```

Without the `PATH` change, start flux with
`--ffmpeg "$env:LOCALAPPDATA\Programs\jellyfin-ffmpeg\ffmpeg.exe"`; it takes
`ffprobe.exe` from the same folder. Video encoders on Windows: NVENC on
NVIDIA GPUs (checked on Windows 11); Quick Sync on Intel GPUs is detected
but not yet tested on Windows; AMD's AMF isn't used, so AMD GPUs transcode
with libx264.

### First run

```powershell
cd "$env:LOCALAPPDATA\Programs\flux"
.\flux-server.exe --check
.\flux-server.exe
```

`--check` shows the ffmpeg, the encoder, the data dir and the web client
flux found. Then open <http://127.0.0.1:8096> and follow *Set up flux*
([Setting flux up](#setting-flux-up)), or name the libraries when you start
flux (`--library "Movies:movies:D:\Media\Movies"`, repeatable) and it creates
them on first run. The catalog, artwork and caches live in
`%LOCALAPPDATA%\flux` unless you pass `--data-dir` (or set
`FLUX_DATA_DIR`); the first log line names the folder. Builds up to
0.1.0-alpha.2 used `%HOME%\.local\share\flux`, or `flux-data` in the
working directory: if flux finds a catalog in one of those, it says so at
startup and in `--check`. Pass `--data-dir` to keep using it, or, with flux
stopped, move that folder's contents into `%LOCALAPPDATA%\flux`; the
catalog's artwork moves with it.

Stop flux with Ctrl-C; Ctrl-Break and closing its console window also stop
it gracefully. `taskkill /PID` without `/F` can't stop a console program
(Windows refuses), and `taskkill /F` ends it at once, without the graceful
stop. Windows doesn't send a console program the logoff and shutdown
events, so at logoff or shutdown only a service wrapper (below) stops flux
gracefully.

### A config file

Settings can live in a TOML file instead of flags (`--config`; the keys
are the long flag names with `_`, as in the
[README](../README.md#quickstart-build-from-source)). Write Windows paths in
'single quotes', TOML's literal strings, or with forward slashes. In
"double quotes" a backslash starts an escape: `"D:\Media"` doesn't parse,
and `"D:\tv"` would hold a tab; flux refuses paths with control characters,
and `--check` shows every path it read.

```powershell
New-Item -ItemType Directory -Force "$env:ProgramData\flux" | Out-Null
@'
ffmpeg = 'C:\Program Files\jellyfin-ffmpeg\ffmpeg.exe'   # where you unpacked it
data_dir = 'D:\flux\data'
# lan = "0.0.0.0:8097"            # see "LAN access and the firewall"

[[library]]
name = "Movies"
kind = "movies"
path = 'D:\Media\Movies'

[[library]]
name = "TV"
kind = "shows"
path = '\\nas\media\TV'
'@ | Set-Content -Encoding utf8 "$env:ProgramData\flux\flux.toml"
.\flux-server.exe --config "$env:ProgramData\flux\flux.toml" --check
```

### As a Windows service

flux runs as a Windows service of its own. A service runs as LocalSystem,
so its program must be where only administrators can change it: move the
flux folder to `C:\Program Files\flux` first, then, in a terminal opened as
administrator:

```powershell
& 'C:\Program Files\flux\flux-server.exe' service install    # with --config, if you use one
& 'C:\Program Files\flux\flux-server.exe' service status
```

The service is called `flux` ("flux media server" in Services). It starts
with Windows, a little after boot, and the service manager starts it again
if it stops on its own, after an update too. LocalSystem can read your user
folders and local drives. Unless you give `--config` or `--data-dir`, its
data dir is `C:\ProgramData\flux`, and its log is `flux.log` there.
`service stop`, `service start` and `service uninstall` (which leaves the
data) work the same way, as administrator; so do Services and `sc stop
flux`. `service install` refuses a program or config file in your user
folder, where any program you run could change them.

- **Another data dir than in a terminal.** flux in a terminal uses
  `%LOCALAPPDATA%\flux`. To keep the libraries and settings from there,
  stop flux and give `service install` `--data-dir
  "$env:LOCALAPPDATA\flux"`.
- **Network drives.** Drive letters mapped in your session (`Z:`) don't
  exist for a service: use UNC paths (`\\nas\media\TV`). LocalSystem
  reaches a share as the computer, which a NAS usually refuses; run the
  service as a user who can read the share (Services, flux, Log On), and
  give that user write access to the data dir.
- **The firewall** asks no one: see below.

Service wrappers such as [NSSM](https://nssm.cc/),
[WinSW](https://github.com/winsw/winsw) and
[Servy](https://github.com/aelassas/servy) work too. They stop flux with
Ctrl-C; give it 15 s (NSSM's `AppStopMethodConsole` defaults to 1.5 s).

### LAN access and the firewall

`--lan 0.0.0.0:8097` (or `lan = "0.0.0.0:8097"`) adds a listener for other
devices; its callers are anonymous, as on other platforms. The first time
`flux-server.exe` listens there, Windows Defender Firewall asks whether to
allow it: allow private networks. A service gets no prompt and stays
blocked until you add a rule (as administrator):

```powershell
New-NetFirewallRule -DisplayName 'flux (LAN)' -Direction Inbound -Action Allow -Protocol TCP `
  -LocalPort 8097 -Profile Private -Program 'C:\Program Files\flux\flux-server.exe'
```

The 127.0.0.1 listener never needs a rule. flux answers to this machine's
own names and addresses; add others, such as a reverse proxy's, with
`allow_host`.

### Remote access on Windows

`--tailnet system` uses the installed Tailscale for Windows
(`tailscale.exe`), and `--tailnet embedded` runs the bundled
`sidecar-slim.exe`. System mode runs on Windows 11 with Tailscale 1.102,
HTTPS on the tailnet included (`--tailnet-https`); embedded mode hasn't
been tried on Windows yet.

### Upgrading on Windows

`flux-server update` does this for you ([Updating](#updating)). By hand:
stop flux (or the service), swap the folder, and start it again. The data
dir is separate and is not touched:

```powershell
Remove-Item -Recurse -Force "$env:LOCALAPPDATA\Programs\flux.old" -ErrorAction SilentlyContinue
Rename-Item "$env:LOCALAPPDATA\Programs\flux" flux.old
Move-Item "flux-$version-x86_64-pc-windows-msvc" "$env:LOCALAPPDATA\Programs\flux"
```

## Docker

The image is `ghcr.io/vibecook-dev/flux`, for linux/amd64 and linux/arm64.
It holds the same Linux package plus jellyfin-ffmpeg. Tags: a version
(`0.1.0-alpha.1`), `latest` (the newest stable release, once there is one)
and `edge` (the tip of `main`).

<!-- x-release-please-start-version -->
```sh
docker run -d --name flux \
  -p 8096:8096 \
  -v flux-config:/config \
  -v /path/to/movies:/media/movies:ro \
  -v /path/to/tv:/media/tv:ro \
  -e FLUX_LIBRARIES="Movies:movies:/media/movies;TV:shows:/media/tv" \
  -e FLUX_ALLOW_HOSTS="nas.local,media.example.com" \
  ghcr.io/vibecook-dev/flux:0.1.0-alpha.4
```
<!-- x-release-please-end -->

A Compose file with the same settings is in
[docker/compose.yaml](../docker/compose.yaml).

| | |
|---|---|
| `/config` | Data dir (`FLUX_DATA_DIR`): catalog, artwork, caches. Keep it on a volume. |
| `/media` | Mount libraries under it, read-only is fine. |
| Port 8096 | The web client and API. |
| User | Runs as uid:gid 1000:1000. Use `--user` for another; it must be able to write `/config`. |
| Health | A `HEALTHCHECK` polls `/api/v1/server`. |

### Who is the Owner in a container

In the container flux listens on every interface (`FLUX_BIND=0.0.0.0`), because
the host and the LAN reach it through Docker's network. flux decides roles by
where a connection comes from, not by the listener:

- Connections from the container's own loopback are the **Owner**: `docker
  exec flux curl …`, and the health check.
- Everyone else, including a browser on the Docker host itself, arrives from
  the Docker network and is an **anonymous LAN caller**. Anonymous callers
  can browse and play, but cannot add libraries, start scans or change
  remote access.

So to set flux up from a browser, even on the Docker host, open the
[owner link](#the-owner-link) in the container's log (`docker logs flux`),
or make a new one with `docker exec flux flux-server owner-link`. Setting up
picks folders inside the container: mount your media under `/media` first.
Or name the libraries up front with `FLUX_LIBRARIES` (or a TOML file passed
with `FLUX_CONFIG`, kept in `/config`). New files are picked up by the folder
watcher; with `FLUX_NO_WATCH=1`, restart the container to rescan.

### Host names

flux answers only to host names it knows, which defeats DNS rebinding:
`localhost` and loopback addresses, the container's own name and
addresses, the tailnet's name, and anything in `FLUX_ALLOW_HOSTS`. Requests
for any other name get `421 Misdirected Request`. Inside Docker, the
machine's own LAN name and address are not the container's, so list the
names your browsers use, comma-separated, e.g.
`FLUX_ALLOW_HOSTS=nas.local,192.168.1.10,media.example.com`. Behind a reverse
proxy, list the proxy's public name.

### Hardware transcoding

- **Intel and AMD (VAAPI, Quick Sync):** pass the GPU and the group that
  owns it: `--device /dev/dri:/dev/dri --group-add "$(getent group render | cut -d: -f3)"`.
- **NVIDIA (NVENC):** install the NVIDIA Container Toolkit, then add
  `--gpus all -e NVIDIA_DRIVER_CAPABILITIES=compute,video,utility`.

`docker exec flux flux-server --check` shows which encoders flux found.

### Remote access in a container

`FLUX_TAILNET=embedded` runs flux's own Tailscale node, with its state in
`/config`, when the image includes Truffle's sidecar
(`/opt/flux/bin/sidecar-slim`). `system` mode needs the host's `tailscale`
CLI and is not available in the image; run flux outside Docker for that.

### Licences in the image

flux is MIT (`/opt/flux/LICENSE`, `/opt/flux/THIRD_PARTY_LICENSES`). The
bundled jellyfin-ffmpeg is GPL: its licence, the source links and a written
offer are in `/usr/share/licenses/jellyfin-ffmpeg/`, and every release
attaches the matching source tarball.

## Setting flux up

The first time flux runs it has no libraries, and its page offers *Set up
flux*: three steps, which you can come back to from the Libraries screen.

1. **Your films and shows.** Pick folders on the computer flux runs on: its
   home folder, drives and mounted shares come first, or type a path. flux
   counts the videos in a folder (by their names; it opens no files), says
   whether they look like films, TV shows or both, and suggests the
   library's name and kind.
2. **Playback.** Whether flux found ffmpeg, and which encoder converts video.
   Without ffmpeg flux plays only what a device can play as it is; this step
   says how to install it on this system.
3. **Other devices.** This machine's addresses on the network, with QR codes,
   and remote access over Tailscale.

Setting up needs the Owner. flux knows its owner by where a call comes from:
the computer flux runs on (unless `--localhost-role none`), or the owner's
Tailscale login on the tailnet. A browser elsewhere on the network, or on
the host of a Docker container (which reaches flux through Docker's
network), can browse and play but not add libraries until an owner link
makes it an owner.

### The owner link

While flux has no libraries and another device needs a link (one on the
network, or the Docker host), its log has one that works once, within a
day:

```text
flux has no libraries yet: set it up at http://localhost:8096/setup, or open http://192.168.1.20:8097/own#Xr3… on another device on this network: that makes the browser an owner …
```

Open it in the browser that should be an owner and confirm. In Docker the
link says `localhost`, which is right on the Docker host; from another
machine, put the host's address in its place. That browser becomes an
**owner device**: flux gives it a key, which the page keeps in the
browser's storage for flux's address and sends with every request, and
treats it as the Owner. A link made by an admin makes admins instead. To
make another link:

- `flux-server owner-link`, as the account flux runs as (it reads a key
  in the data dir), with the server's `--config` or `--data-dir` if it has
  them. As another user, `sudo -u flux flux-server owner-link --config
  /etc/flux.toml` and the like; with the Windows installer, in a terminal
  opened as administrator, with no flags;
- `docker exec flux flux-server owner-link` for the container;
- *Add a device* under *Owner devices* on the Libraries screen, with a QR
  code for a phone.

When this machine's browsers aren't flux's owner (`--localhost-role none`
on a machine other people log in to), the log has no link, since anyone
who can read the log could open it: use `flux-server owner-link`.

The Libraries screen lists owner devices, with who added each; *Remove*
signs one out at once, and one that hasn't been seen for 180 days stops
working. The data dir keeps only a hash of each device's key
(`devices.json`, readable by flux's account only). Over plain HTTP (the
`--lan` listener, or Docker's published port), someone who can watch your
network's traffic could copy a key, so remove devices you no longer use;
the tailnet encrypts it. Clearing the browser's site data signs it out
too (Safari clears a site's data by itself when it goes unopened for seven
days of browsing): make another link.

## Updating

flux checks GitHub for a new version once a day; GitHub sees this
machine's address and flux's version, nothing about your library. A
pre-release (`0.1.0-alpha.N`) follows pre-releases, a stable version
stable releases. When one is out, the Owner sees *flux X is out* over the
library, and the Libraries screen shows what's in it. Switch the check off
there, or with `--update-check false` (`FLUX_UPDATE_CHECK=false`,
`update_check = false` in the config file), which the screen then can't
change.

How flux updates depends on how it was installed:

- **Homebrew:** `brew upgrade vibecook-dev/tap/flux`, then `brew services
  restart vibecook-dev/tap/flux`.
- **The .deb or .rpm:** install the new release's package; the service
  restarts as the new version (the Libraries screen shows the command).
- **The Windows installer:** *Update now* on the Libraries screen, or
  `flux-server update` in a terminal opened as administrator. flux
  downloads the release's installer into `C:\ProgramData\flux\updates`
  (where only administrators can change it), checks it against
  `SHA256SUMS`, and hands it to Windows Installer, which stops the
  service, replaces the old version and starts the new one. If it fails,
  Windows Installer puts the old version back, and the Libraries screen
  says why, with the installer's log (`msi-<version>.log` in that
  folder). Running the new release's installer yourself works too.

- **A release package** (unpacked by hand, as above): *Update now* on the
  Libraries screen, or `flux-server update` in a terminal. flux downloads
  the package for this system, checks it against the release's
  `SHA256SUMS`, unpacks it beside the install and runs the new
  `flux-server --version` to make sure it starts here. Only then does it
  put the new version in place, file by file, and restart as it. Anything
  playing stops for a moment. Files you added to the install folder stay.
  If a step fails, nothing changes.
  - flux has to be able to write to its folder. When it can't (`/opt/flux`
    owned by root and flux running as a service user), run
    `sudo flux-server update`, or on Windows `flux-server update` in a
    terminal opened as administrator. Give it the server's `--config` or
    `--data-dir`, so it finds the running flux and asks it to restart.
  - `flux-server update --check` only says whether a new version is out
    (exit status 10 when one is). `--yes` skips the question.
- **Docker:** pull the new image and recreate the container:
  `docker compose pull && docker compose up -d`, or
  `docker pull ghcr.io/vibecook-dev/flux:<version>` and run it again with
  the same options.
- **Built from source:** pull and build again.

### Restarting after an update

- **macOS and Linux:** flux starts the new version in its own place (the
  same process), so a terminal, systemd or launchd carries on with it.
- **Windows:** run in a terminal, flux starts the new version in the same
  window. As a service (`flux-server service install`) it exits with status
  75, and the service manager starts the new version. The installer's
  service is restarted by Windows Installer. Under a service
  wrapper (NSSM, WinSW, Servy) the same exit asks the wrapper to start it
  again: NSSM does by default; with WinSW, add `<onfailure
  action="restart"/>`. `FLUX_RESTART=exit` or `spawn` makes the choice
  explicit.

### Going back

The version an update replaced stays in `.flux-previous` in the install
folder. To go back to it, stop flux, then run `flux-server update
--rollback` (with the server's `--config` or `--data-dir`).

A newer flux that changes the catalog's format first keeps a copy of it
(`catalog.db.v<N>.bak` in the data dir; flux keeps the two newest), because
the older flux can't read the new format. `--rollback` puts that copy back
too, after saying so: what changed since (scans, edits, watched state) is
lost.

## The app on your devices

flux's web client installs as an app (a PWA), with its own window or Home
Screen icon, on any device that can open flux on a secure page:

- **Chrome and Edge** (desktop and Android): *Install flux* in flux's corner
  menu, or the install button in the address bar.
- **iPhone and iPad:** Share, then *Add to Home Screen* (flux's corner menu
  says so too).
- **Safari on a Mac** (Safari 17 and later): File, then *Add to Dock*.

A secure page is flux on the machine it runs on (<http://127.0.0.1:8096>), or
flux over the tailnet with HTTPS on ([README § HTTPS](../README.md#https)).
Over plain HTTP from another machine (the `--lan` listener), browsers don't
run flux's service worker and Chrome and Edge don't offer to install it;
iPhone and iPad can still add it to the Home Screen, and it works as a web
page everywhere.

The installed app keeps flux's own files (the web client, about 2 MB), so it
opens even when flux can't be reached, and says so. It never stores your
library, artwork or video: those always come from the server.

When flux is updated, pages that are already open say *flux was updated*,
with a button to reload. A page that needs a screen whose code changed
reloads onto the new version by itself, or says why it can't.

## Building from source

You need:

- Rust: the workspace `rust-version` in `Cargo.toml` (1.89) or newer; CI
  uses the toolchain pinned in `.github/workflows/ci.yml`.
- Node.js 24 (LTS) and pnpm, the version in `web/package.json`
  (`packageManager`); `corepack enable` or `npm install -g pnpm@<version>`.
- ffmpeg and ffprobe, as above.

```sh
git clone https://github.com/vibecook-dev/flux && cd flux
export CARGO_TARGET_DIR="$PWD/target"
cargo build --release --locked -p flux-server
(cd web && pnpm install --frozen-lockfile && VITE_FLUX_API=/ pnpm build)
./target/release/flux-server --web-dir web/dist --library Movies:movies:/path/to/Movies
```

`VITE_FLUX_API=/` builds the client for the server it is served from;
without it the web client runs against its built-in mock backend. Run from
the repository root, flux also finds `web/dist` on its own.

On Windows, Rust needs the MSVC toolchain (Visual Studio Build Tools with
"Desktop development with C++"; rustup offers to install it), and the
environment variable is set the PowerShell way:

```powershell
cargo build --release --locked -p flux-server
cd web; pnpm install --frozen-lockfile; $env:VITE_FLUX_API = '/'; pnpm build; cd ..
.\target\release\flux-server.exe --web-dir web\dist --library 'Movies:movies:D:\Media\Movies'
```

That exe loads `vcruntime140.dll`, which comes with the Visual C++
Redistributable, not with Windows. To build one that runs on a Windows
without it, as the release zip does, set
`$env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS = '-C target-feature=+crt-static'`
before `cargo build`.

`scripts/package/` (packaging and the smoke test) runs in Git Bash.

To make a package like the release ones, see
[RELEASING.md § Packaging locally](RELEASING.md#packaging-locally).
