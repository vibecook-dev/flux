# flux-net prototype

`crates/flux-net` serves flux's `axum::Router` on three listeners: localhost, an optional LAN address, and the tailnet. It tells every handler who is calling and how they connected.

On the tailnet it works in one of two modes:
- **Embedded:** flux's own Tailscale node through Truffle (branch `flux/dogfood`, see `truffle-changes.md`).
- **System:** "use this machine's Tailscale", through the running tailscaled.

```
127.0.0.1:8096 ─┐                         ┌─ Extension<Option<flux_core::api::Identity>>
LAN (optional) ─┼─ hyper http1 ───────────┤     (Tailscale WhoIs → RolePolicy → Role)
tailnet ────────┘  half_close(true)       └─ Extension<flux_net::ConnInfo>  (path, remote, server name, grants)
   ├ embedded: Truffle Node, app_id "flux", listen_tcp_opts(port, tls), identity from the bridge header
   └ system:   bind this machine's 100.x / fd7a: addresses, `tailscale whois --json` per caller IP (TTL cache)
```

## Public API (for flux-server)

```rust
use flux_net::{FluxNet, FluxNetConfig, TailnetConfig, EmbeddedConfig, SystemConfig, RolePolicy,
               TailnetStatus, ConnInfo, NetPath};

let net = FluxNet::start(FluxNetConfig {
    local_addr: "127.0.0.1:8096".parse()?,          // default
    lan_addr: None,                                 // e.g. Some("0.0.0.0:8097")
    tailnet: TailnetConfig::Embedded(EmbeddedConfig::new(&data_dir, "Mac mini")),
    //       TailnetConfig::System(SystemConfig::default()) | TailnetConfig::Off
    roles: RolePolicy::default(),                   // owner filled in from the tailnet
}, router).await?;                                  // Err only if local/LAN can't bind

let mut status = net.tailnet_status();              // watch::Receiver<TailnetStatus>
// Starting → NeedsLogin{auth_url} (show link + QR, may be replaced by a newer URL)
//          → NeedsApproval → Running(TailnetInfo{dns_name, addrs, tls, owner_login, url()})
//          | Unavailable{reason} (system mode; retries every 15 s) | Failed{error}
net.shutdown(Duration::from_secs(5)).await;         // in-flight requests finish
```

**Handler extensions:**
- `Extension<Option<Identity>>`:
  - **Localhost:** `Some`, Owner by default (`RolePolicy::localhost_role`; set it to `None` on multi-user hosts).
  - **LAN:** `None`.
  - **Tailnet:** `Some` WhoIs identity with a role, or `None` when WhoIs knows no one.
