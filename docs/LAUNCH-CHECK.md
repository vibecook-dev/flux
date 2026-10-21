# Launch check

An adversarial check of the public repository and its release pipeline before
the first release (v0.1.0-alpha.1), made on 2026-10-20 against `main` at
`4b4626c`. The goal was to find what is wrong, not to confirm what works. Every
check worked from a fresh `git clone` of github.com/vibecook-dev/flux, with no
local `.cargo/config.toml` and no `[patch]` overrides.

No release had been cut yet. Release Please's PR (#1) stays open until
truffle-core 0.7.20 is in `Cargo.toml` (#6). The release checks below were
therefore run on the **release dry run** artifacts
(`.github/workflows/release-dry-run.yml`) of the #6 branch, which bundle
Truffle 0.7.20's sidecar. Attestations and the versioned Docker image only
exist after a real release; they are listed under [Open items](#open-items).

v0.1.0-alpha.1 was published on 2026-10-21. What was checked then, and what
is still open, is in [After the release](#after-the-release).

| Area | Result |
|---|---|
| [Leaks](#1-leaks) | No secrets, personal paths, private notes or private tailnet data. One error message referred to the dev setup; fixed in #10 |
| [Fresh-machine build and tests](#2-fresh-machine-build-and-tests) | Clean on GitHub's fresh runners (Linux x86_64 and arm64, macOS, MSRV 1.89, web). A local run with the media corpus is still open |
| [Workflows](#3-workflows) | Sound overall. Three fixes in #8: release builds used caches, a release could ship without the sidecar, and CI could skip every transcode silently |
| [Release artifacts](#4-release-artifacts-dry-run) | Checksums, contents, architecture and glibc floor are right. On macOS, the documented symlink install served no web client; fixed in #11 |
| [Docs](#5-docs) | Some statements predated the public repo. Fixed in this PR |
| [Mille and Truffle](#6-mille-and-truffle) | Mille merged and pinned; truffle-core 0.7.20 is on crates.io and #6 switches to it |

## 1. Leaks

- **Secrets.** gitleaks 8.30.1 (`gitleaks git --log-opts=--all`) over all 15
  commits on every branch: no leaks. `gitleaks dir` over the unpacked dry-run
  packages: no leaks.
- **Personal and internal data.** A search over every blob on every branch
  for home and volume paths, e-mail addresses, host and device names, tailnet
  names and addresses, session and workflow identifiers, and the private
  design notes the research cites (`initial-discussion.md`, not published).
  - Paths: only illustrative ones (`/Volumes/NAS/Movies`, `/path/to/…`) and,
    inside the binaries, the CI runner's `/Users/runner/.cargo/…`.
  - E-mail addresses: only `@example.com` test fixtures. Commit metadata
    carries the maintainer's public address, the same one Mille and Truffle use.
  - Tailnet: fixtures use `100.64.0.x`, `fd7a:115c:a1e0::x` and `tail1234` /
    `tail4e2b1` placeholders. The prototype's real tailnet name, device name
    and addresses appear nowhere.
  - The private design notes: no prose from them is published. The only shared
    strings are public URLs and one public fact.
  - What remains is technical context: benchmark caveats ("a shared
    machine"), the benchmark harness's pause file (`$FLUX_WORK/QUIET`) and
    benchmark result-file labels.
- **User-facing strings (fixed in #10).** One runtime message pointed at the
  prototype's development setup: a missing sidecar told users to "build it
  from the Truffle worktree's packages/sidecar-slim". A search of the server
  and web client strings found nothing else.

## 2. Fresh-machine build and tests

GitHub's runners are fresh machines. They have no `.cargo/config.toml`, no
`[patch]`, no `FLUX_TEST_MEDIA` and no Mille or Truffle checkout, and they
fetch `mille-core` and `truffle-core` from their pinned sources. On `main`
(CI run 39505740476):

- `cargo clippy --locked --workspace --all-targets -D warnings`,
  `cargo test --locked --workspace` and `cargo doc` (`-D warnings`) pass on
  Linux x86_64, Linux arm64 and macOS on Rust 1.99.1: 628 tests on Linux,
  0 failed. Every test that needs the media corpus skipped itself without
  failing. With #8, CI also proves the synthetic-clip tests ran (629 with
  the guard).
- MSRV: `cargo check --locked --workspace` on Rust 1.89.0.
- Web: `pnpm install --frozen-lockfile`, `typecheck`, `lint`
  (`--max-warnings 0`), `test` (130 tests in 11 files) and
  `VITE_FLUX_API=/ pnpm build`. The golden API samples match flux-core's
  serde output.

**Not done in this check:** a local build and test run from a fresh clone
on macOS, with and without the real-media corpus (`FLUX_TEST_MEDIA`). The
corpus isn't in the repository, so CI never runs those tests. The machine
wasn't available for heavy builds while this check ran. See
[Open items](#open-items), 9.

## 3. Workflows

Checked by reading every workflow and with zizmor 1.30.1 (auditor persona;
its online audits for impostor commits and known-vulnerable actions ran too).

- **Pinning.** Every third-party action is pinned to a full commit SHA with
  a version comment. actionlint and hadolint binaries are checksum-verified.
  Dependabot covers `github-actions`, cargo (both lockfiles) and npm.
- **Permissions.** Every workflow starts from `permissions: {}` or
  `contents: read`. Write scopes are per job: release assets, attestations,
  the ghcr.io push, and Release Please's PR and dispatch.
- **Injection.** No `github.event.*` field reaches a `run:` block. Inputs and
  step outputs go through `env:`. zizmor's two template-injection hints are
  `matrix.os`, a static list.
- **Triggers.** There is no `pull_request_target` and no `workflow_run`. The
  release only runs from `release-please.yml` when a release was created
  (`release_created`, which release-please-action also sets for drafts), or
  by hand through `workflow_dispatch`, which needs write access. Only
  `docker.yml` pushes an image, and only from `main` or a release.
- **Cache poisoning (fixed in #8).** The release and `:edge` builds restored
  Rust, pnpm and BuildKit caches written by earlier main-branch runs. Fork
  PRs can't write to main's cache scope, so the remaining risk was a
  compromised build dependency in any main-branch job. #8 removes caches
  from `build.yml` and the image action. `ci.yml` keeps its caches. The cost
  is time: a full dry run took 11 min instead of 6 (runs 39516075848 and
  39509607663).
- **A release without the sidecar (fixed in #8).** `fetch-sidecar.sh` only
  warns when it can't bundle the sidecar: truffle-core from git, no pinned
  checksum, or no release asset. Nothing in `release.yml` checked for it, so
  a release could have shipped four packages that can't run
  `--tailnet embedded`. `release.yml` now builds with `require-sidecar: true`,
  which fails the build before anything is uploaded, pushed or published.
  The dry run takes the same input to rehearse the check.
- **Real transcodes in CI.** The Linux x86_64 job log of CI run 39505740476
  shows libx264 clips being cut and checked
  (`libx264_transcode_pieces_start_on_idrs_and_keep_every_frame`,
  `vfr_transcode_keeps_every_frame`, `transcoded_play_of_a_generated_clip`),
  and `flux-server --check` picks libx264 after NVENC, QSV and VAAPI fail
  their probes. However, every one of those tests returns early, without
  failing, when ffmpeg or an encoder is missing, so a broken runner would have
  passed. **Fixed in #8:** CI sets `FLUX_TEST_REQUIRE_FFMPEG=1`, and
  `crates/flux-test-media/tests/required_tools.rs` then requires ffmpeg,
  ffprobe, every encoder the clips need, and all 17 clips.
- **Licence gate.** `deny.toml` has a closed allow-list (permissive licences
  plus MPL-2.0), no `allow-osi-fsf-free`, `confidence-threshold = 0.8`, and
  one exception (IJG for `jpeg-encoder`). A GPL, LGPL or AGPL crate matches
  nothing on the list and fails `cargo deny check licenses`. A dual licence
  such as `MIT OR GPL-2.0` passes, correctly, through its MIT branch.
  Tested with cargo-deny 0.20.2 and this `deny.toml` on a scratch
  workspace of path crates, which adds nothing to flux:
  `GPL-3.0-only`, `LGPL-2.1-or-later`, `AGPL-3.0-only` and
  `MIT AND GPL-2.0-only` are rejected, and `MIT OR GPL-2.0-only` passes.
  `scripts/package/web-licenses.mjs` applies the same rule to
  the npm packages in the web bundle and exits non-zero on anything else.
- **Dependabot cooldown (fixed in #8).** zizmor's only medium findings:
  version updates now wait 7 days after a release.
- **Ruleset `protect-main`.** Changes go through a PR. The aggregate
  `CI OK` check from GitHub Actions is required on an up-to-date branch.
  Force pushes and deletion are blocked, and admins can bypass only through a
  PR. With 0 required approvals, review is the maintainer's choice; require
  code-owner review once others have write access.

## 4. Release artifacts (dry run)

From dry run [39509607663](https://github.com/vibecook-dev/flux/actions/runs/39509607663)
on the #6 branch (`2298916`), artifact `release-dry-run`, downloaded with
`gh run download`:

| Check | Result |
|---|---|
| `shasum -a 256 -c SHA256SUMS` | 5/5 OK: the four packages and `jellyfin-ffmpeg-8.1.3-1-source.tar.gz` |
| Contents | Each package has `bin/flux-server`, `bin/sidecar-slim`, `share/flux/web/` (index.html and assets), `LICENSE`, `README.md` and `THIRD_PARTY_LICENSES` (which includes the sidecar's Go licences) |
| `file` | aarch64-apple-darwin: Mach-O arm64; x86_64-apple-darwin: Mach-O x86_64; x86_64-unknown-linux-gnu: ELF x86-64; aarch64-unknown-linux-gnu: ELF aarch64. The sidecars match, and are static on Linux |
| glibc (`objdump -T`) | Highest symbol version GLIBC_2.34 on both Linux targets, within the documented 2.35 |
| Sidecar | SHA-256 of each `bin/sidecar-slim` = the pin in `scripts/package/truffle-sidecar.sha256` (#6) = the truffle-v0.7.20 release's asset digest = Truffle's `sidecar-checksums.json` |
| `flux-server --version` / `--check` (macOS arm64, from the tarball) | `0.1.0`; exit 0, ffmpeg and ffprobe found, `h264_videotoolbox` and `libx264` test encodes pass |
| Attestations | Not applicable: a dry run makes none. `release.yml` attests every tarball (`actions/attest-build-provenance`) |
| Image labels | Checked in the log of the Docker run that pushed `:edge` / `sha-4b4626c` (run 39505740375), because the package is private: OCI title, description, licenses=MIT, source, revision, version and documentation. The Dockerfile adds `dev.vibecook.flux.ffmpeg` (jellyfin-ffmpeg, GPL-3.0-or-later). The smoke test checks that the image has `/usr/share/licenses/jellyfin-ffmpeg/copyright` and `SOURCE-OFFER.md` |
| README / INSTALL install steps | The layout (`bin/`, `share/flux/web`, sidecar next to the binary) and the asset names match. **One step was broken on macOS (fixed in #11):** INSTALL links `/usr/local/bin/flux-server` to `/opt/flux/bin/flux-server`. Started through that link, the macOS package served the API only, because `current_exe()` there is the symlink's path, so the web client and sidecar were looked up next to the link. Linux resolves `/proc/self/exe` and was fine |

The smoke tests missed that because they accepted any HTML for `GET /`, and
that includes the "flux is running" page served when the client isn't
found. #11 makes both smoke tests require the built client's
`<div id="root">` and starts the package smoke test through a symlink.
Against the unfixed macOS package, the new test fails as it should. With
the fix, it passes in release dry run 39517870199 (macOS arm64 started
through the symlink, `GET /` serving the client).

The packages also ship `share/flux/web/mockServiceWorker.js`, the web client's
mock-backend worker. A production build (`VITE_FLUX_API=/`) never registers
it, so it is dead weight, not a risk.

## 5. Docs

Badges and every link in the top-level docs resolve: a relative link and
anchor checker over every Markdown file, and HTTP checks of the external links.
The licence is MIT in `LICENSE`, the workspace (every crate inherits it),
`web/scripts/golden-rs` and `web/package.json`.

Fixed in this PR:

- README's status line said flux "has only been run on macOS", and its
  feature list that video transcodes go through VideoToolbox. CI builds and
  tests Linux x86_64 and arm64, and the runtime encoder choice
  (VideoToolbox, NVENC, QSV, VAAPI, libx264) is implemented. The "macOS
  only" bullet under Prototype gaps says the same, and also "Windows was
  never compiled" (it type-checks; docs/ENCODERS.md). That bullet sat next
  to the line #6 changed, so it was replaced after #6 merged
  ([Open items](#open-items), 2).
- README: "no packages, Docker image or service files" (they exist, with
  a systemd unit in INSTALL.md). Linux source builds need a C compiler and
  `make`. The Linux data dir is `~/.local/share/flux`.
- INSTALL linked `README.md#quickstart`, an anchor that doesn't exist.
- DECISIONS said "today's prototype encodes with `h264_videotoolbox` only".

Fixed in other PRs:

- #8: RELEASING, `ci.yml` and `release-please.yml` said Release Please's PR
  gets no `pull_request` runs. It gets them, waiting for approval
  (`github-actions[bot]` is a first-time contributor).
- #6: RELEASING calls the active ruleset `main`. It is
  `protect-main`, and an unrelated, inert ruleset really is named `main`
  ([Open items](#open-items)).

## 6. Mille and Truffle

- **Mille.** vibecook-dev/mille#43 is merged with a merge commit,
  `c9708f4`, which is Mille's `main`. `Cargo.toml` and `Cargo.lock` pin
  `mille-core` to that commit.
- **Truffle.** vibecook-dev/truffle#218 is squash-merged as `dff6033`.
  truffle-core 0.7.20 is on crates.io: published at 22:12 UTC, built from
  `7bc3bd1`, whose `crates/truffle-core` differs from `dff6033` only in its
  version line. `main` still takes truffle-core from git `dff6033` (with
  `TODO(truffle-0.7.20)`, listed in RELEASING.md). **#6** switches it to
  `truffle-core = "0.7.20"`: its Cargo.lock checksum matches crates.io's, and
  it pins the four sidecars. flux builds only truffle-core; neither the
  `truffle` crate nor `truffle-sidecar` is in `Cargo.lock`.
- **History.** No force push to `main` in either repository (GitHub's
  activity API). Force pushes went only to the PR branches flux used
  (`flux/dogfood`) and to Release Please and Dependabot branches. The only
  branches deleted were temporary CI probe branches made for #43.

## Open items

In order:

1. **Done:** the fix PRs merged in this order, each brought up to date with
   `main` first because the ruleset requires it:
   **#6** (truffle-core 0.7.20 from crates.io, sidecar pins), **#7** (bounded
   synthetic-clip generation), **#8** (release builds without caches, a
   required sidecar, CI that fails without ffmpeg), **#10** (the
   missing-sidecar message), **#11** (symlinked `flux-server` on macOS) and
   **#9** (this file and the doc fixes). None of them conflicted with each
   other (`git merge-tree`, pairwise).
2. **Done in this PR:** **README "Prototype gaps", first bullet**. It sits
   next to the line #6 changes, so it was replaced after #6 merged, with:
   ```markdown
   - **Platforms.** flux was developed and measured on macOS (Apple Silicon). CI also builds and tests it on Linux x86_64 and arm64, where video transcodes use libx264. The Linux hardware encoders (NVENC, Quick Sync, VAAPI) are detected at runtime ([docs/ENCODERS.md](docs/ENCODERS.md)) but not yet tested on real GPUs. Windows compiles but is not supported.
   ```
3. **Done: v0.1.0-alpha.1 is out.** Release Please's PR #1 was
   squash-merged as `f5b29ac` (2026-10-21, 05:22 UTC). Release Please
   tagged `v0.1.0-alpha.1` at that commit and called `release.yml` in run
   [39549172609](https://github.com/vibecook-dev/flux/actions/runs/39549172609):
   12 jobs passed, and *Release / Docker / Build* was skipped as designed
   (the release builds the image from its own packages). The run published
   <https://github.com/vibecook-dev/flux/releases/tag/v0.1.0-alpha.1> as a
   pre-release at 05:32:50 UTC.
4. **Done: after the release.** The checks, from a fresh clone of the tag
   and the downloaded assets:
   - `gh release view v0.1.0-alpha.1`: a pre-release, not a draft, target
     `f5b29ac`, with the four packages,
     `jellyfin-ffmpeg-8.1.3-1-source.tar.gz` and `SHA256SUMS`.
     `gh api repos/vibecook-dev/flux/releases/latest` returns 404: an alpha
     is not the Latest release, as intended. `shasum -a 256 -c SHA256SUMS`
     passes, and each package's `bin/sidecar-slim` matches its pin.
   - `gh attestation verify <tarball> --repo vibecook-dev/flux` passes for
     all four packages, also with
     `--signer-workflow vibecook-dev/flux/.github/workflows/release.yml` and
     `--source-digest` set to the tagged commit. `--source-ref
     refs/tags/v0.1.0-alpha.1` fails, because the attestation records
     `refs/heads/main` (Release Please runs the release from the push to
     `main`); INSTALL.md now shows the stronger command.
   - The image, read through the anonymous registry API instead of
     `docker buildx imagetools inspect` (no Docker daemon was used):
     `ghcr.io/vibecook-dev/flux:0.1.0-alpha.1` is the index
     `sha256:f2c7e6d739f6e613c428c58f80faf91980c7866bd15b6a235fcb47493dd4c62e`,
     with linux/amd64 (`sha256:05e192e6…`), linux/arm64 (`sha256:7a0203f0…`)
     and their SBOM and provenance attestation manifests. The labels give
     version `0.1.0-alpha.1` and revision `f5b29ac`, and the image runs as
     1000:1000. `gh attestation verify
     oci://ghcr.io/vibecook-dev/flux:0.1.0-alpha.1 --repo vibecook-dev/flux`
     passes with no login, because the package is public. There is no
     `latest` tag, as intended for a pre-release, and `edge` is `f5b29ac`
     too.
   - The packages run. `flux-server --version` prints `0.1.0-alpha.1`.
     `scripts/package/smoke-test.sh` passes on the aarch64-apple-darwin
     package, started through a symlink, and INSTALL's release steps work on
     macOS (Apple Silicon). The x86_64-apple-darwin package, which CI
     didn't smoke-test until #14, answers `/api/v1/server` and serves the
     web client under Rosetta.
   - README's Status, Install and Operations lines now describe the
     release (#12; see [After the release](#after-the-release)).
5. **Done:** **Make the container package public** (org Packages → flux →
   Settings → Change visibility). Anonymous pulls now succeed.
6. **The ruleset named `main`** was added by the owner to protect `main`, but
   its include list is empty, so it applies to nothing. `protect-main` is the
   one in force on the default branch.
7. **Dependabot PRs #2–#5** were opened against the initial commit and
   failed CI. After the release, `gh pr checks` shows `CI OK` passing on #2,
   #3 and #4. #5 (msw 2.15.0 → 3.0.3) fails its `Web` job: fix or close it.
8. Optional hardening: turn on *Require actions to be pinned to a full-length
   commit SHA* (Settings → Actions); everything already complies.
9. **Run the corpus tests** from a fresh clone. Use no `.cargo/config.toml`
   and a new `CARGO_HOME` and target dir:
   - `cargo build --locked --workspace` and `cargo test --locked --workspace`
     without `FLUX_TEST_MEDIA` (the corpus tests must skip cleanly);
   - then `cargo test` again with `FLUX_TEST_MEDIA` pointing at the corpus;
   - the web gate.

   Check that the run modifies nothing under the corpus.
10. Known product gaps, already documented elsewhere:
    - VideoToolbox is verified only on Apple hardware: macOS CI uses libx264
      (ENCODERS.md).
    - flux-net's sidecar lookup has no `.exe` on Windows.
    - The packages ship the unused `mockServiceWorker.js`.

## After the release

Checked on 2026-10-21 against the published v0.1.0-alpha.1 (the tag and
`main` both at `f5b29ac`), with the results in [Open items](#open-items) 3
and 4.

Fixed in #12 (docs only):

- README said there were no releases yet (Status, Install, Operations) and
  never linked INSTALL.md from its Install section.
- INSTALL's systemd unit reads `/etc/flux.toml`, which no step created.
  `flux-server --config <missing file>` exits 2, so the service could not
  start. INSTALL now creates the file, with libraries and `lan`, and
  the unit sets `NO_COLOR=1` so the journal gets no colour codes.
- Running INSTALL's steps again for a new version moved it *into*
  `/opt/flux` and kept the old binary on `PATH`. INSTALL now has an
  Upgrading section.
- INSTALL promised macOS 11 for the whole package, but the bundled sidecar
  needs macOS 13 (below).
- Smaller fixes: INSTALL's attestation command now checks the workflow and
  the commit; the macOS quarantine note comes before the install steps;
  INSTALL no longer says ffmpeg goes "next to" `flux-server`, a place flux
  never looks; RELEASING's manifest line and its description of the image
  smoke test (linux/amd64 only); and SECURITY's supported versions now
  include pre-releases.

Fixed for v0.1.0-alpha.2 in #14 (release pipeline; `ci:`, `build(docker):`,
`chore(deps):` and `docs:` only, so it makes no release on its own):

- **A manual Release run could overwrite a published release.** Run by
  hand, `release.yml` rebuilt everything and replaced the release's assets
  with `gh release upload --clobber`, whatever state the release was in.
  v0.1.0-alpha.1 was published before immutable releases were turned on
  (the API reports `"immutable": false` for it), so nothing else protected
  its assets. Its first job now fails, before anything is built, unless the
  release is still a draft, and says to release the next version instead.
  Re-running it for a draft, the documented recovery, still works
  (RELEASING.md, *When something fails*).
- **The x86_64-apple-darwin package was never smoke-tested** in CI
  (`native: false` in `build.yml`; the check above ran it under Rosetta by
  hand). `build.yml` now runs `smoke-test.sh` on it under Rosetta 2 on the
  macos-15 arm64 runner, after showing that `arch -x86_64` runs translated
  there (installing Rosetta if the runner lacks it).
- **Only the linux/amd64 image was smoke-tested.** The arm64 image only
  ran `--version` under QEMU. It is now built and smoke-tested on an
  ubuntu-24.04-arm runner, and the multi-arch image takes linux/arm64 from
  that tested build's layers, without QEMU; the job fails if its amd64
  runner could emulate arm64, so a step missing from the layers can't be
  rebuilt untested. `docker.yml` (and so the release) and the release dry
  run both get this through `.github/actions/flux-image`.
- **No check of the minimum macOS.** `smoke-test.sh` now reads the
  `LC_BUILD_VERSION` `minos` of `bin/flux-server` and `bin/sidecar-slim`
  (`otool -arch all -l`, every slice of a universal binary) and fails above
  11.0 and 13.0, the versions INSTALL.md promises. The script, INSTALL's
  requirements and RELEASING's target table point at each other.
- **Attestation warnings.** The image's attest step warned twice on every
  push (release run 39549172609: "Failed to create storage record … no
  artifacts found", then "Please check that the "artifact-metadata:write"
  permission has been included"). The packages' attest step didn't warn:
  `actions/attest` only makes storage records for an image pushed to a
  registry. The record only feeds the organization's Linked Artifacts page,
  so both attest steps now set `create-storage-record: false` instead of
  getting another write permission.
- **The image's OCI vendor label was `vibecook`**, an unrelated GitHub
  organization. It is `vibecook-dev` now, in the `Dockerfile` and the
  image action's labels.
- **Dependabot.** Every bump was titled `chore(deps)`, so no dependency
  fix ever made a release, and the Debian base image digest wasn't
  watched: it sat in an `ARG`, which Dependabot doesn't read. Cargo
  (workspace), npm production and Docker bumps are now `fix(deps):`, which
  makes a release; npm development bumps are `chore(deps-dev):`,
  `web/scripts/golden-rs` (a dev tool, not shipped) `chore(deps):` and
  GitHub Actions `ci(deps):`. The `Dockerfile` names the base image in its
  `FROM` lines, and a `docker` entry watches it. Dependabot has no
  development dependencies for cargo, so workspace dev-dependency bumps
  make a release too.
- **RELEASING.md listed `deps:` as making a release.** It doesn't:
  `release-please-config.json` sets no `changelog-sections`, and Release
  Please's default types have no `deps`, so a `deps:` commit makes no
  release and stays out of the changelog. RELEASING.md now says so, and
  Dependabot uses `fix(deps):` instead. (Adding a visible `deps` section
  to `changelog-sections` would make `deps:` release, if that is ever
  wanted.)

Fixed for v0.1.0-alpha.2 in #13 (`fix:`): the licence item (it completes
`THIRD_PARTY_LICENSES`) and the log-colour item (flux colours its log only
when stderr is a terminal, so the journal gets plain text). See that PR.

Still open:

- **The macOS sidecar needs macOS 13.** Truffle 0.7.20's macOS
  `sidecar-slim` is built with Go 1.27.2 and declares `minos 13.0`
  (`otool -l`), while `flux-server` declares `minos 11.0`. Go 1.27 requires
  macOS 13 Ventura or later (its release notes), so `--tailnet embedded`
  needs macOS 13 even though the server runs on 11. The docs now say so.
  How the sidecar fails on macOS 11 or 12 was not tested (no such machine).
  To support them, Truffle would have to build its macOS sidecars with
  Go 1.24 (macOS 11) or Go 1.26 (macOS 12). Since #14 the package smoke
  test fails on the next rise.
- **The README inside the v0.1.0-alpha.1 packages and image** is the old one
  that says there are no releases. The published assets stay as they are;
  v0.1.0-alpha.2 will carry the new README.
- **README's Operations line** says CI smoke-tests every package but
  `x86_64-apple-darwin`, and only the linux/amd64 image. After #14 it
  smoke-tests all four packages and both image platforms. README isn't
  in #14, and it ships in the packages and the image, so it needs a
  one-line fix before the v0.1.0-alpha.2 release PR merges.
