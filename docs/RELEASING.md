# Releasing flux

Releases are automated with [Release Please](https://github.com/googleapis/release-please).
Nobody edits version numbers or tags by hand: commit messages decide the
next version, a release PR collects the changes, and merging that PR
publishes the release.

## Before the first release

The first release, **v0.1.0-alpha.1**, needed **Truffle 0.7.20**, the
Truffle release with flux's changes: `Cargo.toml` takes
`truffle-core = "0.7.20"` from crates.io, and
`scripts/package/truffle-sidecar.sha256` pins that release's sidecars
(checked against the release's own asset digests and Truffle's
`crates/truffle-sidecar/sidecar-checksums.json`). Before merging the release
PR (`chore(main): release 0.1.0-alpha.1`), run the
[release dry run](#release-dry-run) on `main` and check that its summary
shows the sidecar bundled in all the packages.

The release PR's own `pull_request` CI run waits for approval (*Actions →
the run → Approve and run*, or
`gh api -X POST repos/vibecook-dev/flux/actions/runs/<id>/approve`):
`github-actions[bot]` counts as a first-time contributor until one of its
PRs is merged. Until it is approved, the PR shows no checks and stays
blocked, even after the CI run that `release-please.yml` dispatches has
passed.

Just before merging, check the end of the release PR's description. The
Blacksmith app (codesmith) on the organization appends a footer that starts
with a `---` line. Release Please takes the release notes from the PR
description up to its *last* `---` line, so with that footer the notes would
end with Release Please's own "This PR was generated with Release Please"
line. Delete everything from `<!-- codesmith:footer -->` on (*Edit* on the
description) and merge. Turning the footer off in Blacksmith's settings
would remove this step.

## How a release happens

1. **Commits on `main` follow [Conventional Commits](https://www.conventionalcommits.org/):**
   `feat:` (feature), `fix:` (bug fix), `perf:`, `revert:`, and `!` or
   a `BREAKING CHANGE:` footer for breaking changes. These make a release.
   Every other type (`docs:`, `ci:`, `chore:`, `refactor:`, `test:`,
   `build:`, and also `deps:`) doesn't make one on its own, and stays out
   of the changelog. Release Please only releases for a type with a visible
   changelog section (or a breaking change); `release-please-config.json`
   sets no `changelog-sections`, so its defaults apply, and they have no
   `deps` section. Dependabot follows the same rule
   (`.github/dependabot.yml`): bumps of what ships (cargo crates in the
   workspace, npm production dependencies, the Docker base image) are
   `fix(deps):`; npm development dependencies are `chore(deps-dev):`,
   `web/scripts/golden-rs` (a dev tool, not shipped) `chore(deps):` and
   GitHub Actions `ci(deps):`. Dependabot counts every cargo dependency as
   a production one, so workspace dev-dependency bumps are `fix(deps):`
   too.
2. **Release Please keeps a release PR open** (`chore(main): release …`),
   updated on every push to `main` (`.github/workflows/release-please.yml`).
   It bumps the version in `Cargo.toml` (`[workspace.package]`),
   `web/package.json` and the example versions in `docs/INSTALL.md` and
   `docker/compose.yaml`, and writes `CHANGELOG.md`. The workflow then runs
   `cargo metadata` on the release branch, which rewrites the workspace
   crates' entries in `Cargo.lock` and `web/scripts/golden-rs/Cargo.lock`
   (nothing else), and commits that to the PR. The PR's own `pull_request`
   CI runs wait for a maintainer's approval (*Actions → the run → Approve
   and run*): `github-actions[bot]` counts as a first-time contributor. The
   workflow also dispatches CI on the release branch; its checks land on
   the PR's head commit.
3. **Merging the release PR** makes Release Please create the tag
   `v<version>` and a **draft** GitHub release with the changelog, then call
   `.github/workflows/release.yml` in the same run, which:
   - checks the release is still a draft, and the tag matches `Cargo.toml`
     and `web/package.json`;
   - builds the web client once and `THIRD_PARTY_LICENSES` once, then
     `flux-server` for five targets (`build.yml`):

     | Target | Runner | Notes |
     |---|---|---|
     | `aarch64-apple-darwin` | macos-15 | macOS 11+ (`sidecar-slim`: 13+) |
     | `x86_64-apple-darwin` | macos-15 | cross-compiled; smoke-tested under Rosetta 2; macOS 11+ (`sidecar-slim`: 13+) |
     | `x86_64-unknown-linux-gnu` | ubuntu-22.04 | glibc 2.35+ |
     | `aarch64-unknown-linux-gnu` | ubuntu-22.04-arm | glibc 2.35+, 4/16/64 KiB pages |
     | `x86_64-pc-windows-msvc` | windows-2025 | Windows 10+; C runtime linked in (`+crt-static`), so no Visual C++ Redistributable; packaged as a `.zip`; smoke-tested with Ctrl-C and Ctrl-Break stops |

     The macOS minimums are the ones [INSTALL.md](INSTALL.md#requirements)
     promises, and `scripts/package/smoke-test.sh` enforces them: it fails a
     package whose `bin/flux-server` or `bin/sidecar-slim` declares a
     higher `minos` (`otool -arch all -l`, `LC_BUILD_VERSION`) than
     `MACOS_FLUX_SERVER` (11.0) or `MACOS_SIDECAR` (13.0). Raise one only
     in the script, this table and INSTALL.md together.

     Windows doesn't include `vcruntime140.dll`; the Visual C++
     Redistributable does. So `build.yml` (and `ci.yml`'s Windows job) set
     `CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS=-C target-feature=+crt-static`,
     which links the C runtime into `flux-server.exe`. The runners have the
     Redistributable, so running the exe there can't catch a regression:
     `smoke-test.sh` (and the Windows CI job) fail an `.exe` that imports
     `vcruntime*.dll`, `msvcp*.dll` or another of its DLLs
     (`scripts/package/pe-imports.py`). The Universal CRT
     (`api-ms-win-crt-*`) is part of Windows 10 and later.

   - packages each as `flux-<version>-<target>.tar.gz`, or `.zip` for
     Windows (`scripts/package/package.sh`), and smoke-tests every package
     (`scripts/package/smoke-test.sh`): on its own architecture, and the
     x86_64 macOS one under Rosetta 2 (`arch -x86_64`) on the arm64
     runner. The job installs Rosetta if the runner lacks it, and fails
     if `arch -x86_64` doesn't run translated. The package job runs in
     bash on every runner (Git Bash on Windows). The Windows test starts
     `flux-server.exe` in a console of its own
     (`scripts/package/windows-console.ps1`) and stops it as NSSM or WinSW
     would, with Ctrl-C and then, after a restart, Ctrl-Break; each stop
     must log "flux stopped" and exit 0;
   - uploads the packages, `SHA256SUMS` and the jellyfin-ffmpeg source
     tarball to the release, with a build-provenance attestation per package;
   - builds `ghcr.io/vibecook-dev/flux:<version>` for linux/amd64 and
     linux/arm64 from the Linux packages (`docker.yml`, through
     `.github/actions/flux-image`). Each platform is built and
     smoke-tested (`scripts/package/docker-smoke-test.sh`) on a runner of
     its own architecture. The multi-arch image takes linux/arm64 from the
     tested build's layers, with no QEMU (the job fails if the runner can
     emulate arm64), so a mismatch fails the build instead of shipping an
     untested rebuild. It is pushed and attested; `:latest` too for a
     stable release;
   - publishes the draft: a pre-release for `-alpha`/`-beta`/`-rc` versions,
     otherwise the repository's Latest release.

Every push to `main` also publishes `ghcr.io/vibecook-dev/flux:edge`.

### Release assets

| Asset | |
|---|---|
| `flux-<version>-<target>.tar.gz` | `bin/flux-server`, `share/flux/web/`, `share/flux/install-kind` (`archive`), `LICENSE`, `README.md`, `THIRD_PARTY_LICENSES`, and `bin/sidecar-slim` when a Truffle sidecar is pinned (below). |
| `flux-<version>-x86_64-pc-windows-msvc.zip` | One folder: `flux-server.exe`, `sidecar-slim.exe`, `web\`, `install-kind` (`archive`), `LICENSE.txt`, `THIRD_PARTY_LICENSES.txt` and `README.txt` (`scripts/package/README-windows.txt`). Written by `scripts/package/zip.py`: sorted entries, one timestamp, the same bytes on any OS. |
| `SHA256SUMS` | SHA-256 of every package. |
| `jellyfin-ffmpeg-<version>-source.tar.gz` | Source of the GPL ffmpeg in the Docker image. |

Attestations are on the repository's Attestations page and verify with
`gh attestation verify <file> --repo vibecook-dev/flux`
(`gh attestation verify oci://ghcr.io/vibecook-dev/flux:<version> --repo vibecook-dev/flux`
for the image). Add `--signer-workflow vibecook-dev/flux/.github/workflows/release.yml`
(`docker.yml` for the image) and `--source-digest <tagged commit>` to check
the workflow and the commit too ([INSTALL.md](INSTALL.md#release-packages)).
`--source-ref refs/tags/v<version>` fails: release-please.yml calls the
release from the push to `main`, so attestations record `refs/heads/main`.
Only the image's multi-arch index is attested, so verify the tag or the
index digest, not a per-platform digest. `SHA256SUMS` and the
jellyfin-ffmpeg source tarball have no attestation. The attest steps set
`create-storage-record: false`: an artifact-metadata storage record only
feeds the organization's Linked Artifacts page, needs the
`artifact-metadata: write` permission, and plays no part in
`gh attestation verify`.

### What the updater relies on

Running servers update from the published releases
(`crates/flux-server/src/update`, [INSTALL.md § Updating](INSTALL.md#updating)),
so these are a contract with every installed flux, old ones included:

- Releases are listed by GitHub's API (`/repos/vibecook-dev/flux/releases`),
  published (drafts are skipped), tagged `v<semver>`. The version alone says
  whether it is a pre-release: an install of a pre-release follows
  pre-releases, a stable install only stable releases.
- Each target's package is named `flux-<version>-<target>.tar.gz` (`.zip` for
  Windows), with one top-level folder in the layout above and the
  `install-kind` marker (`package.sh` writes `archive`; the smoke test fails
  a package without it). `PACKAGED_TARGETS` in `update/mod.rs` lists the
  targets that have one, as `build.yml` does.
- `SHA256SUMS` lists each package as `sha256sum` writes it. A package that
  doesn't match is refused, and immutable releases keep both from changing
  after publication.
- The new `flux-server --version` prints `flux-server <version>`: the
  updater runs it before putting anything in place.
- The notes' first bullet points (Release Please's `* **scope:** …` lines)
  are what the Libraries screen shows of a release.

## Release dry run

`.github/workflows/release-dry-run.yml` builds everything a release builds
and publishes nothing. Run it from **Actions → Release dry run → Run
workflow**, or:

```sh
gh workflow run release-dry-run.yml --repo vibecook-dev/flux --ref main
```

It runs the same steps as a release, on the chosen ref, with read-only
repository access:

- `build.yml` for all five targets, with every package's smoke test
  (x86_64 macOS under Rosetta 2, Windows with its Ctrl-C and Ctrl-Break
  stops) and the macOS `minos` check; the packages carry the version in
  `Cargo.toml`;
- `scripts/package/release-assets.sh` (as `release.yml` does): the
  jellyfin-ffmpeg source tarball and `SHA256SUMS`;
- the linux/amd64 + linux/arm64 image through `.github/actions/flux-image`
  (as `docker.yml` does), each platform smoke-tested on its own runner,
  **not pushed**.

It creates no tag, release, attestation or ghcr.io image. The packages,
the source tarball and `SHA256SUMS` are the run's `release-dry-run`
artifact (kept 7 days):

```sh
gh run download <run-id> --repo vibecook-dev/flux -n release-dry-run -D dry/
(cd dry && shasum -a 256 -c SHA256SUMS)
```

**The sidecar in a dry run** comes from the same place as in a release:
`fetch-sidecar.sh` downloads the Truffle release asset matching
`truffle-core` in `Cargo.lock` and checks its pinned SHA-256. When
`truffle-core` is not a crates.io release (the git pin before 0.7.20), the
packages are built without it, and the run's summary says so with a
warning and a per-package table. The dry run doesn't build a sidecar from
the pinned Truffle commit with Go: a release must ship Truffle's own,
checksummed release asset, so a locally built one would test a path no
release takes.

## Versions: pre-releases now, stable later

`release-please-config.json` starts flux in a pre-release phase:

```json
"prerelease": true,
"prerelease-type": "alpha",
"versioning": "prerelease",
"initial-version": "0.1.0-alpha.1"
```

`.release-please-manifest.json` holds the last released version. It
started at `"0.0.0"`, and the first release was **v0.1.0-alpha.1**
(2026-10-21). Every later release PR bumps the pre-release number:
`0.1.0-alpha.2`, `0.1.0-alpha.3`, … whatever the commit types. A release PR
needs at least one commit on `main` of a type that makes a release (see
above). These are GitHub pre-releases, the Docker image gets no `:latest`,
and GitHub doesn't mark them Latest.

To move to a **beta**, change `"prerelease-type"` to `"beta"` and land a commit
with the footer `Release-As: 0.1.0-beta.1`.

To make the **first stable release**, in one PR:

1. In `release-please-config.json`, set `"prerelease": false` and delete
   `"prerelease-type"` and `"versioning"` (back to plain SemVer bumps).
2. Merge it with a commit whose footer forces the version, e.g.

   ```
   chore: release 0.1.0

   Release-As: 0.1.0
   ```

The release PR then proposes `0.1.0`. From there `fix:` bumps the patch,
`feat:` the minor (pre-1.0, breaking changes bump the minor too, because of
`"bump-minor-pre-major": true`). The release becomes Latest and the image
gets `:latest`.

`Release-As: <version>` in any commit footer forces the next version at any
time.

## Repository setup (once)

No secrets are needed: everything runs on `GITHUB_TOKEN`, with
least-privilege `permissions:` per job. These settings are needed once:

- **Settings → Actions → General → Workflow permissions:** check *Allow
  GitHub Actions to create and approve pull requests* (Release Please opens
  its PR with `GITHUB_TOKEN`). If the organization restricts it, enable it
  under the `vibecook-dev` organization's Actions settings first.
- **Ruleset `protect-main`** (Settings → Rules → Rulesets), on the default branch:
  - changes land through a pull request (0 approvals needed; squash merge
    only, titled by the PR title, so each PR is one Conventional Commit).
    The squash commit's body is the branch's commit messages, and Release
    Please parses it too: a line starting with `BREAKING CHANGE` or
    `Release-As:` changes the release, and a paragraph that starts like a
    commit subject (`fix: …`, `feat(web): …`) counts as a commit of its
    own. Release Please also reads the merged PR's *description*: a
    `BEGIN_COMMIT_OVERRIDE` … `END_COMMIT_OVERRIDE` block there replaces
    the commit's whole message;
  - the status check **`CI OK`** (GitHub Actions) must pass, on a branch
    that is up to date with `main`. It fails when any CI job fails:
    `Rustfmt`, `Rust (ubuntu-24.04)`, `Rust (ubuntu-24.04-arm)`,
    `Rust (macos-15)`, `Rust (windows-2025)`, `MSRV`, `Web`, `cargo-deny`
    and `Lint workflows, scripts and Dockerfile`;
  - no force pushes and no deletion of `main`;
  - repository admins may bypass it **only through a pull request** (merge
    a PR with a red check in an emergency, `gh pr merge --admin`); nobody
    pushes to `main` directly.

  Release Please's PR gets its `CI OK` from the CI run that
  `release-please.yml` dispatches on the release branch. To recreate the
  ruleset: `gh api repos/vibecook-dev/flux/rulesets` lists it, and
  `gh api -X POST repos/vibecook-dev/flux/rulesets --input <file>` takes the
  same JSON.
- **Ruleset `protect-release-tags`** on `refs/tags/v*`: release tags can't
  be deleted or moved.
- **Immutable releases:** on. Once a release is published, its assets and
  tag can't change; `release.yml` only builds and publishes drafts.
- **Other repository settings:** squash merging only, branches deleted on
  merge, Dependabot alerts and security updates, secret scanning with push
  protection, and private vulnerability reporting (`SECURITY.md`).
- **The container package:** after the first push, open
  <https://github.com/orgs/vibecook-dev/packages/container/flux/settings>
  and set the visibility to *Public* (new packages start private). The image
  label `org.opencontainers.image.source` links it to the repository.

## When something fails

- **CI on the release PR didn't run or went stale:** run *CI* from the
  Actions tab on the `release-please--branches--main` branch.
- **A release build failed:** the tag and the draft release exist, nothing is
  published. Fix the cause on `main` if it is in the workflow, then run
  **Actions → Release → Run workflow** with the tag (e.g.
  `v0.1.0-alpha.2`), or `gh workflow run release.yml --repo vibecook-dev/flux -f tag=<tag>`.
  It rebuilds everything from the tagged commit, replaces the draft's
  assets (`--clobber`) and publishes it. A broken *commit* needs a new
  release instead: land the fix, and let the next release PR go out.
- **The release is already published:** `release.yml` refuses it. Its first
  job, *Check the `<tag>` draft*, fails with "`<tag>` is already
  published" before anything is built, and the upload step checks again.
  Immutable releases are on for the repository, so GitHub would reject new
  or replaced assets anyway, and the tag can't move. Release a new version
  instead. (v0.1.0-alpha.1 was published before immutable releases were
  turned on, so GitHub doesn't lock its assets: this check stops
  `release.yml`, but a maintainer could still replace or delete them by
  hand, with `gh release upload --clobber` or the web UI. Don't.)
- **Only the image job failed:** re-running just that job works for a
  day. It takes linux/arm64 from the run's `image-layers-arm64` artifact,
  which is kept 1 day; after that its download fails. Run Release for the
  tag instead (above), which builds everything again, the arm64 layers
  included.
- **Never move or delete a published tag**; release a new version. The tag
  ruleset `protect-release-tags` blocks it for `v*`.

## Packaging locally

The same scripts CI uses, for one target (here macOS on Apple Silicon; on
Windows, run them in Git Bash with `--target x86_64-pc-windows-msvc`,
`flux-server.exe` and `sidecar/sidecar-slim.exe`, which gives a `.zip` and
needs Python 3, and first
`export CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS='-C target-feature=+crt-static'`
as `build.yml` does, or the smoke test fails the exe for needing the
Visual C++ Redistributable):

```sh
export CARGO_TARGET_DIR="$PWD/target"
cargo build --release --locked -p flux-server
(cd web && pnpm install --frozen-lockfile && VITE_FLUX_API=/ pnpm build)
cargo install cargo-about --locked --version 0.9.2   # once; the script also needs jq
scripts/package/third-party-licenses.sh --out THIRD_PARTY_LICENSES
scripts/package/fetch-sidecar.sh --target aarch64-apple-darwin --out sidecar
scripts/package/package.sh --target aarch64-apple-darwin --version 0.0.0-local \
  --bin "$CARGO_TARGET_DIR/release/flux-server" --web web/dist \
  --third-party THIRD_PARTY_LICENSES --out dist \
  $( [ -f sidecar/sidecar-slim ] && echo --sidecar sidecar/sidecar-slim )
scripts/package/smoke-test.sh dist/flux-0.0.0-local-aarch64-apple-darwin.tar.gz --port 18097
```

`smoke-test.sh` unpacks the archive, starts the server on 127.0.0.1 with an
empty library and a scratch data dir, and checks `GET /` and
`GET /api/v1/server`. For the Docker image from local Linux packages, see
[docker/README.md](../docker/README.md).

## The Truffle sidecar

Embedded remote access (`--tailnet embedded`) needs Truffle's Go sidecar,
`sidecar-slim`, next to `flux-server`. `scripts/package/fetch-sidecar.sh`
downloads it from the Truffle release whose version matches `truffle-core`
in `Cargo.lock` and checks it against
`scripts/package/truffle-sidecar.sha256`. It bundles nothing (with a
warning) when `truffle-core` isn't a crates.io release, when that version
has no pinned checksums, or when the release has no asset for the target.
A release doesn't accept that: `release.yml` calls `build.yml` with
`require-sidecar: true`, so a package without the sidecar fails the build
before anything is uploaded, pushed or published. The release dry run only
warns, unless it is run with `-f require-sidecar=true`.

When flux moves to a new Truffle version, pin its sidecars in the same PR:

```sh
scripts/package/fetch-sidecar.sh --pin 0.7.20
```

This downloads the five `tsnet-sidecar-*` assets of `truffle-v0.7.20`
(`tsnet-sidecar-windows-amd64.exe` among them) and records their SHA-256. Then regenerate the sidecar's licence notice and
commit it with the checksums:

```sh
scripts/package/sidecar-notice.py
```

It reads the Go modules each pinned sidecar was built from (with their
`go.sum` hashes), downloads them from the Go module proxy, checks the hashes
and writes `scripts/package/sidecar-NOTICE.txt`, which `package.sh` appends
to `THIRD_PARTY_LICENSES`. `package.sh` refuses a sidecar with a module that
file doesn't list, so a bump without it fails the PR's package build.

The sidecar sets its own minimum OS. Truffle 0.7.20's macOS sidecars are
built with Go 1.27.2 and declare macOS 13.0 (`otool -l bin/sidecar-slim`,
`LC_BUILD_VERSION` `minos`), while `flux-server` declares 11.0
(`MACOSX_DEPLOYMENT_TARGET` in `build.yml`). INSTALL.md's requirements say
so, and the package smoke test fails if either rises above them (the
target table above), so a Truffle bump that needs a newer macOS fails the
release dry run and the release.

## Maintenance

| What | Where |
|---|---|
| Rust toolchain for CI and releases | `RUST_TOOLCHAIN` in `.github/workflows/ci.yml`, `build.yml` and `release-please.yml`. Bump them in one PR and fix new lints there. |
| MSRV | `rust-version` in `Cargo.toml`; the `MSRV` job reads it. Raise it when a dependency needs a newer Rust. |
| Node and pnpm | Node in `ci.yml`/`build.yml` (`node-version`); pnpm from `web/package.json` `packageManager`. |
| Action versions | Every action is pinned to a commit SHA with its version in a comment. Update the SHA and the comment together. |
| Caches | Only `ci.yml` uses caches (Rust and pnpm). `build.yml` and `.github/actions/flux-image` use none, because what they build is published (releases and `:edge`): a cache written by another run must not reach a release. Keep it that way. The image's arm64 layers pass from one job to the next within a run (artifact `image-layers-arm64`, kept 1 day); that is not a cache. |
| cargo-deny, cargo-about, actionlint, hadolint | Versions (and checksums) in `ci.yml` and `build.yml`. |
| Licence policy | `deny.toml` (CI gate) and `scripts/package/about.toml` (THIRD_PARTY_LICENSES); keep the allowed lists in step. npm licences: `scripts/package/web-licenses.mjs`. |
| jellyfin-ffmpeg | `JELLYFIN_FFMPEG_VERSION` and both `.deb` checksums in the `Dockerfile` ([docker/README.md](../docker/README.md#jellyfin-ffmpeg)), and the same version with its portable win64 zip's checksum in `ci.yml`'s Windows job (`JELLYFIN_FFMPEG_VERSION`, `JELLYFIN_FFMPEG_WIN64_SHA256`) and in INSTALL.md's [Windows install steps](INSTALL.md#install) (the jellyfin-ffmpeg snippet's version and hash). |
| Base image | The `debian:trixie-slim` digest in both `FROM` lines of the `Dockerfile`. Dependabot updates them (`docker` in `.github/dependabot.yml`); it doesn't read image references kept in an `ARG`. |
