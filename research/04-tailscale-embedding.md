# 04 — Embedding Tailscale in flux: "just log in with Tailscale"

**Status:** research report, 2026-10-18
**Scope:** how flux (Rust media server + native/TV/mobile clients) can embed Tailscale so that users never port-forward, never run a reverse proxy, and never type a password. The report covers the embedding options, the performance limits that matter for 4K remux streaming, the auth, identity and sharing UX, client platforms, alternatives, ToS and licensing, a recommended onboarding flow and a throughput test plan.
**Not in scope:** a deep review of Truffle. Another agent is doing that. Truffle is covered only where it determines flux's data path.

### Confidence legend and method

- **[H]** high confidence. Verified in primary docs or in source code that I read in this session (file:line given).
- **[M]** medium confidence. Comes from a primary doc summary, or is a straightforward inference from verified facts.
- **[L]** low confidence. Extrapolation or anecdote. Treat it as a hypothesis for the test plan.
- **Source code read locally:**
  - Tailscale Go module `tailscale.com@v1.104.1`, the latest tagged release (2026-10-07). It sits in the Go module cache at `$GOMODCACHE/tailscale.com@v1.104.1`; below it is called `$TS`.
  - gVisor `v0.0.0-20260915211658-a6f909f08a72`.
  - `tailscale/tailscale-rs` at commit `0d41a6f` (2026-10-16), cloned to `$FLUX_WORK/research-04/tailscale-rs`; below it is called `$TSRS`.
  - `tailscale/libtailscale` at `d1dc45d` (2026-10-13), cloned to `$FLUX_WORK/research-04/libtailscale`; below it is called `$LIBTS`.
  - The Tailscale OpenAPI schema, downloaded to `$FLUX_WORK/research-04/dl/ts-openapi.yaml`.
  - Truffle `truffle` at `d6f6664` (v0.7.19). Mille `mille` at `c8160d5`.
  - The maintainer's earlier private note on embedding Tailscale in an iOS app without installing the Tailscale app (not published).
- **No node was started on the maintainer's tailnet.** The only measurement I ran was an in-memory netstack micro-benchmark (§4.5).
- **Web search budget:** this session hit its web-search cap partway through. A few items are marked "not re-verified" for that reason. Every one of them is listed in Open Questions.

---

## 0. TL;DR

