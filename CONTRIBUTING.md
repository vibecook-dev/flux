# Contributing to flux

Thanks for your interest in flux. It is an early research prototype (see the [README](README.md#feature-status)), so expect rough edges and large changes. Bug reports, measurements on hardware we haven't tried, and focused pull requests are all welcome. For anything large, open an issue first so we can agree on the approach.

By contributing, you agree that your contributions are licensed under the [MIT licence](LICENSE), like the rest of flux. Everyone taking part is expected to follow the [Code of Conduct](CODE_OF_CONDUCT.md). Report security issues privately, as described in [SECURITY.md](SECURITY.md), never in a public issue.

## Prerequisites

- Rust stable (the workspace's minimum is the `rust-version` in `Cargo.toml`), with `clippy` and `rustfmt`.
- `ffmpeg` and `ffprobe` on `PATH`, for transcode, poster and probe-fallback tests.
- Node.js and pnpm (the version pinned in `web/package.json`'s `packageManager`), for the web client.
- Go, only to build Truffle's `sidecar-slim` for embedded remote access.

## Build, test and lint

From the repo root:

```sh
cargo build --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

The web client:

```sh
cd web
pnpm install --frozen-lockfile
pnpm typecheck && pnpm lint && pnpm test
VITE_FLUX_API=/ pnpm build     # → web/dist, which flux-server serves
```

CI runs the same checks on every pull request. New stable Rust releases sometimes add lints, so if CI's clippy fails on code that is clean locally, update your toolchain (`rustup update stable`) and run clippy again.

Some tests are `#[ignore]`d on purpose because they need a network service, a key, a running `tailscaled` or a long time: live TMDB, live Ollama, this machine's `tailscaled`, and the probe's deep fuzz. Run them explicitly with `cargo test -p <crate> -- --ignored <name>` when you have what they need.

## Local Mille and Truffle checkouts

flux uses two libraries by the same maintainer: [Mille](https://github.com/vibecook-dev/mille) (file walking and watching) and [Truffle](https://github.com/vibecook-dev/truffle) (embedded Tailscale). `Cargo.toml` takes Mille as a git dependency on `vibecook-dev/mille`, and Truffle's `truffle-core` from a pinned commit of `vibecook-dev/truffle` until truffle-core 0.7.20 is on crates.io (then from crates.io).

To work on flux against local checkouts of either, override them in a `.cargo/config.toml` at the repo root. That file is in `.gitignore`, so it never gets committed. Start from [`.cargo/config.toml.example`](.cargo/config.toml.example), or write:

```toml
# .cargo/config.toml (untracked). Relative paths are resolved from the repo
# root; these assume mille/ and truffle/ are checked out next to flux/.

[patch."https://github.com/vibecook-dev/mille"]
mille-core = { path = "../mille/crates/mille-core" }

# While Cargo.toml pins truffle-core to a git commit:
[patch."https://github.com/vibecook-dev/truffle"]
truffle-core = { path = "../truffle/crates/truffle-core" }

# Once truffle-core comes from crates.io, use this instead:
# [patch.crates-io]
# truffle-core = { path = "../truffle/crates/truffle-core" }
```

Then `cargo build` uses your checkouts. Check `cargo tree -i mille-core` and `cargo tree -i truffle-core` to confirm that it does: a crates.io patch must carry a version that satisfies `Cargo.toml`'s requirement, or Cargo ignores the patch and prints a warning. Don't commit the `Cargo.lock` changes a patch causes. Changes to Mille or Truffle themselves go to those repositories as their own pull requests.

The same file can move the build output elsewhere with `[build] target-dir = "..."`, as the example shows. `CARGO_TARGET_DIR` overrides it. pnpm settings such as `store-dir` go in an untracked `web/.npmrc`; see [`web/.npmrc.example`](web/.npmrc.example).

## Media-corpus tests

Many tests, examples and benchmarks need real media files. They read the corpus from the directory named by `FLUX_TEST_MEDIA`. When it is unset or the files are missing, those tests skip themselves, so `cargo test` passes without a corpus but covers less.

The corpus layout:

| Path under `$FLUX_TEST_MEDIA` | What it is | How to make it |
|---|---|---|
| `features/` | Feature clips covering containers, codecs, audio, subtitles and HDR, with `manifest.json` | `tools/corpus/gen-features.sh "$FLUX_TEST_MEDIA/features"` |
| `library/`, `library.manifest.jsonl` | The 16.7k-file messy library and its ground truth | `tools/corpus/gen-library.py --out "$FLUX_TEST_MEDIA/library"` |
| `library-tiny/`, `library-tiny.manifest.jsonl` | A 424-file version of the library | `tools/corpus/gen-library.py` with a smaller `--movies` / `--shows` |
| `seeds/` | The seed clips the library is cloned from | Made by `gen-library.py` |
| `probe-fixtures/` | Edge-case files for the probe | `crates/flux-probe/tests/gen-fixtures.sh "$FLUX_TEST_MEDIA/probe-fixtures"` |

Everything is generated from ffmpeg's test sources, so the corpus is reproducible and contains no copyrighted material. `gen-library.py` makes APFS clones on macOS, so the library costs almost no disk there; elsewhere it copies the small seed clips. Then:

```sh
export FLUX_TEST_MEDIA=/path/to/flux-media
cargo test --workspace
```

The benchmark and end-to-end harnesses (`tools/bench/`, `web/scripts/`) also need `FLUX_WORK`, a scratch directory for results, clones and temp files. `FLUX_TEST_MEDIA` defaults to `$FLUX_WORK/media` there. They stop with a message naming the variable when one they need is unset. Treat the corpus as read-only: harnesses that modify a library work on a clone under `$FLUX_WORK`.

## Commits and releases

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/):

```
feat(scan): pick up renames from the FSEvents journal
fix(hls): keep audio in sync after a seek past the last keyframe
docs: explain FLUX_TEST_MEDIA
```

Use `feat:` for user-visible features, `fix:` for bug fixes, and `docs:`, `test:`, `refactor:`, `perf:`, `ci:` or `chore:` for the rest. Mark breaking changes with `!` (`feat(api)!: …`) or a `BREAKING CHANGE:` footer. The optional scope is usually the crate without its `flux-` prefix (`scan`, `hls`, `server`) or `web`.

Releases are automated with [release-please](https://github.com/googleapis/release-please). It reads the commit messages on `main`, keeps a release pull request open with the next version and the changelog, and tags a release when that pull request is merged. So the commit messages that land on `main` are the changelog. When a pull request is squash-merged, its title becomes the commit message, so make the title a Conventional Commit too.

## Pull requests

- Keep each pull request to one change, and add or update tests with it.
- Run the checks above before you push.
- Update the docs when behaviour changes: the README for users, [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the contract between crates.
- If a change affects a published number (scan time, time to first segment, memory), say how you measured it.