- `Extension<ConnInfo>`:
  - `path` (`NetPath::as_str()` is what `ServerInfo.path` carries: `localhost`, `lan`, `tailnet-direct`, `tailnet-relay`, or `tailnet` when the path is unknown)
  - `remote_addr`
  - `tailnet_name` (the server's MagicDNS name, for `ServerInfo.tailnet_name`)
  - `grants` (raw values of the flux capability, for future library or rating limits)

**Building blocks are public too:**
- `serve::serve_connection(io, router, identity, conn, shutdown)` and `serve::http1_builder()`
- `system::SystemTailscale` (`status`, `cached_status`, `whois`), `system::parse_status`, `system::parse_whois`
- `RolePolicy::{role_for, identity_for, localhost_identity}`, `PeerFacts`
- `embedded::resolve_sidecar`, `is_tailscale_ip`

**Cargo feature:** `embedded` (on by default) pulls in `truffle-core`. Without it, flux-net still does localhost, LAN and system mode.

## Design decisions

- **hyper, not `axum::serve`.** Each connection is served with `http1::Builder::new().half_close(true)`, plus `keep_alive`, `TokioTimer` and a 30 s header read timeout, with `.with_upgrades()`. `axum::serve` can't set `half_close`.

  With hyper's default, a client half-close aborts the response. Our control test measured **0 bytes** of a 40 KiB slow stream delivered; research/06 measured 9.4 KB of 62 KB.

  Extensions are inserted by a per-request `service_fn` wrapper (two clones). `Router::layer` per connection would rebuild every route.
- **Embedded serving uses `listen_tcp`, not the sidecar HTTP proxy** (research/06 §9.1). The identity comes from the sidecar's bridge header, which other local processes can't forge. hyper keeps control of Range, half-close and upgrades. `set_nodelay(true)` is set on every accepted stream (Truffle's bridge has Nagle on).
- **Embedded defaults:**

  | Setting | Value | Why |
  |---|---|---|
  | `app_id` | `"flux"` | |
  | Hostname | `truffle-flux-<slug(device)>` | The convention flux clients use to find servers with `peers()` until Truffle has announcement (research/06 §9.1 #2). A pretty `hostname` is opt-in. |
  | State dir | `<data_dir>/tailscale` | |
  | `idle_timeout` | 6 h | Covers a paused movie. The fixed sidecar reaps only connections idle in both directions. |
  | `start_timeout` | `None` | Headless first run: wait for the owner's login. |
  | Login URL | `TailnetStatus::NeedsLogin` | Re-sent when it changes (Truffle fix). |
  | Device approval | `NeedsApproval` | |
  | Owner | From `local_info().login_name` | Updated on `LoginKnown`. |
- **System mode** uses the `tailscale` CLI: `status --json` once at startup, then `whois --json <ip>` per new caller IP.
  - **Why the CLI.** It already knows every platform's LocalAPI transport: the macOS app's authenticated localhost port, the Linux socket, Windows pipes. CLI discovery order: explicit path → `FLUX_TAILSCALE_CLI` → `/Applications/Tailscale.app/Contents/MacOS/Tailscale` (matches the daemon's version; Homebrew's 1.92.5 CLI warns about the mismatch) → `PATH`.
  - **WhoIs caching.** Answers are cached per IP for 60 s, with single-flight per IP (8 concurrent connections from one IP make one CLI call). "Peer not found" is cached as anonymous; CLI failures aren't cached.
  - **Path snapshot.** Path classification uses a `status` snapshot primed at startup and refreshed in the background after 10 s (stale-while-revalidate), so a new connection waits on at most one `whois`.
  - **Bind.** flux binds each Tailscale address on the configured port. If a wildcard LAN or local listener already covers that port, it classifies there instead.
  - **What counts as tailnet.** A connection only counts as tailnet if it *arrives on one of our Tailscale addresses from a Tailscale address*. Replies to a spoofed 100.x source route into the tailnet, so a LAN host can't complete that handshake.
  - **Retries.** If tailscaled is stopped or logged out, the status is `Unavailable` (with its login URL if it has one) and flux retries every 15 s.
- **Roles** (`RolePolicy`, research/04 §5.3), resolved in this order:
  1. **Capability grant.** The app capability `vibecook.dev/cap/flux` from tailnet policy is authoritative. `{"role":"admin"|"viewer"|"kid"|…}` maps to `flux_core::api::Role`. Several grants combine to the most privileged; an unknown role string fails closed to Guest; grants without a `role` (only `libraries` etc.) fall through.
  2. Flux-local `overrides` by login (case-insensitive).
  3. The login that owns the server node → Owner (tagged devices never count as the owner).
  4. `tagged_role` / `default_role` (Viewer).

  If the sidecar had to drop a capability map that was too big for its header, everyone except the owner becomes a Guest.
- **Path classification:**
  - **System mode:** `CurAddr` set → `tailnet-direct`, otherwise `tailnet-relay`. The home DERP field (`Relay`) is set either way, so it isn't used.
  - **Embedded mode:** `Peer.connection_type` from `node.peers()`. But Truffle lists only app peers, so a phone browser on the Tailscale app shows as `tailnet` (unknown). See the gaps below.

## Tests

| Command | Result |
|---|---|
| `cargo test -p flux-net` | **28 passed** (21 unit, 6 integration, 1 doctest), **2 ignored** (the live test and an illustrative doc snippet) |
| `cargo test -p flux-net --no-default-features` | 18 unit + 6 integration pass |
| `cargo clippy -p flux-net --all-targets` (with and without `embedded`) | clean |
| `cargo test -p flux-net --test system_live -- --ignored --nocapture` | **live, read-only, passes** (below) |

What the tests cover:
- **Loopback integration** (`tests/serving.rs`):
  - Localhost is the Owner; untrusted localhost is anonymous; LAN callers (bound on the host's LAN IP) are anonymous with `path = lan`.
  - **A 40 × 1 KiB stream over about 0.8 s arrives in full after the client closes its write half.** Flipping `half_close` to `false` makes this test fail with 0 bytes. A control test pins that hyper's default truncates.
  - Graceful shutdown completes an in-flight stream, then closes the listener.
- **Embedded path without a node** (`embedded::tests`):
  - A real Truffle `RawListener` (`RawListener::new`) is fed loopback streams carrying fabricated `TailscalePeerIdentity`s. A friend's grant `{"role":"kid","libraries":["Cartoons"]}` arrives as role `kid` with the grant in `ConnInfo.grants`, path `tailnet-relay`, the server's MagicDNS name and the remote address. The owner without grants is Owner; a missing identity is anonymous.
  - Sidecar resolution errors explain what was tried and how to build the sidecar.
  - Embedded defaults are checked.
- **Unit tests:**
  - `status`/`whois` JSON parsing against real field names (`AuthURL`, `DNSName`, `CurAddr`, `CapMap`)
  - a fake `tailscale` CLI proving caching, single-flight and anonymous misses
  - role resolution (10 cases)
  - Tailscale IP ranges and IPv4-mapped addresses
  - `TailnetInfo::url()`
- **Live system-tailscaled test** (this Mac, tailscaled 1.104.1, read-only):
  - flux bound the machine's `100.x` and `fd7a:…` addresses on ephemeral ports.
  - `GET http://<100.x address>:<port>/whoami` from this machine returned the **maintainer's login, role `owner`, `path: tailnet-direct`, `server: <host>.<tailnet>.ts.net`**.
  - The owner was learned from `tailscale status`, not configured.
  - Cold first request: **37–59 ms** (one `whois` CLI call; it was 291 ms before the status snapshot was primed). Cached: **1.2–1.5 ms**.
- **Embedded failure path** (smoke run of the example with no sidecar): a clear `Failed` status naming the paths tried and the build command. No process is started and no state dir is created.

No embedded node was started (that would add a device to the maintainer's tailnet).

**Manual test for the maintainer (adds a device to your tailnet):**

```sh
cd $FLUX_WORK/wt/truffle/packages/sidecar-slim && \
  go build -trimpath -ldflags "-s -w" -o $FLUX_WORK/bin/sidecar-slim .
TRUFFLE_SIDECAR_PATH=$FLUX_WORK/bin/sidecar-slim \
  cargo run -p flux-net --example embedded -- \
  --data-dir $FLUX_WORK/tmp/flux-embedded --device-name flux-dev [--port 8096] [--tls]
```

It prints `Connect with Tailscale: https://login.tailscale.com/a/…`. After you log in, it prints `tailnet: http://truffle-flux-flux-dev.<tailnet>.ts.net:8096/whoami`. Open that from a phone or another machine to see your identity, role and path. To undo: Ctrl-C, delete the data dir, and remove the device in the admin console. To try roles, add a grant with `"app": {"vibecook.dev/cap/flux": [{"role": "kid"}]}`.

## Benchmarks

```sh
cargo run --release -p flux-net --example bench_throughput -- --runs 5 --seeks 50
# → $FLUX_WORK/bench/results/flux-net-throughput.json
```

Setup: a 1 GiB synthetic in-memory object served with HTTP Range (zero-copy 256 KiB chunks), fetched with `Range: bytes=0-1073741823`. Client and server run in one release-build process. The machine was shared and loaded (load average ~51), so expect noise. The lead should re-run.

| Path | Throughput (median of 5) | Process CPU | Tailscale daemon CPU | TTFB | 1 MiB seek p50 / p95 | New-connection small GET: first / p50 |
|---|---|---|---|---|---|---|
| loopback 127.0.0.1 | **59,956 Mb/s** (53.6–69.5k) | 0.16 ms/MB | — | 0.19 ms | 0.22 / 0.33 ms | 0.71 / 0.10 ms |
| system tailnet, same host (its 100.x address) | **745 Mb/s** (720–785) | 4.8 ms/MB | **12.5 ms/MB** | 0.37 ms | 11.3 / 17.6 ms | **51.6** (cold WhoIs) / 0.45 ms |

How to read these:
- **The same-host tailnet path still crosses Tailscale's userspace.** On macOS a request to this machine's own Tailscale IP loops through the Tailscale network extension: utun, MTU 1280, about 12.5 ms of extension CPU per MB. So this row measures the extension's packet path plus flux. There is no real network and no WireGuard hop to a peer.
  - **Throughput:** about 750 Mb/s is far above any stream flux serves (a 4K remux is 40–80 Mb/s, research/04 §3).
  - **CPU:** 12.5 ms per MB is in the same range as research/06's Truffle numbers: about 62 ms/MB per side for tsnet's whole stack, two processes.
  - **Real peer-to-peer numbers** need a second device. They aren't measured here.
- **WhoIs latency.** The first connection from a new tailnet IP pays one `tailscale whois` process spawn (40–60 ms). Every later connection from that IP within 60 s costs about 0.5 ms. Measured against Jellyfin's 0.18–0.62 s time to first segment (benchmarks.md), this is small, but LocalAPI-direct would cut it to about 1 ms (see the gaps below).
- **Bridge cost.** The Truffle bridge relay benchmark is in `truffle-changes.md` (about 1.9 GB/s on one loopback hop, unchanged by the idle-reap fix).

## Known gaps

- **The embedded mode was never run against a real tailnet** (per the rules). Only its serving path (`RawListener` → hyper → identity) is tested, with fabricated identities. `build_with_auth_handler`, `listen_tcp_opts(tls)` and owner discovery are compiled but unverified end to end. Run the manual test above.
- **Embedded path classification** only knows app peers. Any non-flux tailnet client shows `path = "tailnet"`.
  - A `Node::path_to(ip)` in Truffle (LocalAPI status per IP) would fix this.
  - `"tailnet"` is a fifth `ServerInfo.path` value that flux-core's doc doesn't list (proposed below).
- **System mode cost and refresh:**
  - It uses the CLI. Talking to LocalAPI directly (the Linux unix socket; macOS needs the app's token discovery) would make cold WhoIs about 1 ms.
  - Tailscale addresses are read once. If tailscaled changes IPs or logs out after startup, flux keeps the old binding until restart.
  - TLS in system mode is opt-in (`--tailnet-https`, or the switch in Remote access): rustls with the certificate from `tailscale cert`, HTTPS and plain HTTP on one port, told apart by the first byte (`src/tls.rs`; README §HTTPS). Without it, browsers see `http://…ts.net:8096`, which WireGuard still encrypts but browsers don't count as a secure page.
- **Security trade-offs:**
  - **Localhost = Owner by default.** On a multi-user NAS any local process can reach 127.0.0.1. Set `localhost_role: None` there (flux-server: `--localhost-role none`, Phase D). A web page reaching 127.0.0.1 by DNS rebinding is refused by flux-server's Host allowlist (Phase D).
  - The LAN listener has no pairing or auth yet. LAN callers are just anonymous, and the server decides what anonymous may do.
- **Failure handling:**
  - **WhoIs failures are anonymous for the whole connection.** A transient whois error makes that keep-alive connection anonymous until it closes; new connections retry.
  - **No retry or restart.** A start timeout (if one is configured) or a sidecar crash leaves the status `Failed`; flux-net doesn't rebuild the node. Truffle has no sidecar supervision (research/06 §9.2 #2).
- **Benchmark scope.** The benchmark doesn't cover the embedded path (it needs a tailnet node) or a real two-device tailnet.

## Proposed flux-core changes

- **`ServerInfo.path` doc:** add `"tailnet"` (path unknown) to the listed values, or make `path` an enum shared with `flux_net::NetPath`.
- **`Identity`:** document that localhost callers get a synthetic Owner identity (login = the node owner, or `"local"`) and LAN callers `None`. It might gain `tags: Vec<String>` so the UI can label shared, tag-owned devices such as "Living-room TV".

## Phase D: admission, write timeout, set_tailnet

> **After the merge** ([fixes.md](fixes.md)): flux-net also consumes Truffle's protocol 10: `TailnetInfo::{key_expiry, control_server}`, `NeedsApproval { approve_url }` (no admin-console link on Headscale) and `SidecarExited` → `Failed`.

*Branch `phase-d/server`. Review findings SEC-3 and SEC-6 (`review/findings-d1.md`); the Host/Origin contract for SEC-1/SEC-7 lives in flux-server's router (`guard.rs`, see [server.md](server.md#phase-d-security-and-robustness-fixes)), which every listener serves.*

**New public API**

```rust
FluxNetConfig { limits: ServeLimits, .. }          // new field; Default keeps the old behaviour + limits
ServeLimits { max_connections: 512, max_connections_per_ip: 64, write_timeout: Some(60 s) }
net.set_tailnet(TailnetConfig, grace).await;        // replace only the tailnet listener
serve::serve_connection(io, router, identity, conn, shutdown, write_timeout)  // + write_timeout
limits::WriteTimeout::new(io, Option<Duration>)    // the write-deadline IO wrapper
```

- **Admission (SEC-3).** Right after `accept`, before any HTTP and on every listener (embedded tailnet included, keyed by the tailnet peer's address): at most `max_connections_per_ip` open connections from one address and `max_connections` from all non-loopback addresses together. Loopback counts against its per-IP budget but not the total, so a LAN flood can't lock the owner out of `127.0.0.1`. Over a limit, the connection gets one non-blocking `503 Too many connections` write and is closed at once — it neither waits in the backlog nor costs a task — and a warning is logged at most once a minute. (Before: the reviewer's 245 idle SSE connections under `ulimit -n 256` made `accept` fail with EMFILE and every client got ECONNRESET.) flux-server also raises `RLIMIT_NOFILE` and sizes the limits to it.
- **Write timeout (SEC-6).** Every connection's IO is wrapped in `WriteTimeout`: while a write or flush makes no progress (the client stopped reading), a timer runs; at `write_timeout` the write fails with `TimedOut`, hyper ends the connection and the response body (and its buffers) is dropped. Any progress resets the timer, so a slow reader is never cut, and a response merely waiting for its next body chunk (SSE between events, a transcode) isn't writing, so it isn't timed. 60 s matches nginx's `send_timeout`; a paused `<video>` that stopped reading reconnects with a `Range` request. (Before: 100 stalled readers held +208 MB until they disconnected.)
- **HTTP/1.1 limits.** `max_header_size` 64 KiB (431 beyond); the 30 s header-read timeout also closes idle keep-alive connections (hyper restarts it after each response); pipelined requests are answered one at a time and hyper reads ahead at most its ~400 KB buffer. HTTP/2: not offered — browsers can't use h2c, and TLS terminates in Truffle (embedded) or not at all.
- **`FluxNet::set_tailnet` (stretch).** The tailnet listener and its connections now live in their own scope (a child cancellation token of the shutdown token plus their own `TaskTracker`). `set_tailnet` cancels that scope, gives its in-flight requests `grace`, waits for its node to stop (so two embedded nodes never share a state dir), clears the system-mode binding, sets the status to `Off`/`Starting` and starts the new listener in a fresh scope. Local and LAN listeners and their connections are untouched. flux-server's `POST /tailnet/connect` uses it instead of restarting everything. `shutdown` waits for both scopes.

**Tests (flux-net)**

| test | covers |
|---|---|
| `serving::connections_over_the_per_ip_limit_are_refused_at_once` | SEC-3: with 4 per IP, 4 idle connections held; the fifth is answered `503 … Too many connections` without sending anything and closed; held connections are served; a closed one's slot comes back |
| `serving::a_client_that_stops_reading_is_dropped_after_the_write_timeout` | SEC-6, the verifier's stall.py: 4 KiB receive buffer, request an endless body, never read → the body is dropped after the 1 s test timeout |
| `serving::a_slow_but_reading_client_keeps_its_stream` | reading 64 KiB every 200 ms with a 2 s timeout: never cut |
| `serving::pipelined_requests_are_answered_in_order` | three pipelined requests, three 200s in order |
| `serving::set_tailnet_replaces_only_the_tailnet_listener` | a fake `tailscale` CLI reports a running node at 127.0.0.1, so system mode binds a loopback port (no Tailscale): Off → System (served) → Off (closed, status Off, mode Off) → System again, while a 0.8 s local download in flight completes in full; shutdown closes both |
| `limits::per_ip_and_total_limits_with_a_separate_loopback_budget` | per-IP and total limits, IPv4-mapped addresses, loopback's own budget, slots returned on drop |
| `limits::a_stalled_write_fails_after_the_limit` | paused clock: `TimedOut` after exactly 60 s; no limit waits forever |
| `limits::busy_response_is_well_formed` | the 503's `Content-Length` |

`cargo test -p flux-net`: **37 passed** (25 unit, 11 integration, 1 doctest), 2 ignored (the live test, an illustrative doc snippet); `--no-default-features`: 22 unit + 11 integration + doctest pass; clippy clean both ways. The admission and write-timeout tests fail with the fixes disabled (connection 5 waits for a request; the stalled body is never dropped). The `refuse` path writes through the std socket: tokio's `try_write` reports `WouldBlock` until the reactor has seen a new socket writable, so the first version closed refused connections without the 503 (caught by the test).

**Known gaps (Phase D)**

- Loopback's budget is per address, so a local flood (or a page that rebinds many names to 127.0.0.1 and holds connections open; they get 421s but stay connected until the 30 s header timeout) can use up the owner's own 64 loopback connections for a while. The total is not affected.
- A slow reader that keeps reading a little (a few KB a minute) is never cut; only the per-IP limit bounds it.
- `set_tailnet` waits at most `grace` for the old tailnet tasks; if an old embedded node takes longer to stop, it is abandoned (cancelled) while the new one starts.
