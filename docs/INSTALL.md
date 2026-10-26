# Installing flux

flux is one program, `flux-server`, that serves its own web client. It runs
`ffmpeg` and `ffprobe`, which you install separately (the Docker image
brings its own; see [ffmpeg](#ffmpeg)). There are three ways to install it:

- [a release package](#release-packages) for macOS or Linux, or a zip for
  [Windows](#windows);
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

flux looks for them on `PATH`, then in `/usr/lib/jellyfin-ffmpeg`,
`/opt/homebrew/bin`, `/usr/local/bin` and `/usr/bin`; it doesn't look next
to `flux-server`. On Windows it looks only on `PATH`. (When `PATH` has none,
Windows' own program search also finds an `ffmpeg.exe` and `ffprobe.exe`
placed next to `flux-server.exe`, but `--check` then shows them without a
folder; prefer `PATH` or `--ffmpeg`.) `--ffmpeg PATH` (or `FLUX_FFMPEG`)
picks another, and `ffprobe` defaults to the one next to `ffmpeg`
(`--ffprobe`, `FLUX_FFPROBE`).

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
- **Windows:** Jellyfin's portable build,
  `jellyfin-ffmpeg_<version>_portable_win64-clang-gpl.zip` from
  [jellyfin-ffmpeg's releases](https://github.com/jellyfin/jellyfin-ffmpeg/releases)
  (has NVENC, Quick Sync, libx264 and `tonemapx`); see [Windows](#windows).
- **Docker:** the image includes jellyfin-ffmpeg.

To check what flux found and what it will use:

```sh
flux-server --check
```

It prints the ffmpeg and ffprobe it resolved, their versions and the
encoders it detected, and exits non-zero if something required is missing.

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
flux-server --library Movies:movies:/path/to/Movies --library TV:shows:/path/to/TV
```

Open <http://127.0.0.1:8096>. Libraries named on the command line are created
on first run; you can also add them on the Admin screen. The catalog,
artwork and caches live in `~/Library/Application Support/flux` (macOS) or
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

### As a service (Linux, systemd)

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
stops with exit status 1 and says which library has it. The Admin screen
refuses such a folder too. Earlier alphas allowed overlapping libraries;
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
server's own 127.0.0.1. To use the Admin screen from another machine,
forward the port over SSH (`ssh -L 8096:127.0.0.1:8096 <server>`) and open
<http://127.0.0.1:8096>.

For hardware transcoding through VAAPI or Quick Sync, add the `flux` user to
the `render` (and `video`) group.

### Upgrading

Download, check and unpack the new version with the steps above (with the
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

flux runs on 64-bit Windows 10 or 11 and Windows Server 2016 or later. The
first release with a Windows package is the one after v0.1.0-alpha.2. Each
release has `flux-<version>-x86_64-pc-windows-msvc.zip`, one folder:

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

**SmartScreen.** The binaries aren't code-signed yet. Files downloaded
with `curl.exe` aren't marked as coming from the internet, but a zip
downloaded with a browser is, and so is everything unpacked from it:
SmartScreen may then stop `flux-server.exe` with "Windows protected your
PC". Check the zip as above, then clear the mark before unpacking it with
`Unblock-File $zip` (or choose *More info → Run anyway*). On Windows 11 with
Smart App Control turned on, unsigned programs can be blocked outright.

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
.\flux-server.exe --library "Movies:movies:D:\Media\Movies" --library "TV:shows:D:\Media\TV"
```

`--check` shows the ffmpeg, the encoder, the data dir and the web client
flux found. Then open <http://127.0.0.1:8096>; you can also add libraries
on the Admin screen. The catalog, artwork and caches live in
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

flux has no Windows service mode of its own: run it under a service
wrapper such as [NSSM](https://nssm.cc/),
[WinSW](https://github.com/winsw/winsw) or
[Servy](https://github.com/aelassas/servy). They start `flux-server.exe`
with a hidden console and stop it with Ctrl-C, then end it after a timeout.
On Ctrl-C flux lets requests in flight finish for up to 5 s, then exits
within 5 s more (at once when idle); give the wrapper 15 s (NSSM's
`AppStopMethodConsole` defaults to 1.5 s, WinSW's `<stoptimeout>` to 15 s;
Servy calls it `StopTimeout`). These wrappers haven't been tested with flux
yet; the release smoke test stops flux the way they do.

Under a service, some things differ:

- **The account.** A service runs as LocalSystem unless you pick a user,
  and LocalSystem's `%LOCALAPPDATA%` is under
  `C:\Windows\System32\config\systemprofile`. Set `data_dir` (or
  `--data-dir`), and run the service as an account that can write there
  and read your media.
- **Network drives.** Drive letters mapped in your session (`Z:`) don't
  exist for a service; use UNC paths (`\\nas\media\TV`) and an account with
  access to the share.
- **The firewall** asks no one: see below.

For example, with NSSM, flux installed in `C:\Program Files\flux` and the
config file above (as administrator):

```powershell
nssm install flux 'C:\Program Files\flux\flux-server.exe' --config C:\ProgramData\flux\flux.toml
nssm set flux AppStopMethodConsole 15000
nssm set flux AppStderr C:\ProgramData\flux\flux.log
nssm set flux ObjectName .\<user> <password>
nssm start flux
```

flux logs to stderr, without colour when it isn't a terminal.

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

Stop flux (or the service), swap the folder, and start it again. The data
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

So name the libraries up front with `FLUX_LIBRARIES` (or a TOML file passed
with `FLUX_CONFIG`, kept in `/config`). New files are picked up by the folder
watcher; with `FLUX_NO_WATCH=1`, restart the container to rescan. To act as
the Owner from the host, run the container with `--network host` and use
<http://localhost:8096>, or reach it over your tailnet.

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
