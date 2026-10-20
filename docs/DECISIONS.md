# Decisions

Settled project decisions, newest first, each with the reasoning behind it. The questions that are still open are in the README's [Open decisions](../README.md#open-decisions). The research that informed these is in [research/00-synthesis.md](../research/00-synthesis.md) §"Decisions for the maintainer" and [research/01](../research/01-competitive-landscape.md).

## 2026-10-20: flux is open source under the MIT licence

**Decision.** All of flux (the Rust crates, the server, the web client and the tools) is released under the MIT licence, `Copyright (c) 2026 James Yong`. Every crate inherits `license = "MIT"` from the workspace, and `web/package.json` says MIT too. The public repository is [github.com/vibecook-dev/flux](https://github.com/vibecook-dev/flux).

**Why.**
- Mille and Truffle, the two libraries flux dogfoods, ship the same MIT licence under the same copyright line. One licence across the three means code can move between them in either direction without relicensing.
- The research proposed `MIT OR Apache-2.0` for the engine crates and AGPL-3.0 for the server. A split licence adds friction for contributors and for anyone who wants to embed the engine crates, and a single permissive licence keeps the project simple while it has one maintainer.
- Deciding before the first outside contribution means nobody's code has to be relicensed later.

## 2026-10-20: no Jellyfin-API compatibility

**Decision.** flux does not implement the Jellyfin API. It builds its own web client and its own native client apps against its own API (`flux_core::api`).

**Why.**
- flux's API is designed around what makes it fast and private: playback plans with reasons, segment URLs that are pure functions of `(asset, recipe, index)`, and identity from Tailscale instead of passwords. A compatibility layer would have to map Jellyfin's password login and its playback model onto that, and would hold flux's own API back to what existing clients expect.
- Clean-room compatibility is a large, permanent surface to test and track as Jellyfin changes, and other projects already offer it (research/01 §4).
- The open question is which native clients come first; see the README's [Open decisions](../README.md#open-decisions).

## 2026-10-20: FFmpeg runs only as a separate process

**Decision.**
- flux only ever runs `ffmpeg` and `ffprobe` as separate processes. It never links `libav*`, statically or dynamically.
- **Release binaries** don't bundle FFmpeg. They use the system's `ffmpeg` and `ffprobe`, check what that build can do, and take `--ffmpeg` / `--ffprobe` paths (`FLUX_FFMPEG` / `FLUX_FFPROBE`) to pick a specific one.
- **The Docker image** (`ghcr.io/vibecook-dev/flux`) bundles [jellyfin-ffmpeg](https://github.com/jellyfin/jellyfin-ffmpeg), which is GPL. The image ships its licence text and a link to its source.
- **Video transcoding** picks an encoder at runtime from what the FFmpeg build offers: VideoToolbox on macOS, NVENC, QSV or VAAPI on Linux, and `libx264` as the software fallback.

**Why.**
- Running FFmpeg as a separate program keeps flux itself MIT. The GPL or LGPL obligations of an FFmpeg build attach to that binary only, and flux's own code never has to follow them.
- Bundling jellyfin-ffmpeg in the container image is what Jellyfin and Immich do. It carries patches for hardware acceleration and tone mapping that are already proven in media servers, and a container gives one known-good build on every Linux host.
- Using the system FFmpeg for release binaries avoids redistributing a GPL build alongside every download, and lets users bring a build that matches their hardware. The capability check turns a missing encoder into a clear message instead of a failed transcode.
- A future macOS app may bundle an LGPL FFmpeg build with VideoToolbox: hardware encoding covers what the app needs, so it would not need the GPL-only `libx264`/`libx265`.
- Today's prototype encodes video with `h264_videotoolbox` only, so the runtime encoder choice is what makes video transcoding work on Linux.

## 2026-10-20: one clean initial commit for the public repository

**Decision.** The public repository starts from one initial commit. The prototype's full history (about 145 commits) stays in the maintainer's local repository on a `prototype-history` branch.

**Why.** The prototype history was written on one machine for one person: worktree paths, scratch directories and work-in-progress notes. The reports that matter are kept in [docs/prototype/](prototype/) and [research/](../research/), where they are the record of how flux was built and measured.
