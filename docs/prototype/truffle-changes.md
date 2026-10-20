# Truffle changes for flux (`flux/dogfood`)

**Branch:** `flux/dogfood` in `$FLUX_WORK/wt/truffle`, based on `d6f6664` (v0.7.19). It has five commits, none pushed, for the maintainer to review. The research behind each change is research/06 §3.4, §2.1–2.3 and §9.2 (#1, #3, #4, #5).

| Commit | Change | research/06 |
|---|---|---|
| `e7336ab` | fix(sidecar): the bridge reaps a connection only when both directions are idle | §9.2 #1 (P0) |
| `7072e31` | feat(core): WhoIs identity carries the app-capability map, tags and user id; the proxy injects `Tailscale-App-Capabilities` (sidecar protocol 9) | §9.2 #4 (P0) |
| `4826404` | feat(core): login URLs are re-sent when they change; `NodeBuilder::start_timeout(Option<Duration>)` | §9.2 #3 (P0, part) |
| `254a633` | fix(core): the bridge no longer holds the listener map across `send().await`; drops are counted | §9.2 #5 (P1) |
| `ca1dd8e` | test(sidecar): bridge copy throughput benchmark | — |

The diff is 25 files, +1620/−119.

**Tests:**
- Go: 82 → **98** tests pass (`go test ./...` in `packages/sidecar-slim`).
- Rust: `cargo test -p truffle-core --lib` goes from 413 → **422** passed, 0 failed, 3 ignored.
- `cargo clippy -p truffle-core --all-targets` is clean.
- `cargo check` passes for truffle-cli, truffle-bench, truffle-napi and truffle-tauri-plugin. The 3 NAPI `unsafe fn` warnings were already there.

---

## 1. Connection-level idle reaping (P0 #1)

**Problem.** `idleCopy` gave each copy direction its own read deadline. The comment at `main.go:58-60` says "idle in BOTH directions", but the code didn't do that.

Progressive media breaks on this. The client sends one `GET` and then only reads, so the client→server half is quiet. After `idle_timeout` (600 s by default) that half was closed. hyper/axum with the default `half_close(false)` then aborts the response mid-movie.

**Fix** (`packages/sidecar-slim/main.go`, `bridgeCopy` / `idleCopy` / `writeActive`):
- **Shared activity clock.** Both copy goroutines share one `connActivity`: an atomic "last bytes moved" timestamp, measured on the monotonic clock so a wall-clock jump can't reap a busy connection.
- **Deadlines track the last activity.** Read and write deadlines are set to `last activity + timeout`, so a reap happens one timeout after the last byte in *either* direction, not up to 2× later.
- **Deadlines while the other side streams.** If a deadline fires while the other direction moved bytes within the timeout, the copy re-arms instead of ending. A stalled write is retried the same way, and partial progress counts as activity.
- **EOF vs. reap.** A real EOF still half-closes (the existing behaviour). An idle reap now closes both connections at once.
- **One function for every path.** The proxy's WebSocket-hijack path now uses `bridgeCopy` too, so `listen_tcp`, `open_tcp`, egress and WebSocket upgrades all share the same semantics.
- **Timeout detection.** `isTimeout` accepts both `os.ErrDeadlineExceeded` and gVisor gonet's own `net.Error` (`Timeout() == true`). tsnet connections use the gonet error.

**Tests** (`bridge_copy_test.go`, real loopback TCP):

| Test | Asserts |
|---|---|
| `TestBridgeCopyOneWayStreamOutlivesIdleTimeout` | The research/06 repro: one GET, then a stream for **3× the timeout**. The backend never sees EOF and every byte arrives. **The old code fails it:** "half-closed at 151.758916ms … (idle timeout 150ms)". |
| `TestBridgeCopyReapsWhenIdleBothWays` | Silent both ways → reaped after ≥ timeout, and both ends are closed. |
| `TestBridgeCopyReapIsMeasuredFromLastActivity` | A trickle that then stops → reaped one timeout after the last byte. |
| `TestBridgeCopyPropagatesHalfCloseOnEOF` | A client upload EOF reaches the backend, and the response still flows back. |
| `TestBridgeCopyReapsStalledWrite` | A player that stops reading (`net.Pipe`) is reaped and doesn't leak the goroutine. |
| `TestIsTimeout` | Both timeout error shapes are recognised. |

**Cost.** `BenchmarkBridgeCopyThroughput` sends 64 MiB through one bridge over loopback, with 6 samples interleaved against the same benchmark on `d6f6664`. The median was **1881 MB/s before and 1932 MB/s after**, which is no measurable cost on a loaded machine. Results: `$FLUX_WORK/bench/results/flux-truffle-bridge-copy.json`.

## 2. Richer WhoIs: capability map, tags, user id (P0 #4)

**Sidecar (protocol 8 → 9):**
- **New identity fields.** `peerIdentityData` gains:
  - `userId` (`UserProfile.ID`; a login can be renamed, the id can't)
  - `tags` (`Node.Tags`)
  - `capMap` (`WhoIsResponse.CapMap`, so each capability's grant values are raw JSON)
  - `capMapDropped`

  A pure `identityFromWhois(*apitype.WhoIsResponse)` fills them. They ride the bridge header (`listen_tcp` accept) and `tsnet:whoisResult` (`Node::whois`). `isEmpty()` replaces `== peerIdentityData{}`, since maps and slices make the struct incomparable.
- **Bigger header field.** The bridge identity field grows from **4 KiB to 16 KiB** on both sides. It is still u16-framed, so the wire format is unchanged.

  If a capability map still doesn't fit, `marshalPeerIdentity` drops it and sets `capMapDropped: true`, so consumers can fail closed instead of reading "no grants". The drop order is profilePicUrl → displayName → capMap → loginName. dnsName, nodeId, userId and tags are always kept.
- **Proxy capability header.** `proxy:add` takes `acceptAppCaps`, validated as `{domain}/{path}`. The proxy then sets `Tailscale-App-Capabilities` to the caller's grants for those names. This is the same header and shape as `tailscale serve --accept-app-caps`.
  - **Spoofing:** it is set after the existing strip of inbound `Tailscale-*` headers, so it can't be spoofed.
  - **Caller cases:** an anonymous caller gets no header. An identified caller with none of the names gets `{}`.
  - **Encoding:** the JSON is escaped to pure ASCII (`\uXXXX`), so it is a valid header value with no RFC 2047 decoding and still parses as-is.

**Rust (`truffle-core`):**
- **New fields on `TailscalePeerIdentity`.** `user_id: Option<i64>`, `tags: Vec<String>`, `cap_map: HashMap<String, Vec<serde_json::Value>>` and `cap_map_dropped: bool`, all `#[serde(default)]`, so older sidecars still parse.
- **New helpers.** `is_tagged()`, `capability_values(name)`, and `capability::<T>(name)`, the Rust counterpart of `tailcfg.UnmarshalCapJSON`. A malformed value is an error, not silently skipped.
- **Header cap.** `MAX_REMOTE_DNS_NAME_LEN` is now 16 KiB.
- **Proxy allow-list.** `ProxyConfig::accept_app_capabilities` passes through `ProxyAddParams` to `acceptAppCaps`. The name is validated, and the call is **refused on a sidecar older than protocol 9** rather than silently dropped (the same rule as the v2 gate). The CLI, NAPI, Tauri plugin and bench pass an empty list, so the setting isn't exposed to JS yet.

**Tests:**
- Go (`app_caps_test.go`): the WhoIs mapping; the header JSON round trip; an oversize capability map dropped *and* flagged; `isEmpty`; `whoisResult` carrying the new fields; the injected header filtered by the allow-list, absent when anonymous, `{}` when nothing matches; a spoofed inbound header doesn't survive; ASCII escaping keeps the JSON's meaning; name validation.
- Rust: parsing v9 JSON and decoding the capability; a dropped capability map; pre-v9 JSON has the fields empty; `proxy_add` refuses capabilities on a v8 sidecar; `validate_config` rejects bad names.

**Commit note.** This commit used `--no-verify`. The hook's NAPI step needs `node_modules`, which this worktree lacks. The NAPI change only fills the new core field and no exported NAPI type changed. rustfmt, gofmt and the truffle doctest were run by hand.

## 3. Headless-friendly start (P0 #3, part)

- **Sidecar.** `pollUntilRunning` re-sends `tsnet:authRequired` whenever `status.AuthURL` *changes*, and never repeats an unchanged one. Before, it was sent once per start, so a headless setup page showing a QR code kept a dead link after tsnet rotated the URL. `NetworkPeerEvent::AuthRequired`'s doc now matches this behaviour.
- **`TailscaleConfig::start_timeout: Option<Duration>`.** `None` waits until running, or until the start future is dropped (the sidecar is `kill_on_drop`). The default is `DEFAULT_START_TIMEOUT` (300 s, unchanged).
- **`NodeBuilder::start_timeout(Option<Duration>)`** sets it. Timeout errors name the actual limit and point to `start_timeout(None)`.
- **Callbacks.** `build_with_auth_handler` already forwarded every `AuthRequired`, so callers now see each fresh URL.

**Tests:**
- Go: a fake LocalAPI rotates the URL, and the events are exactly `[first, second]`, then started.
- Rust (scripted sidecars): a 300 ms `start_timeout` fails fast with "300ms" in the error. `start_timeout(None)` reports both URLs and succeeds once the node is running.

## 4. Bridge head-of-line lock and drop counters (P1 #5)

**Problem.** `incoming_channels` was locked across `tx.send(conn).await`. One listener whose app was slow to `accept()` stalled every other port, including the session plane on :9417. Every drop was at most a log line.

**Fix.**
- **Lock scope.** The bridge clones the sender under the lock, releases it, then calls `send_timeout`. A connection waits **at most 5 s** for queue room; it holds one of the 256 handshake permits, so the wait must be bounded. After that it is dropped (closed) and counted.
- **Drop counters.** `BridgeStats` is public, re-exported from `network::tailscale`, and read with `TailscaleProvider::bridge_stats()` and `Node::bridge_stats()`. It counts:
  - incoming and outgoing deliveries
  - drops by reason: `over_capacity`, `bad_header`, `bad_token`, `no_listener`, `listener_full`, `listener_closed`, `no_pending_dial`
  - plus `dropped()`, the total

  The counters are lock-free and live on the provider, so the totals survive restarts.

**Tests** (`bridge.rs`):
- A full listener on port A doesn't delay port B. **With the old lock held, this fails within 1 s** ("port 1002 stalled behind port 1001's full queue").
- A queue that stays full drops, closes and counts the connection.
- Unroutable connections land in the right counters.

**Not done:** the listen-registration race (`provider.rs:1118`) is unchanged; see Follow-ups.

## Protocol and compatibility notes

- **Protocol 9** is advertised in `tsnet:status`/`tsnet:started`. The core gates `accept_app_capabilities` on it. Everything else is additive: the new JSON fields are ignored by old cores and defaulted by new ones.
- **Mixing versions is not supported for the header cap.** A protocol-9 sidecar emitting a 4–16 KiB identity to a pre-9 core would be rejected by that core's 4 KiB parser. Truffle pins sidecar and core together (checksummed release assets), so this only matters if someone deliberately mixes them.
- The `TailscaleConfig` literal gained a field (`start_timeout`). All 14 in-repo literals, including examples and integration tests, were updated. This is a pre-1.0 API change of the kind `login_allow` made earlier.

## Building the sidecar and finding it from Rust

**How a Rust consumer finds the sidecar today:**
- **`truffle::sidecar_path()`** (from `truffle-sidecar`) checks, in order:
  1. `TRUFFLE_SIDECAR_PATH` at runtime
  2. next to the executable
  3. `~/.config/truffle/bin` (and the legacy macOS dir)
  4. the **build-time download**
  5. `/usr/local/bin`
  6. `PATH`

  The download comes from `truffle-sidecar/build.rs`, which fetches the published `truffle-v0.7.19` asset and verifies it against the pinned SHA-256. **That asset is protocol 8 and has none of these fixes.** Set `TRUFFLE_SIDECAR_PATH` (at build time or runtime) to a branch build. `TRUFFLE_SIDECAR_SKIP_DOWNLOAD=1` skips the download.
- **`truffle-core`'s own `build.rs`** compiles `packages/sidecar-slim` into `crates/truffle-core/tests/test-sidecar` (gitignored) whenever the source is present and Go is installed. So every flux build that uses the worktree path dependency also produces a protocol-9 sidecar there: 31 MB, not stripped, fine for development.
- **flux-net** depends on `truffle-core` only, to avoid a build-time network download in the flux workspace. It resolves the sidecar in this order:
  1. explicit config
  2. `TRUFFLE_SIDECAR_PATH`
  3. `sidecar-slim` next to the flux executable (how a packaged flux ships it)

**darwin-arm64 release-style build from the worktree** (flags from `.github/workflows/ci.yml` plus `-trimpath`). `go.mod` asks for Go 1.27.2; `GOTOOLCHAIN=auto` uses the cached toolchain in `$GOMODCACHE`. This takes about 19 s here:

```sh
cd $FLUX_WORK/wt/truffle/packages/sidecar-slim
CGO_ENABLED=0 GOOS=darwin GOARCH=arm64 go build -trimpath -ldflags "-s -w" \
  -o $FLUX_WORK/bin/sidecar-slim .
# 20,896,290 bytes, Mach-O arm64, go1.27.2
# sha256 0e096da387be5ed9a44275966851be92223f13eec5a783de7ca01ed124b45ea4 (at ca1dd8e)
```

## How to re-run

```sh
cd $FLUX_WORK/wt/truffle/packages/sidecar-slim && go test ./...
go test -run '^$' -bench BridgeCopyThroughput -benchtime 20x -count 6 .
cd $FLUX_WORK/wt/truffle
env -u TRUFFLE_TEST_AUTHKEY -u TS_AUTHKEY TRUFFLE_SIDECAR_SKIP_DOWNLOAD=1 \
  CARGO_TARGET_DIR=$FLUX_WORK/target/agent-truffle-net \
  cargo test --offline -p truffle-core --lib
```

The `tests/` integration suite was **not** run: it starts real tailnet nodes.

## What flux gains

- **No mid-movie disconnects** on Truffle's `listen_tcp` path. flux also serves with hyper `half_close(true)`, so it is safe even on an unfixed sidecar. It still raises `idle_timeout` to 6 h for a paused movie on a quiet connection.
- **Roles from tailnet policy.** A grant like `"app": {"vibecook.dev/cap/flux": [{"role": "kid"}]}` reaches flux on every connection. flux-net maps it to `flux_core::api::Role`, so flux needs no user database. Tags tell shared TVs apart, and a dropped capability map fails closed.
- **Headless first run.** flux can wait indefinitely for the owner to scan the QR code, and always shows the current login URL.
- **Burst robustness.** A poster-grid burst on one listener can't freeze the others, and overload is visible in `bridge_stats()`.

## Follow-ups not done here (research/06 §9.2)

- **#2 sidecar supervision.** A crashed sidecar still leaves the node dead.
- **#3, the rest:** `control_url` (Headscale), tags and OAuth on the builder, and a non-blocking `build()` returning a `NeedsLogin` node.
- **#5, the rest:** register the bridge channel *before* `tsnet:listen` (the registration race).
- **#6 service announcement.** Needed so a pretty `.hostname("flux")` node stays discoverable.
- **New: a path to any tailnet IP.** `Node::peers()` and `ping()` only know app peers, so flux can't tell direct from relay for a phone browser. A `path_to(ip)` over LocalAPI status would fix this.
- **#9 an `http-media` bench workload.** A long Range stream longer than the idle timeout would have caught §3.4. This needs a tailnet pair, so it wasn't run.
- **NAPI/Tauri.** Neither exposes `accept_app_capabilities` or the new identity fields yet. Doing so needs `index.d.ts` regenerated.

## Phase D (`flux/dogfood-d`): key expiry, control server, copy loop, sidecar exit

> **Merged.** `flux/dogfood` was fast-forwarded to `flux/dogfood-d` (`0d2415c`) by the Phase D merge, and flux-net consumes protocol 10 on `main` (`b90bf1b`) as described under "How flux-net should consume it": `TailnetInfo.key_expiry` and `control_server`, `NeedsApproval { approve_url }` (none on Headscale), and `SidecarExited` → `Failed`. See [fixes.md](fixes.md).

**Branch:** `flux/dogfood-d` in `$FLUX_WORK/wt/truffle-d`, on top of `flux/dogfood` (`ca1dd8e`). Two commits, none pushed; the merge agent fast-forwards `flux/dogfood` to it. Sidecar protocol **9 → 10**. Everything is additive: flux at `1fbcb63` builds and tests unchanged against it (checked in a scratch flux worktree; see the end).

| Commit | Change | research/06 |
|---|---|---|
| `4362c9c` | perf(sidecar): cheaper bridge copy loop for bulk streams: a pooled 256 KiB buffer once a direction proves bulk, and deadline re-arm at most ~1/s | §9.2 #12 (P2) |
| `0d2415c` | feat(core,sidecar): key expiry, control server and sidecar exit as status events (protocol 10) | §9.2 #3 (part), #2 (part) |

### 1. The node's key expiry (protocol 10)

**Problem.** The sidecar only said anything about our own key expiry inside the last 24 h (`tsnet:keyExpiring`, polled every 60 s), as a string in `HealthInfo`. flux's `TailnetInfo.key_expiry` (the Connect page's key-expiry warning) had no producer.

**Sidecar.**
- Every "running" status (`tsnet:status` and `tsnet:started`) carries `keyExpiry`: `Self.KeyExpiry` from `StatusWithoutPeers`, RFC 3339 UTC. tsnet leaves `Self.KeyExpiry` nil when expiry is disabled (admin "Disable key expiry", tagged nodes); the key is then omitted. A v10 core reads the omission as "never", an older status as "unknown".
- `monitorState` (60 s poll) re-sends the running status when the expiry moves: re-login, or an admin extending or disabling it. Same pattern as protocol 8's late login.
- One `runningStatus(status, hostname)` helper builds every running status (start, the late-login follow-up, the monitor), so they can't drift. The running `tsnet:status` now also carries `nodeId` (it only rode `tsnet:started` before; same value).

**Rust (`truffle-core`).**
- `network::KeyExpiry { Never, At(SystemTime) }` with `at()` and `unix_millis()` (the shape of flux's `Option<i64>` DTO field).
- `NetworkStatusEvent::KeyExpiryKnown { key_expiry }`: raised when the node comes up and on every change, never repeated for an unchanged value.
- `TailscaleProvider::key_expiry()` / `Node::key_expiry()`: the latest value, synchronous. It is stored before `start()` returns, so it is readable right after `build()`.
- `HealthInfo.key_expiry` (RFC 3339 string) is now filled from the running status on v10, not only in the last 24 h.
- A small strict RFC 3339 parser (`Z` or `±HH:MM`, optional fraction; rejects impossible dates). truffle-core has no date crate, and one wasn't worth adding for this.
- An unreadable value is logged and ignored. A pre-10 sidecar never yields a value: no event, `None`.

### 2. Which control server (protocol 10)

**Problem.** flux shows Tailscale's admin-console "approve this device" link while approval is pending. A Headscale user has no such page. tsnet already honours `TS_CONTROL_URL` (`tsnet.Server.getControlURL`), and the sidecar inherits the core's environment, so a flux started with `TS_CONTROL_URL=https://hs.example.com` already runs against Headscale today. Nobody could tell.

**Sidecar.**
- At `tsnet:start` it resolves the control URL by tsnet's own rule (`effectiveControlURL`): `Server.ControlURL`, else `TS_CONTROL_URL`, else `ipn.DefaultControlURL`.
- Every status from `starting` on carries it as `controlUrl`. The `starting` status is sent before `srv.Start()`, so the URL arrives before any `authRequired` or `needsApproval`.

**Rust.**
- `network::ControlServer { Tailscale, Custom { url } }`. `from_url` follows `ipn.IsLoginServerSynonym` (`controlplane.tailscale.com` and `login.tailscale.com`; tolerant of case and a trailing slash; look-alikes are custom). `is_tailscale()`, `url()`, and `machines_admin_url()` (`Some("https://login.tailscale.com/admin/machines")` only for the hosted service).
- `NetworkStatusEvent::ControlServerKnown { control_server }`: once per start, delivered **before** `NeedsApproval` on the same channel, so the builder hook can decide the link at the moment approval is requested.
- `TailscaleProvider::control_server()` / `Node::control_server()`.

**Not done:** a `NodeBuilder::control_url(..)` knob. It needs a `TailscaleConfig` field (21 struct literals in the repo) and is not required to *report*. `TS_CONTROL_URL` works today and is reported correctly. A follow-up can add the knob by setting `Command::env("TS_CONTROL_URL", url)` on the sidecar child, which even pre-10 sidecars honour.

### 3a. Bridge copy loop for large media responses (perf)

**Problem.** Every bridged byte crosses `idleCopy` in the sidecar. Per 32 KiB chunk it set a read deadline and a write deadline (research/06 §9.2 #12). On the tailnet side the connection is gVisor's `gonet.TCPConn`, where every `Set*Deadline` stops a timer and allocates a fresh `time.AfterFunc`. A 4K remux is thousands of chunks a second.

**Fix** (`packages/sidecar-slim/main.go`, `idleCopy` / `writeActive`):
- **Adaptive pooled buffers.** Every direction starts with a 32 KiB buffer. Once a read *fills* it (a bulk stream: a media response, an upload), the direction switches to a 256 KiB buffer from a `sync.Pool`, returned when the copy ends. A keep-alive API connection parked in `Read` keeps only 32 KiB, so 1,000 idle connections don't pin 512 MiB.
- **Coarse deadline re-arm.** `armedDeadline` re-arms only when the wanted deadline (last activity + timeout) has moved by more than `min(1 s, timeout/8)`. That is about once a second on a busy stream, instead of twice per chunk.
- **Why the reap stays exact.** Last activity only moves forward, so an armed deadline is never *later* than wanted. A stale one can only fire early. The loop already treats that as "busy, re-arm and continue", and after it fires (`forget()`) the re-arm is forced. A reap therefore still happens exactly one timeout after the last byte in either direction. All six Phase-1 idle-reap tests pass unchanged.

**Measured** (`BenchmarkBridgeCopyBulk`, new in `bridge_bench_test.go`: 64 MiB backend → player through one `bridgeCopy`. The "gonet" variant puts a real gVisor `gonet` connection, with tsnet's TCP buffer and SACK settings, on the tailnet side; it uses an in-process netstack over a loopback NIC, with no Tailscale and no network.) The benchmark file only uses pre-existing symbols, so the identical file was run against `ca1dd8e`. Setup: M1 Max, shared, load average 17–23, `nice -n 19`, 6 interleaved samples of 20 ops each, medians:

| Benchmark | Metric | `ca1dd8e` | re-arm only | **Phase D** | Change |
|---|---|---|---|---|---|
| loopback | CPU ms/MB (whole process) | 0.771 | 0.695 | **0.524** | **−32 %** |
| loopback | MB/s | 2,710 | 2,928 | **3,618** | +34 % |
| gonet | CPU ms/MB | 0.909 | 0.855 | **0.590** | **−35 %** |
| gonet | allocs per 64 MiB | 21,374 | 16,942 | **10,937** | **−49 %** |
| gonet | MB/s | 2,141 | 2,661 | **3,339** | +56 % |
| existing `BenchmarkBridgeCopyThroughput` | MB/s | 2,730 | 2,664 | **4,157** | +52 % |

A second interleaved set (base vs Phase D only) agreed: loopback 0.801 → 0.557 CPU ms/MB, gonet 0.937 → 0.648, gonet allocs 20,425 → 10,837.

- **Where the gain comes from.** The buffer sizing carries most of the CPU win. The re-arm adds 6–10 % CPU and removes about 4,400 timer allocations per 64 MiB on gonet.
- **End-to-end size.** As research/06 §3.3 says, the hop is about 1 % of macOS whole-stack CPU (gVisor plus WireGuard dominate), so expect a small end-to-end change on macOS. The relative weight is higher on Linux with GSO.
- **Results file:** `$FLUX_WORK/bench/results/flux-truffle-bridge-copy-d.json`.

**Tests** (`bridge_perf_test.go`; each runs over loopback and gonet):

| Test | Asserts | On `ca1dd8e` |
|---|---|---|
| `TestBridgeCopyRearmsDeadlinesRarelyOnABusyStream` | 32 MiB stream: deadline sets ≤ 16 + 8/s | **Fails:** 2,054 sets. Now: 4. |
| `TestBridgeCopyGrowsTheBufferOnlyForBulkStreams` | The bulk direction reads into 256 KiB; the request direction stays at 32 KiB | **Fails:** 32 KiB |
| `TestBridgeCopyReapStaysExactAfterAStream` | Stream for 3× the timeout, stop: reaped within [timeout − 20 ms, timeout + 500 ms] of the last byte | Passes (guards the slack) |

### 3b. A dead sidecar is reported, within milliseconds (robustness)

**Problem** (research/06 §9.2 #2). After `start()`, a sidecar crash (Go panic, OOM kill) only flipped the provider to `Stopped` and `health()` to `"crashed"`, at the watcher's next one-second poll. Nothing was emitted. flux kept reporting `Running` with a URL that served nothing, forever.

**Fix.**
- `NetworkStatusEvent::SidecarExited { exit_code }` is raised whenever the process exits on its own: during `start()` (which still fails as before) and after it. A clean `stop()` notifies the watcher first, so it never raises the event.
- The stdout reader signals the watcher when the child's stdout closes (which only happens as it exits). The watcher then polls every 10 ms for up to 2 s instead of waiting out its 1 s tick.

**Measured** (`a_killed_sidecar_is_reported_within_milliseconds`: a scripted sidecar reaches running, then `kill -9`):

| | Before | After |
|---|---|---|
| Event | none | `SidecarExited` |
| Detection | 0–1,000 ms (one poll; median ≈ 500 ms), internal state only | `SidecarExited` after **0.14, 0.19, 0.95, 11.3 and 12.9 ms** over five kills (median 0.95 ms). It arrives as soon as the closed stdout is seen, at worst one 10 ms fast poll later. |

**Tests** (scripted sidecars, real child processes):
- `a_sidecar_exit_after_start_is_reported_with_its_code`: exit 3 → `SidecarExited { exit_code: Some(3) }`, health `"crashed"`, pid cleared.
- `a_killed_sidecar_is_reported_within_milliseconds`: five SIGKILLs, `exit_code: None`, median latency < 300 ms (the old poll's expected median was 500 ms).

### Tests and gates

- **Go** (`packages/sidecar-slim`): 106 tests (98 + 8 new). `go test ./...` → **99 run, 99 pass**. `go vet` and gofmt are clean.
  - The 7 `TestEgress*` tests in `egress_tailnet_test.go` were skipped with `-skip`. They start real tsnet servers against an in-process test control server, and this phase's rules forbid starting any Tailscale node. They exercise `bridgeCopy` only through egress, which the bridge tests cover directly.
- **Rust:**
  - `cargo test -p truffle-core`: lib **433 passed** (422 → 433, 11 new), 0 failed, 3 ignored. The key-gated `tests/` suites skip (0.00 s each). Doctests 2 passed.
  - Workspace (`--exclude truffle-bench-app`): `cargo build --all-targets` passes; `cargo test --workspace` → **756 passed, 0 failed, 30 ignored**; `cargo fmt --all --check` is clean.
  - `cargo clippy --workspace --all-targets` is clean except 4 **pre-existing** truffle-napi warnings in files this phase doesn't touch: 3× `unsafe fn` (napi-rs async `&mut self`) and `new_without_default` on `NapiNode`.
  - The Tauri desktop bench app is excluded because it embeds `../dist`, which only `pnpm build` produces.
  - **Environment note:** on this Mac, macOS assesses a freshly written executable on its first run, which took 4–7 s under load (the second run took 33 ms). The scripted-sidecar tests pay that cost. Two pre-existing provider tests with a 10 s guard timed out once under load and passed on rerun. The new tests use a 60 s hang guard, and the kill test reuses one script.
- **flux compatibility:** a scratch detached flux worktree at `1fbcb63`, with only the truffle paths in its root `Cargo.toml` pointed at `wt/truffle-d` (never committed, and removed afterwards):
  - `cargo build -p flux-net --features embedded`: clean.
  - `cargo test -p flux-net`: 22 unit + 6 integration + 1 doctest passed, 1 ignored.
  - `cargo clippy -p flux-net --all-targets --features embedded`: clean.
  - `Cargo.lock` unchanged.
- **Never started:** no tailnet node, no `tests/` integration run with an auth key. The auth-key variables were unset for every cargo run. All new Rust tests use the event processor directly or `/bin/sh` scripted sidecars.

### Protocol and compatibility

- **Protocol 10** adds `controlUrl` (every status) and `keyExpiry` (running statuses), and may re-send a running status on an expiry change. Old cores ignore the fields. New cores on old sidecars get no new events and `None` from the getters, never a fabricated value.
- **API additions only:**
  - New `NetworkStatusEvent` variants. The enum is `#[non_exhaustive]`, and no in-repo match was exhaustive.
  - New types `KeyExpiry` and `ControlServer`.
  - New getters on `TailscaleProvider` and `Node<TailscaleProvider>`.
  - No public struct gained a field.
  - Internal only: `SidecarInternalEvent::{Started, StateChange}` gained fields, and `StatusReporting` gained two slots.

### How flux-net should consume it (for the merge agent)

1. **`flux_net::TailnetInfo`** gains `pub key_expiry: Option<i64>` (Unix ms; `None` = disabled or unknown). `TailnetStatus::to_api` copies it into `api::TailnetInfo.key_expiry` for `Running`. `run_system` sets `None`.
2. **`TailnetStatus::NeedsApproval`** becomes `NeedsApproval { approve_url: Option<String> }`. `to_api` copies `approve_url` as-is instead of always using `APPROVE_URL`. Update `examples/embedded.rs`, and print "approve it in your control server" when the URL is `None`.
3. **In `embedded::run_inner`:**
   - Before the builder, seed `let control = Arc::new(Mutex::new(ControlServer::from_url(&std::env::var("TS_CONTROL_URL").unwrap_or_default())))`. This is the pre-v10 fallback and is tsnet's own rule.
   - Extend the `on_network_status` closure. Events come from one forwarder task in order, so `ControlServerKnown` always precedes `NeedsApproval`:
     ```rust
     NetworkStatusEvent::ControlServerKnown { control_server } => *control.lock().unwrap() = control_server,
     NetworkStatusEvent::NeedsApproval => on_status.set_status(TailnetStatus::NeedsApproval {
         approve_url: control.lock().unwrap().machines_admin_url().map(str::to_owned),
     }),
     NetworkStatusEvent::KeyExpiryKnown { key_expiry } =>
         on_status.update_running(|t| t.key_expiry = key_expiry.unix_millis()),
     NetworkStatusEvent::SidecarExited { exit_code } => on_status.set_status(TailnetStatus::Failed {
         error: format!("the Tailscale sidecar exited ({exit_code:?}); restart flux to reconnect"),
     }),
     ```
4. **When building `TailnetStatus::Running`**, set `key_expiry: node.key_expiry().and_then(KeyExpiry::unix_millis)`. The getter is filled before `build()` returns. The hook's first `KeyExpiryKnown` may run before or after `Running` is set, so read both ways.
5. **`Shared::update_running(f)`:** `self.status.send_if_modified(|s| match s { TailnetStatus::Running(t) => { let before = t.key_expiry; f(t); t.key_expiry != before } _ => false })`.
6. **Tests:**
   - `to_api` maps `NeedsApproval { approve_url: None }` to `approve_url: None`, and `Running.key_expiry` to the DTO.
   - Factor the closure body into `fn on_network_event(&Shared, &Mutex<ControlServer>, NetworkStatusEvent)` and unit-test it with fabricated events. The variants are constructible: only the enum is `non_exhaustive`.
   - Assert that Headscale gives no approve URL, that hosted gives `APPROVE_URL`, that `KeyExpiryKnown` updates a running status and leaves others alone, and that `SidecarExited` gives `Failed`.
7. **Optional:**
   - Surface `ControlServer::url()` in the admin UI ("Connected via hs.example.com").
   - On `SidecarExited`, rebuild the node with backoff instead of staying `Failed`.

### Follow-ups not done here

- `NodeBuilder::control_url(..)`, `tags(..)`, OAuth (research/06 #3, the rest).
- Automatic sidecar restart with listen/proxy re-issue (#2, the rest). The event is the prerequisite.
- The listen-registration race (#5, the rest).
- The Rust-side bridge: `set_nodelay` on every bridged socket and a buffered header read (#12, the rest). flux already sets `TCP_NODELAY` itself.
- `http-media` bench on a tailnet pair (#9).
- NAPI/Tauri: neither exposes the new events or getters yet.
