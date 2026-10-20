# flux container image

`ghcr.io/vibecook-dev/flux`, for linux/amd64 and linux/arm64. How to run it
is in [docs/INSTALL.md](../docs/INSTALL.md#docker). This page is about how
the image is built.

| Tag | What |
|---|---|
| `0.1.0-alpha.1`, `0.1.0`, … | A release. |
| `latest` | The newest stable (non-pre-release) release. |
| `edge` | The tip of `main`. |
| `sha-<commit>` | One commit of `main`. |

## Layout

| Path | |
|---|---|
| `/opt/flux/bin/flux-server` | The release binary (also on `PATH`). |
| `/opt/flux/bin/sidecar-slim` | Truffle's Tailscale sidecar, when the release bundles one. |
| `/opt/flux/share/flux/web` | The web client (`FLUX_WEB_DIR`). |
| `/opt/flux/LICENSE`, `/opt/flux/THIRD_PARTY_LICENSES` | flux's licence and its dependencies' licences. |
| `/usr/lib/jellyfin-ffmpeg/{ffmpeg,ffprobe}` | jellyfin-ffmpeg (`FLUX_FFMPEG`, `FLUX_FFPROBE`). |
| `/usr/share/licenses/jellyfin-ffmpeg/` | FFmpeg's licence and `SOURCE-OFFER.md`. |
| `/config` | `FLUX_DATA_DIR`: catalog, artwork, caches. A volume. |
| `/media` | Mount your libraries here (read-only is fine). A volume. |

The image runs as uid:gid `1000:1000`, under `tini`, with a `HEALTHCHECK` on
`/api/v1/server`.

## How it is built

Nothing is compiled inside Docker. The `Dockerfile` copies an extracted
release package for the target architecture from a build context called
`dist`, laid out as:

```
dist/
  amd64/  bin/flux-server  share/flux/web/  LICENSE  THIRD_PARTY_LICENSES  …
  arm64/  …
```

CI (`.github/workflows/docker.yml`) builds the two Linux packages with
`build.yml`, lays them out with `scripts/package/docker-context.sh` and
passes `--build-context dist=docker-dist`. Without that context, the
`download` stage fetches the packages of a published release and checks
them against its `SHA256SUMS`.

From a published release:

```sh
docker buildx build --build-arg FLUX_VERSION=0.1.0-alpha.1 \
  --platform linux/amd64,linux/arm64 -t flux:0.1.0-alpha.1 .
```

From packages you built (`scripts/package/package.sh`, see
[docs/RELEASING.md](../docs/RELEASING.md)):

```sh
scripts/package/docker-context.sh dist docker-dist
docker buildx build --build-context dist=docker-dist --platform linux/amd64 -t flux:local --load .
scripts/package/docker-smoke-test.sh flux:local
```

## jellyfin-ffmpeg

The runtime stage installs the pinned jellyfin-ffmpeg `.deb` for Debian 13
from https://github.com/jellyfin/jellyfin-ffmpeg/releases and checks its
SHA-256. To update it, change `JELLYFIN_FFMPEG_VERSION` and both
`JELLYFIN_FFMPEG_SHA256_*` arguments in the `Dockerfile`. The checksums are
on the release page (each asset's `sha256:` digest) or from
`shasum -a 256` on the downloaded `.deb`. It is GPL software; the image
carries its licence and a pointer to its source
([SOURCE-OFFER.md](SOURCE-OFFER.md)), and every flux release attaches the
matching source tarball.
