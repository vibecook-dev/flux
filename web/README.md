# flux web client

React 19 + TypeScript + Vite. Talks the flux HTTP API (`crates/flux-core/src/api.rs`); plays with hls.js (MSE) or native HLS.
Design notes, numbers and gaps: [`docs/prototype/web.md`](../docs/prototype/web.md).

```sh
pnpm install
pnpm dev                                   # mock backend (no server needed) → http://localhost:5173
VITE_FLUX_API=http://127.0.0.1:8096 pnpm dev   # real flux-server; /api and /media are proxied
VITE_FLUX_API=/ pnpm build                 # build for flux-server to serve from web/dist (mock code dropped)

pnpm typecheck && pnpm lint && pnpm test && pnpm build   # quality gates
```

Mock backend extras:

| | |
|---|---|
| `pnpm fixtures` | rebuild `src/mock/fixtures.json` from `library.manifest.jsonl` + the feature clips' ffprobe facts |
| `pnpm mock-media` | build local HLS renditions of the feature clips (served at `/mock-media` by dev/preview) |
| `?mock_tailnet=running` | start with the server already on a tailnet |
| `?mock_path=tailnet-direct` | how the "server" says you reached it |
| `?mock_latency=0` | simulated API latency in ms (default 25) |

Checks and benchmarks (need Google Chrome; driven by `playwright-core`, no browser download):

```sh
pnpm golden                               # regenerate src/api/__golden__ from flux-core (cargo)
pnpm build && pnpm preview --port 4199 &
BASE=http://localhost:4199 pnpm screens   # every screen × dark/light × desktop/phone
BASE=http://localhost:4199 pnpm bench:grid
BASE=http://localhost:4199 pnpm bench:playback
pnpm sizes                                # gzip/brotli bundle report
```

Results land in `$FLUX_WORK/bench/results/flux-web-*.json` (or `$RESULTS`); screenshots in `$FLUX_WORK/bench/web-screens/` (or `$OUT`).
`FLUX_WORK` is a scratch directory and `FLUX_TEST_MEDIA` the test corpus; the scripts stop with a message when one they need is unset.
See [CONTRIBUTING.md](../CONTRIBUTING.md) for both.
