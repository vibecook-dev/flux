# 06 — Deep code review: Truffle as flux's network layer

**Date:** 2026-10-18 · **Reviewed:** Truffle `main` @ `d6f6664` (v0.7.19, released 2026-10-17; matches `origin/main` per `gh api`), a local checkout (read-only). Embedded Tailscale: `tailscale.com v1.104.1` (the newest client release, dated 2026-10-07, per [tailscale.com/changelog](https://tailscale.com/changelog)).
**Method:** I read the Rust core, the Go sidecar, the Swift package, the RFCs and the benchmark hub. I checked tsnet/netstack behaviour against the exact `tailscale.com@v1.104.1` and gVisor sources in the SSD Go module cache (`$GOMODCACHE/`). I built `truffle-core` and ran its unit tests (`--lib` only, offline, `CARGO_TARGET_DIR=$FLUX_WORK/target`), and wrote three small local reproductions (loopback only; no tailnet node was started).

> Paths below are relative to the Truffle repo root unless they start with `/`. Confidence markers: **[V]** = verified by reading code or running it here; **[I]** = inference from verified facts; **[E]** = external source.

---

## 0. TL;DR for flux

1. **Truffle already gives a Rust server everything it needs to *serve* over the tailnet.** It has `listen_tcp` with per-connection, unforgeable WhoIs identity, `Node::whois()`, a sidecar reverse proxy that injects `Tailscale-User-*` headers, TLS with MagicDNS certs, egress (SOCKS5/CONNECT/fixed routes), QUIC/UDP and peer discovery. The "need WhoIs-based identity" item on the wishlist is **already shipped**. What's missing is the **capability map (grants), node tags, and a trust story for the proxy headers**. [V]
2. **I found a streaming-relevant bug and verified it.** The sidecar's bridge applies its idle timeout **per direction** (`packages/sidecar-slim/main.go:2853-2893`). The comment at `main.go:58-60` says "idle in BOTH directions", but the code doesn't do that. A long single HTTP response, such as progressive direct play where the client sends nothing after the `GET`, gets its request half closed after `idle_timeout` (default **600 s**). hyper/axum's default `half_close(false)` then **aborts the response**. My repro delivered 9.4 KB of a 62 KB stream before the abort. With `half_close(true)` all 62 KB arrived. This affects the `listen_tcp`, `open_tcp` and egress paths. It does not affect the sidecar proxy path. [V]
3. **The Rust↔sidecar bridge is not the throughput bottleneck.** One loopback relay hop costs **~0.4–0.5 ms CPU per MB** (measured here, 8.4 Gb/s on one core with Truffle's 32 KiB + per-read-deadline loop). The sidecar's own netstack and WireGuard path costs **~62 ms CPU per MB** on macOS (about 300% CPU at 387 Mb/s per side, from Truffle's own benchmark). So zero-copy or fd-passing work on the bridge buys **under 1–2%**. The real levers are elsewhere. On Linux, tsnet gets netstack GSO/GRO and UDP batching; on macOS it gets **none** of them (verified in the v1.104.1 source). Other levers are MTU, deploying flux servers on Linux, and avoiding DERP. [V]/[I]
4. **Discovery doesn't fit a "pretty" server yet.** Peers are app-filtered by the hostname prefix `truffle-{appId}-` (`provider.rs:747-750`). A flux server built with `.hostname("flux")`, for `https://flux.<tailnet>.ts.net`, is **invisible** in other flux nodes' `peers()`. `ProxyConfig.announce` is still a no-op (`proxy/types.rs:44-49`). Service announcement has to be built. [V]
5. **Apple:** TailscaleKit is **iOS-only**: the XCFramework has `ios-arm64` and the iOS simulator slices only. There is **no tvOS and no macOS-embed** production backend. AVPlayer cannot use the SOCKS-proxied `URLSession`, so an iOS player needs a **loopback HTTP gateway**. libtailscale's per-connection Unix socketpairs use Darwin's **8 KiB** default socket buffers. AF_UNIX measured here at 1.0 Gb/s against 14.9 Gb/s for loopback TCP. [V]
6. **Code quality is high for a 0.x library.** 413 unit tests pass in 30.6 s, plus 46 live integration tests, 82 Go tests and 113 Swift tests. There are 14 CI workflows, pinned sidecar checksums and fail-closed identity. The **product risks** are: pre-1.0 API churn (`docs/API-STABILITY.md`), no sidecar crash supervision, a 5-minute fixed auth timeout, a bridge head-of-line lock, peer-event lag with no resync, and macOS-only performance data.

---

## 1. Architecture as it exists today

```
flux (Rust, tokio) ──── truffle-core (Layers 3–6, Node API)
   │  stdin/stdout JSON lines (control plane: commands + events)
   │  127.0.0.1 TCP "bridge" (data plane: one loopback TCP conn per tailnet TCP conn,
   │                           prefixed by a 40+ byte binary header with a 32-byte session token)
   ▼
Go sidecar "sidecar-slim" (tsnet v1.104.1 → gVisor netstack → wireguard-go → magicsock UDP)
```

| Piece | Where | Size | Notes |
|---|---|---|---|
| Node API (Rust) | `crates/truffle-core/src/node.rs` | 3,839 lines | `Node<N: NetworkProvider>`; `NodeBuilder`. |
| Layer 3 provider | `crates/truffle-core/src/network/tailscale/{provider,sidecar,bridge,header,protocol}.rs` | ~6.9k lines | Spawns the sidecar and owns the bridge. |
| Go sidecar | `packages/sidecar-slim/{main,egress,udp_relay,netstack_tuning}.go` | 4,703 lines | tsnet, the proxy engine, egress, the UDP relay. |
| Swift | `apple/Sources/{Truffle,TruffleSwiftUI,TruffleTailscale}` | ~5k lines | TailscaleKit (libtailscale) in-process, iOS only. |
| Bench hub | `benchmark/engine` (+ app) | ~30k lines | RFC 027 workloads and v3 reports. |

`truffle-core/src` is **24.7k non-test lines / 31.8k total** [V]. The repo's own `CLAUDE.md` says "~12k LOC", which is out of date.

---

## 2. Rust API surface usable directly from a Rust server

All of the following is on `truffle_core::Node<TailscaleProvider>`, re-exported by `truffle` (`crates/truffle/src/lib.rs:49`). [V]

### 2.1 Lifecycle and configuration

| API | file:line | Notes for flux |
|---|---|---|
| `Node::builder()` → `NodeBuilder` | `node.rs:594`, `1619-1640` | Fields: `app_id`, `device_name`, `device_id`, `hostname`, `sidecar_path`, `state_dir`, `auth_key`, `ephemeral`, `ws_port` (default 9417), `idle_timeout_secs`, `eager_identity` (default **true**), `login_allow`, `on_network_status`. |
| `.app_id(s)?` | `node.rs:1792` | Regex `^[a-z][a-z0-9-]{1,31}$`. **Required.** |
| `.hostname(s)?` | `node.rs:1822` (trade-off doc `1816-1821`) | Gives a pretty MagicDNS name, but see §4.3: such nodes drop out of other nodes' `peers()`. |
| `.state_dir(..)` | `node.rs:1849` | Default `{data_dir}/truffle/{app_id}/{slug}` with no temp fallback (`node.rs:2004-2025`). Holds tsnet keys and `device-id.txt`. |
| `.idle_timeout_secs(n)` | `node.rs:1911` | Sidecar bridge idle reap, default 600 s (`main.go:61`). **Raise it for flux** (§3.4). |
| `.login_allow(globs)` | `node.rs:1892` | RFC 025 login gate for the *session plane* only. Raw `listen_tcp`, QUIC and served routes are not gated (`node.rs:1886-1889`). |
| **not exposed:** tags, `ControlURL`, WireGuard `Port`, OAuth `ClientSecret` | `node.rs:2066` hard-codes `tags: None`; sidecar `startData` (`main.go:87-99`) has no control URL or port | Blocks Headscale users, `ListenService` (needs tags), and fixed UDP-port forwarding. |
| `build().await` | `node.rs:2082-2135` | Starts the provider and blocks until Running, with a **fixed 5-minute timeout** (`provider.rs:877`). |
| `build_with_auth_handler(on_auth)` | `node.rs:2148-2228` | Same, but forwards `AuthRequired{url}` to a callback (`node.rs:2168-2180`). |
| `on_network_status(handler)` / `Node::on_network_status()` | `node.rs:1951`, `882` | `NeedsApproval`, `LoginKnown` (`network/mod.rs:395-420`). |
| `stop().await` | `node.rs:769-810` | Idempotent; cancels tasks, then the session, then the provider; bounded 5 s drain. |
| `sidecar_pid()` | `node.rs:1544` | Becomes `None` after the sidecar dies. **No auto-restart** (§7). |

**How the login URL surfaces.** [V] The sidecar polls `LocalClient.StatusWithoutPeers` every 500 ms. The first time `status.AuthURL != ""` it emits `tsnet:authRequired` (`main.go:1094-1117`). Rust maps that to `NetworkPeerEvent::AuthRequired{url}` (`provider.rs:532-537`). `build_with_auth_handler` calls your closure with it. Two caveats:
- **It is emitted only once per start** (`authURLSent`, `main.go:1114`). The trait doc says it "may be emitted multiple times if the URL expires and is refreshed" (`network/mod.rs:373-375`), which the code doesn't do.
- `start()` gives up after 300 s (`provider.rs:877-893`), and the app must rebuild the node. On a headless NAS on first run, flux has to show the URL somewhere *other than* its tailnet UI, which isn't reachable yet: a LAN or localhost setup page, stdout, or a QR code.

### 2.2 Peers, discovery and diagnostics

| API | file:line | Notes |
|---|---|---|
| `peers()` → `Vec<Peer>` | `node.rs:825-833`; `Peer` `node.rs:117-153` | `device_id`, `display_name`, `hostname`, `tailscale_id`, `ip`, `online`, `ws_connected`, `connection_type` (`"direct"`/`"relay:xxx"`), `os`, `login_name`. |
| `on_peer_change()` | `node.rs:869` | `PeerEvent::{Joined,Left,Updated,Identity,WsConnected,WsDisconnected,AuthRequired}` (`session/mod.rs:140-158`). |
| `peer(query, wait_ms)` | `node.rs:894-943` | Resolves a ULID, name, hostname, IP or stable id. Can return `AmbiguousPeer`. |
| `ping(peer)` | `node.rs:1089`; sidecar `main.go:1612-1682` | Disco ping by default, with direct-vs-DERP information. Useful for flux's "connection quality" badge. |
| `health()` | `node.rs:1131` | State, key expiry, warnings. |
| **Filter** | `provider.rs:299-321`, `747-750` | Only hostnames starting with `truffle-{app_id}-` are peers (`RowVerdict::NotOurApp`). |

Discovery is event-driven from the sidecar's `WatchIPNBus` (`main.go:1756+`). On a burst, the Rust event processor only logs `"event processor lagged by {n} events"` (`provider.rs:725-727`) and **does not resync**. The bench README records a run where a peer took ~30 s to appear on a tailnet with 7–11 peers (`benchmark/README.md`, "Still open").

### 2.3 Identity (WhoIs): already shipped

This is the most important correction to the earlier hypotheses. **Truffle exposes Tailscale WhoIs on three paths.** [V]

| Path | How identity arrives | Fields | Trust |
|---|---|---|---|
| `listen_tcp` accept | `RawIncoming.remote_identity: Option<TailscalePeerIdentity>` (`transport/mod.rs:189-199`). Parsed from the bridge header (`transport/tcp.rs:82,131`). The sidecar resolves WhoIs per accepted conn in a goroutine (`main.go:1578-1581`, `2934-2953`). | `dns_name`, `login_name`, `display_name`, `profile_pic_url`, `node_id` (`network/mod.rs:520-531`) | **Unforgeable** from other local processes: the bridge checks a 32-byte session token in constant time (`bridge.rs:178`). |
| Sidecar reverse proxy (`node.proxy().add`) | Headers `Tailscale-User-Login`, `-User-Name`, `-User-Profile-Pic`, `Tailscale-Node-Id`, `-Node-Name` (`main.go:3047-3053`), injected after **stripping** any inbound `Tailscale-*` headers (`main.go:2463-2464`, `3058-3064`, `3066-3082`). Uses a 60 s TTL cache (`main.go:3010-3018`, `3056`). | same | **Forgeable by any local process** that can reach the loopback target (e.g. `127.0.0.1:8096`). Fine on a single-user Mac; risky on multi-user NAS hosts [I]. |
| `Node::whois(addr)` | `node.rs:1108-1128` → `tsnet:whois` (`main.go:1700-1739`). Accepts IP, `ip:port`, or any peer query. | same; `Ok(None)` = anonymous | Reaches **any** tailnet node, not only app peers. |

**What's missing** [V]:
- **`CapMap`** (grants application capabilities). `apitype.WhoIsResponse` has `Node`, `UserProfile` and `CapMap tailcfg.PeerCapMap` (`tailscale.com@v1.104.1/client/tailscale/apitype/apitype.go:35-42`). The sidecar copies only five string fields (`main.go:193-199`, `2942-2951`).
- **Node tags** and user ID. Tagged devices report the pseudo-login `tagged-devices`, so a shared living-room device can only be told apart by `node_id`.

Tailscale's docs say apps read granted capabilities "through … the LocalAPI, the `tailscale serve` capabilities header, or the `tsnet` Go library" ([KB 1537, application capabilities](https://tailscale.com/kb/1537/grants-app-capabilities)) [E]. Exposing `CapMap` would let flux take **roles from tailnet policy**: for example, a grant `vibecook.dev/cap/flux: [{"role":"admin"}]` for admin, viewer or kids. Flux wouldn't need its own user database.

### 2.4 Messaging, synced stores, request/reply

| API | file:line | Wire / cost |
|---|---|---|
| `send_json`, `send_typed`, `broadcast_json`, `broadcast_typed` | `node.rs:1197`, `1240`, `1321`, `1259` | JSON `Envelope` over the per-peer WebSocket on port 9417. Max message 16 MiB (`transport/mod.rs:343`). |
| `send_bytes` / `broadcast_bytes` | `node.rs:1214-1231`, `1333` | **base64 in JSON** (`{"encoding":"base64","data":…}`, `node.rs:1225-1231`). That's +33% size plus JSON escape and parse, decoded by `payload_bytes()` (`node.rs:96-103`). **Confirmed.** |
| `subscribe(ns)` | `node.rs:1362-1392` | `broadcast::channel(256)` per namespace. The session's incoming broadcast holds **1024** messages (`session/mod.rs:664`). Drops were measured at ~250k msg/s (bench README). |
| `synced_store::<T>(id)` | `node.rs:704`; `synced_store/mod.rs:99-299` | Each device owns one slice, last-write-wins, and the **whole slice** is rebroadcast on `set()` (`mod.rs:178`). Fits "presence / now playing / capabilities". **Doesn't fit** catalog sync. |
| `request_reply::send_and_wait` | `request_reply.rs:56` | RPC over the envelope bus. RTT p50 ≈ 0.27 ms same-host (§5). |

### 2.5 Raw transports

| API | file:line | Data path |
|---|---|---|
| `open_tcp(peer, port)` → `tokio::net::TcpStream` | `node.rs:1405-1413` | Rust sends `bridge:dial` (JSON) → Go `srv.Dial` (`main.go:1425`) → Go connects back to the Rust bridge (`main.go:2729`). The 30 s dial timeout is in `bridge.rs:40` and `main.go:64`. |
| `listen_tcp(port)` / `listen_tcp_opts(port, ListenOpts{tls})` → `RawListener` | `node.rs:1422-1454`; `transport/tcp.rs:60-98` | `tls:true` → tsnet `ListenTLS` with MagicDNS/Let's Encrypt certs (`main.go:1496-1510`); Rust receives plaintext. Port 9417 is reserved (`node.rs:1590-1596`). |
| `unlisten_tcp(port)` | `node.rs:1460` | Dropping `RawListener` alone leaves the tsnet port bound. |
| `connect_quic` / `listen_quic` | `node.rs:1476-1502` | quinn over the UDP relay. ALPN `truffle-raw.{app_id}`. **No GSO, no ECN, IPv4 only, MTU discovery off** (`transport/quic_socket.rs:21-26`). Double encryption (QUIC inside WireGuard). |
| `bind_udp(port)` → `DatagramSocket` | `node.rs:1513-1520` | Loopback UDP to the sidecar with a 6-byte address header. **A `Vec` is allocated and copied per datagram** in both directions (`network/mod.rs:619-622`, `650-666`). Keep datagrams ≤ ~1200 B. |

### 2.6 HTTP proxy (`Node::proxy().add(ProxyConfig)`)

`proxy/mod.rs:98-143` → `proxy:add` → sidecar `startProxyAdd` (`main.go:2386-2581`) [V]:
- `ProxyConfig` (`proxy/types.rs:33-69`): `listen_port`, `target{host,port,scheme}`, `tls` (default **true**), `allow_non_loopback` (default false), `allow` (login globs), `routes` (prefix → `target_url` or a static `dir`, `fallback`, `strip_prefix`, per-route `allow`), and **`announce` (documented no-op)**.
- The engine is Go `http.Server` (`ReadHeaderTimeout 10 s`, `IdleTimeout 120 s`, no `ReadTimeout`, `main.go:2554-2559`) plus `httputil.NewSingleHostReverseProxy`. The upstream pool keeps **64 idle conns per host** (`main.go:3239-3268`), and WebSocket upgrades are hijacked (`main.go:2589+`).
- Static `dir` routes use `os.OpenRoot` and `http.ServeContent`, with Range, ETag and MIME (`main.go:446-452`, `3206`). That is a fast path for the bytes, but authorization is limited to login globs.
- **HTTP/2 isn't negotiated on TLS listeners** [V]. `tsnet.ListenTLS` returns `tls.NewListener(ln, &tls.Config{GetCertificate: s.getCert})`, with no `NextProtos` (`tailscale.com@v1.104.1/tsnet/tsnet.go:1409-1431`). `http.Server.Serve` (unlike `ServeTLS`) never adds `h2` to ALPN (Go `net/http/server.go:3519` adjusts `NextProtos` only in `ServeTLS`). RFC 023's constraints table claims "h2 free" for the engine path (`docs/rfcs/023-http-serving.md:387`). **That claim is wrong for `tls: true`.** For flux's web UI this means 6 parallel HTTP/1.1 connections per origin when loading poster grids.

### 2.7 Egress (RFC 026)

`Node::loopback()` gives tsnet's own SOCKS5 server (`node.rs:635`). `egress_add(EgressRoute{target, login})` binds a 127.0.0.1 listener with a fixed tailnet target, spliced raw (`node.rs:663`; Go `egress.go:575-609`). `egress_http_proxy()` is a CONNECT proxy for browsers (`node.rs:691`). All three re-check the login gate per connection. E2 (fixed route) is exactly the **"loopback gateway"** pattern a desktop flux player (mpv, Electron) would use to play `http://127.0.0.1:PORT/...`. There is **no Swift equivalent**.

### 2.8 File transfer

`node.file_transfer().send_file/pull_file` (`file_transfer/mod.rs`). Signalling uses WS namespace `ft`, and bulk data goes over raw TCP in 64 KiB chunks (`sender.rs:227`). It **hashes the whole file with SHA-256 before the first byte** (`sender.rs:82-100`). The default cap is **1 GiB** (`mod.rs:55`, configurable via `set_max_transfer_size`). `ft.done` arrives ~100 ms late because of a `sleep(100ms)` (`receiver.rs:602`). **Not suitable for media download-for-offline**; flux should serve resumable HTTP Range downloads instead. [V]/[I]

---

## 3. The byte data path, with copies, syscalls and limits

### 3.1 Transport between Rust and the sidecar [V]

| Plane | Transport | Code |
|---|---|---|
| Control | **stdin/stdout, newline-delimited JSON.** Rust has an `mpsc` of 64 commands, then a writer task (`sidecar.rs:256`, `364-400`). The sidecar reads with `bufio.Reader` and 8 MiB max lines (`main.go:71`, `899-1004`), and stdout is a JSON event stream (`main.go:889`). Events fan out on a `broadcast(256)` (`sidecar.rs:255`). | `sidecar.rs:354-362`: `Command::new(binary)` with piped stdio and `kill_on_drop(true)`. **The environment is inherited**, so `TS_*` envknobs pass through. |
| Data (TCP) | **One loopback TCP connection per tailnet TCP connection.** Rust binds the bridge on `127.0.0.1:0` (`bridge.rs:64`). The sidecar `net.Dial`s it per connection (`main.go:2729`) and writes a header: magic `TRFF`, version, 32-byte token, direction, port, request id, remote address, and up to 4 KiB of WhoIs JSON (`header.rs:8-19`, `41`; Go `main.go:2765-2828`). After that it's a raw byte stream. | Rust reads the header with ~10 unbuffered small reads (`header.rs:128-200`). |
| Data (UDP/QUIC) | Loopback UDP to the sidecar relay with a 6-byte `[IPv4][port]` prefix (`network/mod.rs:565-641`; Go `udp_relay.go`, 64 KiB buffers at `:165,191-192`). | |
| Proxy path | **No bridge.** The sidecar's Go HTTP server speaks to flux's loopback HTTP port through `ReverseProxy`. | `main.go:3247-3285` |

### 3.2 One byte of video, flux → tailnet, via `listen_tcp` on macOS [V/I]

| # | Step | Copy? | Syscalls / switches |
|---|---|---|---|
| 1 | flux reads the file (page cache → user buffer) | copy | `read`/`pread` (+ blocking-pool hop with `tokio::fs`) |
| 2 | flux `write()` to the bridged loopback `TcpStream` | copy into kernel | 1 `write` per chunk; **Nagle is on** (no `set_nodelay` anywhere in core) |
| 3 | Kernel loopback TCP → sidecar socket buffer | kernel-internal | — |
| 4 | Sidecar `idleCopy` `Read` into a **32 KiB** buffer (`main.go:2877-2893`) | copy to user | kevent wakeup + `read`. `SetReadDeadline`/`SetWriteDeadline` per chunk (runtime timers) |
| 5 | `gonet.TCPConn.Write` → gVisor send buffer | copy | in-process |
| 6 | gVisor segments at MSS ≈ 1240 B (TUN MTU **1280**, `tstun/mtu.go:79,133-149`). **No GSO on macOS** (`netstack.go:401-407`: GSO/GRO only when `runtime.GOOS == "linux"`) | per-packet buffers | — |
| 7 | wireguard-go ChaCha20-Poly1305 per packet | transform pass | — |
| 8 | magicsock `sendmsg` **per packet**. UDP batching is a no-op off Linux (`net/batching/conn_default.go`: `TryUpgradeToConn` returns `pconn` unchanged) | copy to kernel | **~1 syscall per 1.24 KB** |

That's about **5 copies and 1 crypto pass per byte, and on macOS ~26 `sendmsg` calls per 32 KiB**. A plain LAN server with `sendfile`, or kernel Tailscale on Linux with TSO/GSO, does a fraction of this. On **Linux**, tsnet enables netstack GSO/GRO (`HostGSOSupported`, `tcpGROSupported`) and UDP GSO/GRO batching. That is the path behind Tailscale's "10 Gb/s" results: 13.0 Gb/s single-stream wireguard-go on an i5-12400 ([Tailscale blog, "more throughput"](https://tailscale.com/blog/more-throughput)) [E].

The **sidecar reverse-proxy path** has the same loopback hop and gVisor/WireGuard tail. It adds Go HTTP parsing in front, plus ReverseProxy's 32 KiB copy loop.

### 3.3 Where the CPU actually goes (quantified)

| Measurement | Value | Source |
|---|---|---|
| One loopback relay hop, Truffle's exact loop (32 KiB + per-read deadline) | **8.4 Gb/s on one core, 0.39–0.48 ms CPU/MB** | Local Go bench (§10.3) [V] |
| Same hop, 256 KiB buffer, no deadline | 13.0–18.2 Gb/s, 0.25–0.34 ms CPU/MB | Local Go bench [V] |
| Whole stack, same host, release, two processes | **~300% CPU per sidecar at 387 Mb/s** ≈ **62 ms CPU/MB** per side | `benchmark/README.md` ("The sidecars are the ceiling") [V, derived] |
| Rust processes in the same runs | 25–40% CPU | same |
| Egress (one bridge) vs TCP (two bridges) | +12.6% (397.6 vs 353.0 Mb/s), "within the run-to-run spread" | same |

**Conclusion [I, medium-high confidence]:** the Rust↔Go loopback hop is roughly **0.7–1.5%** of per-byte CPU on macOS. "Raw stream handoff", "fd passing" or "zero-copy bridge" work would at best recover that, plus a little setup latency. The 62 ms/MB is per-packet userspace cost on macOS (no GSO/GRO, no UDP batching, 1280 MTU). At ~0.78 cores per 100 Mb/s per side, a macOS flux server can carry several 4K remuxes, but each one costs real CPU. **Linux servers should be much cheaper. Truffle has no Linux numbers yet** (every result in `benchmark/README.md` is from a Mac).

### 3.4 Per-connection limits and behaviours that matter for streaming [V]

| Limit / behaviour | Value | file:line | Flux impact |
|---|---|---|---|
| **Idle reap, per direction** | 600 s default (`idleDeadline`), configurable through `idle_timeout_secs` | `main.go:61`, `2853-2893`, `1008-1013` | **Cuts long single-request streams** (see below). |
| Bridge concurrent *handshakes* | 256 (`Semaphore::try_acquire_owned`). On overflow the connection is **silently dropped** | `bridge.rs:34`, `124-131` | The permit lives only through header and delivery (`bridge.rs:137-147`), so it caps bursts, not open conns. RFC 023's table says "256 concurrent conns" (`023-http-serving.md:386`). |
| Header read timeout | 2 s | `bridge.rs:37` | — |
| **Head-of-line lock** | The `incoming_channels` `Mutex` is held across `tx.send(conn).await` | `bridge.rs:209-217` | If one listener's 64-slot channel is full (flux slow to `accept`), **every port's incoming connections stall**. |
| Listen registration race | The bridge registers the port **after** the sidecar confirms the listen | `provider.rs:1118`; drop at `bridge.rs:220-223` | A connection arriving in that window is dropped (small window). |
| Two-stage accept channel | 64 → forwarding task → 64 | `provider.rs:1042`, `transport/tcp.rs:73-95` | Extra task hop per connection (minor). |
| Loopback ephemeral ports | One per bridged conn. macOS MSL 15 s → 30 s TIME_WAIT; ports 49152–65535 | `sysctl` here | Roughly 550 new conns/s sustained before exhaustion [I]. Keep-alive matters. |
| tsnet TIME_WAIT | Tuned to **1 s** (gVisor default 60 s) | `netstack_tuning.go:35-68`, applied at `main.go:1064` | Fixed 30 s SYN stalls. About 0.8–0.9% of new conns still pay a ~1 s retransmit at ~450 conn/s (bench README). |
| WebSocket-hijack idle (proxy) | Fixed 600 s, not configurable | `main.go:2651`, `2658` | — |
| Proxy upstream pool | 64 idle/host, 90 s idle | `main.go:3239-3268` | — |
| Proxy server | `IdleTimeout 120 s`, `ReadHeaderTimeout 10 s` | `main.go:2554-2559` | — |
| WhoIs per accept | ~86 µs median after the 0.7.18 fix (was ~41 ms) | bench README "Core fixes"; `main.go:747-798` | — |

**The idle-reap bug, verified twice.**
1. *Go repro* (`<scratch>/idlecopy/main.go`, `bridgeCopy`/`idleCopy` copied verbatim, 500 ms timeout). The "player" sent one `GET`, then streamed reads for 3.2 s. The Rust side saw **read-EOF at 501 ms while the stream was still flowing**. Bytes kept arriving, so the sidecar only half-closes.
2. *hyper/axum repro* (`…/scratchpad/halfclose/src/main.rs`, axum 0.8.9 / hyper 1.11.0, 60×1 KiB chunks over 3 s, client write-half closed at 500 ms):

| Server config | Bytes delivered | Ended at |
|---|---|---|
| `axum::serve` (default) | **9,363 of ~61,949** | 503 ms (connection closed) |
| `hyper::server::conn::http1::Builder::new().half_close(false)` (default) | 9,382 | 502 ms |
| `.half_close(true)` | **61,968** (complete) | 3.1 s |

hyper's docs give the default as `false` ([docs.rs hyper 1.11 `http1::Builder::half_close`](https://docs.rs/hyper/1.11.0/hyper/server/conn/http1/struct.Builder.html)) [E]. **Combined effect:** any flux response streamed over `listen_tcp` that lasts longer than `idle_timeout`, with a silent client direction, is cut. Chrome `<video>` with `Range: bytes=0-`, VLC, mpv and progressive direct play all fit that description. HLS (many short requests) and the sidecar proxy path are not affected.

---

## 4. tsnet / gVisor tuning knobs

### 4.1 What tsnet v1.104.1 does by default [V]

| Knob | Value | Where |
|---|---|---|
| TCP congestion control | **CUBIC** (was reno) | `wgengine/netstack/netstack.go:385-396` |
| TCP RX buffer {min, default, max} | 4 KiB / 1 MiB / **8 MiB** (non-iOS) | `netstack_tcpbuf_default.go`; `setTCPBufSizes` `netstack.go:295-330` |
| TCP TX buffer | 4 KiB / 1 MiB / **6 MiB** | same |
| iOS RX/TX max | gVisor default **4 MiB** (memory limit) | `netstack_tcpbuf_ios.go`; gVisor `tcp/protocol.go:41-52` |
| GSO / GRO | **Linux only** | `netstack.go:401-407` |
| `GSOMaxSize` | 64 KiB | `link_endpoint.go:327-331` |
| TUN MTU | **1280** (`safeTUNMTU`). Override with `TS_DEBUG_MTU` (debug knob). `TS_DEBUG_ENABLE_PMTUD` probes only | `net/tstun/mtu.go:79`, `133-149` |
| UDP send/recv batching | **Linux only** (`conn_linux.go`). No-op elsewhere | `net/batching/conn_default.go` |
| In-flight forwarded-conn attempts | 2048 (macOS/Windows), 8192 (Linux), 1024 (mobile); 2/3 of that per client | `netstack.go:76-121` |
| Keepalive knobs | `TS_NETSTACK_KEEPALIVE_IDLE`, `TS_NETSTACK_KEEPALIVE_INTERVAL` | `netstack.go:133-139` |
| Netmap disk cache | Used when the control plane sets `NodeAttrCacheNetworkMaps`; `TS_USE_CACHED_NETMAP` defaults true | `ipn/ipnlocal/local.go:3205-3235` |

Tailscale's September 2026 "Making Tailscale faster" post lists more Linux and Android work: packet buffers shared across a batch (~5%), shorter queues, `writev`, multi-queue, and netmap caching "on by default in v1.104" ([blog](https://tailscale.com/blog/making-tailscale-faster)) [E]. **None of it is described for macOS, iOS or tsnet.**

### 4.2 What Truffle sets [V]

- **Only TIME_WAIT → 1 s** via `ns.SetTransportProtocolOption(tcp.ProtocolNumber, TCPTimeWaitTimeoutOption)` (`netstack_tuning.go:40-68`). It reaches the netstack through `srv.Sys().Netstack.GetOK()`, so **other gVisor options are reachable the same way**: buffer ranges, SACK, `TCPModerateReceiveBufferOption`, delayed ACK.
- No MTU, buffer-size or GSO changes, and no `ControlURL`/`Port` passthrough. The sidecar inherits the parent environment (`sidecar.rs:354-361`), so flux can experiment with `TS_DEBUG_MTU` and similar **without patching Truffle**. These are debug knobs, not production settings.
- Logging: tsnet's backend `Logf` is off unless `TRUFFLE_DEBUG` is set (`main.go:1043-1050`).

---

## 5. The benchmark suite (`benchmark/`)

**What it is** [V]: the `truffle-bench` engine and CLI (`serve`, `peers`, `run`, `pair`, `suite`, `report`, legacy `transport`), a Tauri desktop app, and a generated v3 report schema with 90 fixtures. Workloads (`benchmark/README.md` §Workloads):

| Workload | Measures |
|---|---|
| `path` | disco RTT, path (direct/DERP), start-up costs |
| `tcp` | goodput, connect time, CPU ms/MB, loaded RTT; latency mode with `nodelay` A/B |
| `udp` | paced stream loss/jitter, echo RTT, ramp to max sustainable rate |
| `quic` | throughput/latency per stream |
| `ws` | envelope request/reply RTT, stream msgs/s and drops |
| `file-transfer` | wire/e2e Mb/s, `hash_ms`, `negotiate_ms` |
| `synced-store` | convergence ping-pong, burst coalescing |
| `http` | HTTP/1.1 through the sidecar proxy or a direct listener, keep-alive or new-connection |
| `video` | synthetic paced I/P frame stream over udp/quic/tcp (720p30 … 4k30): delivered fps, late/dropped %, frame latency |
| `egress` | TCP through an RFC 026 route |

**How to run** (from `CLAUDE.md`, `docs/tailscale-performance.md`): `cargo run --locked -p truffle-bench --release -- suite --profile smoke --pair` (needs an auth key via `--auth-key-file` or `TRUFFLE_BENCH_AUTH_KEY`). For two machines: `truffle-bench serve` on one and `suite --peer <name>` on the other. Native baseline: `go run ./cmd/tsnet-bench`. **I didn't run any of these.** They start tailnet nodes.

**Stored results.**
- `target/bench-results/` in the checkout holds two small 2026-09-13 reports. `synced-store`: 200 ops, convergence 28 ms. `request-reply`: 1 KiB, RTT p50 0.293 ms / p99 0.56 ms.
- `<scratch>/truffle-tailscale/bench-results/` holds release-build smoke runs from 2026-10-12, all same-host 1 MiB transfers dominated by slow start. TCP 31.5–34.0 MB/s (connect 0.45–1.06 ms). QUIC concurrency 2: 33.2 MB/s aggregate (connect 12.7–30.3 ms). File transfer 45–49 MB/s. A native tsnet→Rust interop run measured 41 MB/s. **Not steady-state numbers.**

**Documented measurements** (`benchmark/README.md`, all on Macs, 2026-10-16):

| Metric | Value |
|---|---|
| TCP 1 stream, same host | **bimodal: ~330–365 or ~480–550 Mb/s** |
| QUIC 4 streams | 390–467 Mb/s (403 in A/B) |
| Egress | 360–494 Mb/s |
| HTTP via sidecar proxy, keep-alive | 3.9k / 6.1k / 8.9k req/s at 1 / 2 / 4 workers, p50 0.25–0.43 ms |
| HTTP direct listener, keep-alive, 4 workers | 9.7–10.4k req/s |
| HTTP proxy, new conn per request, 4 workers | 451 req/s, p50 0.79 ms, p99 5.1 ms (was 24.6 req/s before the 0.7.18 WhoIs fix) |
| QUIC connect | ~1.5–1.8 ms (was ~43 ms) |
| WS request/reply p50 | 0.267 ms |
| UDP echo p50 / p99 | ~0.76 ms / 1.14 ms |
| Video 720p30 over UDP, LAN, 2 Macs | 30 fps, 0% late or dropped, frame latency p50 38.4 ms (33 ms of that is pacing) |
| Two-machine LAN, debug initiator | RTT ~6 ms, p99 ~100 ms spikes (likely Wi-Fi power save) |
| First WS dial to a just-joined peer | sometimes ~5 s (crossing WireGuard handshakes, still open) |

**Gaps for flux:** no long-lived HTTP range-stream workload (it would have caught §3.4), no seek or TTFB-for-media workload, no Linux host, no WAN/DERP run, no `tls: true` HTTP run. The README still says the proxy's `MaxIdleConnsPerHost` "is 2" (`benchmark/README.md:327`) while the code uses 64 (`main.go:3239`), so that doc is stale.

---

## 6. Apple / iOS (`apple/`, `Package.swift`)

### 6.1 What exists [V]

- `Package.swift`: swift-tools 6.0, **platforms `.macOS(.v14)`, `.iOS("18.1")`**, so no tvOS or visionOS. The `TailscaleKit` binary target is pinned by URL and checksum (`tailscalekit-59d4bb82-ts1.104.1-go1.27.2`, `Package.swift:48-53`) and linked **only on iOS** (`Package.swift:67`).
- XCFramework slices (`Info.plist`): `ios-arm64` and `ios-arm64_x86_64-simulator`. **No tvOS, no macOS.**
- `TailscaleKitBackend` compiles only `#if os(iOS) && canImport(TailscaleKit)` (`TailscaleKitBackend.swift:1`). Usage:
  - `TailscaleNode(config:)` on a blocking queue (`:54-63`); `loopback()` starts tsnet's SOCKS5 and LocalAPI (`:69`); an IPN bus consumer and status poll (`:73-81`); `startLoginInteractive` for the interactive auth flow (`:82-84`).
  - `dial`/`listen` → `tailscale_dial`/`tailscale_listen` return **fds** (`:125-150`).
  - Reads are `Darwin.read` on a dispatch queue, allocating a fresh `[UInt8]` and `Data` per read (`:491-508`). The callers read 64 KiB (`LengthPrefixFrameTransport.swift:111`).
  - WhoIs goes through an authenticated LocalAPI GET over the loopback (`:153-224`).
  - `makeURLSession` copies a `URLSessionConfiguration`, applies `proxyVia(node)` (a SOCKS5 `ProxyConfiguration`, iOS 17+), and **rejects background sessions** (`:226-243`, check at `:230-232`).
- libtailscale (`<scratch>/truffle-tailscale/apple-vendor-source/libtailscale/tailscale.go:344-395`): each connection is an **AF_UNIX socketpair**. Go copies `io.CopyBuffer` with 64 KiB arrays in both directions, and **no `SO_SNDBUF`/`SO_RCVBUF` is set**. Truffle carries four patches: remote-address preservation across `SCM_RIGHTS`, peer notifications, IPN stream recovery, and testcontrol (`apple/Vendor/README.md`).

### 6.2 Measured: Darwin AF_UNIX vs loopback TCP (blocking I/O, 64 KiB chunks, 256 MiB, this M1 Max) [V]

| Socket | Effective buffers | Throughput |
|---|---|---|
| loopback TCP | 147 KB / 408 KB | **14.9 Gb/s** |
| AF_UNIX socketpair, default | **8 KiB / 8 KiB** (`net.local.stream.sendspace=8192`) | **1.02 Gb/s** |
| AF_UNIX, `SO_SNDBUF`/`SO_RCVBUF` = 1 MiB | 1 MiB | 3.31 Gb/s |
| AF_UNIX, 4 MiB | 4 MiB | 2.80 Gb/s |

(I also tried a Go netpoller variant over AF_UNIX. It hadn't finished 1 GiB after ~8 minutes and I killed it. The cause is unverified; it may be pathological.) **Implications:**
- On iOS, every libtailscale stream rides an 8 KiB-buffered socketpair. That's ~1 Gb/s and many syscalls, assuming iOS shares the macOS XNU default (unverified on device). It's enough for one 4K stream, but it wastes CPU and battery, and a 1-line `setsockopt` patch fixes most of it.
- On the desktop, **don't** move the bridge's data plane to AF_UNIX on macOS. Loopback TCP is 4.5–15× faster there.

### 6.3 Is tvOS supported? **No.** [V]/[I]
- There's no tvOS slice or platform in `Package.swift`, and upstream libtailscale's Xcode project lists `SUPPORTED_PLATFORMS = "iphonesimulator iphoneos"` (macOS scheme for dev) (`…/libtailscale-59d4bb82…/swift/TailscaleKit.xcodeproj/project.pbxproj:498`).
- Go has no tvOS GOOS: `go tool dist list` shows only `ios/arm64` and `ios/amd64`. Go's own `cmd/link` test `TestBuildForTvOS` builds tvOS as `GOOS=ios` against the Apple TV SDK ([golang.org/issue/71990](https://golang.org/issue/71990)) [E], so a tvOS TailscaleKit slice is **plausible but unsupported**.
- Pragmatic path: Tailscale's own Apple TV app is a system VPN on tvOS 17+, with 18+ recommended ([Tailscale docs, Apple TV](https://tailscale.com/docs/install/appletv)) [E]. A flux tvOS app can use plain URLs (MagicDNS or 100.x) when that app is installed.

### 6.4 How an iOS player could consume media [I]

| Option | Works with AVPlayer? | Notes |
|---|---|---|
| `MeshNode.urlSession()` (SOCKS5 via `proxyVia`) | **No.** AVPlayer uses its own loader. Custom `URLProtocol` and proxy settings aren't honoured, per developer reports ([Apple forums 128989](https://developer.apple.com/forums/thread/128989), [113063](https://developer.apple.com/forums/thread/113063?page=2)) [E] | Fine for JSON, API and image fetches in the foreground. |
| `AVAssetResourceLoaderDelegate` with a custom scheme | Fragile for HLS segments (error -12881 reports, [forum 771302](https://developer.apple.com/forums/thread/771302)) [E] | Not recommended. |
| **Loopback HTTP gateway** (127.0.0.1:port → fixed tailnet target) | **Yes**, ordinary `http://127.0.0.1:…/master.m3u8` or `.mp4` URLs | Best done **in Go** as a libtailscale patch mirroring RFC 026 E2, so bytes don't cross Swift. ATS local-networking config needs on-device verification. Stops when the app is suspended, but runs while AVPlayer holds background audio or PiP. AirPlay receivers can't reach a 127.0.0.1 URL. |
| Custom player (libmpv/VLCKit/FFmpeg) on `dial()` fds or the gateway | Yes | Good for direct play of MKV and similar. |
| Background downloads (`URLSession` background config) | **No.** It's rejected (`TailscaleKitBackend.swift:230-232`) | A background session runs out of process and can't reach the in-app loopback SOCKS [I]. Use foreground downloads plus resumable Range requests. |

---

## 7. Known gaps: confirmed or refuted

| Claim (from `initial-discussion.md` or the task) | Verdict | Evidence |
|---|---|---|
| `ProxyConfig.announce` is a no-op | **Confirmed** | `proxy/types.rs:44-49` ("Currently a no-op"). `proxy/discovery.rs` helpers have no callers. RFC 023 G1 and Phase 5 (`023-http-serving.md:96`, `411-417`, `484`). |
| `send_bytes` uses base64 | **Confirmed** | `node.rs:1214-1231`, `96-103`. |
| Background `URLSession` is rejected | **Confirmed** | `TailscaleKitBackend.swift:230-232`; RFC 024 (`024-truffle-swift.md:986`). |
| Funnel is rejected | **Confirmed** | `packages/core/src/serve.ts:316-317` throws. The Rust `ProxyConfig` has no funnel field and the sidecar never calls `ListenFunnel`. Phase 4 is future work (`023-http-serving.md:3`, `409`). |
| No service discovery | **Confirmed** | `peers()` is hostname-prefix filtered (`provider.rs:747-750`). tsnet `ListenService` exists (`tsnet.go:1873+`, needs tagged hosts) but is unused. |
| "Need WhoIs identity on proxied requests" | **Refuted (already there)**, with gaps | Headers at `main.go:2463-2485`; `RawIncoming.remote_identity`; `Node::whois`. Missing: CapMap, tags, and a loopback trust model for the headers. |
| "The proxy path supports HTTP/2" (RFC 023 table) | **Refuted for TLS listeners** | `tsnet.go:1429-1431` (no `NextProtos`); Go `Server.Serve` doesn't add `h2`. |
| "256 concurrent conns" on the bridge | **Imprecise.** It's 256 concurrent *handshakes*, and the excess is dropped silently | `bridge.rs:124-147`. |
| `AuthRequired` "may be emitted multiple times" | **Refuted.** It fires once per start | `main.go:1114-1117` vs `network/mod.rs:373-375`. |
| Idle reap applies "when idle in both directions" | **Refuted by repro.** It's per direction, with a half-close | `main.go:58-60` vs `2853-2893`; §3.4. |
| "The Truffle HTTP proxy keeps video bytes off the envelope path" | **Confirmed** | The proxy is Go `ReverseProxy` and static routes use `ServeContent`. No envelope involved. |
| `truffle-rfc025` checkout = newer roadmap | **Refuted.** It's detached at **v0.7.14**, older than `main` (v0.7.19), and RFC 025 has already merged | `git -C /path/to/truffle-rfc025 log -1` → `9f0d628 release 0.7.14`. |

**Roadmap direction from the RFCs:** RFC 023 Phase 4 (Funnel) and Phase 5 (service discovery via `ss:truffle-services`, `announce` default flips to `false`) are both future work. RFC 024 still needs its device release gates; uniFFI was deferred. RFC 025 (login-scoped nodes) and RFC 026 (egress) have shipped. Planned in RFC 027: QUIC datagrams, QUIC stats, a NAPI lane, Swift peers, a "tsnet ceiling" `bench:*` command, TLS HTTP, a latency waterfall, a Taildrop comparison.

---

## 8. Code quality, test coverage, stability risks

**Strengths** [V]
- **Tests:** `cargo test -p truffle-core --lib`: **413 passed, 0 failed, 3 ignored, 30.58 s** (run here, offline, mock providers and shell-script fake sidecars only). Plus 46 integration tests in `crates/truffle-core/tests` (live tailnet, gated on `TRUFFLE_TEST_AUTHKEY`), 82 Go tests, and 113 Swift tests and assertions.
- **CI:** 14 workflows (`ci`, `integration-tests` with a live pair on every PR, `benchmarks` nightly, `codeql`, `security-audit`, `apple-tests`, release pipelines). `cargo-deny`. Dependabot is active, and RUSTSEC fixes land within a day (0.7.14, 0.7.19).
- **Security hygiene:** sidecar downloads are verified against committed SHA-256 (`truffle-sidecar/build.rs`, *refuse unverified executable*). Auth keys are redacted in `Debug` (`node.rs:1664-1682`). Constant-time token compare (`bridge.rs:178`). Spoofed `Tailscale-*` headers are stripped. `os.OpenRoot` for static routes. Fail-closed login gates. Atomic device-id writes (`node.rs:1696-1727`).
- **Structured lifecycle:** `TaskTracker` + `CancellationToken`, and a bounded `stop()` (`node.rs:353-398`, `769-810`).
- **Velocity:** 0.7.12 (2026-08-03) → 0.7.19 (2026-10-17), and every perf root cause in the bench README got fixed within days (the 41 ms WhoIs stall, the 30 s TIME_WAIT hang, the proxy pool size).

**Risks for building a product on it**

| Risk | Severity | Evidence |
|---|---|---|
| Per-direction idle reap cuts long streams | **High** for direct play over `listen_tcp` | §3.4 |
| Pre-1.0: minor releases may break the API | Medium | `docs/API-STABILITY.md`; e.g. `send`/`broadcast` deprecated in 0.7.0 |
| Sidecar crash → node stays dead (`ProviderState::Stopped`, health "crashed"), no event, no restart | **High** for a 24/7 server | `provider.rs:689-700`; `sidecar.rs:505-561` |
| Fixed 5-minute auth/approval timeout | Medium (headless first run) | `provider.rs:877` |
| Bridge HOL lock across `send().await` | Medium (bursty thumbnail traffic) | `bridge.rs:209-217` |
| Peer-event lag with no resync | Medium (larger tailnets) | `provider.rs:725-727`; bench README restart scenario |
| Discovery tied to hostname prefix | Medium | `provider.rs:747-750` |
| Session broadcast drops at ~250k msg/s | Low for flux | bench README |
| Go sidecar is a 20–33 MB binary per platform; build-time download; targets: darwin arm64/amd64, linux gnu amd64/arm64, windows amd64. No musl, armv7 or FreeBSD | Medium (NAS and Docker packaging) | `crates/truffle-sidecar/build.rs:25-37` (`TRUFFLE_SIDECAR_PATH` escape hatch) |
| Very large files (`node.rs` 3.8k, `provider.rs` 3.2k, `main.go` 3.5k lines) | Low (maintainability) | — |
| No Linux or WAN performance data; macOS throughput is bimodal (350 vs 500 Mb/s) | Medium | bench README |
| Proxy identity headers forgeable by local processes | Medium on multi-user hosts | §2.3 |
| `std::sync::Mutex` `unwrap`/`expect("… poisoned")` in task and proxy state | Low | `node.rs:389,394`; `proxy/mod.rs:48,85` |

---

## 9. Recommendations for flux

### 9.1 How flux should use Truffle **today** (no Truffle changes)

1. **Serve flux's HTTP API and media directly from Rust with `listen_tcp`**, not the sidecar proxy. You get unforgeable per-connection WhoIs identity and full control over HTTP semantics. Use hyper directly, because `axum::serve` doesn't expose `half_close`:
   ```rust
   let node = Node::<TailscaleProvider>::builder()
       .app_id("flux")?.device_name(&name)
       .sidecar_path(truffle::sidecar_path())
       .state_dir("/var/lib/flux/truffle")
       .idle_timeout_secs(6 * 3600)                 // mitigate §3.4 until fixed upstream
       .build_with_auth_handler(|url| setup_ui::show_login(url)).await?;
   let mut ln = node.listen_tcp_opts(443, ListenOpts { tls: true }).await?; // browsers: https://<dns_name>/
   while let Some(inc) = ln.accept().await {        // RawIncoming { stream, remote_addr, remote_identity }
       inc.stream.set_nodelay(true)?;
       let svc = TowerToHyperService::new(router.clone().layer(Extension(inc.remote_identity)));
       tokio::spawn(hyper::server::conn::http1::Builder::new()
           .half_close(true)                        // verified: keeps streaming after a half-close
           .serve_connection(TokioIo::new(inc.stream), svc).with_upgrades());
   }
   ```
   You could implement `axum::serve::Listener` for `RawListener` (trait: `accept() -> (Io, Addr)`, `local_addr()`; [docs.rs axum 0.8.9](https://docs.rs/axum/0.8.9/axum/serve/trait.Listener.html)). That keeps the half-close bug, though, so hyper is the safer choice.
2. **Keep the hostname convention** (`truffle-flux-<device>`) until announcement exists, so flux clients can find servers with `peers()`. Show `local_info().dns_name` in the UI rather than building URLs.
3. **Use `SyncedStore` for presence and "now playing"** (small slices). Use **HTTP (Range) for everything with bytes**: posters, media, downloads. Don't use `send_bytes` or `file_transfer` for media.
4. **Surface path quality**: show `Peer.connection_type` and `ping()` (direct vs DERP) in the player's stats overlay.
5. **Plan for Linux servers** (NAS and Docker) as the high-throughput target. Treat macOS as "a few streams".
6. **Apple TV for v1:** rely on the official Tailscale tvOS app. **iPhone/iPad:** in-app TailscaleKit plus a loopback gateway (9.2 #8).

### 9.2 Prioritized Truffle improvements flux needs

Effort: S ≤ 2 days, M ≤ 2 weeks, L > 2 weeks.

| # | Pri | Improvement | Problem / evidence | Proposed change | Expected impact | Effort |
|---|---|---|---|---|---|---|
| 1 | **P0** | **Connection-level idle reaping** | `idleCopy` deadlines are per direction (`main.go:2853-2893`), contradicting `main.go:58-60`. Verified cut at 600 s together with hyper's default `half_close(false)` (§3.4). | Share an atomic "last activity" across both copy goroutines. Reap only when *both* are idle for `timeout`. Don't treat a read-deadline timeout as EOF while the peer direction is active. Add a Go test (stream one way for 3× timeout) and a bench workload. | Removes mid-movie disconnects on the `listen_tcp`, `open_tcp` and egress paths. | **S** |
| 2 | **P0** | **Sidecar supervision** | `ProcessExited` sets the provider to `Stopped` and nothing else (`provider.rs:689-700`). | Emit `NetworkStatusEvent::SidecarExited{code}`. Optionally auto-restart with backoff and re-issue listens and proxies (the state is known in Rust). | Lets a 24/7 media server survive a Go panic or OOM. | **M** |
| 3 | **P0** | **Headless-friendly start** | Fixed 300 s timeout (`provider.rs:877`); auth URL emitted once (`main.go:1114`); no `ControlURL`, tags or OAuth (`node.rs:2066`, `main.go:87-99`). | `NodeBuilder::{start_timeout(Option<Duration>), control_url(..), tags(..), oauth_client_secret(..)}`. Re-emit a changed `AuthURL`. Add a non-blocking `build()` that returns a node in `NeedsLogin` state. | First-run UX on a NAS, Headscale users (important for an OSS Plex alternative), tag-owned servers. | **S–M** |
| 4 | **P0** | **Richer WhoIs: CapMap and tags** | Only 5 string fields are copied (`main.go:193-199`, `2942-2951`); `WhoIsResponse.CapMap` is dropped. | Add `cap_map: HashMap<String, Vec<serde_json::Value>>`, `tags`, `user_id` to `TailscalePeerIdentity`. Inject `Tailscale-App-Capabilities` (JSON) on the proxy path. Bump the 4 KiB header cap or move identity into a follow-up frame. | Flux roles and permissions straight from tailnet grants ([KB 1537](https://tailscale.com/kb/1537/grants-app-capabilities)); no separate user DB. | **S–M** |
| 5 | **P1** | **Bridge robustness** | HOL `Mutex` across `send().await` (`bridge.rs:209-217`); silent drop past 256 handshakes (`bridge.rs:124-131`); listen registration race (`provider.rs:1118`). | Clone the sender and drop the lock before `send`, or use `try_send` with a counted drop and a log line. Register the bridge channel *before* sending `tsnet:listen`. Expose drop counters. | Avoids stalls under bursts of hundreds of thumbnail requests and makes failures visible. | **S** |
| 6 | **P1** | **Service announcement and discovery that survives custom hostnames** | `announce` no-op (`proxy/types.rs:44-49`); prefix filter (`provider.rs:747-750`). | (a) RFC 023 Phase 5: `Node::services().announce(ServiceInfo{name, port, scheme, meta})` / `.watch()` on a SyncedStore slice. (b) Admit peers by an *app marker* other than the hostname prefix (e.g. a hello on 9417 or a node attribute), so `.hostname("flux")` nodes stay peers. (c) Optional `ListenService` wrapper for tagged HA servers (`svc:flux`, Tailscale ≥ 1.86, [docs](https://tailscale.com/docs/features/tsnet/how-to/register-service)). | Clients find "Flux on Mac mini" with an API version and capabilities; pretty URLs keep working. | **M** |
| 7 | **P1** | **Linux performance baseline and tsnet tuning hooks** | GSO/GRO and UDP batching are Linux-only (`netstack.go:401-407`, `net/batching/conn_default.go`); all data is macOS-only (bench README). | Run `suite --profile standard` on Linux x86 and arm64. Add `NodeBuilder::netstack_options(...)` (buffer ranges, MTU via a supported path) applied the way `tuneNetstack` does. Let the env passthrough be explicit. | Tells flux where the streaming ceiling is. An inference: Linux will be several times cheaper per Mb. | **S** bench / **M** knobs |
| 8 | **P1** | **iOS loopback gateway, plus socketpair buffers** | AVPlayer ignores the SOCKS `URLSession` (§6.4); libtailscale socketpairs at 8 KiB (`tailscale.go:344-395`; §6.2 measured 1.0 Gb/s). | Add a libtailscale patch: `tailscale_forward_loopback(target) -> port`, a Go net.Listen on 127.0.0.1 spliced to `srv.Dial`, mirroring `egress.go` E2, exposed as `MeshNode.loopbackGateway(to: peer, port:)`. Separately, `setsockopt(SO_SNDBUF/SO_RCVBUF, 1 MiB)` on socketpairs (3.3× in my bench). | AVPlayer and HLS work with embedded Tailscale; less per-byte CPU on phones. | **M** |
| 9 | **P1** | **Media-aware bench workloads** | No long-lived Range stream, seek or TTFB workload; the §3.4 bug went unnoticed. | Add `http-media`: paced 80 Mb/s single Range stream for > 2× idle timeout, seek latency (a new Range on a keep-alive conn), TTFB, HLS segment fetch at 2–6 s cadence. | Guards flux's core experience in CI. | **S–M** |
| 10 | **P2** | **HTTP/2 on TLS listeners** | `tsnet.ListenTLS` gives no ALPN `h2` (`tsnet.go:1429-1431`); RFC 023's "h2 free" is wrong. | Build the TLS listener in the sidecar with `NextProtos: ["h2","http/1.1"]` and `GetCertificate` from tsnet. For `listen_tcp_opts(tls)`, pass the ALPN result to Rust or terminate in Rust. | Multiplexed artwork and API loads in the web UI; fewer TLS handshakes. | **S–M** |
| 11 | **P2** | **"X-Accel-Redirect" for the proxy** | Direct-play bytes cross Go → loopback → Rust → loopback → Go. The Go static path already has `ServeContent` (`main.go:3206`). | Upstream sets `X-Truffle-File: <root-id>/<rel-path>` (+ optional `Content-Type`). A `ReverseProxy.ModifyResponse` hook validates the path under an `os.OpenRoot` root registered at `proxy:add` and serves it via `http.ServeContent`. | Removes the loopback hop and Rust CPU for direct play while Rust keeps authorization. It's only ~1% of stack CPU (§3.3), so do it for simplicity and Range correctness, not speed. | **S** |
| 12 | **P2** | **Cheaper bridge loop** | 32 KiB buffer and two deadline calls per chunk (`main.go:2877-2893`); Rust sockets have Nagle on; ~10 small header reads (`header.rs:128-200`). | 256 KiB pooled buffers. Refresh deadlines at most every ~1 s. `set_nodelay(true)` on bridge sockets. Buffered header read. | Hop measured at 8.4 → 13–18 Gb/s and 0.48 → 0.34 ms/MB. **Small** end-to-end (§3.3). | **S** |
| 13 | **P2** | **Raw stream handoff / fd passing** | Each conn costs a loopback `connect`, a header, a token and a TIME_WAIT port (~550 conn/s ceiling on macOS [I]). | Keep loopback TCP for *data* on macOS (AF_UNIX is 4.5–15× slower there, §6.2). Optionally create the loopback pair inside Go and pass one fd to Rust via `SCM_RIGHTS` over a control Unix socket, which removes the listener, semaphore and token handshake. | Lower connection setup latency and no exposed bridge port. **Throughput unchanged.** | **M** |
| 14 | **P2** | **Binary messaging** | base64 JSON (`node.rs:1214-1231`). | Binary WS frames for `bytes` payloads (a new envelope codec), or a documented "use a raw stream" API. | Only matters if flux pushes binary over the bus (it shouldn't). | **M** |
| 15 | **P2** | **Funnel** (public share links) | Reserved and rejected (`serve.ts:316-317`). | RFC 023 Phase 4 with anonymous-identity semantics. | "Share with a friend outside the tailnet" feature. | **M** |
| 16 | **P2** | **Fixed WireGuard UDP port and peer-relay visibility** | `tsnet.Server.Port` not exposed. | `NodeBuilder::wireguard_port(u16)` for manual router forwards. Surface peer-relay and DERP state in `Peer`. | Better direct-path odds for remote (away-from-home) streaming. | **S** |
| 17 | **P2** | **Peer-event lag resync** | `Lagged` only logs (`provider.rs:725-727`). | On lag, send `tsnet:getPeers` and diff against the registry. | Peers appear promptly on large tailnets. | **S** |

### 9.3 What not to spend time on [I]
- **A zero-copy bridge, or moving the netstack into Rust, just for throughput.** The hop is about 1% of CPU, and the cost sits in per-packet userspace WireGuard on macOS.
- **QUIC over tsnet for media.** It's double-encrypted, uses 1200-byte packets with no GSO, and allocates per datagram. Its throughput matches TCP in Truffle's own runs, it adds setup latency, and HTTP Range over TCP is what every player speaks.

---

## 10. Appendix: what I ran (no tailnet nodes started)

`<scratch>` below is a throwaway scratch directory; the repro files in it were not kept. `$FLUX_WORK` is the scratch directory described in docs/ARCHITECTURE.md §Environment.

### 10.1 Unit tests
```
cd /path/to/truffle
env -u TRUFFLE_TEST_AUTHKEY -u TS_AUTHKEY CARGO_TARGET_DIR=$FLUX_WORK/target \
  TRUFFLE_SIDECAR_SKIP_DOWNLOAD=1 cargo test --offline --locked -p truffle-core --lib
→ test result: ok. 413 passed; 0 failed; 3 ignored; finished in 30.58s
```
(`--lib` only. The `tests/` integration suite loads `.env`, which holds a `TRUFFLE_TEST_AUTHKEY` entry, and would start real nodes. I didn't run it. I grepped `src/` to confirm there are no env-var reads and only shell-script fake sidecars, e.g. `node.rs:3764-3782`.)

### 10.2 Idle-reap and half-close repros
- `<scratch>/idlecopy/main.go` → `RESULT: rust saw read-EOF (client->server half-closed) at 501ms while the stream was still active`.
- `<scratch>/halfclose/src/main.rs` (built in `$FLUX_WORK/target-scratch`) → `half_close(false): 9382 bytes in 501ms`; `half_close(true): 61968 bytes in 3.1s`.

### 10.3 Relay-hop and socket benchmarks
- `<scratch>/hopbench/main.go`. Two runs, 1–4 GiB through one relay goroutine. Loopback TCP, 32 KiB + deadline: 7.65 / 8.36 Gb/s, 0.39 / 0.48 ms CPU/MB. 256 KiB, no deadline: 18.23 / 12.98 Gb/s, 0.25 / 0.34 ms CPU/MB.
- `<scratch>/unixpy.py`. Table in §6.2. `sysctl`: `net.local.stream.sendspace=8192`, `net.inet.tcp.sendspace=131072`, `net.inet.tcp.msl=15000`, ports 49152–65535.

### 10.4 Sources consulted
- Local: Truffle `d6f6664`; `tailscale.com@v1.104.1` and gVisor `v0.0.0-20260915211658-a6f909f08a72` in `$GOMODCACHE/`; libtailscale `59d4bb82` in `<scratch>/truffle-tailscale/apple-vendor-source/`; Go 1.25.6 `net/http/server.go`.
- Web: [Tailscale changelog](https://tailscale.com/changelog) · [Making Tailscale faster (2026-09-22)](https://tailscale.com/blog/making-tailscale-faster) · [Surpassing 10Gb/s](https://tailscale.com/blog/more-throughput) · [Register a tsnet app as a Tailscale Service](https://tailscale.com/docs/features/tsnet/how-to/register-service) · [Grants: application capabilities](https://tailscale.com/kb/1537/grants-app-capabilities) · [Tailscale on Apple TV](https://tailscale.com/docs/install/appletv) · [hyper http1::Builder](https://docs.rs/hyper/1.11.0/hyper/server/conn/http1/struct.Builder.html) · [axum Listener](https://docs.rs/axum/0.8.9/axum/serve/trait.Listener.html) · [Apple ProxyConfiguration](https://developer.apple.com/documentation/network/proxyconfiguration.md) · [Apple forums: AVPlayer custom loading](https://developer.apple.com/forums/thread/128989), [113063](https://developer.apple.com/forums/thread/113063?page=2), [771302](https://developer.apple.com/forums/thread/771302) · [golang.org/issue/71990 (TestBuildForTvOS)](https://golang.org/issue/71990) · [tailscale.com/wgengine/netstack/gro](https://godocs.io/tailscale.com/wgengine/netstack/gro).
- The session's web-search budget ran out partway through. I didn't verify ATS rules for AVPlayer on loopback HTTP or iOS AF_UNIX defaults; both are listed as open questions.

## 11. Open questions
1. What are tsnet sidecar throughput and CPU per Mb on **Linux** (x86 NAS, arm64 SBC) with GSO/GRO, compared with macOS's ~62 ms CPU/MB?
2. Remote streaming: how often do flux users end up on **DERP**, and what throughput does DERP or a peer relay sustain for 20–80 Mb/s streams?
3. Does iOS use the same 8 KiB AF_UNIX default as macOS? What does a libtailscale stream actually sustain on an iPhone?
4. Can AVPlayer play HLS from an in-app `http://127.0.0.1` gateway with only `NSAllowsLocalNetworking`, and does it keep running in PiP or background audio?
5. Is a tvOS TailscaleKit slice (Go `GOOS=ios` + appletvos SDK) viable for an embedded Apple TV client, or should flux require the Tailscale tvOS app?
6. Will Truffle's maintainer accept the API additions: CapMap identity, service announcement, `control_url`/tags, a supervised sidecar? Should flux carry a fork in the meantime?
7. Why did the Go netpoller AF_UNIX relay crawl (> 8 min for 1 GiB) on macOS? It matters if any design passes data over AF_UNIX on Darwin.
8. Should flux's server identity be **user-owned** (works with the login gate and `loginName`) or **tag-owned** (required for `ListenService` and HA, reports `tagged-devices`)?
