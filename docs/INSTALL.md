# Installing flux

flux is one program, `flux-server`, that serves its own web client. It runs
`ffmpeg` and `ffprobe`, which you install separately (the Docker image
brings its own; see [ffmpeg](#ffmpeg)). There are three ways to install it:

- [a release package](#release-packages) for macOS or Linux;
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
| ffmpeg | `ffmpeg` and `ffprobe`, any recent version. See [ffmpeg](#ffmpeg). |

Windows is not supported yet.

## ffmpeg

flux runs `ffmpeg` and `ffprobe` as separate programs, for transcodes, frame
posters and probe fallbacks. It never links FFmpeg's libraries, so any
ffmpeg build works, including GPL builds, and flux stays MIT-licensed.
Direct play and remuxing (most playback) don't use ffmpeg at all.

flux looks for them on `PATH`, then in `/usr/lib/jellyfin-ffmpeg`,
`/opt/homebrew/bin`, `/usr/local/bin` and `/usr/bin`; it doesn't look next
to `flux-server`. `--ffmpeg PATH` (or `FLUX_FFMPEG`) picks another, and
`ffprobe` defaults to the one next to `ffmpeg` (`--ffprobe`,
`FLUX_FFPROBE`).

At startup flux asks ffmpeg which H.264 encoders it has and picks the best
one for video transcodes:

| Platform | Hardware encoders | Software fallback |
|---|---|---|
| macOS | VideoToolbox (`h264_videotoolbox`) | `libx264` |
| Linux, NVIDIA | NVENC (`h264_nvenc`) | `libx264` |
| Linux, Intel | Quick Sync (`h264_qsv`) or VAAPI (`h264_vaapi`) | `libx264` |
| Linux, AMD | VAAPI (`h264_vaapi`) | `libx264` |

Where to get ffmpeg:

- **macOS:** `brew install ffmpeg` (has VideoToolbox and libx264).
- **Debian, Ubuntu, Raspberry Pi OS:** `sudo apt install ffmpeg` (has libx264
  and VAAPI). For Jellyfin's tuned build, install
  [jellyfin-ffmpeg](https://github.com/jellyfin/jellyfin-ffmpeg/releases)
  and pass `--ffmpeg /usr/lib/jellyfin-ffmpeg/ffmpeg`.
- **Fedora:** the `ffmpeg` package from RPM Fusion (Fedora's own
  `ffmpeg-free` has no libx264).
- **Docker:** the image includes jellyfin-ffmpeg.

To check what flux found and what it will use:

```sh
flux-server --check
```

It prints the ffmpeg and ffprobe it resolved, their versions and the
encoders it detected, and exits non-zero if something required is missing.

## Release packages

Each release has a package per platform, named
`flux-<version>-<target>.tar.gz`:

```
flux-<version>-<target>/
  bin/flux-server
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
version=0.1.0-alpha.1
target=aarch64-apple-darwin      # or x86_64-apple-darwin, x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu
base=https://github.com/vibecook-dev/flux/releases/download/v$version
curl -fLO "$base/flux-$version-$target.tar.gz"
curl -fLO "$base/SHA256SUMS"
shasum -a 256 -c --ignore-missing SHA256SUMS      # Linux: sha256sum -c --ignore-missing SHA256SUMS
tar -xzf "flux-$version-$target.tar.gz"
sudo mv "flux-$version-$target" /opt/flux
sudo ln -sf /opt/flux/bin/flux-server /usr/local/bin/flux-server
flux-server --check
```
<!-- x-release-please-end -->

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
`~/.local/share/flux` (Linux) unless you pass `--data-dir`. Every flag, its
environment variable and the TOML config file are in the
[README](../README.md#quickstart-build-from-source) and `flux-server --help`.

flux is private by default: it listens on 127.0.0.1 only, where callers are
the Owner. `--lan 0.0.0.0:8097` adds a LAN listener whose callers are
anonymous (they can browse and play, but not add libraries, scan or change
remote access). Remote access with identities goes through Tailscale
(`--tailnet system|embedded`, README §Remote access).

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
kind = "movies"                         # movies | shows | mixed
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
  ghcr.io/vibecook-dev/flux:0.1.0-alpha.1
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

To make a package like the release ones, see
[RELEASING.md § Packaging locally](RELEASING.md#packaging-locally).