1. **Use the "bring your own Tailscale account" model.** The flux server and every flux client run their own embedded Tailscale node, and each one shows an interactive login URL (a QR code on TVs). Each household's media then lives in *its own* tailnet, under Tailscale's free Personal plan: **6 users, unlimited user devices, 50 tagged resources** ([pricing](https://tailscale.com/pricing), changed on 2026-04-08). Never ship a shared auth key or a shared tailnet. That would be a security disaster and would conflict with ToS §2.3 ("sell, resell … use of the Services").
2. **The only production-grade embedding today is still Go `tsnet`.** The ways to reach it are a Go sidecar (Truffle's model), `libtailscale` (C ABI over cgo; Go still runs in-process), TailscaleKit (Swift, built on libtailscale), or a gomobile/cgo build on Android.
   - Tailscale now ships an **official Rust library, `tailscale-rs`** ([launch post](https://tailscale.com/blog/tailscale-rs-rust-tsnet-library-preview), 2026-04-15; crate `tailscale` 0.6.1 on 2026-09-17; BSD-3).
   - In 0.6.x it has **no MagicDNS, no HTTPS certs, no app capabilities, no node sharing, no peer relays, no Funnel/Serve and no iOS/Android support**. Its NAT traversal is still "a work in progress".
   - Its netstack (smoltcp 0.14) is hard-coded to **64 KiB TCP buffers** (`$TSRS/crates/ts_runtime/src/lib.rs:119`). That caps a single TCP stream at about **26 Mbps at 20 ms RTT and about 10 Mbps at 50 ms RTT**, below one 4K remux.
   - Track it and contribute to it, but don't build v1 on it. [H]
3. **For media throughput, the transport path matters more than the stack.**
   - A direct WireGuard path is fine for 4K remux on almost any server CPU from the last 8 years. [M]
   - Tailscale's shared **DERP relays are rate-limited per client** (`$TS/derp/derpserver/derpserver.go:648`). One measurement saw **2.2 Mbps** ([Tailscale blog, 2026-01-26](https://tailscale.com/blog/peer-relays-international-networks)).
   - **Peer relays** have been GA since 2026-02-18 and are free on all plans. The same test reached **27.5 Mbps (12.5× DERP)** with one.
   - So flux **must detect the path per client** (LocalAPI `PeerStatus.CurAddr` / `Relay` / `PeerRelay`, `$TS/ipn/ipnstate/ipnstate.go:270-272`) and choose bitrate from it. [H]
4. **tsnet (gVisor netstack) is not the throughput ceiling on modern hardware, but its CPU cost is real.**
   - Tailscale sets netstack TCP buffers to an 8 MiB receive maximum and a 6 MiB send maximum (4 MiB on iOS), uses CUBIC, and enables TCP GRO/GSO into netstack **only on Linux** (`$TS/wgengine/netstack/netstack_tcpbuf_default.go:13-19`, `netstack.go:402-408`).
   - Truffle's own same-host benchmark on Apple Silicon (macOS, so no GRO/GSO) moved **330–550 Mb/s with about 300% CPU per sidecar** (`truffle/benchmark/README.md:615-620, 733-736`).
   - Extrapolation: one 130 Mbps UHD peak costs about 1 core on a Mac-class CPU and more on an N100 or Pi 5. **This has to be measured. It's the P0 item of the test plan.** [M]/[L]
5. **Identity needs no passwords.** `WhoIs(remoteAddr)` returns `UserProfile` plus **`CapMap`** (`$TS/client/tailscale/apitype/apitype.go:35-42`). Tailscale ACL **grants with app capabilities** (`"app": {"<your-domain>/cap/flux": [...]}`) can express per-user library permissions centrally. Truffle already injects `Tailscale-User-Login` and related headers, **but doesn't forward `CapMap` yet** (verified: no `CapMap` reference in `sidecar-slim/*.go`).
6. **Friends and family.** **Node sharing** works across tailnets: reusable invites accepted up to 1,000 times, quarantined by default, and creatable through the API (`POST /device/{id}/device-invites`). **Only user-owned (untagged) nodes can be shared**, and creating invites by API needs a *user* API token, not an OAuth-client token (OpenAPI spec line 831). So the flux server should be a **user-owned node with key expiry disabled** (`POST /device/{id}/key {"keyExpiryDisabled":true}`), not a tagged node. [H]
7. **Clients:**
   - Apple TV (tvOS 17+), iOS, Android, Android TV and Fire TV all have official Tailscale apps.
   - **In-process embedding without a VPN profile** works on iOS (TailscaleKit; Truffle RFC 024 has a device-verified prototype) and on Android (tsnet with `netmon.RegisterInterfaceGetter`).
   - tvOS embedding is plausible. Tailscale's own build tooling supports `TVOS_DEPLOYMENT_TARGET` (`$TS/tool/gocross/autoflags.go:151-156`), but TailscaleKit ships no tvOS target.
   - **AVPlayer ignores in-app URLSession proxies** because media loads in `mediaserverd`. So embedded clients need a **loopback HTTP gateway** (127.0.0.1 → tailnet). Truffle's `egress:add` (fixed-target loopback splice, `sidecar-slim/egress.go:9-23`) already has this shape on desktop.
   - LG, Samsung and Roku TVs and non-tailnet browsers can't join. Serve them over LAN at home and accept "no remote" or a self-hosted relay away from home. **Funnel isn't a video path**: its bandwidth limits are undisclosed, and Tailscale staff call it "a funnel, not a hose".

---

## 1. Embedding options in October 2026

### 1.1 Option matrix

| Option | Language / ABI | Maturity (Oct 2026) | Platforms | Data path inside the process | Identity (WhoIs user + CapMap) | Certs / MagicDNS / Funnel / Services | Notes |
|---|---|---|---|---|---|---|---|
| **System `tailscaled` / Tailscale app** (user installs it) | n/a (flux binds to the 100.x address, or uses LocalAPI over a unix socket) | GA | Every OS Tailscale supports | **Kernel TUN** on Linux with UDP GSO/GRO and TUN TSO/GRO; the fastest path | Yes, via the LocalAPI `whois` | All | Baseline; "power-user mode" |
| **tsnet** (Go) | Go | GA, the reference embedding (`$TS/tsnet/README.md`) | Linux, macOS, Windows, BSD; iOS and Android with work | gVisor netstack (userspace TCP) → wireguard-go → magicsock (UDP GSO/GRO on Linux) | Yes (`LocalClient().WhoIs`) | `ListenTLS` (ts.net certs), `ListenFunnel`, `ListenService` (VIP), `Loopback()` (SOCKS5 + LocalAPI) | Uploads logs to log.tailscale.com unless `TS_NO_LOGS_NO_SUPPORT=true` (`$TS/logpolicy/logpolicy.go:895`) |
| **Truffle** (Go tsnet sidecar + Rust core) | Rust API; Go child process over a stdin JSON control channel; data over loopback TCP | Maintainer's library, v0.7.19 | macOS, Linux, Windows desktop; Swift/TailscaleKit plane for iOS (RFC 024) | tsnet → Go `io.Copy` → loopback TCP → Rust, **or** Go `httputil.ReverseProxy` → local HTTP service (`proxy:add`) | WhoIs login, name, avatar, node id; **no CapMap** | `ListenTLS` used for proxies; Funnel rejected by design | Pins `tailscale.com v1.104.1`; TIME_WAIT tuned to 1 s (`netstack_tuning.go:35`) |
| **libtailscale** (C ABI) | C via cgo `c-archive` / `c-shared` | Official repo, BSD-3; bindings bumped to 0.2.0 (2026-10-13) | Unix-likes; Makefile targets for macOS and iOS (`$LIBTS/Makefile`) | tsnet in-process; **each conn is a `socketpair(2)` fd spliced by Go `io.CopyBuffer` with 64 KiB buffers** (`$LIBTS/tailscale.go:243, 403-450`) | Through LocalAPI over `tailscale_loopback` | Small C surface: `dial`, `listen`, `loopback`, `status_json`, `enable_funnel_to_localhost_plaintext_http1` (`$LIBTS/tailscale.h:31-218`) | Rust crates such as `tsnet` (passcod) wrap it |
| **TailscaleKit** (Swift) | Swift 6 over libtailscale | Official, "real but evolving" | **macOS 15 and iOS builds only** (`make macos`, `ios`, `ios-sim`, `ios-fat`); APIs carry `tvOS 18` availability annotations, but there's no tvOS build target | Same socketpair model; `URLSessionConfiguration.tailscaleSession()` uses the **SOCKS5 loopback proxy** (`$LIBTS/swift/TailscaleKit/URLSession+Tailscale.swift`) | Partial LocalAPI client ("somewhat incomplete", swift/README) | Through LocalAPI | Truffle Swift (RFC 024) vendors a pinned build (`tailscalekit-5e89501d`) |
| **Android embed** | Go via gomobile or cgo | What the official Android app does; no official third-party SDK | Android 11+ restricts netlink, so `net.Interfaces()` fails; tsnet needs `netmon.RegisterInterfaceGetter(...)` (Java-backed) in cgo builds; the `androidbin` fallback applies only to non-cgo builds (`$TS/net/netmon/state.go:821-835`, `$TS/feature/androidbin/register.go:4-19`) | tsnet in-process | Yes | All | Community Flutter packages (`tailscale`, `tsnet_flutter`) report that Android is still rough |
| **tailscale-rs** (official Rust) | Pure Rust; C, Python and Elixir bindings | **Preview/experimental**; 0.6.1 (2026-09-17); 1.2k stars; third-party crypto audit of `ts_tunnel` done in 0.6.0 | Linux x86_64/arm64, macOS arm64, Windows x86_64. **No iOS or Android** | smoltcp 0.14 netstack with **64 KiB TCP buffers**; plain tokio `UdpSocket` (no GSO/GRO/mmsg batching, `$TSRS/crates/ts_runtime/src/direct.rs`) | `NodeInfo` has hostname, tags and keys but **no user profile** (`$TSRS/crates/control_plane/ts_control/src/node.rs:144-190`); app capabilities unsupported | None (README "Unsupported": MagicDNS, HTTPS certs, Funnel, Serve, Services, node sharing, peer relays, app capabilities) | Has `tailscale::axum::Listener` glue (`$TSRS/src/axum.rs`) and interactive login via `AuthState::NotAuthorized(url)` (`$TSRS/src/lib.rs:156-168`) |
| Community Rust ports | Rust | e.g. GeiserX/tailscale-rs (claims MagicDNS, subnet routing), Erizeez/tailscale-rs (Headscale focus) | — | — | — | — | Unvetted; not recommended [M] |

Sources: [tailscale-rs README](https://github.com/tailscale/tailscale-rs), `$TSRS/CHANGELOG.md`, [libtailscale](https://github.com/tailscale/libtailscale), `$TS/tsnet/tsnet.go`, crates.io API (`tailscale` 0.6.1 on 2026-09-18; `smoltcp` 0.14.0 on 2026-08-17).

### 1.2 tsnet facts that matter to flux (v1.104.1, verified in source)

- **Auth resolution order:**
  1. `Server.AuthKey`
  2. `TS_AUTHKEY`
  3. `TS_AUTH_KEY`
  4. An OAuth client secret (`ClientSecret` / `TS_CLIENT_SECRET`), which mints an auth key and requires `AdvertiseTags`
  5. **Workload identity federation** (`ClientID` plus `IDToken` or `Audience`). Linked only if the program imports `tailscale.com/feature/identityfederation`.
  6. **An interactive login URL** printed to `UserLogf`

  ([`$TS/tsnet/README.md`](https://pkg.go.dev/tailscale.com/tsnet); `tsnet.go:1037-1090`, `1156-1190`). Once a node is enrolled, a new key is ignored unless `TSNET_FORCE_LOGIN=1`. [H]
- **Ways to surface the login URL:**
  - `printAuthURLLoop` logs it every 5 s.
  - Embedders should instead poll `LocalClient().StatusWithoutPeers().AuthURL` or watch the IPN bus. Truffle does the polling (`sidecar-slim/main.go:1095-1124`) and also emits `tsnet:needsApproval` when `BackendState == "NeedsMachineAuth"` (device approval). [H]
- **Listeners:**
  - `Listen` (plain TCP over WireGuard).
  - `ListenTLS`, which provisions a Let's Encrypt cert for `<host>.<tailnet>.ts.net` through the control plane's DNS-01 helper.
  - `ListenFunnel` (public; ports 443, 8443 and 10000 only).
  - `ListenService(name, mode)`, which advertises a **Tailscale Service** (VIP). The **host must be tagged**, and an admin or an auto-approver must approve it (`tsnet.go:1862-1873`).
  - `Loopback()` gives a SOCKS5 proxy plus LocalAPI on 127.0.0.1 with random credentials (`tsnet.go:474`). [H]
- **Netstack tuning:**
  - Receive buffer default 1 MiB, max 8 MiB, with gVisor auto-tuning ("moderate rcvbuf").
  - Send buffer default 1 MiB, max 6 MiB.
  - iOS uses gVisor's defaults (max 4 MiB) "as to not increase pressure towards the NetworkExtension memory limit" (`netstack_tcpbuf_default.go`, `netstack_tcpbuf_ios.go:13-23`, gVisor `tcp/protocol.go:41-52`).
  - Congestion control is CUBIC. TCP GRO and host GSO are enabled **only when `GOOS == "linux"`** (`netstack.go:395-408`). [H]
- **Peer relay server:** tsnet doesn't link `feature/relayserver` by default. Its imports are c2n, netlog, oauthkey, portmapper and useproxy (`tsnet.go:181-185`). A tsnet node probably can't *act as* a peer relay unless flux imports that feature. [M; untested]
- **The NAT-PMP/UPnP/PCP port mapper is linked** (`condregister/portmapper`), which raises the direct-connection rate for home servers. [H]

### 1.3 How Truffle embeds Tailscale (summary; the deep review is elsewhere)

- **Process model:** `truffle-core` (Rust) spawns `sidecar-slim` (Go, about 3.5k lines in `main.go`). Lifecycle commands travel as stdin/stdout JSON lines. Data travels over a **loopback TCP bridge** with a 32-byte session token and a `TRFF` binary header (`main.go:1-60`).
- **Inbound:** two choices.
  - `tsnet:listen` bridges each tailnet connection to a Rust `TcpStream`.
  - `proxy:add` runs a Go `httputil.ReverseProxy` from a tailnet port, with optional `ListenTLS`, to a local HTTP target. It keeps 64 idle upstream connections per host (`main.go:3239`) and serves static files with `http.ServeContent`, which handles Range (`main.go:3162-3206`).
- **Identity:** each accepted connection gets a WhoIs, cached for 60 s (`main.go:2931-3020`).
  - The proxy **strips** any inbound `Tailscale-*` headers and **injects** `Tailscale-User-Login`, `-User-Name`, `-User-Profile-Pic`, `-Node-Id` and `-Node-Name` (`main.go:3040-3085`).
  - Login allow-globs fail closed for tagged or anonymous callers (`main.go:3088`; RFC 025).
- **Egress (RFC 026):**
  - E1 is tsnet's SOCKS5 loopback.
  - **E2 `egress:add` is a 127.0.0.1 listener with a fixed tailnet target, spliced raw.** That's exactly the loopback-gateway shape a desktop media player needs.
  - E3 is an authenticated HTTP CONNECT proxy for browsers (`egress.go:1-23`).
- **Gaps for flux:**
  - (a) `CapMap` isn't forwarded, so app-capability permissions can't reach the Rust service.
  - (b) The tsnet log upload isn't configurable (no `TS_NO_LOGS_NO_SUPPORT` reference).
  - (c) Two copies per byte on the bridge path. Using `proxy:add` toward flux's own HTTP server is the cheaper path for media.
  - (d) The measured ceiling is about 350–550 Mb/s with the sidecars using about 6 logical CPUs, same-host on a Mac (`benchmark/README.md:727-736`).

---

## 2. Data-path architectures for the flux server

Every option below serves the same flux Axum HTTP API (HLS fMP4 plus progressive range GETs, per the initial discussion). The difference is how bytes get from the tailnet to that server.

| # | Architecture | Copies / hops per byte (server side) | Pros | Cons | Verdict |
|---|---|---|---|---|---|
| **A** | **Truffle sidecar `proxy:add` → flux on 127.0.0.1** (Go ReverseProxy) | netstack → Go proxy → loopback TCP → Axum | Already built; WhoIs headers injected; Range/keep-alive tuned; Rust stays pure; process isolation (a sidecar crash doesn't kill the server) | One loopback hop plus Go proxy CPU; CapMap not forwarded yet; HTTP-only | **v1 default** |
| B | Truffle `tsnet:listen` bridge → Axum on the bridged `TcpStream` | netstack → Go `io.Copy` → loopback TCP → Rust | Rust sees raw TCP (custom protocols, QUIC-over-UDP via `listenPacket`) | Identity travels in the bridge header; same copy count as A without HTTP-level niceties | For non-HTTP channels (control, sync) |
| C | **libtailscale linked into the flux binary** (cgo `c-archive` + Rust FFI) | netstack → Go `io.CopyBuffer` → AF_UNIX socketpair → Rust | Single binary; socketpair cheaper than TCP loopback; still the official Go stack | Go runtime inside the Rust process (signals, two schedulers, ~10–20 MB RSS [L]); cross-compiling cgo for NAS targets; a C ABI smaller than tsnet's | Option for mobile clients; not needed on the server |
| D | **System Tailscale** (tailscaled with kernel TUN); flux listens on the 100.x address or `tailscale0` | Kernel TCP → kernel TUN → wireguard-go with offloads | Highest throughput; zero embedding code; Tailscale handles updates | User must install Tailscale (the friction we wanted to avoid); flux shares the host's node identity | **Always support it** as "use this machine's Tailscale" (detect the LocalAPI socket) |
| E | **tailscale-rs native** in the flux process | smoltcp → Rust (zero extra copies) | Pure Rust, no Go, no sidecar; axum glue exists | 64 KiB windows; no GSO/GRO; no certs, MagicDNS, user identity, app caps or sharing; no iOS/Android; pre-1.0 | **Track; prototype later (P2)**; upstream contributions needed |

**Recommendation:** ship **A + D** in v1, use **B** for non-media control channels, and keep **E** behind a feature flag once tailscale-rs gains user identity in `NodeInfo`, larger configurable TCP buffers and reliable NAT traversal. [M]

---

## 3. What bitrate does flux need to carry?

| Content | Typical average | Peak / ceiling | Source / confidence |
|---|---|---|---|
| 1080p Blu-ray remux (H.264/AVC) | ~20–35 Mbps | Blu-ray video max ~40 Mbps (mux ~48 Mbps) | Widely cited BD spec; not re-verified this session [M] |
| 4K UHD Blu-ray remux (HEVC, HDR) | ~40–80 Mbps | **~100–128 Mbps video**; BDA data-rate tiers for 66/100 GB discs are 92, 123 and **144 Mbps** | [cinemaconfig UHD BD reference](https://cinemaconfig.com/reference/uhd-blu-ray) and Jellyfin forum discussion via search; [M] |
| A 4K remux "torture" scene | — | 130–150 Mbps for several seconds | Inference from the spec ceiling [M] |
| Commercial 4K streaming (for comparison) | ~15–25 Mbps | — | General knowledge [L] |

**Requirements derived for flux** (my targets, not from any source):
- **Sustained goodput ≥ 1.5× the peak moving average over the client buffer window.** With a 30 s client buffer, a remux whose 30 s peak average is about 100 Mbps needs **≥ 150 Mbps**. To survive 130–150 Mbps bursts with a short buffer, plan for **~200–250 Mbps** per UHD stream on LAN.
- **Time-to-first-frame and seek** are burst problems. Getting the first 3 s of a 100 Mbps stream (about 37.5 MB) takes 0.75 s at 400 Mbps and 3 s at 100 Mbps. **Throughput headroom is directly visible as TTFF and seek latency.** That's the metric where flux plans to beat Plex and Jellyfin.
- **Remote reality:** a remote stream is usually limited by the server's *home upload*, not by Tailscale. ABR and transcode tiers stay mandatory for WAN. On LAN (or a LAN-like direct path), the embedded stack must not become the bottleneck.

---

## 4. Performance: userspace Tailscale vs kernel, relays and hardware

### 4.1 Tailscale throughput history (primary sources)

| Date | Change | Result | Platform | Source |
|---|---|---|---|---|
| Dec 2022 (v1.36) | wireguard-go TUN TSO/GRO, `sendmmsg`/`recvmmsg` | c6i.8xlarge: 2.42 → **5.36 Gbps** single stream (kernel WireGuard 2.66) | Linux only | [throughput-improvements](https://tailscale.com/blog/throughput-improvements) [H] |
| Apr 2023 (v1.40) | UDP GSO (Linux ≥ 4.18), UDP GRO (≥ 5.0), checksum unrolling | i5-12400 bare metal: **13.0 Gbps** (kernel WG 11.8); c6i.8xlarge 7.32; 2012 Xeon E3-1230v2: 4.86 (6.59 with HW UDP GSO) | Linux only | [more-throughput](https://tailscale.com/blog/more-throughput) [H] |
| Nov 2023 | QUIC/UDP throughput work | "UDP throughput … over 4x" | Linux | [quic-udp-throughput](https://tailscale.com/blog/quic-udp-throughput) (cited in the 2026 post) [M] |
| 2024 (PR #12921) | **TCP GRO into gVisor netstack** | "up to ~2x" in the TUN-write direction; high-BDP paths still limited by gVisor's receive window | Linux (Windows TODO, `netstack.go:404`) | Commit f0230ce0b5 via mirror (search result) [M] |
| Oct 2025 → Feb 2026 GA | **Peer relays** | Delhi→US: DERP 2.2 Mbps vs peer relay **27.5 Mbps** (ISP baseline ~31 Mbps) | All plans; relay host not iOS/tvOS/Android; needs ≥ v1.86 | [peer-relays-international-networks](https://tailscale.com/blog/peer-relays-international-networks), [peer-relays-ga](https://tailscale.com/blog/peer-relays-ga), [docs](https://tailscale.com/docs/features/peer-relay.md) [H] |
| Sep 2026 (v1.104+) | Linux/Android: stop copying ~1 KiB packets into 64 KiB buffers, `writev`, shorter queues, netmap caching; multi-queue planned for 2H 2026 | ~5% speed-up; warm start "one to two orders of magnitude faster" | Linux, Android | [making-tailscale-faster](https://tailscale.com/blog/making-tailscale-faster) (2026-09-22) [H] |

**What none of these posts measure is tsnet or netstack throughput.** Every headline number is kernel-TUN mode on Linux. Tailscale's own docs say userspace (netstack) routing is "less optimized" than kernel mode and is fine "for smaller user counts or low bandwidth" ([kernel-vs-userspace routers](https://tailscale.com/kb/1177/kernel-vs-userspace-routers)). [H]

### 4.2 What the netstack needs for one UHD stream (window math)

Single-stream TCP throughput ≤ window / RTT. The relevant window is the **receiver's** receive window and the **sender's** send buffer. For a stream from the flux server, that means the server's TX buffer and the client's RX buffer.

| Window | RTT 1 ms (LAN) | 5 ms (metro) | 20 ms (regional) | 50 ms (cross-country) | 100 ms (intercontinental) |
|---|---|---|---|---|---|
| **64 KiB** (tailscale-rs 0.6 `tcp_buffer_size`) | 524 Mbps | 105 | **26** | **10.5** | 5.2 |
| 256 KiB | 2.1 Gbps | 419 | 105 | 42 | 21 |
| 1 MiB (gVisor default / initial) | 8.4 Gbps | 1.7 Gbps | 419 | 168 | 84 |
| 4 MiB (iOS netstack max) | — | 6.7 Gbps | 1.7 Gbps | 671 | 336 |
| 8 MiB (tsnet RX max, non-iOS) | — | — | 3.4 Gbps | 1.3 Gbps | 671 |

Arithmetic from the verified buffer constants (§1.2 and `$TSRS/crates/ts_runtime/src/lib.rs:119`). Window isn't the only limit (loss, cwnd and CPU also cap throughput), so these are ceilings.

**Conclusions:**
- tsnet's buffers (with auto-tuning reaching MiB windows) easily cover a 150 Mbps stream up to about 100 ms RTT, *if* the CPU keeps up and loss is low. [H math; M real-world]
- **tailscale-rs 0.6 can't carry one UHD remux beyond about 5 ms RTT.** It's fine on LAN, too slow even regionally. The size is hard-coded in `ts_runtime`, not exposed in `tailscale::Config` (`$TSRS/src/config.rs:12-44`). [H]
- gVisor's TCP has known weaknesses on lossy, high-BDP paths: cwnd collapse on shrinking windows ([gvisor#10312](https://github.com/google/gvisor/issues/10312)). Wi-Fi and cellular clients should therefore be tested with induced loss (§9). [M]

### 4.3 CPU cost: the measurement we do have (Truffle bench, 2026-10-16)

From `truffle/benchmark/README.md`. These are same-host numbers on Apple silicon (10 logical CPUs, macOS, release builds), so they include two netstacks, two WireGuard endpoints and Truffle's bridges on one machine:
- TCP 1 stream **333–508 Mb/s**, bimodal from run to run; 4 streams 316–506; QUIC 4 streams 390–467 (`README.md:615-620, 727-728`).
- "**The sidecars are the ceiling.** At ~350–500 Mb/s the two sidecars together use about 6 logical CPUs (590–660%) … ~300% per side in the two-process run at 387 Mb/s" (`README.md:733-736`).
- WhoIs per accepted connection took ~41 ms until the LocalAPI token lookup was removed. Now it's about **86 µs** (`README.md:647-652`). That's a useful warning: per-request identity lookups must be cached, and Truffle caches them for 60 s.
- On the first two-machine run (MacBook → Mac mini over Wi-Fi LAN, debug initiator), the RTT p50 was 6.7 ms with ~100 ms spikes, and **paced 720p30 video delivered 0% late and 0% dropped** (`README.md:771-790`).

**Extrapolation [L]:**
- About 0.8 core per 100 Mb/s per side on macOS (no GRO/GSO in netstack).
- One UHD peak (130 Mbps) ≈ 1 core on the server.
- **Linux with UDP GSO/GRO plus netstack TCP GRO should be noticeably cheaper per bit**, but nobody has published tsnet-on-Linux CPU numbers. The Linux NAS/N100 case is exactly what flux must measure first.

### 4.4 Hardware expectations (to be validated)

| Server hardware | Kernel-mode Tailscale (system tailscaled), single stream | tsnet / Truffle (userspace) | 4K remux feasible? | Confidence |
|---|---|---|---|---|
| Apple M1 Max (dev box) | n/a (macOS has no kernel WireGuard; the Tailscale app uses a utun + Network Extension) | 330–550 Mb/s same-host Truffle; ~300% CPU/side | Yes, but costs ~1 core per UHD stream | [H] measured by Truffle |
| Intel N100 (4E, 6 W) | Probably ≥ 1 Gbps (WireGuard DB numbers >4 Gbps in netns per a forum post; Tailscale 13 Gbps on an i5-12400) | Probably 300–800 Mb/s at 1.5–3 cores | Yes, 1–3 concurrent UHD streams | [L] |
| Raspberry Pi 5 (4× A76) | A user saw 390–820 Mbps (single anecdote); an old Pi→Pi forum test saw 180 Mbps on v1.22 | Probably 150–400 Mb/s and core-heavy | 1 UHD stream likely; not alongside a transcode | [L] (forum anecdotes) |
| Older NAS (Celeron J4125, ARM Realtek) | Unknown | Unknown; may fall below 150 Mbps | Risky; prefer system Tailscale (kernel) | [L] |

### 4.5 My in-memory netstack micro-benchmark (indicative only)

I built `ts_netstack_smoltcp`'s `throughput_tcp` bench on the SSD (`CARGO_TARGET_DIR=$FLUX_WORK/target`, Rust 1.99.1) and made `tcp_buffer_size` configurable. The bench links two smoltcp stacks with an in-memory pipe, so there's **no crypto, no UDP and zero RTT**.
- On the first pass it reached **~1.2–1.3 GB/s (~10 Gbit/s)** at 64 KiB. Larger buffers were erratic: 256 KiB gave 1.18 and 0.24 GB/s; 1 MiB gave 0.13–0.16 GB/s.
- A repeat pass gave 0.11–0.38 GB/s across all sizes.
- **The machine had a load average of ~48 on 10 cores during the runs** (other agents were compiling), so the variance is noise and **these numbers aren't citable**.
- One real signal: with the pipe bounded at 32 packets, smoltcp at large windows became loss-sensitive. That's consistent with smoltcp having weaker loss recovery than Linux or gVisor (no SACK) [M]. Re-run it on an idle machine as part of the plan.

### 4.6 Relays: DERP, peer relays and Funnel

- **DERP** is shared infrastructure "optimized for availability and broad coverage, not raw throughput". The server implements a **per-client token-bucket rate limit** (`PerClientRateLimitBytesPerSec`, `$TS/derp/derpserver/derpserver.go:636-704`). Tailscale doesn't publish the values. Observed: **2.2 Mbps** (Tailscale's own blog, Delhi→US); forum reports from ~10 Mbps up to "maxed my 100 Mbps". Plan for **≤ 2–5 Mbps on DERP** and treat it as a "remote control / 720p" path. [H for the mechanism, M for the numbers]
- **Peer relays** (GA 2026-02-18; `tailscale set --relay-server-port=…`; grant `"app": {"tailscale.com/cap/relay": []}`; free on all plans; ≥ v1.86; the relay can't be iOS, tvOS or Android) remove DERP's cap. In the same test they gave **12.5× DERP**. Peer relays only serve devices *in the same tailnet*, so a friend on a shared node can't use the owner's relay. [H; the sharing interaction is M]
- **Funnel:** "non-configurable bandwidth limits" ([docs](https://tailscale.com/kb/1223/funnel)). A Tailscale staffer said "it's a funnel, not a hose. We don't announce what the bandwidth limit is". One user couldn't stream reliably above **~1 Mbps**. **Funnel isn't a video transport.** [M]
- **For comparison, Plex's relay** is reported at 1 Mbps free / 2 Mbps with Plex Pass (Plex forums; the official article returned 403 to my fetch) [M]. flux on a direct WireGuard path beats this by 1–2 orders of magnitude, and **even DERP is roughly at parity with Plex Relay**.

**Implication:** flux needs **path-aware playback**. Read `PeerStatus.CurAddr` (direct), `Relay` (DERP region) and `PeerRelay` (`ipnstate.go:270-272`), optionally `LocalClient.Ping(…, PingDisco)`. Then:
- Show a path badge ("Direct · LAN", "Direct · Internet", "Relay (peer)", "Relay (Tailscale DERP – limited)").
- Cap the ABR ladder on DERP.
- Explain fixes: "enable UPnP/NAT-PMP on your router", or "run a peer relay on a VPS".

---

## 5. Auth and identity UX

### 5.1 Logging the server in

| Mechanism | UX | Node ownership | Key expiry | Use for flux |
|---|---|---|---|---|
| **Interactive AuthURL** (tsnet, no key) | Wizard shows a link or QR code; user logs in at login.tailscale.com with Google, Microsoft, GitHub, Apple, OIDC or passkey; creates a tailnet if it's their first time | **User-owned** (the logged-in user) | 180 days by default (1–180 configurable per tailnet) | **Default** |
| Auth key (`tskey-auth-…`) | Paste a key into the env or UI; one-off, reusable, ephemeral, pre-approved or tagged; **max 90-day key validity** | User who created it, or the tags | Tagged nodes: **expiry disabled by default** | Docker/headless |
| OAuth client secret (`TS_CLIENT_SECRET` + tags) | Admin creates an OAuth client once; tsnet mints keys | **Tag-owned** (tags required) | Disabled (tagged) | Fleet/advanced |
| Workload identity federation | Cloud OIDC token exchanged for a key | Tag-owned | — | Cloud deployments only |
| OAuth apps "device provisioning" (alpha) | Tailscale consent screen; token = one-time auth key | User-owned | — | **Not usable.** "Works only within a single tailnet"; the app must be created by that tailnet's admin through the API ([docs](https://tailscale.com/docs/features/oauth-apps/device-provisioning)) |

Sources: [auth keys](https://tailscale.com/kb/1085/auth-keys), [key expiry](https://tailscale.com/kb/1028/key-expiry), `tsnet/README.md`. [H]

**The tagged vs user-owned decision (important):**
- **Tagged server** (`tag:flux`):
  - No key expiry.
  - Can host Tailscale Services (VIP) and has a clean ACL identity.
  - **Cannot be shared with other tailnets** ("Tagged machines can't be shared", [sharing docs](https://tailscale.com/kb/1084/sharing)).
  - Needs `tagOwners` in the policy, and counts against the 50 tagged resources.
- **User-owned server:**
  - Shareable, and shows as "owned by <you>".
  - Key expiry must be disabled: in the console, or through `POST /api/v2/device/{deviceId}/key {"keyExpiryDisabled": true}` (OAuth scope `devices:core`, OpenAPI spec lines 583-609).
- **Recommendation:** make the server **user-owned** in v1. Friend sharing is a core Plex/Jellyfin-replacement feature. Prompt the owner to disable key expiry (deep link), or do it for them if they paste an API access token. [H facts; M recommendation]

**Device approval:** if the tailnet requires approval, tsnet reports `NeedsMachineAuth`. Truffle forwards it as `tsnet:needsApproval`, and the wizard should show "approve flux in the admin console" with a deep link. [H]

### 5.2 Identifying the remote user with zero passwords

- `LocalClient().WhoIs(ctx, r.RemoteAddr)` returns `{Node, UserProfile, CapMap}` (`apitype.go:35-42`).
  - `UserProfile.LoginName` (an email or `user@github`) is the stable user key.
  - Tagged peers have no human user; their UserProfile is the tag's synthetic profile.
- Truffle's proxy path already exposes this as headers, so flux maps `Tailscale-User-Login` to a flux user record. Bootstrap: the **login that authorized the server node becomes the flux owner/admin**, read from `Status().Self.UserID` plus `User` map.
- **Security invariant:** flux must trust identity headers **only from the sidecar's loopback connection**, with a per-boot shared secret header or a unix socket. Truffle already strips spoofed `Tailscale-*` headers on the tailnet side (`main.go:3040-3062`).
- **LAN-direct access** (same household, no tailnet) has no WhoIs. Use a pairing flow: a code displayed on the TV and confirmed in the admin app, then a device token. Or require tailnet for everything but the setup wizard on localhost. [M]

### 5.3 Per-user media permissions with ACL grants and app capabilities

App capabilities attach arbitrary JSON to a src→dst grant. The app reads them from `WhoIs().CapMap`, and `tailcfg.UnmarshalCapJSON[T]` (`tailcfg.go:1683`) decodes them. `tailscale serve --accept-app-caps` forwards them in a `Tailscale-App-Capabilities` header. Names must be `{domain}/{path}` with a domain you control; `tailscale.com` and `tailscale.io` are reserved ([grants app capabilities](https://tailscale.com/kb/1537/grants-app-capabilities), [app-capabilities blog](https://tailscale.com/blog/app-capabilities), 2025-10-30). [H]

Proposed schema (illustrative; the domain is a placeholder):

```json
{
  "grants": [
    { "src": ["autogroup:member"], "dst": ["autogroup:self"], "ip": ["*"] },
    { "src": ["alice@example.com"], "dst": ["flux-server"],
      "ip": ["tcp:443", "tcp:8096"],
      "app": { "flux.example/cap/media": [ { "role": "admin" } ] } },
    { "src": ["group:kids"], "dst": ["flux-server"],
      "ip": ["tcp:443", "tcp:8096"],
      "app": { "flux.example/cap/media": [ { "role": "viewer",
               "libraries": ["Kids Movies", "Cartoons"], "maxRating": "PG",
               "maxBitrateMbps": 20, "download": false } ] } },
    { "src": ["autogroup:shared"], "dst": ["flux-server"],
      "ip": ["tcp:443"],
      "app": { "flux.example/cap/media": [ { "role": "guest", "libraries": ["Movies"],
               "transcodeOnly": true } ] } }
  ]
}
```

How flux uses it:
- On each request: WhoIs (cached), then CapMap. If the capability is present, it's **authoritative** (the tailnet admin owns policy as code). If it's absent, fall back to **flux-local roles** keyed by `LoginName`, because most households won't edit HuJSON policy.
- Caveats:
  - Plan: grants and app caps are available on Personal (the pricing page caps ACL groups at 3 on Personal) [M].
  - `autogroup:shared` is valid as a *source* but not a destination [H].
  - Whether CapMap is populated for **shared-in** (cross-tailnet) users must be tested. The owner tailnet's policy governs sharee traffic, so it should be [M].
  - `"ip"` must be present or implied. Without a network grant the connection is dropped before flux sees it.
- **Truffle work item:** add `CapMap` to `peerIdentityData` and inject it as `Tailscale-App-Capabilities` (the same name `tailscale serve` uses) for allow-listed capability names.

### 5.4 Friends and family

| Mechanism | Friend needs | Limits | Friend's identity at flux | Recommended for |
|---|---|---|---|---|
| **Invite to the owner's tailnet** (user invite) | A Tailscale login; becomes a *member* of the owner's tailnet | **Personal: 6 users total**; friend's devices visible to the owner | Full WhoIs; grants by email/group | Household (≤ 6) |
| **Node sharing** (share the flux server node) | Own Tailscale account/tailnet (free); accepts an invite link; must be Owner/Admin/IT admin of *their* tailnet (true for a personal tailnet) | Available on all plans; **reusable link up to 1,000 accepts; unused links expire in 30 days**; shared node is **quarantined** (can't initiate connections to the friend); visible only to the accepting user; reachable as `<host>.<owner-tailnet>.ts.net`; tags/subnets stripped; **only user-owned nodes are shareable** | Login visible; grants can target the sharee email or `autogroup:shared` | **Friends** (Plex-style sharing) |
| Declarative node sharing | Policy between trusted tailnets | **Alpha**, waitlist ([Aug 2026 blog](https://tailscale.com/blog/easier-building-with-tailscale)) | — | Later |
| Tailscale Services (svc:flux) | Member of the tailnet | Hosts must be tagged; TCP-only; approval needed ([docs](https://tailscale.com/kb/1552/tailscale-services)) | — | Multi-server HA later (P2) |

**Automation:**
- `POST /api/v2/device/{deviceId}/device-invites` with `{"multiUse": true}` returns `inviteUrl` (OpenAPI lines 776-895). It **can't be called with an OAuth-client token** (line 831), so flux needs a *user* API access token: 1–90 days, pasted by the owner.
- Without a token, deep-link to the admin console's machine share dialog.
- `POST /tailnet/{tailnet}/user-invites` covers the household path. [H]

### 5.5 Free-plan limits (2026)

Personal (free):
- **6 users, unlimited user devices.**
- **50 tagged resources** (+$1/month each).
- 1,000 ephemeral minutes/month and 3 ACL groups.
- Funnel and peer relays on all plans.
- The plan was reworked on **2026-04-08**: Personal Plus was retired and Starter became Standard at $8/user/month ([pricing](https://tailscale.com/pricing); secondary sources confirm the date). [H for limits, M for the date]

**Implication:** every flux client (phone, TV, tablet) is a *user device*, so there's no quota pressure. Friends on node sharing consume **their own** tailnet's quota. A family larger than 6 *members* should use node sharing.

### 5.6 MagicDNS, HTTPS certs and when flux needs them

- `ListenTLS` / `tailscale cert` get Let's Encrypt certs for `host.<tailnet>.ts.net` using DNS-01. Keys stay local.
- **Every cert is published in Certificate Transparency logs, so machine names leak.** Name the node `flux` rather than "alice-movies".
- Let's Encrypt rate limits can force a **~34-hour wait**; certs last 90 days ([HTTPS docs](https://tailscale.com/kb/1153/enabling-https)). [H]
- MagicDNS plus HTTPS must be enabled by the admin in the console. It can't be done from tsnet.
- **Native flux apps don't need TLS:** WireGuard already provides end-to-end encryption and peer authentication. Use `http://flux.<tailnet>.ts.net:8096` or the 100.x address. **Browsers do**, for secure-context features (service workers, EME/DRM-free MSE is fine without TLS, clipboard, PWA install) and to avoid "Not secure". Offer HTTPS as an optional step, with the CT warning. [M]

### 5.7 Optional: "Log in using Tailscale" for other web apps

`tsidp` (experimental OIDC IdP on tsnet; grant `tailscale.com/cap/tsidp`) gives OIDC to apps that can't do WhoIs ([docs](https://tailscale.com/docs/features/tsidp.md)). flux doesn't need it, because it reads WhoIs directly. It could matter if flux later federates with third-party web UIs. [M]

---

## 6. Clients: TVs, phones, browsers

### 6.1 Platform matrix

| Platform | Official Tailscale app | In-process embed (no VPN profile) | Player | How flux reaches the server |
|---|---|---|---|---|
| **iOS / iPadOS** | Yes (Network Extension VPN) | **Yes**: TailscaleKit/libtailscale; Truffle RFC 024 has a working, device-verified backend (iPhone 14 Pro, iOS 26.5) | AVPlayer | If the system Tailscale VPN is up, use MagicDNS directly. Otherwise embed and use a **loopback HTTP gateway** |
| **tvOS (Apple TV)** | Yes, **tvOS ≥ 17** (18 recommended), QR or auth-key login ([docs](https://tailscale.com/docs/install/appletv.md)) | **Plausible, unproven.** Tailscale's `gocross` sets `-mtvos-version-min` for `TVOS_DEPLOYMENT_TARGET` (`autoflags.go:151-156`), but TailscaleKit has no tvOS make target | AVPlayer | Same as iOS; prototype needed [M] |
| **Android phones** | Yes | **Yes**, with work: tsnet plus Java `RegisterInterfaceGetter` (netlink blocked on Android 11+, [golang/go#40569](https://github.com/golang/go/issues/40569)) | ExoPlayer/Media3 | Embed + loopback gateway, or an OkHttp `DataSource` with a custom `SocketFactory`/`Dns` dialing via the embedded node |
| **Android TV / Google TV** | Yes (Play Store; D-pad UI since 2022, [blog](https://tailscale.com/blog/android-tv)) | As Android | ExoPlayer | As Android |
| **Fire TV** | Yes (Amazon Appstore; most devices since 2018 except Fire TV Stick 4K Select; QR login, [docs](https://tailscale.com/kb/1394/install-amazon-fire)) | As Android | ExoPlayer | As Android |
| macOS / Windows / Linux desktop | Yes | Yes (Truffle sidecar) | mpv/libmpv, AVPlayer (macOS), Electron/Chromium | Truffle `egress:add` (fixed-target loopback) or system Tailscale |
| Browsers on a tailnet device | n/a (the device runs Tailscale) | — | `<video>` + hls.js/MSE | `https://flux.<tailnet>.ts.net` |
| Browsers on a non-tailnet device | No | WASM `tsconnect` exists but is **DERP-only over WebSockets** (no UDP in browsers) ([2022 design](https://tailscale.com/blog/ssh-console)) | — | Not viable for video; LAN only |
| **LG webOS, Samsung Tizen, Roku, Chromecast (non-Google TV), consoles** | **No** (not on Tailscale's install index, [docs](https://tailscale.com/docs/install)) | Not feasible (no native sockets SDK on Roku; webOS/Tizen web apps have no UDP) [M] | Native | **LAN only** (see §6.4) |

### 6.2 In-process embedding on mobile: what to expect

- **No VPN entitlement and no VPN icon.** Only the app's own sockets use the tailnet. It coexists with the user's system Tailscale or another VPN. This is confirmed in the maintainer's iOS prototype notes and in TailscaleKit's design. [H]
- **Foreground only.** When the app is suspended, the netstack freezes. Video playback (foreground or Picture-in-Picture) keeps the app alive. Background downloads with `URLSession` background configurations **don't work through an in-process node**; Truffle's backend explicitly rejects them. Offline downloads must happen in the foreground. [H for Truffle, M in general]
- **iOS Local Network privacy prompt.** magicsock probes LAN candidate endpoints (192.168.x.x), and connecting to local-network addresses triggers iOS's Local Network permission prompt. Without it, a phone at home falls back to a hairpin or DERP path. Ship `NSLocalNetworkUsageDescription` and pre-explain it in onboarding. Whether tvOS behaves the same is an open question. [M]
- **App Transport Security:** a loopback HTTP gateway needs `NSAllowsLocalNetworking = YES`. [M]
- **Memory:** iOS keeps netstack buffers at gVisor defaults (4 MiB max) to limit memory (`netstack_tcpbuf_ios.go`). Budget roughly tens of MB for the Go runtime plus netstack. The exact binary and RSS cost is an open question. [L]
- **Android:** cgo builds must register a Java-backed interface getter. Community packages report that Android tunnels are still rough. Prototype early. [M]

### 6.3 Making a stock player stream over an in-app userspace tailnet

- **AVPlayer** does its HTTP in `mediaserverd`, so neither `NSURLProtocol` nor `URLSessionConfiguration.proxyConfigurations` (TailscaleKit's SOCKS route) applies to it (Apple DTS, [forum thread](https://developer.apple.com/forums/thread/75328)). [M] There are two options.
  1. **A loopback HTTP gateway (recommended).**
     - The app binds `127.0.0.1:<random>`.
     - Each accepted connection is spliced to `flux-server:8096` with `tailscale_dial` (TailscaleKit `OutgoingConnection`). That's a raw TCP splice, exactly Truffle E2.
     - The player gets `http://127.0.0.1:<port>/<session-token>/library/…/master.m3u8`.
     - Pros: every player feature works (HLS, byte-range, keep-alive, AirPlay hand-off excluded).
     - Cost: one extra in-process copy.
  2. **`AVAssetResourceLoaderDelegate` with a custom URL scheme.** The app fetches bytes itself over TailscaleKit's URLSession. This is fragile for HLS (keys and segment loading quirks) and slower. It's a fallback only.
- **ExoPlayer/Media3:** either the same loopback gateway, or an `OkHttpDataSource` whose `OkHttpClient` uses a custom `SocketFactory` that dials through the embedded node. The second avoids the gateway but is more code. [M]
- **Desktop and Electron:** Truffle E2 `egress:add` (fixed target, raw splice) for mpv/AVPlayer, or E3 (HTTP CONNECT, Basic auth) for Chromium sessions. [H that the features exist; M for player behaviour]
- **Gateway security:**
  - Bind 127.0.0.1 only. Use a random port plus an **unguessable path token** that rotates per session, because other apps on Android can connect to loopback ports.
  - Don't expose LocalAPI.
  - Serve plain HTTP on loopback. The tailnet hop is WireGuard.
  - Keep `Range`, `If-Range` and keep-alive intact.
  - Gateway overhead must be measured (target ≤ 10% vs system Tailscale).

### 6.4 Devices that can't run Tailscale

| Option | How | Bandwidth | Security | Verdict |
|---|---|---|---|---|
| **LAN direct** | flux also listens on the LAN IP; discovery via mDNS/SSDP; pairing code for auth | Full LAN | LAN-trust + pairing token | **Yes, v1** (at home) |
| **flux edge gateway** at the remote site | A flux-gateway binary on a Pi, NAS or Android TV box in the friend's house joins the tailnet and re-serves HTTP to that LAN | Direct WireGuard to the server | Gateway authenticated by its tailnet identity; LAN clients pair to it | P2 (niche, but it's the only remote path for LG/Samsung/Roku) |
| **Tailscale Funnel** | `ListenFunnel(":443")` | **Undisclosed cap; ~1 Mbps observed** | Public internet; needs flux's own auth | **No for video.** Maybe for small "share a trailer link" or admin use |
| **Self-hosted public relay** (VPS + Caddy, or a router port-forward) | Classic remote access | VPS egress limits/cost | Public; needs real auth (passkeys) | Out of scope for the "no port-forward" pitch; document it for experts |
| Subnet router at the remote site | A Tailscale device advertises the remote LAN; the TV still needs a route to 100.64.0.0/10 | — | — | Not practical for consumer TVs [M] |

---

## 7. Control-plane and mesh alternatives

| Option | Status (Oct 2026) | License | Fit for flux | Why or why not |
|---|---|---|---|---|
| **Tailscale SaaS control plane** | v1.104.1 (2026-10-07) | Client BSD-3; service under ToS | **Primary** | Free Personal plan, best NAT traversal, DERP everywhere, official mobile/TV apps, sharing, certs |
| **Headscale** | **v0.29.5 (2026-10-13)**; min client v1.80 since 0.29.3 ([releases](https://github.com/juanfont/headscale/releases)) | BSD-3 | **Supported config** (tsnet `ControlURL` / `TS_CONTROL_URL`) | Supports auth keys, web auth, OIDC, tags, ACLs/grants, embedded DERP, **peer relays**, MagicDNS, Taildrop ([features](https://headscale.net/stable/about/features/)). **No ts.net HTTPS certs, no cross-tailnet node sharing; Funnel/Serve status unclear**. Single tailnet. Good for privacy purists; friend sharing degrades to "make them a user" |
| **tailscale-rs on Tailscale/Headscale** | 0.6.1 preview | BSD-3 + PATENTS | P2 server backend | See §1.1 and §4.2 |
| **iroh** | **1.0.0 on 2026-06-15** (crates.io); now 1.3.0 (2026-09-28); [road to 1.0](https://iroh.computer/blog/the-road-to-iroh-1-0) | MIT OR Apache-2.0 | Possible *data-plane* experiment | Pure Rust QUIC (noq, a quinn fork with multipath), Tailscale-style hole punching, relays, stable wire format and API, used in production (e.g. Delta Chat). **But no identity provider, no user accounts, no ACLs, no MagicDNS, no TV apps.** flux would have to build login, sharing and permissions itself, which is exactly what "log in with Tailscale" avoids. Relay capacity and policy are flux's problem |
| **NetBird** | `client/embed` Go package, BSD-3 (management server parts AGPLv3), v0.81.x | BSD-3 / AGPL | No | Embedded client auth is setup key, JWT or private key only, **no interactive SSO for embedded clients** (pkg.go.dev); also a Go embed; smaller ecosystem; no TV apps |
| **ZeroTier libzt** | — | **BSL 1.1** (commercial licence needed for closed-source products/SaaS) | No | License friction for an OSS-plus-possibly-commercial product; weaker identity story |

---

## 8. ToS, licensing and privacy

- **Client code licenses:** Tailscale client, tsnet, libtailscale, TailscaleKit and tailscale-rs are **BSD-3-Clause** (tailscale-rs also ships a **PATENTS** file, and "WireGuard is a registered trademark of Jason A. Donenfeld"). They're compatible with flux under any OSI license (MIT, Apache or AGPL). Keep the notices in the about screen and the SBOM. [H]
- **Service terms** ([ToS, last updated 2026-08-25](https://tailscale.com/terms)):
  - §2.1: use is "solely for your own personal use or internal business purposes (as applicable depending on your Plan)". A household media server under the user's *own* account is personal use. [H]
  - §2.3 forbids:
    - "frame, mirror, sell, resell, rent or lease use of the Services". **So flux must never operate a shared tailnet or sell Tailscale access**; each user brings their own account.
    - "access or use the Services for benchmarking or competitive analysis". **Keep throughput results internal or frame them as flux measurements; don't publish Tailscale-vs-competitor comparisons without checking.** [H text; M interpretation]
  - §1.8 defines "Integrations" (software that "embed[s] certain functionality into a third party platform … via an API or SDK"). flux is an integration the user chooses.
- **AUP** ([last updated 2025-06-30](https://tailscale.com/tailscale-aup)): no streaming clause, but it forbids "creating an undue burden on the Tailscale Solution" and distributing IP-infringing content. **Design consequence:** don't push sustained high-bitrate video through **DERP** or **Funnel**. Cap the bitrate automatically on DERP (also a UX necessity), and steer users to peer relays. [M]
- **Privacy:** tsnet uploads client logs to `log.tailscale.com` by default (`tsnet.go:1091-1129`). Setting `TS_NO_LOGS_NO_SUPPORT=true` swaps in a no-op transport (`logpolicy.go:895`). flux should expose this as a setting (an informed default; Tailscale support needs logs). Truffle doesn't set it today. [H]
- **Branding:** a "Log in with Tailscale" button uses Tailscale's name and logo. Check Tailscale's brand guidelines (open question). Don't imply endorsement.
- **Headscale users** are bound only by BSD-3 and their own server.

---

## 9. Recommended end-to-end onboarding flow

"Install the flux server → log in with Tailscale → open the app on a TV or phone → the library appears."

| # | User sees | Mechanism | Failure handling |
|---|---|---|---|
| 1 | Install flux (Docker image, `.deb`, Homebrew, Windows or macOS app) | Single flux binary plus bundled Truffle `sidecar-slim` (tsnet v1.104.x). State in `/var/lib/flux/tailscale` (Docker: a **volume**; losing it means a new node identity) | On start, flux probes for a system tailscaled (LocalAPI socket). If found: "Use this machine's Tailscale (faster)" or "Give flux its own Tailscale identity" |
| 2 | Opens `http://localhost:8096` (or the LAN IP; or the AuthURL printed in Docker logs) → **"Connect with Tailscale"** | Starts tsnet with `Hostname: "flux"`, `Ephemeral: false`, no key. Polls `StatusWithoutPeers().AuthURL`, shown as a link and QR code (Truffle `tsnet:authRequired`) | Headless: `TS_AUTHKEY` / `TS_CLIENT_SECRET`+tags env (documented). Optional `TS_CONTROL_URL` for Headscale |
| 3 | Logs in at login.tailscale.com (SSO) → new users get a tailnet created → "flux connected as flux.tail1234.ts.net" | Node registers user-owned; BackendState `Running`; flux reads `Status().Self` → **that login becomes flux owner/admin** | `NeedsMachineAuth` → "Approve flux in your admin console" deep link (Truffle `tsnet:needsApproval`) |
| 4 | Wizard checklist: ① disable key expiry (recommended) ② enable HTTPS (optional; CT warning) ③ allow UPnP/NAT-PMP on the router (optional) | ① console deep link, or `POST /device/{id}/key` with a pasted API token ② console toggle, then `ListenTLS` ③ tsnet port mapper does the rest | Show "key expires in N days" in admin settings; email or notification 14 days before |
| 5 | Add libraries (the Mille-based scan; out of scope here) | — | — |
| 6 | On a phone or TV: install the flux app → **"Sign in with Tailscale"** | App checks whether the system Tailscale app is connected (does the MagicDNS name resolve or 100.64/10 route?). If yes: no embed. Else: start the embedded node (TailscaleKit / tsnet-android) → AuthURL → **phone:** ASWebAuthenticationSession / Custom Tab; **TV:** QR code to scan with the phone (same pattern as Tailscale's Apple TV and Fire TV apps) | iOS Local Network prompt pre-explained; the embedded node persists state (Application Support, excluded from backup) |
| 7 | **The library appears** | App lists peers (LocalAPI `Status`) → candidates = peers with hostname `flux*` or a `flux` service tag → `GET /.well-known/flux` on each (bounded parallel probe) → server does WhoIs → returns the user's libraries (CapMap or flux roles). No password | No server found: "Is flux running and logged into the same Tailscale account? Or ask the owner for an invite" |
| 8 | Press play | Player URL: system-Tailscale mode → `http://flux.<tailnet>.ts.net:8096/...`; embedded mode → loopback gateway URL. Path badge from `PeerStatus` (direct / peer-relay / DERP); ABR ceiling chosen from the path and measured goodput | DERP → cap at ~2–4 Mbps tier, show "Limited relay connection – how to fix" |
| 9 | Owner: **"Invite a friend"** | With a user API token: `POST /device/{id}/device-invites {"multiUse": false}` → share `inviteUrl`. Without one: deep link to the console share dialog. Household: `POST /tailnet/-/user-invites` (≤ 6 users) | Friend: installs flux → signs in with **their own** Tailscale (new free tailnet) → opens the invite link → the server appears; flux maps the friend's login to the guest role (or the `autogroup:shared` capability) |

---

## 10. Throughput and latency test plan

**Goal:** decide the server stack (A, D, later E) and client stack (system vs embedded plus gateway) with numbers, before building the playback engine on assumptions. Use Truffle's benchmark hub (`truffle-bench`, `tsnet-bench`, video workload) where it fits, plus media-specific workloads.

### 10.1 Matrix

| Dimension | Values |
|---|---|
| **Server stack** | S0 plain HTTP over LAN (no Tailscale; baseline) · S1 system tailscaled (Linux kernel TUN) · S2 Truffle `proxy:add` → flux Axum · S3 Truffle `tsnet:listen` bridge → Axum · S4 libtailscale linked via FFI (socketpair) · S5 tailscale-rs 0.6 + `tailscale::axum` (LAN only, for reference) |
| **Client stack** | C1 system Tailscale (macOS, iOS, tvOS, Android TV) · C2 embedded TailscaleKit + loopback gateway → AVPlayer (iPhone, Apple TV prototype) · C3 embedded tsnet-android + gateway → ExoPlayer (Android TV / Fire TV) · C4 desktop Truffle E2 → mpv |
| **Path** | P1 same LAN, wired · P2 same LAN, Wi-Fi 6 · P3 WAN direct (phone on 5G; or a friend's house) · P4 forced DERP (block UDP 41641 + STUN on the client) · P5 peer relay on a VPS · P6 netem-shaped (Linux router VM): RTT 20/50/100 ms × loss 0/0.1/1% |
| **Server hardware** | Apple M1 Max (dev) · Intel N100 mini-PC · Raspberry Pi 5 (8 GB) · one low-end NAS CPU if available (J4125 or similar) |
| **Media** | R1 1080p BD remux (~35 Mbps) · R2 4K HDR10 remux (avg ~60, peaks ~120–130 Mbps) · R3 synthetic CBR 150 Mbps HEVC (stress) · R4 HLS fMP4 2 s segments of R2 |

### 10.2 Workloads and metrics

1. **Bulk single TCP stream**, 30 s each direction: `truffle-bench pair tcp` / `tsnet-bench` (S2/S3), iperf3 (S1), and a small Rust HTTP GET client (all). Metrics: goodput (Mb/s), **CPU-ms per MB** on both ends (`pidstat`/`powermetrics`), RSS.
2. **HTTP range reads of R2** (64 MiB ranges, keep-alive, 1/2/4 parallel): goodput, TTFB p50/p99.
3. **Real playback.** AVPlayer (C1/C2), ExoPlayer (C3) and mpv (C4) play R1–R4. Metrics:
   - **TTFF** (tap → first frame)
   - **Seek latency** (10 random seeks, p50/p95)
   - Rebuffer count and duration
   - Dropped frames
   - Steady-state buffer level
4. **Concurrency:** 1, 2 and 4 simultaneous R2 streams per server. Find the stream count where CPU saturates or rebuffering starts.
5. **Connection life-cycle:**
   - Cold start: process start → `Running` → first byte.
   - Warm reconnect after Wi-Fi → cellular.
   - Time for direct-path upgrade from DERP (`tailscale ping` style).
   - Embedded node resume after 10 min of iOS suspension.
6. **Path detection:** the time for flux to classify direct, peer-relay or DERP, and to apply the bitrate cap.

### 10.3 Pass criteria (proposed; revisit after the first run)

| Criterion | Target |
|---|---|
| S2 single-stream goodput, N100, P1 | ≥ 400 Mb/s; CPU ≤ 1 core per 150 Mb/s |
| S2 single-stream goodput, Pi 5, P1 | ≥ 200 Mb/s |
| R2 4K remux playback on P1/P2/P3 (when link capacity ≥ 1.5× peak) | **0 rebuffers** over 20 min |
| TTFF (R2, direct play) | ≤ 1.5 s LAN; ≤ 3 s WAN-direct |
| Seek p95 (R2) | ≤ 1.0 s LAN; ≤ 2.5 s WAN-direct |
| Embedded gateway overhead (C2/C3 vs C1) | ≤ 10% goodput loss; ≤ 150 ms added TTFF |
| DERP detection → bitrate cap | ≤ 5 s after playback start |
| S5 (tailscale-rs) | Record only; expect ~window-limited results on P6 |

### 10.4 Hygiene

- Release builds only.
- Record the versions: tailscale v1.104.x, Go, Rust, gVisor commit, Truffle sidecar hash.
- **Verify the path for every run** (`tailscale status`, LocalAPI `CurAddr`/`Relay`/`PeerRelay`).
- 5 runs; report medians and IQR.
- Wired links unless the run tests Wi-Fi.
- **Idle machines.** The dev box had a load average of ~48 during this research, which invalidated my micro-bench.
- Put build output on `$FLUX_WORK`.
- Use a dedicated test tailnet or scoped test ACLs, and never put keys in command lines (Truffle convention).
- Keep the published framing to "flux performance", in light of ToS §2.3.

---

## 11. Recommendations for flux

| Priority | Recommendation | Rationale |
|---|---|---|
| **P0** | **BYO Tailscale account; interactive AuthURL login on the server and every client; QR on TVs.** No shared keys or tailnets | Best UX (SSO, no passwords), ToS-clean (§2.1/§2.3), free Personal plan covers households |
| **P0** | **Server v1 = Truffle `proxy:add` → flux Axum on 127.0.0.1**, plus a **"use this machine's Tailscale" mode** (system tailscaled) | Existing, tested code with WhoIs headers and Range support; kernel mode is the escape hatch for weak NAS CPUs |
| **P0** | **Run the §10 test plan on N100 and Pi 5 before building the playback engine** | The only hard numbers for tsnet CPU cost are same-host macOS; they suggest ~1 core per UHD stream |
| **P0** | **Path-aware streaming:** classify direct / peer-relay / DERP per client and cap ABR on DERP | DERP is per-client rate-limited (2.2 Mbps observed); AUP "undue burden" |
| **P0** | **Server node user-owned (not tagged); guide the owner to disable key expiry** | Tagged nodes can't be shared; user-owned nodes expire in 180 days by default |
| **P1** | **Extend Truffle:** forward `CapMap` (as `Tailscale-App-Capabilities` JSON for allow-listed names); make `TS_NO_LOGS_NO_SUPPORT` and the netstack buffers configurable; expose `PeerStatus` path info to Rust | Needed for permissions, privacy and path-aware ABR |
| **P1** | **Permissions = app capabilities when present, flux-local roles keyed by `LoginName` otherwise.** Publish a `<flux-domain>/cap/media` schema | Policy-as-code for power users; zero-config for everyone else |
| **P1** | **Mobile/TV clients embed Tailscale** (TailscaleKit on iOS; tsnet-android on Android TV / Fire TV) **with a loopback HTTP gateway**; prefer the system Tailscale route when it exists | AVPlayer ignores in-app proxies; the gateway keeps stock players and HLS intact |
| **P1** | **Friend sharing via node sharing**, with an "Invite" button (user API token → `device-invites`) and a console deep-link fallback; household members via user invites | Plex-style sharing without port forwarding; up to 1,000 accepts per reusable link |
| **P1** | **tvOS embed spike**: build libtailscale for tvOS with `TVOS_DEPLOYMENT_TARGET`, run TailscaleKit + gateway + AVPlayer on a real Apple TV | Apple TV is the #1 enthusiast client; no official build target exists |
| **P2** | **tailscale-rs backend behind a feature flag.** Upstream asks: configurable TCP buffers or auto-tuning, user profile in `NodeInfo`, app caps, certs, iOS/Android | A pure-Rust single binary is the long-term ideal; not viable at 0.6 |
| **P2** | **Headscale as a documented, CI-tested config** (`TS_CONTROL_URL`), listing what degrades (no ts.net certs, no cross-tailnet sharing) | Self-hosting purists; cheap to support |
| **P2** | LAN-direct mode with pairing for webOS/Tizen/Roku; optional flux edge gateway for remote LANs; **no video over Funnel** | Covers devices Tailscale can't reach without breaking the "no port forward" promise |
| **P2** | Tailscale Services (`svc:flux`) for multi-server or HA households (tagged hosts) | Stable VIP; only once sharing isn't needed for those hosts |

---

## 12. Open questions

1. **tsnet CPU per Gbps on Linux** (N100, Pi 5) with UDP GSO/GRO and netstack TCP GRO. No published numbers; the main unknown for "crush Plex on resource use".
2. Is `WhoIs().CapMap` populated for **shared-in** users from another tailnet when grants target `autogroup:shared` or the sharee's email? Test on two personal tailnets.
3. **tvOS:** does a libtailscale or TailscaleKit build work on tvOS 18+, and does tvOS show a Local Network prompt for an in-process node?
4. Binary size and RSS of TailscaleKit or libtailscale in an iOS or tvOS app (Truffle's vendored `tailscalekit-5e89501d` could answer this).
5. **Funnel's** actual bandwidth cap. Undisclosed. Also whether Tailscale tolerates flux defaulting to "relay but capped" on DERP for remote viewers.
6. Can a **tsnet** node act as a **peer relay** if flux imports `feature/relayserver`? Is that wise on a home server? Peer relays only help same-tailnet peers, not sharees.
7. Tailscale **brand guidelines** for a "Log in with Tailscale" button.
8. When will tailscale-rs add NAT traversal parity, user identity in `NodeInfo`, larger configurable TCP windows, and mobile platforms? Is the team open to PRs (the ts_runtime buffer constant is a one-line change)?
9. Headscale **Funnel/Serve** status. The features page lists them with issue links but no clear status.
10. Not re-verified this session (search budget): exact Blu-ray 1080p bitrate limits; Plex's current relay caps (official page returned 403); gVisor TCP GRO commit details (PR #12921).

---

## 13. Sources

**Primary: Tailscale docs and blog**
- [tailscale-rs launch (2026-04-15)](https://tailscale.com/blog/tailscale-rs-rust-tsnet-library-preview); [repo](https://github.com/tailscale/tailscale-rs); [releases](https://github.com/tailscale/tailscale-rs/releases)
- [Build with Tailscale (2026-08-31)](https://tailscale.com/blog/easier-building-with-tailscale)
- [Making Tailscale faster (2026-09-22)](https://tailscale.com/blog/making-tailscale-faster)
- [Surpassing 10Gb/s (2023)](https://tailscale.com/blog/more-throughput); [Userspace isn't slow (2022)](https://tailscale.com/blog/throughput-improvements)
- [Peer relays GA (2026-02-18)](https://tailscale.com/blog/peer-relays-ga); [Peer relays docs](https://tailscale.com/docs/features/peer-relay.md); [Peer relays and international networks (2026-01-26)](https://tailscale.com/blog/peer-relays-international-networks)
- [Poor performance troubleshooting](https://tailscale.com/docs/reference/troubleshooting/poor-performance-tailnet); [Kernel vs netstack routers](https://tailscale.com/kb/1177/kernel-vs-userspace-routers)
- [App capabilities grants](https://tailscale.com/kb/1537/grants-app-capabilities); [App capabilities blog (2025-10-30)](https://tailscale.com/blog/app-capabilities)
- [Sharing](https://tailscale.com/kb/1084/sharing); [Funnel](https://tailscale.com/kb/1223/funnel); [Services](https://tailscale.com/kb/1552/tailscale-services); [HTTPS certs](https://tailscale.com/kb/1153/enabling-https); [Auth keys](https://tailscale.com/kb/1085/auth-keys); [Key expiry](https://tailscale.com/kb/1028/key-expiry)
- [OAuth apps device provisioning](https://tailscale.com/docs/features/oauth-apps/device-provisioning); [tsidp](https://tailscale.com/docs/features/tsidp.md)
- [Apple TV](https://tailscale.com/docs/install/appletv.md); [Amazon Fire](https://tailscale.com/kb/1394/install-amazon-fire); [Android TV blog (2022)](https://tailscale.com/blog/android-tv); [Install index](https://tailscale.com/docs/install)
- [Pricing](https://tailscale.com/pricing); [Terms (2026-08-25)](https://tailscale.com/terms); [AUP (2025-06-30)](https://tailscale.com/tailscale-aup)
- Tailscale API OpenAPI schema (`https://api.tailscale.com/api/v2?outputOpenapiSchema=true`, saved locally): `/device/{id}/key` (L583), `/device/{id}/device-invites` (L776, note L831), `/tailnet/{tailnet}/user-invites` (L895)

**Primary: source code (local, versions above)**
- `$TS/tsnet/tsnet.go` (Server fields 223-372; auth 1037-1090; AuthURL loop 1156; ListenTLS 1409; ListenFunnel 1512; ListenService 1862-1873); `$TS/tsnet/README.md`
- `$TS/wgengine/netstack/netstack.go:290-330, 395-408`; `netstack_tcpbuf_default.go:13-19`; `netstack_tcpbuf_ios.go:13-23`; gVisor `pkg/tcpip/transport/tcp/protocol.go:41-52`
- `$TS/derp/derpserver/derpserver.go:636-704`; `$TS/ipn/ipnstate/ipnstate.go:270-272`; `$TS/client/tailscale/apitype/apitype.go:35-42`; `$TS/tailcfg/tailcfg.go:1683`
- `$TS/net/netmon/state.go:821-835`; `$TS/feature/androidbin/register.go`; `$TS/tool/gocross/autoflags.go:151-156`; `$TS/logpolicy/logpolicy.go:895`; `$TS/net/batching/conn_linux.go:547-564`
- `$TSRS/README.md`, `ARCHITECTURE.md`, `CHANGELOG.md`, `src/lib.rs:156-340`, `src/axum.rs`, `src/config.rs:12-44`, `crates/ts_runtime/src/lib.rs:119`, `crates/ts_runtime/src/direct.rs`, `crates/control_plane/ts_control/src/node.rs:144-190`, `crates/net/overlay/ts_netstack_smoltcp/src/netcore/config.rs:41`
- `$LIBTS/tailscale.h`, `tailscale.go:243, 403-450`, `Makefile`, `swift/README.md`, `swift/TailscaleKit/URLSession+Tailscale.swift`
- Truffle: `packages/sidecar-slim/main.go` (1015-1124 start/auth; 2931-3100 WhoIs and identity headers; 3239-3247 proxy), `netstack_tuning.go`, `egress.go:1-23`, `benchmark/README.md:560-790`, `docs/rfcs/024-truffle-swift.md`, `docs/rfcs/025-login-scoped-nodes.md`
- Maintainer's private note on iOS embedding without the Tailscale app (not published)

**Other**
- [libtailscale](https://github.com/tailscale/libtailscale); [Headscale releases](https://github.com/juanfont/headscale/releases) and [features](https://headscale.net/stable/about/features/); [iroh road to 1.0](https://iroh.computer/blog/the-road-to-iroh-1-0) plus the crates.io API (iroh 1.0.0 on 2026-06-15; 1.3.0 on 2026-09-28; MIT/Apache-2.0)
- [NetBird client/embed](https://pkg.go.dev/github.com/netbirdio/netbird/client/embed); ZeroTier libzt BSL 1.1 (repo README via search)
- [gVisor issue #10312](https://github.com/google/gvisor/issues/10312); [golang/go#40569](https://github.com/golang/go/issues/40569); [Apple DTS on AVPlayer/mediaserverd](https://developer.apple.com/forums/thread/75328)
- [Channels DVR embedded Tailscale (2022, precedent)](https://community.getchannels.com/t/new-dvr-server-tailscale-integration-for-easier-away-from-home-access-experimental/33392)
- Plex relay limits (Plex forums via search; [official article](https://support.plex.tv/articles/216766168-accessing-a-server-through-relay/) blocked my fetch)
- [cinemaconfig UHD BD reference](https://cinemaconfig.com/reference/uhd-blu-ray)
