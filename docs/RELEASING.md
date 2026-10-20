# Releasing flux

Releases are automated with [Release Please](https://github.com/googleapis/release-please).
Nobody edits version numbers or tags by hand: commit messages decide the
next version, a release PR collects the changes, and merging that PR
publishes the release.

## Before the first release

The first release, **v0.1.0-alpha.1**, waits for **Truffle 0.7.20**, the
Truffle release with flux's changes. Until it is on crates.io, `Cargo.toml`
takes `truffle-core` from the commit that merged them
(vibecook-dev/truffle#218, marked `TODO(truffle-0.7.20)`), and packages carry
no Truffle sidecar: `fetch-sidecar.sh` only bundles the sidecar of a
crates.io release whose checksums are pinned. The release PR stays open
meanwhile. Once `truffle-core` 0.7.20 is published, in one PR:

1. In `Cargo.toml`, replace the git `truffle-core` line and its TODO with
   `truffle-core = "0.7.20"`, then run `cargo update -p truffle-core`.
2. Pin the sidecars: `scripts/package/fetch-sidecar.sh --pin 0.7.20`, and
   check the recorded SHA-256 against the release's own digests:
   `gh api repos/vibecook-dev/truffle/releases/tags/truffle-v0.7.20 --jq '.assets[] | .name + " " + .digest'`.
3. Point the `[patch]` examples in `CONTRIBUTING.md` and
   `.cargo/config.toml.example` at `crates-io`, and drop the "pinned commit"
   wording in `README.md` and `docs/ARCHITECTURE.md`.

After it merges, run the [release dry run](#release-dry-run) and check that
its summary shows the sidecar bundled in all four packages. Then merge the
release PR (`chore(main): release 0.1.0-alpha.1`) to cut v0.1.0-alpha.1.

## How a release happens

1. **Commits on `main` follow [Conventional Commits](https://www.conventionalcommits.org/):**
   `feat:` (feature), `fix:` (bug fix), `perf:`, `deps:`, `revert:`, and `!` or
   a `BREAKING CHANGE:` footer for breaking changes. These make a release.
   `docs:`, `ci:`, `chore:`, `refactor:`, `test:` and `build:` don't make
   one on their own, and stay out of the changelog.
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
   - checks the tag matches `Cargo.toml` and `web/package.json`;
   - builds the web client once and `THIRD_PARTY_LICENSES` once, then
     `flux-server` for four targets (`build.yml`):

     | Target | Runner | Notes |
     |---|---|---|
     | `aarch64-apple-darwin` | macos-15 | macOS 11+ |
     | `x86_64-apple-darwin` | macos-15 | cross-compiled |
     | `x86_64-unknown-linux-gnu` | ubuntu-22.04 | glibc 2.35+ |
     | `aarch64-unknown-linux-gnu` | ubuntu-22.04-arm | glibc 2.35+, 4/16/64 KiB pages |

   - packages each as `flux-<version>-<target>.tar.gz`
     (`scripts/package/package.sh`) and smoke-tests the native ones
     (`scripts/package/smoke-test.sh`);
   - uploads the packages, `SHA256SUMS` and the jellyfin-ffmpeg source
     tarball to the release, with a build-provenance attestation per package;
   - builds `ghcr.io/vibecook-dev/flux:<version>` for linux/amd64 and
     linux/arm64 from the Linux packages, smoke-tests it, pushes it and
     attests it (`docker.yml`); `:latest` too for a stable release;
   - publishes the draft: a pre-release for `-alpha`/`-beta`/`-rc` versions,
     otherwise the repository's Latest release.

Every push to `main` also publishes `ghcr.io/vibecook-dev/flux:edge`.

### Release assets

| Asset | |
|---|---|
| `flux-<version>-<target>.tar.gz` | `bin/flux-server`, `share/flux/web/`, `LICENSE`, `README.md`, `THIRD_PARTY_LICENSES`, and `bin/sidecar-slim` when a Truffle sidecar is pinned (below). |
| `SHA256SUMS` | SHA-256 of every tarball. |
| `jellyfin-ffmpeg-<version>-source.tar.gz` | Source of the GPL ffmpeg in the Docker image. |

Attestations are on the repository's Attestations page and verify with
`gh attestation verify <file> --repo vibecook-dev/flux`
(`gh attestation verify oci://ghcr.io/vibecook-dev/flux:<version> --repo vibecook-dev/flux`
for the image).

## Release dry run

`.github/workflows/release-dry-run.yml` builds everything a release builds
and publishes nothing. Run it from **Actions → Release dry run → Run
workflow**, or:

```sh
gh workflow run release-dry-run.yml --repo vibecook-dev/flux --ref main
```

It runs the same steps as a release, on the chosen ref, with read-only
repository access:

- `build.yml` for all four targets, with the native smoke tests; the
  packages carry the version in `Cargo.toml`;
- `scripts/package/release-assets.sh` (as `release.yml` does): the
  jellyfin-ffmpeg source tarball and `SHA256SUMS`;
- the linux/amd64 + linux/arm64 image through `.github/actions/flux-image`
  (as `docker.yml` does), with its smoke test, **not pushed**.

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

with `.release-please-manifest.json` at `"0.0.0"` (nothing released yet).
The first release is **v0.1.0-alpha.1**; it needs at least one releasable
commit (`feat:` or `fix:`) on `main`. After that every release PR bumps the
pre-release number: `0.1.0-alpha.2`, `0.1.0-alpha.3`, … whatever the commit
types. These are GitHub pre-releases, the Docker image gets no `:latest`,
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
- **Ruleset `main`** (Settings → Rules → Rulesets), on the default branch:
  - changes land through a pull request (0 approvals needed; squash merge
    only, titled by the PR title, so each PR is one Conventional Commit);
  - the status check **`CI OK`** (GitHub Actions) must pass, on a branch
    that is up to date with `main`. It fails when any CI job fails:
    `Rustfmt`, `Rust (ubuntu-24.04)`, `Rust (ubuntu-24.04-arm)`,
    `Rust (macos-15)`, `MSRV`, `Web`, `cargo-deny` and
    `Lint workflows, scripts and Dockerfile`;
  - no force pushes and no deletion of `main`;
  - repository admins may bypass it **only through a pull request** (merge
    a PR with a red check in an emergency, `gh pr merge --admin`); nobody
    pushes to `main` directly.

  Release Please's PR gets its `CI OK` from the CI run that
  `release-please.yml` dispatches on the release branch. To recreate the
  ruleset: `gh api repos/vibecook-dev/flux/rulesets` lists it, and
  `gh api -X POST repos/vibecook-dev/flux/rulesets --input <file>` takes the
  same JSON.
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
  `v0.1.0-alpha.1`). It rebuilds everything from the tagged commit, replaces
  the assets (`--clobber`) and publishes the draft. A broken *commit* needs
  a new release instead: land the fix, and let the next release PR go out.
- **Never move or delete a published tag**; release a new version.

## Packaging locally

The same scripts CI uses, for one target (here macOS on Apple Silicon):

```sh
export CARGO_TARGET_DIR="$PWD/target"
cargo build --release --locked -p flux-server
(cd web && pnpm install --frozen-lockfile && VITE_FLUX_API=/ pnpm build)
cargo install cargo-about --locked --version 0.9.2   # once
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

When flux moves to a new Truffle version, pin its sidecars in the same PR:

```sh
scripts/package/fetch-sidecar.sh --pin 0.7.20
```

This downloads the four `tsnet-sidecar-*` assets of `truffle-v0.7.20` and
records their SHA-256.

## Maintenance

| What | Where |
|---|---|
| Rust toolchain for CI and releases | `RUST_TOOLCHAIN` in `.github/workflows/ci.yml`, `build.yml` and `release-please.yml`. Bump them in one PR and fix new lints there. |
| MSRV | `rust-version` in `Cargo.toml`; the `MSRV` job reads it. Raise it when a dependency needs a newer Rust. |
| Node and pnpm | Node in `ci.yml`/`build.yml` (`node-version`); pnpm from `web/package.json` `packageManager`. |
| Action versions | Every action is pinned to a commit SHA with its version in a comment. Update the SHA and the comment together. |
| Caches | Only `ci.yml` uses caches (Rust and pnpm). `build.yml` and `.github/actions/flux-image` use none, because what they build is published (releases and `:edge`): a cache written by another run must not reach a release. Keep it that way. |
| cargo-deny, cargo-about, actionlint, hadolint | Versions (and checksums) in `ci.yml` and `build.yml`. |
| Licence policy | `deny.toml` (CI gate) and `scripts/package/about.toml` (THIRD_PARTY_LICENSES); keep the allowed lists in step. npm licences: `scripts/package/web-licenses.mjs`. |
| jellyfin-ffmpeg | `JELLYFIN_FFMPEG_VERSION` and both `.deb` checksums in the `Dockerfile` ([docker/README.md](../docker/README.md#jellyfin-ffmpeg)). |
| Base image | `DEBIAN_IMAGE` digest in the `Dockerfile`. |
