# 01 — Competitive Landscape & Positioning for flux

*Research date: 2026-10-18. Scope: Plex, Jellyfin, Emby, Kodi, Infuse, Channels DVR, Stremio, the Jellyfin third-party client ecosystem, new Rust/OSS media servers, adjacent OSS successes (Immich, Navidrome, Audiobookshelf), client-platform priorities, and licensing. Ends with a positioning statement and five measurable claims.*

**Evidence legend:** **[P]** = primary source read this session (official post, release, source code, issue tracker, vendor page). **[S]** = secondary source (press, blog, aggregator). **[I]** = my inference or recommendation. **[U]** = unverified or conflicting. Local code is cited as `path:line`.

**Method note:** About 60 web searches and fetches, the GitHub REST/GraphQL APIs (`gh api`) for repository, release and issue data, the raw selfh.st 2025 survey JSON, and the local Truffle and Mille checkouts. Partway through, the session hit its shared web-search budget, and gnu.org rate-limited fetches (HTTP 429). Because of that, a few legal points, such as the wording of the GPL FAQ on "aggregates", are paraphrased from well-known text and marked as such. No Truffle node was started and no tailnet device was authenticated.

---

## 0. TL;DR

1. **Plex is monetizing remote access, and that is the opening.** Between 2025-04-29 and 2026-09 Plex moved remote streaming of personal video behind Plex Pass or the Remote Watch Pass. It applied this platform by platform: Roku in Nov 2025, smart TVs and consoles on 2026-03-23, then third-party API clients such as Infuse in late Sep 2026. Prices also rose: Lifetime went from $119.99 to $249.99 to **$749.99** (2026-07-01), and the Remote Watch Pass went from $1.99 to **$2.99/mo** (2026-06-01). On top of that came a 2025 auth-database breach and repeated backlash over app redesigns. Firecore (Infuse) now publicly points users to **Tailscale** as the free path [P]. flux's Tailscale-native remote access answers the exact pain Plex created.
2. **Jellyfin is the open-source default (~61% of self-hosters vs Plex ~30%), but it has a performance and reliability problem.** The 10.11 EF Core migration (2025-10-19) caused **6×–200× scan regressions** in issue reports. Examples: 2m30s → 29m53s; 26 s → 86 min for 19k photos; 9–17 h initial scans. 12.0 (2026-09-07) fixed much of this, but the first week of 12.0 brought OOM bugs: a single `/UserViews` request allocated about 600 MB/s, and a native leak went from 330 MB to 4 GB [P]. Jellyfin's idle RSS is about **356–409 MiB**, and cold start to home screen is about 2.2–2.7 s (third-party benchmark [P-self-reported]).
3. **"Jellyfin, but in Rust" is already taken, and it is cheap to build.** **ferrofin** (Rust, GPL-3.0, created 2026-08-12) claims all 412 of Jellyfin's API operations. It reports being **15.8× faster to cold-start, 6.7× lighter at idle (61 MiB vs 409 MiB) and 3.4× faster on the home screen** than Jellyfin 12.0-rc7, and it was built largely by AI agents in about 6 weeks. **remux** (Rust, AGPL, 608★) is a Jellyfin-API server that adds Stremio add-ons. flux cannot differentiate on "Rust + Jellyfin API" alone. Neither project benchmarks **scan speed, time-to-first-frame, seek, or zero-config remote access**, so those remain open ground.
4. **The OSS projects that won (Immich, Navidrome, Audiobookshelf) share a pattern.** Each had one sharp "replace X" promise, mobile/client apps from day one (or an existing client protocol, as Navidrome has with Subsonic), a single lightweight deployable, a fast release cadence, and no paywalled features. Immich reached 116.6k★ with AGPL and no feature gating.
5. **Client platforms conflict with the Tailscale-first approach.** The largest US TV platforms are Roku (28% most-used device, Parks 2026) and Samsung Tizen (34% of smart-TV owners). Tailscale has **no client** for Roku, Tizen, webOS or consoles. It ships for **Apple TV, Android/Google TV and Fire TV** [P]. In-home TVs reach flux over the LAN without Tailscale, but *remote* living-room viewers in v1 must be on Apple TV or Android/Google/Fire TV. **Recommended first TV targets: Apple TV, then Android TV/Google TV/Fire TV**, bootstrapped through Jellyfin-API-compatible clients (Infuse, Swiftfin, Wholphin, Findroid, Streamyfin).
6. **Licensing.** Truffle is MIT (`truffle/Cargo.toml:26`). Mille is `MIT OR Apache-2.0` (`mille/Cargo.toml:14`). Tailscale and the new official `tailscale-rs` are BSD-3-Clause. Invoking FFmpeg as a **separate process** keeps flux's license independent, even when shipping a GPL FFmpeg build (jellyfin-ffmpeg is configured `--enable-gpl --enable-version3`) [P]. Recommendation [I]: engine crates `MIT OR Apache-2.0`, server application **AGPL-3.0-or-later**, Apple/Android clients **MPL-2.0**, DCO and no CLA.
7. **A legal tripwire for the AI-metadata plan.** TMDB's API terms (updated 2023-10-20) prohibit using "the TMDB APIs or TMDB Content in connection with, including for training, a machine learning (ML) or artificial intelligence (AI) based Application" [P]. Before AI agents touch TMDB-sourced data, flux needs written clarification from TMDB or an architecture that keeps TMDB content out of LLM paths.

---

## 1. Market map (October 2026)

| Product | Type | License / model | Server tech | Latest version (date) | Remote access model | Price to user | GitHub ★ |
|---|---|---|---|---|---|---|---|
| **Plex** | Server + clients, commercial | Proprietary; freemium | C++ (PMS), custom FFmpeg fork | PMS (continuous) | Plex relay + direct; **remote video paywalled** | Plex Pass $6.99/mo, $69.99/yr, $749.99 lifetime; Remote Watch Pass $2.99/mo, $29.99/yr | n/a |
| **Jellyfin** | Server + clients | GPL-2.0, free | C#/.NET 10, EF Core + SQLite, jellyfin-ffmpeg (GPLv3 build) | **12.2** (2026-10-05); 12.0 on 2026-09-07 | DIY: reverse proxy / port forward / VPN | Free | 58,225 |
| **Emby** | Server + clients, commercial | Closed since 4.x (2018) | C#/.NET | 4.10.1.0 stable (2026-09-29); 4.11 beta | DIY (port forward) | Premiere $4.99/mo, $54/yr, $119 lifetime | n/a |
| **Kodi** | Local player / HTPC | GPL-2.0 | C++ | 21.3 "Omega" stable (2025-10-31); 22 "Piers" RC1 (2026-09-27) | n/a (client) | Free | 21,347 |
| **Infuse** | Apple-only client | Proprietary | Swift | 8.x | Uses server's path (Plex / Jellyfin / Emby / SMB / WebDAV) | Free; Pro $1.99/mo, $16.99/yr, $99.99 lifetime | n/a |
| **Channels DVR** | Live-TV DVR + personal media | Proprietary | Go (server) | continuous | "Stream While Away" | $8/mo or $80/yr | n/a |
| **Stremio** | Add-on aggregator client | Client core MIT (Rust `stremio-core`) | Rust core | v5 | n/a (add-ons) | Free | 2,487 (core) |
| **Kyoo** | Server (video only) | GPL-3.0 | TypeScript + Go + Python, PostgreSQL | 5.2.1 (2026-08-27) | DIY + OIDC | Free | 2,542 |
| **Dim** | Server | AGPL-3.0 | Rust | 0.3.0-rc6 (2021-10-19); last push 2025-09-21 | DIY | Free | 4,125 (stalled) |
| **remux** | Jellyfin-API server + Stremio add-ons/torrents | AGPL-3.0 | Rust (Dioxus admin) | 0.37.0 (2026-10-17); created 2026-02-27 | DIY / desktop tray app | Free | 608 |
| **ferrofin** | Drop-in Jellyfin re-implementation | GPL-3.0-only | Rust (~360k LOC) | 1.3.2 (2026-10-03); created 2026-08-12 | DIY | Free | 40 |
| **Immich** (adjacent) | Photos | AGPL-3.0 | TypeScript/Node + ML | 3.3.3 (2026-10-14) | DIY | Free (optional product key) | 116,603 |
| **Navidrome** (adjacent) | Music (Subsonic API) | GPL-3.0 | Go, SQLite | 0.64.2 (2026-09-24) | DIY | Free | 24,229 |
| **Audiobookshelf** (adjacent) | Audiobooks / podcasts | GPL-3.0 | Node | 2.37.1 (2026-09-29) | DIY | Free | 14,704 |

Sources: GitHub API queries run 2026-10-18 for stars, licenses and releases [P]. Plex prices from [plex.tv/plans](https://www.plex.tv/plans/) [P] and the [Plex forum lifetime post](https://forums.plex.tv/t/new-lifetime-plex-pass-pricing/938910) [P]. Emby prices from [emby.media/premiere](https://emby.media/premiere.html) [P]. Infuse prices from [firecore.com/infuse](https://firecore.com/infuse) [P]. Channels pricing from the [Channels support docs](https://getchannels.com/docs/getting-started/quick-start-guide/subscription/) [S, via search snippet]. Stremio from [stremio.com](https://www.stremio.com/) [P]. The Emby 4.10.1.0 date comes from the `MediaBrowser/Emby.Releases` GitHub releases [P].

---

## 2. Plex (2025–2026)

### 2.1 Timeline of changes

| Date | Event | Source |
|---|---|---|
| Nov 2023 | "Discover Together" sends **"Week in Review" emails exposing friends' watch history**. The feature was opt-out, and a large forum thread called it a "data leak". | [Plex forum thread](https://forums.plex.tv/t/weekly-review-emails-data-leak/860206) [P/S] |
| 2025-03-19 | Price increase and remote-playback policy announced, **"first price increase for Plex Pass in over 10 years"**. | [9to5Mac](https://9to5mac.com/2025/03/19/plex-price-increase-remote-streaming-changes/) [S] |
| 2025-04-29 | Plex Pass rises to **$6.99/mo, $69.99/yr, $249.99 lifetime** (from $119.99). Sources disagree on the old monthly and yearly prices: $4.99/$39.99 per [AlternativeTo](https://alternativeto.net/news/2025/3/plex-raises-subscription-prices-up-to-75-and-locks-remote-streaming-behind-a-paywall) vs $5.99/$49.99 per 9to5Mac [U]. **Remote playback of personal media is no longer free.** The new **Remote Watch Pass** costs $1.99/mo or $19.99/yr (introductory), and the mobile 1-minute limit is removed. | [S] (two outlets agree on the new prices) |
| 2025 | New mobile and TV apps **move music and photos out** of the main app into Plexamp and Plex Photos. After backlash, Plex restored the TV left-hand navigation and brought music and photos back on Roku (v8.4.0). | [Neowin](https://neowin.net/news/plex-reverts-tv-app-layout-after-intense-user-backlash/), [Yahoo Tech](https://tech.yahoo.com/streaming/articles/plex-reversing-unpopular-part-app-172641244.html) [S] |
| 2025-09-08 | **Security breach.** "An unauthorized third party accessed a limited subset of customer data from one of our databases". The data included emails, usernames, hashed passwords and authentication data, and Plex did not disclose its hash algorithm. All users were told to reset passwords and sign out devices. (A separate breach in Aug 2022 affected at least 15M accounts.) | [BleepingComputer](https://bleepingcomputer.com/news/security/plex-tells-users-to-reset-passwords-after-new-data-breach) [S, quoting Plex] |
| 2025-11-19 | Plex says it will begin **enforcing the remote paywall on Roku "next week"**, with "all other Plex TV apps (Fire TV, Apple TV, Android TV, etc) and any third party clients using the API" to follow in 2026. | [Plex forum](https://forums.plex.tv/t/changes-coming-to-remote-streaming-on-roku/933671/1) [P] |
| 2026-03-11 | Announcement: from **2026-03-23**, remote streaming on **Samsung, LG and Vizio smart TVs, PlayStation and Xbox** requires Plex Pass or Remote Watch Pass. "Local streaming remains completely free." "Remote streaming from Music and Photo libraries are not affected." | [Plex forum](https://forums.plex.tv/t/changes-coming-to-remote-streaming-on-smart-tvs/937015) [P] |
| 2026-03-31 to 05 | **Fire TV redesign**: a top nav bar replaces the pinned-library sidebar. Users report "two clicks to about six", 30 s+ waits for local direct play, and crashes, with no opt-out. A secondary source says Fire TV remote enforcement came with it (2026-04-29). | [PiunikaWeb](https://piunikaweb.com/2026/05/04/plex-fire-tv-redesign-unusable-users-slam-navigation/) [S]; [JellyWatch](https://jellywatch.app/blog/plex-remote-streaming-paywall-infuse-third-party-apps-jellyfin-options-2026) [S] |
| 2026-05-18 | **Lifetime Plex Pass goes from $249.99 to $749.99**, effective 2026-07-01. Monthly, annual and Remote Watch Pass prices "unchanged". A 5-year recurring pass at $249.99 was also reported. | [Plex forum](https://forums.plex.tv/t/new-lifetime-plex-pass-pricing/938910) [P]; 5-year pass [S/U] |
| 2026-06-01 | **Remote Watch Pass goes to $2.99/mo and $29.99/yr** (introductory pricing ended). | [plex.tv/plans](https://www.plex.tv/plans/) [P] |
| 2026-06-03 | "Social platform for entertainment discovery": lists, public discussions, Match Score and reactions. Plex cites **>100M watching decisions/month and >45M watchlists**. | [The Desk](https://thedesk.net/2026/06/plex-launches-social-discovery-features/) [S] |
| 2026-09-21 | Firecore relays Plex's notice: "remote streaming from a Plex server will require a paid Plex license", **including third-party apps like Infuse**. Firecore suggests "**Tailscale** or a more traditional VPN" as alternatives. | [Firecore community](https://community.firecore.com/t/changes-to-plex-remote-streaming-requirements/60825) [P] |
| 2026 | Tailscale exit-node workarounds, which made remote clients look "local" to Plex, are reported to **no longer bypass** the paywall ("numerous reports in 2026"; "worked throughout most of 2025"). | [fullmetalbrackets](https://fullmetalbrackets.com/blog/plex-remote-access-tailscale) [S, updated 2026-05-05] |

### 2.2 What users hate (2025–2026)
- **Paying to watch their own files remotely.** The paywall is enforced per platform and now covers third-party clients. Sharing with family who lack Plex Pass now costs the server owner or each viewer [P, above].
- **Price escalation.** Lifetime went $119.99 → $749.99 in about 14 months, roughly 6.25× [P/S].
- **Redesigns that make personal media harder to reach**: Fire TV top nav, the mobile music/photos split, forced updates with no opt-out [S].
- **Trust and privacy.** The 2023 Week-in-Review overshare, the 2025 credential breach, and a mandatory plex.tv account that can collect usage data [S].
- **Mixing in Plex's own content (Discover / free ad-supported TV) with personal libraries.** Plex states it does not place ads on personal media ([Neowin](https://www.neowin.net/news/plex-reaffirms-its-commitment-to-personal-media-management-after-streaming-launch)) [S]. The complaint is about prominence ([NASCompares](https://nascompares.com/news/plex-ad-supported-streaming-nas-users-frustrated/)) [S].

### 2.3 What users still love (the bar flux must clear)
- **Client coverage.** Plex has native apps on Roku, Fire TV (including the new Vega OS), Apple TV, Android TV, Samsung (Tizen 2016+), LG (webOS 3.0+), Vizio, Xbox, PlayStation, iOS, Android and web ([Plex smart TV support](https://support.plex.tv/articles/204080173-which-smart-tv-models-are-supported/)) [S via search; the support site returned 403 to fetch].
- **Remote access that "just works" for non-technical relatives.** Account-based sharing with relay fallback; "zero knowledge for relatives" ([Lemmy thread](https://lemmy.mariusdavid.fr/post/423403/1346843)) [S]. An April 2026 commenter on Jellyfin issue #15070 wrote: "I've actually gone through the process of migrating users back to Plex" after 10-hour scans [P].
- **Polish and breadth**: Skip Intro/Credits, downloads, Plexamp, hardware transcoding (Plex Pass features per [plex.tv/plans](https://www.plex.tv/plans/)) [P].

### 2.4 Implications for flux [I]
- The single most monetized Plex feature, **remote playback of your own video**, is what Tailscale makes free, private and direct. Infuse's vendor already recommends Tailscale. flux should make "remote is just your tailnet" the headline, not a workaround.
- Plex's reach rests on client coverage and the account model. flux cannot match the client count in v1. It can match the **"invite a relative" experience** on the platforms Tailscale supports.
- Do not copy Plex's mistakes. Make no redesign that hides personal libraries, put no ads or "discover" content in the library, require no mandatory cloud account, and send no telemetry by default.

---

## 3. Jellyfin

### 3.1 Versions and roadmap (verified)
| Version | Date | Key changes | Source |
|---|---|---|---|
| 10.9.0 | May 2024 | **Trickplay** (scrub thumbnails) built in; DLNA moved to a plugin | [AlternativeTo](https://alternativeto.net/news/2024/5/jellyfin-10-9-introduces-live-video-preview-multiple-subtitle-tracks-and-improved-search) [S] |
| **10.11.0** | **2025-10-19** | "The Library Database conversion to **EF Core** is complete!" Migration "may take up to several hours". Metadata is cached in memory, so RAM can reach "up to the full size of your Library database". Built-in live backup/restore. **armhf (32-bit ARM) dropped.** FFmpeg 7.1. Refuses to start with <2 GB free in data dirs. About 6 months of development plus about 6 months of RCs. | [jellyfin.org 10.11.0](https://jellyfin.org/posts/jellyfin-release-10.11.0) [P] |
| 10.11.1 to 10.11.11 | Oct 2025 to mid-2026 | Point releases. Scan regressions persisted through at least 10.11.8 (Apr 2026, below). | GitHub issues [P] |
| **12.0** | **2026-09-07** (tag 2026-09-08 UTC) | **Versioning reset** (10.12 becomes 12.0). Targets **.NET 10**; "several plugin interfaces changed", so plugins must be rebuilt. Playlists, collections and boxsets stored one row per item. Heavy DB maintenance no longer runs during scans. **Multi-version episodes**. Books/comics in core (Bookshelf deprecated). **Per-library recommendation source; plugins can add search results**. Modern UI is the default. **FFmpeg 8.1**. HLG→SDR via BT.2446 Method B, `dvh1` HLS variant for DV Profile 5, trickplay improvements. Security fixes (path traversal outside served dirs, setup-wizard re-run, XSS). **Breaking**: legacy `/emby/` and `/mediabrowser/` routes removed, `GetItems` async with changed semantics, Swashbuckle v10 so SDKs must be regenerated, and a full library scan is required after upgrade. | [jellyfin.org 12.0](https://jellyfin.org/posts/jellyfin-release-12.0/) [P] |
| 12.1 / 12.2 | 2026-09-15 / 2026-10-05 | Fix releases | GitHub releases API [P] |

The initial discussion's claim that "Jellyfin 12.0, released September 7, added extension points for search and recommendation providers" is **verified**. A blog claim that "Jellyfin 12.0 ships AI tagging" ([dailyaiworld](https://dailyaiworld.com/blogs/jellyfin-120-released-open-source-media-server-ships-ai)) is **not supported** by the official release notes [P] and should be treated as false.

### 3.2 Quantified pain points

**Library scan time.** All rows are user reports with logs on GitHub, except where noted.

| Report | Library | Before | After | Ratio | Source |
|---|---|---|---|---|---|
| No-op rescan, 10.10.7 → 10.11.0 | not stated; 12 plugins, Docker on PVE | 2 min 30 s | 29 min 53 s | **12×** | [#15070](https://github.com/jellyfin/jellyfin/issues/15070) [P] |
| Same user, 10.11.1 | ~500 films, ~150 shows, ~64k audio files | ~3 min | 30 min (10.11.0), **2 h 10 min** (10.11.1) | 10–43× | #15070 comments, 2025-10-27 [P] |
| Another user | ~30k video + 10k audio files | 3–5 min | ~30 min | 6–10× | #15070, 2025-10-27 [P] |
| Raspberry Pi 5 (Docker) | small | 1 min 05 s | ~4 min (10.11.2); "3.5× longer" (10.11.5) | 3.5× | #15070, 2025-11-03 and 2025-12-16 [P] |
| Mixed library | >2 million images and videos | "a few minutes" | **17 h** initial; second scan >7 h with no changes | >100× | #15070, 2025-10-23 [P] |
| Another user | not stated | <5 min | 9 h initial, then ~1 h for every scan | ~12× | #15070, 2025-11-03 [P] |
| 10.11.8 (Apr 2026) | ~8000 missing episodes listed | — | "My last scan took 10 hours"; user migrated their users back to Plex | — | #15070, 2026-04-19 [P] |
| Photo library, controlled test by a triager | ~19,000 photos | **26 s** (10.10.7) | **86 min** (10.11.0) → **3 min 06 s** (12.0 master) | 198× regression; 12.0 still **7.2×** slower than 10.10.7 | [#14386](https://github.com/jellyfin/jellyfin/issues/14386) [P] |
| Forum, 10.10.7 | ~6,000 files, Athlon II, 4 GB | — | 3 h ("Plex takes much less time") | — | [Lemmy](https://lemmy.world/post/33126188) [S] |
| Forum, 10.11 | music library >12,000 tracks | — | stalls at 65–85% for up to 24 h; another scan "Completed after 524 minute(s)" | — | [Jellyfin forum](https://forum.jellyfin.org/t-jellyfin-scan-never-finishes-in-10-11-00) [P/S] |

A maintainer explained on #15070 (2025-10-21) that scans "are expected to take (way) longer in 10.11 because we are now a) properly validating all existing data and b) properly applying concurrency limits" [P]. #15070 was closed 2026-06-04 as a duplicate of #14386, "should be fixed in 12.0". A user then disputed that, noting #14386 is about photo libraries [P].

**Database locking and concurrency**
- Jellyfin's own post, ["SQLite concurrency and why you should care about it"](https://jellyfin.org/posts/SQLite-locking/) (J.-P. Bachmann, 2025-10-04) [P], is the root-cause write-up. Before 10.11, a bug in the parallel-task limit caused "exponential overscheduling of library scan operations", producing "thousands of parallel write requests" that SQLite could not absorb. Jellyfin added three modes: **No-Lock (the default)**, Optimistic (Polly retries) and Pessimistic (`ReaderWriterLockSlim`, a single writer).
- [#15101](https://github.com/jellyfin/jellyfin/issues/15101) (open, 230 comments) covers "Database Locked" on Proxmox LXC with ZFS: "with 10.11 those issues now amplified … There is not much we can do" [P].
- #15070 logs show `SQLite Error 6: 'database table is locked: BaseItems'` 67× in a single scan, and Kestrel "thread pool starvation" warnings during scans [P].

**Memory and API latency**
- [#11588](https://github.com/jellyfin/jellyfin/issues/11588) (10.9): the scan spawns many `ffprobe` processes, "each takes around 700Mb of RAM", which OOMs a Raspberry Pi 4 (4 GB) on a 200-movie 4K HDR library [P].
- 12.0 launch-week regressions [P]:
  - [#17871](https://github.com/jellyfin/jellyfin/issues/17871): one `GET /UserViews` "allocate[s] heap at roughly **600 MB/s** until it is OOM-killed" (library of 311,788 BaseItems, 199,432 of them Person rows). It "kills the server within 30 seconds" for any client login.
  - [#17934](https://github.com/jellyfin/jellyfin/issues/17934): native memory leak "~330MB to 4GB in seconds after login".
  - [#17860](https://github.com/jellyfin/jellyfin/issues/17860): "Scan All Libraries" exhausts memory in about 25 min.
  - [#18088](https://github.com/jellyfin/jellyfin/issues/18088) (12.1.0): `/Items?recursive=true&includeItemTypes=Series` takes **692 s, 635 s and 459 s** on a 51-series, 10,397-episode library.
  - [#18066](https://github.com/jellyfin/jellyfin/issues/18066): a migration "never completes and keeps one CPU core at 100%".
- [#15090](https://github.com/jellyfin/jellyfin/issues/15090) (10.11): the Collections page takes **10–15 s** with about 90 collections [P].
- Third-party benchmark from ferrofin's README (2026-09-04, Ryzen 9 9950X3D, 8 pinned cores, 3,001 movies plus 250 series / 7,490 episodes, median of 3 runs) [P, self-reported by a competitor]:
  - Jellyfin 12.0-rc7: **idle 409 MiB**, peak 588 MiB, cold start to home **2,746 ms**, home screen p50 44 ms, direct-play TTFB (1 MiB range) 4.61 ms.
  - Jellyfin 10.11.8: idle 356 MiB, cold start 2,162 ms.

**Upgrade and migration risk.** The 10.11 migration "may take up to several hours" and must not be interrupted. Upgrading is supported only from 10.10.7, and backups restore only on the same OS/container [P]. [#15027](https://github.com/jellyfin/jellyfin/issues/15027), "Database Migration Failed on update from 10.10 to 10.11", has 121 comments [P]. 12.0 requires a full rescan and fails on usernames that differ only by case [P].

**Remote access and clients.** Jellyfin has no relay and no identity service. Remote access means a reverse proxy, port forwarding or a VPN. Community comments cite this as the main reason to stay on Plex [S]. Roku has codec quirks: Roku players "do NOT support more than 2.0 AAC and OPUS", which triggers transcodes ([Jellyfin forum, Jul 2026](https://forum.jellyfin.org/t-roku-app-direct-play-troubleshooting)) [S].

### 3.3 What Jellyfin does well (and flux must match)
- **Everything free.** That includes hardware transcoding, which Plex and Emby gate behind payment [P, Plex plans / Emby Premiere pages].
- **Official clients** for Android TV and Fire TV, Roku, LG webOS, Samsung Tizen, Xbox, iOS, Android, desktop, and Kodi (JellyCon) ([jellyfin.org/downloads/clients](https://jellyfin.org/downloads/clients/)) [P].
- **A healthy third-party client ecosystem** (section 5.6). **Swiftfin for tvOS reached the App Store on 2026-07-16** (Swiftfin 1.5) ([discussion #1294](https://github.com/jellyfin/Swiftfin/discussions/1294)) [P].
- **Mindshare.** In the selfh.st 2025 survey (section 7.1), 2,487 of 4,081 respondents (60.9%) run Jellyfin vs 1,219 (29.9%) for Plex, and Jellyfin is the #2 "favorite software" overall [P].
- A versioned, documented API with a generated TypeScript SDK, which third-party servers (ferrofin, remux) now implement.

### 3.4 Implications for flux [I]
- Jellyfin's weakest areas are **data-model/DB throughput, scan orchestration, migrations and memory**. Those are where flux's "Rust + incremental scanning + explicit concurrency" thesis directly applies.
- **Benchmark against 12.2, not 10.11.** 12.0 recovered much of the regression (86 min → 3 min in #14386). Claims must be measured against the current release on the same hardware, or they will be dismissed.
- Jellyfin's API churn (12.0 removed routes and changed `GetItems` semantics) means any compatibility layer must be versioned and kept small.

---

## 4. Rust and new entrants (the "Jellyfin-in-Rust" niche is occupied)

| Project | What it is | Evidence | Lesson for flux |
|---|---|---|---|
| **ferrofin** ([mangoleaf/ferrofin](https://github.com/mangoleaf/ferrofin)) | "A drop-in Rust implementation of the Jellyfin media server." Claims 412/412 API operations, adopts an existing Jellyfin DB in place (10.11.8 to 12.2), and runs third-party plugins as sandboxed WASM. GPL-3.0-only. | README benchmarks vs Jellyfin 12.0-rc7: **home 3.4×, series 12.1×, search 2.7× faster; cold start 174 ms vs 2,746 ms (15.8×); idle 61.4 MiB vs 409 MiB (6.7×); peak 155 vs 588 MiB**. "~360k lines of Rust", with "AI agents writing most of the code" over about 6 weeks. 40★, v1.3.2 (2026-10-03). **No scan, TTFF or seek benchmarks.** [P, self-reported] | The low-memory, fast-API Rust server is now **commoditized**. flux must win on things ferrofin does not measure or attempt: scanning, playback latency, zero-config remote, metadata quality and design. |
| **remux** ([lostb1t/remux](https://github.com/lostb1t/remux)) | Jellyfin-compatible API server that merges **Stremio add-ons, local files, WebDAV, torrents** and IPTV. Desktop tray app (macOS/Linux/Windows), import of Jellyfin users. AGPL-3.0. | 608★ in about 8 months, 0.37.0 (2026-10-17) [P] | Using the Jellyfin API to get clients immediately works (Infuse, Swiftfin and Android clients "without changes"). A desktop tray installer lowers setup cost. |
| **Jellyswarrm** | Rust proxy that aggregates many Jellyfin servers | 934★ [P] | Multi-server federation is real demand, and a natural fit for Truffle's mesh. |
| **Kyoo** ([zoriya/Kyoo](https://github.com/zoriya/Kyoo)) | Video-only server. "No folder structure required … Media not being scanned correctly (even with weird names) is considered a bug". Dynamic ABR transcoding, intro/credit detection via audio fingerprinting, OIDC, Helm chart. No plugin system. | v5.0.0 rewrite (2026-04-06), 5.2.1 (2026-08-27). TypeScript, Go, Python and PostgreSQL. "Developed by one person only"; clients are web, Android and Chromecast, with Android TV still planned [P] | The "messy names just work" promise resonates, but a multi-service Postgres deployment and few TV clients cap adoption. |
| **Dim** ([Dusk-Labs/dim](https://github.com/Dusk-Labs/dim)) | Rust media server, AGPL | 4.1k★, last release 0.3.0-rc6 (**2021**), last push 2025-09 [P] | A cautionary tale: a Rust server with a nice web UI and **no TV clients or remote story** stalls. |
| Others (2026) | `rustyfin`, `jellyfin-rs`, `nipaserver` ("agentic media server in Rust … LLM agent"), `peregrine` (Tauri + mpv desktop), `Opal` (Zig) | 0–95★ each [P] | AI-assisted Rust re-implementations are proliferating. Credibility will come from **reproducible benchmarks and real clients**, not from the language. |

---

## 5. Other competitors

### 5.1 Emby
- Closed source since 4.x. The Jellyfin fork was triggered in Dec 2018 "primarily as a result of Emby's decision to take their next release (4.x) closed-source". Other cited reasons were GPL violations, a paywall added in 3.x, and hiding client and server code ([jellyfin.org/docs/general/about](https://jellyfin.org/docs/general/about/)) [P].
- **Emby Premiere**: $4.99/mo, $54/yr, $119 lifetime, one household, 30 devices. It gates **hardware-accelerated transcoding, offline downloads, DVR, Cinema Intros, Backup/Restore, Folder Sync** and full mobile/TV apps ([emby.media/premiere](https://emby.media/premiere.html)) [P].
- Stable 4.10.1.0 (2026-09-29); 4.11 in beta (4.11.0.9 on 2026-10-18) [P]. Mindshare is 149/4,081 (3.7%) of selfh.st respondents [P].
- **Lesson [I]:** A closed fork of an open project loses the community (Emby vs Jellyfin is the canonical example). That argues for flux to **avoid a CLA** and the appearance of a future relicensing path.

### 5.2 Kodi
- Stable **21.3 "Omega" (2025-10-31)**; **22.0 "Piers" RC1 tagged 2026-09-27** (betas 2026-06-21 and 2026-08-31) [P, GitHub releases]. Press reports RC1 adds HDR overlays and UHD PGS subtitles [S].
- A local player/HTPC rather than a server. It connects to Jellyfin through JellyCon or Jellyfin for Kodi. Used by 237 (5.8%) of selfh.st respondents [P].
- **Lesson [I]:** Power users value player quality (DV, HD audio passthrough, subtitle fidelity). A Kodi add-on is a cheap later client for flux.

### 5.3 Infuse (Firecore)
- Apple-only (iOS/iPadOS 18+, tvOS 18.5+, macOS 15+, visionOS 26+). Supports **Plex, Jellyfin, Emby**, SMB/NFS/WebDAV/FTP/SFTP, UPnP and many clouds. Pro is $1.99/mo, $16.99/yr or $99.99 lifetime. Plays MKV/ISO/BDMV without transcoding, Dolby Vision P5/P8, Atmos (E-AC3) passthrough, AV1/HEVC/VVC listed. Metadata "powered by TMDB" ([firecore.com/infuse](https://firecore.com/infuse)) [P].
- Plex's paywall reached Infuse remote playback in late Sep 2026, and Firecore points users to Tailscale [P].
- **Lesson [I]:** Infuse is the premium Apple TV player that many Plex refugees already own. **Day-one Infuse compatibility** (Jellyfin-API subset, or at least WebDAV with clean metadata) gives flux a best-in-class Apple TV client for free. Infuse users are exactly the audience that will notice time-to-first-frame and DV handling.

### 5.4 Channels DVR
- $8/mo or $80/yr ([Channels docs](https://getchannels.com/docs/getting-started/quick-start-guide/subscription/)) [S]. Focused on live TV, with personal media import ("smart scanner", files stay in place). Clients on Apple TV, iOS, Android and Fire TV. The server runs on macOS, Windows, Linux, Raspberry Pi and many NAS ([getchannels.com/dvr-server](https://getchannels.com/dvr-server/)) [P].
- **Lesson [I]:** A small team with a polished, narrow Apple TV and Fire TV experience sustains a subscription for years. Polish on a few platforms beats mediocre coverage on many.

### 5.5 Stremio
- Claims "more than 30 million users". Clients for Android/Android TV, desktop, LG and Samsung TVs, and web for iOS. "We provide no content ourselves": everything comes through add-ons ([stremio.com](https://www.stremio.com/)) [P]. `stremio-core` is Rust (MIT) [P]. App-store removals in 2025–2026 are reported by secondary sources [S/U].
- **Lesson [I]:** Stremio proves the **aggregator UX** and a Rust client core at scale, but its add-on/torrent orientation carries legal and app-store risk. flux should stay a **personal-library** product. remux already bridges Stremio add-ons into Jellyfin clients; flux should not.

### 5.6 Jellyfin third-party clients (what flux can inherit)
| Client | Platform | License | Status | Source |
|---|---|---|---|---|
| Swiftfin | iOS, iPadOS, **tvOS** | MPL-2.0 | 1.5 on the App Store for iOS **and tvOS** (2026-07-16); 1.6.1 (2026-08-23) | [P] |
| Findroid | Android | GPL-3.0 | 4.3k★, active | [P] |
| Streamyfin | iOS, Android (Expo), tvOS (per README) | MPL-2.0 | 5.3k★, v0.55.2 (2026-10-16); downloads, Chromecast, Jellyseerr | [P]/[S] |
| Wholphin | Android TV | GPL-2.0 | 3.1k★; Plex-inspired UI written from scratch | [P]/[S] |
| Official Android TV client | Android TV / Fire TV | GPL-2.0 | 4.6k★ | [P] |
| Infuse | Apple | Proprietary | see 5.3 | [P] |

**Lesson [I]:** Like Navidrome's Subsonic strategy, a **Jellyfin-API compatibility profile** gives flux usable living-room and mobile clients before its own exist. It must be clean-room (from the published OpenAPI/SDK, not ported GPL code), versioned, and limited to the subset these clients use.

---

## 6. Adjacent OSS success stories: why they won

| Project | Signals | Why it won (evidence) | Transferable lesson [I] |
|---|---|---|---|
| **Immich** (photos) | 116.6k★; **v2.0.0 "stable" 2025-10-01** after ~1,337 days, 271 releases and 1,558 contributors; 78k★ at v2.0; now v3.3.3 ([immich.app/blog/v2.0.0-release](https://immich.app/blog/v2.0.0-release)) [P] | One-line pitch ("self-hosted Google Photos"). Mobile **auto-backup** app as the core of the product. Very fast cadence. The **FUTO** deal (May 2024) made the core team full-time while keeping AGPL, "no features locked behind payment and no advertising" ([Linuxiac](https://linuxiac.com/immich-team-goes-full-time/)) [S]. Funded through optional product keys: "we will never implement paywalled features" [P]. selfh.st #3 favorite app (429 votes) [P]. | A crisp "replace X" promise. The client app *is* the product. Ship weekly. Monetize without paywalls. AGPL did not limit adoption. |
| **Navidrome** (music) | 24.2k★; Go single binary plus SQLite; Raspberry Pi binaries; v0.64.2 [P] | "**Compatible with all Subsonic/Madsonic/Airsonic clients**", so dozens of mature clients exist from day one. "Very low resource usage. Runs well even on simple Raspberry Pi Zero" ([navidrome.org docs](https://www.navidrome.org/docs/overview/)) [P]. | **Adopt an existing client protocol** (for flux, the Jellyfin API). Single binary plus SQLite. Measure and advertise footprint. |
| **Audiobookshelf** (audiobooks) | 14.7k★; GPL-3.0; v2.37.1 [P] | Purpose-built **mobile apps with per-user progress sync**. "Auto-detects library updates, no need to re-scan". The iOS TestFlight hit Apple's 10k-tester cap ([README](https://github.com/advplyr/audiobookshelf)) [P]. Used by 840 (20.6%) of selfh.st respondents [P]. | **Watcher-based incremental indexing** is a user-visible feature. A narrow vertical can beat a generalist (Plex/Jellyfin audiobooks). |

**Common pattern [I]:** (1) a single promise against a disliked incumbent; (2) clients on day one, whether built or inherited through a protocol; (3) one container or binary with an embedded DB; (4) visible speed and low footprint; (5) a fast, public cadence; (6) no paywalled features, with sustainability through goodwill, donations, keys or a sponsor.

---

## 7. Demand-side data

### 7.1 Self-hoster survey (selfh.st 2025, n = 4,081, published 2025-11-21)
Raw data from [`selfhst/cdn` `2025-results.json`](https://github.com/selfhst/cdn/tree/main/assets/surveys/annual) [P]. Percentages are of 4,081 respondents (multi-select).

| Question | Result |
|---|---|
| Media streaming server | **Jellyfin 2,487 (60.9%)**, Plex 1,219 (29.9%), Audiobookshelf 840 (20.6%), Navidrome 656 (16.1%), Kodi 237 (5.8%), Emby 149 (3.7%) |
| Remote access | **VPN 2,902 (71.1%)**, reverse proxy 2,716 (66.6%), forwarded ports 721 (17.7%), LAN only 510 (12.5%) |
| Unique users per setup (n = 4,069) | 1 user 27.6%; **1–2 users 48.6%; ≤6 users 84.5%**; ≤10 users 97.6% |
| Containers vs bare metal | Containers 2,720, "depends" 1,041, bare metal 174 (4.4%) |
| Primary mobile OS | Android 66.7%, iOS 31.8% |
| Favorite software | Home Assistant 588, **Jellyfin 522**, **Immich 429**, Plex 208, Audiobookshelf 94 |
| Donated to a self-hosted project last year | 41% |

Implications [I]:
- The enthusiast audience already uses VPNs (71%), and most setups serve six or fewer people. That matches Tailscale's **free Personal plan: up to 6 users, unlimited user devices, non-commercial** ([tailscale.com/pricing](https://tailscale.com/pricing)) [P].
- Docker is the expected install path, but a single native binary and desktop tray (remux-style) widen the funnel toward Plex's non-technical users.
- Caveat: this sample over-represents enthusiasts. Plex's mass-market base (>100M watch decisions per month [S]) is far larger and less technical.

### 7.2 TV platform shares
| Metric | Figures | Source |
|---|---|---|
| Parks Associates, US internet households (n = 8,000), CES Jan 2026 | Smart TV is the **primary streaming device for 61%**. **Samsung Tizen 34%** of smart-TV owners. **Roku 28%** as the most-used CTV device overall. Roku's smart-TV OS share rose from 8% (2020) to 18% (2025). | [Señal News summary](https://senalnews.com/en/data/61-of-us-internet-households-use-their-smart-tv-as-the-primary-streaming-device) [S] |
| Pixalate Q1 2026, North America programmatic-ad share of voice | **Roku 36%, Fire TV 19%, Samsung 15%, Apple TV 13%** (Q4 2025: Roku 32%, Fire 16%, Apple 15%, Samsung 14%) | [Pixalate](https://www.pixalate.com/blog/pixalates-q1-2026-connected-tv-ctv-device-market-share-report) [S via search snippet] |

### 7.3 Where Tailscale (and therefore flux remote) can run
- Tailscale lists **Apple TV (tvOS 17+, must be a home hub; 18+ recommended), Android (8.0+, including Android TV and Google TV), and Amazon Fire devices** among its install targets. **Roku, Samsung Tizen, LG webOS, Xbox, PlayStation and Chromecast are absent** ([tailscale.com/docs/install](https://tailscale.com/docs/install), [Apple TV doc](https://tailscale.com/docs/install/appletv.md), [Android doc](https://tailscale.com/docs/install/android.md)) [P].
- Tailscale's Apple TV doc already shows users entering `jellyfin.<tailnet>.ts.net` in media apps [P]. The pattern exists; flux's job is to remove the manual steps.
- **Truffle's Apple embed currently targets macOS 14 and iOS 18.1 only** (`truffle/Package.swift:38-41`). TailscaleKit is conditioned to `.iOS` (`Package.swift:67`). RFC 024 describes in-app libtailscale "without the Tailscale App Store app" (`truffle-rfc025/docs/rfcs/024-truffle-swift.md:47,79`), but **tvOS is not covered** [P].
- Tailscale announced **`tailscale-rs`**, an embedded Rust implementation, as "experimental (pre-alpha)" on 2026-08-31 ([Tailscale blog](https://tailscale.com/blog/easier-building-with-tailscale)). The repo is [tailscale/tailscale-rs](https://github.com/tailscale/tailscale-rs) (BSD-3-Clause, created 2026-04-05, 1,196★) [P]. In the medium term this could replace Truffle's Go sidecar. [I]

### 7.4 Client platform ranking for flux v1 [I]
Scored on (a) whether remote viewers can use it with Tailscale, (b) enthusiast and Plex-refugee density, (c) native player quality for direct play (HDR/DV/HD audio), (d) whether a client is available now via the Jellyfin API, and (e) cost to build flux's own client.

| Rank | Platform | Tailscale remote | Inheritable clients now | Notes |
|---|---|---|---|---|
| 1 | **Apple TV (tvOS)** | ✅ (app) | Infuse, Swiftfin (App Store 2026-07) | Best direct play (AVPlayer, DV), enthusiast-heavy, Plex refugees own Infuse. Truffle needs a tvOS embed path. |
| 2 | **Android TV / Google TV / Nvidia Shield** | ✅ (app) | Official Jellyfin Android TV, Wholphin | Media3/ExoPlayer; global reach. |
| 3 | **Fire TV (Fire OS)** | ✅ (Amazon Fire devices) | Same APKs as Android TV | Large US share (19% NA SOV). Risk: newer **Vega OS** devices are not Android (Plex ships a separate Vega app [S]). |
| 4 | **Web + iOS + Android phones** | ✅ | Streamyfin, Findroid, Swiftfin | Required for admin, invites and casting. Android is 67% of self-hoster phones. |
| 5 | Samsung Tizen / LG webOS | ❌ (LAN only) | Official Jellyfin apps (LAN) | Biggest smart-TV base. In-home use over LAN works; *remote* viewers need another path. Defer. |
| 6 | Roku | ❌ (LAN only) | Official Jellyfin Roku (LAN) | #1 US device, but BrightScript, codec limits and no Tailscale. Defer. |
| 7 | Consoles | ❌ | — | Low priority. |

Key nuance: **in-home TVs do not need Tailscale**. They reach the server over the LAN. The Tailscale constraint applies only to *remote* households, which is exactly where Plex charges. For remote Roku/Tizen/webOS viewers, later options include a Funnel or public-ingress mode, or a "gateway" device in the remote home. Each weakens the zero-config, private story and should be explicit opt-ins [I].

---

## 8. Table stakes vs differentiators

### 8.1 v1 table stakes (users will not switch without these) [I, derived from Plex/Jellyfin/Emby feature sets above]
| Area | Must-have for v1 | Reference |
|---|---|---|
| Library | Movies and TV (seasons, specials, multi-episode files), **multi-version (4K/1080p) grouping**, correct auto-match, artwork, search | Jellyfin 12.0 added multi-version episodes [P] |
| User state | Multiple users and profiles, resume, watched, **Continue Watching / Next Up**, per-version resume | Jellyfin 12.0 [P] |
| Playback | Direct play, then remux, then audio transcode, then video transcode. **Free hardware transcoding** (VideoToolbox, VAAPI/QSV, NVENC). HLS (fMP4). Audio and subtitle track selection; SRT/ASS/PGS with burn-in fallback. | Jellyfin gives HW transcoding free; Plex and Emby gate it [P] |
| Seek UX | **Trickplay thumbnails** | Jellyfin since 10.9 [S]; Kyoo [P] |
| Skip | Intro/credits skip | Plex Pass feature [P]; Kyoo built-in [P]; Jellyfin media segments |
| Remote | Works away from home with no router configuration | Plex's paid feature; Jellyfin's gap |
| Clients | Web, one TV ecosystem, mobile | §7.4 |
| Ops | Docker image, single binary, backups, safe upgrades, **import from Jellyfin/Plex (users, watch state)** | ferrofin adopts the Jellyfin DB; remux imports users [P] |

### 8.2 Real differentiators (where flux can be measurably better) [I]
1. **Identity-based private remote access.** Your Tailscale login is your flux account. No port forwarding, no reverse proxy, no Plex-style account server, no passwords to breach (contrast Plex 2025 [S]). Direct WireGuard paths, no relay paywall. Built on Truffle's login-scoped nodes (RFC 025 implemented 2026-09-16: login allow-lists and close code `4004 login refused`; `truffle-rfc025/docs/rfcs/025-login-scoped-nodes.md:3,44-52`) [P].
2. **Scan engine.** Watcher-driven incremental indexing (Mille), new file to browsable in seconds, no-op rescans in seconds, and browsing that stays responsive while scanning. This directly addresses Jellyfin #15070, #14386 and #15101.
3. **Playback latency.** Time-to-first-frame and seek, measured and published per client.
4. **Explainable playback planner.** "Why is this transcoding?" shown to users, for example "Roku: AAC 5.1 unsupported → audio-only transcode". Jellyfin users debug this through forum threads today [S].
5. **Footprint.** Tens of MiB idle on a NAS or Pi. Rust peers prove it is achievable (61 MiB [P-self-reported]).
6. **Metadata for the long tail.** AI-assisted matching for messy names (Kyoo's "bad naming is a bug" ethos), plus **clearly labeled** generated posters when no artwork exists. Must clear TMDB terms (§9.4).
7. **Design.** A calm, library-first UI with no Discover/FAST content and no ads, avoiding Plex's 2025–26 navigation backlash.
8. **Trust posture.** No telemetry by default, no ads, open governance, no CLA.

### 8.3 Explicit non-goals for v1 [I]
Live TV/DVR (Channels and Plex own this), music (Navidrome and Plexamp), audiobooks (Audiobookshelf), photos (Immich), a plugin marketplace (ferrofin and Jellyfin cover this; Kyoo deliberately has none), Stremio-style add-ons and torrents (legal and app-store risk), and Roku/Tizen/webOS remote.

---

## 9. Licensing analysis

### 9.1 Facts
| Component | License | Source |
|---|---|---|
| Truffle | MIT | `truffle/LICENSE`; `Cargo.toml:26` (`license = "MIT"`) [P] |
| Truffle's dependency policy | Allows MIT, Apache-2.0, BSD-2/3, ISC, MPL-2.0, Zlib and others; **no GPL** | `truffle/deny.toml:17-35` [P] |
| Tailscale (tsnet, used via Truffle's Go sidecar `tailscale.com v1.104.1`) | BSD-3-Clause | `truffle/packages/sidecar-slim/go.mod:7`; GitHub API [P] |
| tailscale-rs | BSD-3-Clause | GitHub API [P] |
| Mille | `MIT OR Apache-2.0` (crates); MIT (npm packages) | `mille/Cargo.toml:14`; `packages/*/package.json` [P] |
| Jellyfin server | GPL-2.0 | GitHub API [P] |
| jellyfin-ffmpeg | Configured `--enable-gpl --enable-version3`, so the **binary is GPLv3** (`LICENSE_FILE="COPYING.GPLv3"`) | `jellyfin/jellyfin-ffmpeg` `builder/variants/defaults-gpl.sh` [P] |
| FFmpeg (upstream) | "Licensed under the **LGPL v2.1 or later**." Optional parts are GPL v2+, and if used "the GPL applies to all of FFmpeg". LGPL-compliance checklist: no `--enable-gpl` or `--enable-nonfree`, **dynamic linking**, ship matching source plus configure line, attribution, and "make sure your program is not using any GPL libraries (notably libx264)". `--enable-nonfree` makes the result unredistributable. | [ffmpeg.org/legal.html](https://www.ffmpeg.org/legal.html) [P]; libx265 needs `--enable-gpl` per FFmpeg docs [S] |
| Competitor licenses | Immich AGPL-3.0; Dim AGPL-3.0; remux AGPL-3.0; ferrofin GPL-3.0-only; Kyoo GPL-3.0; Navidrome GPL-3.0; Audiobookshelf GPL-3.0; Swiftfin and Streamyfin MPL-2.0; stremio-core MIT; seerr MIT | GitHub API [P] |

### 9.2 Bundled vs invoked FFmpeg
- **Invoking `ffmpeg`/`ffprobe` as a separate process** (exec, CLI arguments, pipes, sockets) is how Jellyfin, Plex, Emby and Channels all work. Under the FSF's own GPL FAQ, programs communicating through these mechanisms are normally separate works ("mere aggregation"), unless the coupling is intimate (shared complex internal data structures) ([GPL FAQ #MereAggregation](https://www.gnu.org/licenses/gpl-faq.html#MereAggregation); paraphrased because gnu.org returned 429) [S/known text]. **flux can therefore keep a permissive or AGPL license while shipping a GPL FFmpeg binary next to it.** Obligations then attach only to the FFmpeg binary: publish its exact source and configure line, and include its license.
- **Linking libav\* in-process** (for example via `ffmpeg-next` or `rsmpeg` for faster probing): with an **LGPL build, dynamically linked**, flux can stay under any license. Static linking requires providing relinkable objects. Linking a **GPL build** (x264/x265 enabled) makes the distributed flux binary a combined GPL work [P, ffmpeg legal checklist; I].
- **Recommendation [I]:** Keep FFmpeg out of process (it also isolates crashes, which matches the "supervised FFmpeg workers" design). Ship **two** FFmpeg profiles: an LGPL build (hardware encoders: VideoToolbox, VAAPI/QSV, NVENC through ffnvcodec headers) as the default, plus an optional GPL build for software x264/x265. Do pure-Rust probing in process where possible. Never ship `--enable-nonfree` (for example fdk-aac). Codec patent pools (H.264/HEVC/AAC) affect binary distribution in some jurisdictions. Most OSS media servers accept this risk, but note it [I; medium confidence].

### 9.3 License options for flux [I]
| Option | Pros | Cons | Precedent |
|---|---|---|---|
| **MIT OR Apache-2.0 (everything)** | Matches Truffle and Mille and the Rust norm. Apache adds a patent grant. NAS vendors and commercial products can bundle it, which maximizes reach. | Anyone can ship a closed fork or a closed hosted "flux cloud". No reciprocity. | stremio-core, seerr |
| **MPL-2.0** | File-level copyleft. App-Store-compatible (used by Swiftfin and Streamyfin). Can be combined with proprietary code. | Weaker protection for a server run as SaaS. | Swiftfin, Streamyfin |
| **GPL-3.0** | Reciprocity for distributed forks. Community trust. | Does not cover network use. **Apple App Store terms conflict with GPL** ([FSF, 2010](https://www.fsf.org/blogs/licensing/more-about-the-app-store-gpl-enforcement); GNU Go was pulled) [P]. Blocks bundling into proprietary NAS software. | Jellyfin (GPL-2.0), Navidrome, ABS |
| **AGPL-3.0** | Reciprocity also for hosted or network forks, which blocks a closed "flux cloud" by others. Immich shows AGPL does not hurt self-hosted adoption (116k★). | Some companies ban AGPL. Same App Store problem for client apps. Embedding flux in other products is harder. | Immich, Dim, remux |

**Recommended structure [I]:**
- **Engine crates** (scanner/indexer, probe, playback planner, HLS segmenter, Truffle/Mille glue): **`MIT OR Apache-2.0`**. They can flow back into Mille and Truffle and attract outside contributors. Performance work becomes reusable infrastructure.
- **flux server application** (DB schema, API, admin UI, AI agents): **AGPL-3.0-or-later**. This protects the product against closed hosted forks and is accepted by the target audience (Immich).
- **Client apps** (tvOS/iOS/Android): **MPL-2.0** (App Store compatible, the Swiftfin precedent) or MIT.
- **DCO, no CLA.** This avoids Emby-style distrust and makes the open license a credible commitment. Trade-off: no future dual-licensing.
- **If the maintainer's priority is NAS-vendor bundling or commercial embedding over reciprocity, use Apache-2.0 for everything.** Decide before the first external PR, because relicensing contributed code later requires every contributor's consent.
- **Jellyfin-API compatibility**: implement from the published OpenAPI spec and SDK docs. **Do not port Jellyfin's GPL C# code**, which would make flux a derivative work. Do not bundle `jellyfin-web` (GPL-2.0) in flux's own artifacts. ferrofin bundles it in its Docker image, which is aggregation, but it is still the wrong look for flux [I].

### 9.4 Metadata licensing tripwires (relevant to positioning)
- **TMDB API Terms** (last updated 2023-10-20), verified by downloading the page [P]:
  - §1.A reserves "the right … to use the TMDB APIs or TMDB Content in connection with, or for training, a machine learning or artificial intelligence based Application."
  - §1.C prohibits: "Use the TMDB APIs or TMDB Content in connection with, including for training, a machine learning (ML) or artificial intelligence (AI) based Application."
  - §2.A treats LLM/chatbot "interactive query-response systems" and "Training or validating a machine learning or artificial intelligence system … using TMDB content" as **commercial use**.
  - Data may not be cached for longer than 6 months. TMDB attribution and logo are required.
  - **Implication [I]:** An "AI metadata agent" that reads or writes TMDB content is plausibly prohibited without a written agreement, even in a free OSS app. Options: (a) ask TMDB for written permission for a local, user-run agent; (b) architect the LLM to see only local signals (filename, folder, embedded tags, audio/visual features) and emit a *query*, with deterministic code fetching TMDB data that never re-enters the model; (c) prefer CC0 sources (Wikidata) for AI-touched fields. **Flag for the metadata research track and for legal review.**
- **TheTVDB v4**: a negotiated license, or a "user-supported key" where each user buys a PIN for about $11.99/yr ([thetvdb.com/subscribe](https://thetvdb.com/subscribe), [v4-api README](https://github.com/thetvdb/v4-api)) [S]. That adds friction for a free OSS default.

---

## 10. Positioning

### 10.1 Target users (in priority order) [I]
1. **Plex refugees with remote family viewers.** They own a library, share it with 2–6 people in other homes, and are angry about Remote Watch Pass and Plex Pass costs (§2). Many already use Infuse on Apple TV.
2. **Jellyfin power users with large libraries** who have suffered through 10.11/12.0 scans, locks, migrations and memory (§3.2). They care about numbers.
3. **Apple-centric and quality-sensitive households** who want fast, correct direct play (DV, Atmos, PGS) and zero fiddling.

### 10.2 Positioning statement
> **For people who share a personal movie and TV library with a few households, flux is the open-source media server that makes your library instant and private everywhere. Sign in with your Tailscale account and your family can stream from anywhere: no port forwarding, no relay paywall, no passwords to leak. Its Rust engine scans and starts playback faster than Plex or Jellyfin on the same hardware, using a fraction of the memory. Unlike Plex, flux never charges to watch your own files and never mixes ads or "discover" content into your library. Unlike Jellyfin, it needs no reverse proxy, and it stays fast and responsive at 100k-item scale.**

Short version: **"Your library, instantly, anywhere you're signed in. Free and open, no ports, no paywall."**

### 10.3 Messaging guardrails [I]
- Lead with the **experience** (remote just works; play starts instantly) and back it with **published numbers**. Do not lead with "Rust".
- Every performance claim names the competitor version, the hardware and the corpus, and links raw data. ferrofin's README (median of 3 runs, pinned cores, explicit caveats) is a good template [P].
- Be honest about v1 limits: no Roku/Tizen/webOS remote, video only.

---

## 11. The five measurable claims flux should aim to prove

All claims are measured by a public, scripted harness (`flux-bench`) against **Jellyfin 12.2+ and current Plex Media Server**, on **two reference rigs**: (A) the maintainer's M1 Max with libraries on an external SSD/HDD, and (B) a NAS-class box (for example Intel N100, 16 GB RAM, HDD over SMB). The **fixed corpus** has 10,000 video files: 2,000 movies plus 8,000 episodes, mixed naming quality, 10% 4K HDR/DV, embedded and external subtitles. A synthetic 100k-file variant is used for scale. Medians of ≥3 runs, with p95 reported. Targets are [I]. Baselines cited are the best currently public.

| # | Claim | Metric and target | Why this target (baseline) |
|---|---|---|---|
| **1** | **Scan: "Add a 10k-file library and browse it in under a minute; rescans are free."** | (a) Cold full scan (walk + probe + DB commit, network metadata excluded) of 10k files **≥10× faster than Jellyfin 12.x** on rig B. (b) **No-op rescan ≤1 s (local SSD) and ≤10 s (SMB)** for 10k files. (c) New file to browsable item **≤5 s** via the watcher. (d) API p95 **≤50 ms during a full scan**. | Jellyfin 10.11 no-op rescans of 30–40k files took about 30 min to 2 h; 12.0 still took 3 min for 19k photos (7.2× slower than 10.10.7); scans cause thread starvation and lock errors [P, §3.2]. Plex has no public numbers, so measure it. |
| **2** | **Playback latency: "Play starts before you can blink."** | **Time-to-first-frame (direct play) p50 ≤300 ms on LAN and ≤800 ms over a direct Tailscale path**; first HLS segment for a hardware transcode **≤1.5 s**; **seek-to-frame p95 ≤500 ms** (direct) and **≤1.5 s** (transcode). Measured on web and Apple TV against Plex and Jellyfin on the same files. | No competitor publishes these numbers. ferrofin measures only server TTFB (Jellyfin 12.0-rc7 4.61 ms, ferrofin 0.48 ms) [P]. That is the gap to own. |
| **3** | **Footprint: "Runs on a Pi; idles in tens of megabytes."** | With the 10k-item library: **idle RSS ≤50 MiB**, **peak during full scan ≤300 MiB** (excluding FFmpeg children), **cold start to serving home ≤200 ms**, and at most N concurrent probes (bounded memory). | Jellyfin 12.0-rc7 idles at 409 MiB, peaks at 588 MiB and cold-starts in 2.7 s. ferrofin shows 61 MiB / 155 MiB / 174 ms is achievable in Rust [P-self-reported]. Jellyfin issue #11588 shows ffprobe fan-out reaching about 700 MB per process [P]. |
| **4** | **Remote: "Invite family in under three minutes; zero open ports."** | **0 inbound ports** opened on the router. A non-technical invitee goes from invite link to playing on an iPhone in **≤3 min median** (usability test, n ≥5). **≥90% of remote sessions on direct (non-DERP) paths** with sustained **≥25 Mbps** to a remote Apple TV. Account sign-in by Tailscale identity (no flux password). | Plex charges $2.99/mo per remote viewer or requires Plex Pass [P]. 71% of self-hosters already run VPNs and 67% reverse proxies, which is complexity flux removes [P]. |
| **5** | **Metadata: "Messy folders just work, and nothing is left without art."** | On a 1,000-file "hard names" benchmark (anime, foreign titles, scene names, mixed folders): **≥98% correct auto-match** (vs Jellyfin and Plex measured), and **100% of items with artwork**, with any AI-generated poster labeled and replaceable. Executed within TMDB/TVDB terms (§9.4). | Kyoo frames bad naming as a bug [P]. Mis-matches and missing art are a classic Plex/Jellyfin chore. This is flux's AI pillar, provided the licensing holds. |

---

## 12. Recommendations for flux

| Priority | Recommendation | Rationale |
|---|---|---|
| **P0** | **Position on "private remote + instant + lean", not "Rust Jellyfin".** | ferrofin and remux already offer Rust plus the Jellyfin API with strong memory and latency numbers (§4). Plex's monetization of remote playback is the market opening (§2). |
| **P0** | **Build `flux-bench` first and publish baselines for Jellyfin 12.2 and Plex PMS** on the fixed corpus before optimizing (§11). | Credibility; avoids comparing against a straw man (10.11); turns the maintainer's performance focus into marketing. |
| **P0** | **Ship a minimal, clean-room Jellyfin-API compatibility profile** targeting Infuse, Swiftfin (tvOS), Wholphin and official Android TV, Findroid and Streamyfin. Version it behind an adapter. | Navidrome's Subsonic lesson; remux and ferrofin prove client compatibility. Gives an Apple TV and Android TV client on day one (§5.6, §7.4). |
| **P0** | **First TV targets: Apple TV, then Android TV/Google TV/Fire OS.** Treat Roku, Tizen and webOS as LAN-only in v1. | Tailscale platform availability (§7.3). Enthusiast density. Best native players. |
| **P0** | **Resolve the TMDB "AI-based Application" clause before shipping AI agents that touch TMDB data** (ask TMDB in writing; design the LLM path to exclude TMDB content). | Verified prohibition text (§9.4). Avoids a takedown of the core differentiator. |
| **P0** | **Decide licenses before the first external contribution**: engine crates `MIT OR Apache-2.0`, server AGPL-3.0-or-later, clients MPL-2.0; DCO, no CLA. Run FFmpeg out of process; default to an LGPL build. | §9.2–9.3; avoids future relicensing impossibility and Emby-style distrust. |
| **P1** | **Migration importers** for Jellyfin (read `jellyfin.db`: users, watched/resume) and Plex (API/DB). Make "switch in 10 minutes, keep your history" a headline. | Users will not abandon watch history. ferrofin adopts the Jellyfin DB and remux imports users (§4). |
| **P1** | **Close Truffle gaps for the remote story**: a tvOS embed (Package.swift is macOS/iOS only), invite and onboarding flows that work within Tailscale's free 6-user plan (and verify how node-shared outside users count), and a watch on `tailscale-rs` maturity to retire the Go sidecar. | §7.3; flux's top differentiator depends on Truffle on Apple TV. |
| **P1** | **Playback planner explainability in the UI**: show why a stream transcodes and what would make it direct play. | A recurring Jellyfin pain on Roku and webOS (§3.2). Cheap and visible. |
| **P1** | **Trust charter**: no telemetry by default, no ads, no Discover content, no cloud account, a published security policy. | Directly contrasts Plex 2023–2026 (§2.2). |
| **P1** | **Single binary plus Docker plus macOS/Windows tray app.** | 69% of self-hosters prefer containers (§7.1). The tray app reaches Plex's non-technical users (remux precedent). |
| **P2** | **AI-generated posters**: label as generated, keep them local and separate from fetched art, use local-model options, never upload. | Keeps trust and avoids passing generated art off as official. |
| **P2** | **Multi-server federation over Truffle** (Jellyswarrm shows demand, 934★). | A natural Truffle-only differentiator after v1. |
| **P2** | **Stay video-only in v1.** Integrate rather than compete with Navidrome, Audiobookshelf and Immich. | Focus. Those projects own their verticals (§6). |

---

## 13. Open questions

1. **Does Plex now classify Tailscale clients as remote?** 2026 reports say the exit-node trick stopped working [S]. Confirm on a test account; it matters for messaging ("Plex blocks Tailscale; flux is built on it").
2. **Tailscale sharing semantics for flux households.** Does a viewer from another tailnet (node sharing) count toward the 6-user free limit? Is node sharing included in Personal? Is "Declarative Node Sharing" (alpha, 2026-08-31) the right primitive? ([Tailscale blog](https://tailscale.com/blog/easier-building-with-tailscale)) [P].
3. **Can libtailscale/TailscaleKit (or tailscale-rs) run inside a tvOS app** without the Tailscale app and without a Network Extension? If not, Apple TV viewers need the Tailscale tvOS app (tvOS 17+, home-hub requirement) [P].
4. **TMDB's interpretation** of "in connection with … an AI based Application" for a free, local, user-run agent.
5. **Plex PMS scan, TTFF and memory baselines**: no public measurements found. Must be measured.
6. **Jellyfin 12.2 real numbers on our corpus**: has 12.x fixed the movie/TV scan regression, or only photos (#15070 vs #14386)?
7. **How much Jellyfin API to implement**, given 12.0's breaking changes and the maintenance cost? Which exact endpoints do Infuse and Swiftfin use? (A traffic capture against a real Jellyfin 12.2 would answer this.)
8. **Fire TV Vega OS**: what share of new Fire TV devices can't run Android APKs or Tailscale?
9. **Plex Pass pre-2025 prices**: $4.99/$39.99 vs $5.99/$49.99. Sources conflict [U]; only the new prices are confirmed.
10. **License final call** (AGPL server vs Apache everything): depends on the maintainer's commercial plans (hosted relays? AI services? NAS partnerships?).

---

## Appendix A. Source index (primary first)

**Plex:** [Roku remote change (forum, 2025-11-19)](https://forums.plex.tv/t/changes-coming-to-remote-streaming-on-roku/933671/1) · [Smart TV/console remote change (forum, 2026-03-11)](https://forums.plex.tv/t/changes-coming-to-remote-streaming-on-smart-tvs/937015) · [Lifetime pricing (forum, 2026-05-18)](https://forums.plex.tv/t/new-lifetime-plex-pass-pricing/938910) · [plex.tv/plans](https://www.plex.tv/plans/) · [Week in Review thread](https://forums.plex.tv/t/weekly-review-emails-data-leak/860206) · [9to5Mac 2025 pricing](https://9to5mac.com/2025/03/19/plex-price-increase-remote-streaming-changes/) · [AlternativeTo 2025 pricing](https://alternativeto.net/news/2025/3/plex-raises-subscription-prices-up-to-75-and-locks-remote-streaming-behind-a-paywall) · [BleepingComputer breach](https://bleepingcomputer.com/news/security/plex-tells-users-to-reset-passwords-after-new-data-breach) · [PiunikaWeb Fire TV redesign](https://piunikaweb.com/2026/05/04/plex-fire-tv-redesign-unusable-users-slam-navigation/) · [The Desk social features](https://thedesk.net/2026/06/plex-launches-social-discovery-features/) · [fullmetalbrackets Tailscale workaround](https://fullmetalbrackets.com/blog/plex-remote-access-tailscale) · [Firecore Plex notice](https://community.firecore.com/t/changes-to-plex-remote-streaming-requirements/60825)

**Jellyfin:** [10.11.0 release](https://jellyfin.org/posts/jellyfin-release-10.11.0) · [12.0 release](https://jellyfin.org/posts/jellyfin-release-12.0/) · [SQLite locking post](https://jellyfin.org/posts/SQLite-locking/) · [About/history](https://jellyfin.org/docs/general/about/) · [Clients](https://jellyfin.org/downloads/clients/) · Issues [#15070](https://github.com/jellyfin/jellyfin/issues/15070), [#14386](https://github.com/jellyfin/jellyfin/issues/14386), [#15101](https://github.com/jellyfin/jellyfin/issues/15101), [#11588](https://github.com/jellyfin/jellyfin/issues/11588), [#15027](https://github.com/jellyfin/jellyfin/issues/15027), [#15090](https://github.com/jellyfin/jellyfin/issues/15090), [#17871](https://github.com/jellyfin/jellyfin/issues/17871), [#17934](https://github.com/jellyfin/jellyfin/issues/17934), [#17860](https://github.com/jellyfin/jellyfin/issues/17860), [#18088](https://github.com/jellyfin/jellyfin/issues/18088), [#18066](https://github.com/jellyfin/jellyfin/issues/18066) · [jellyfin-ffmpeg GPL variant](https://github.com/jellyfin/jellyfin-ffmpeg/blob/jellyfin/builder/variants/defaults-gpl.sh) · [Swiftfin tvOS status #1294](https://github.com/jellyfin/Swiftfin/discussions/1294) · Forum: [scans take days](https://forum.jellyfin.org/t-post-10-11-x-upgrade-library-scans-take-days-now-instead-of-minutes), [scan never finishes](https://forum.jellyfin.org/t-jellyfin-scan-never-finishes-in-10-11-00), [Roku direct play](https://forum.jellyfin.org/t-roku-app-direct-play-troubleshooting)

**Other competitors:** [Emby Premiere](https://emby.media/premiere.html) · [Emby releases](https://github.com/MediaBrowser/Emby.Releases/releases) · [Kodi releases](https://github.com/xbmc/xbmc/releases) · [Infuse](https://firecore.com/infuse) · [Channels DVR server](https://getchannels.com/dvr-server/) · [Stremio](https://www.stremio.com/) · [ferrofin](https://github.com/mangoleaf/ferrofin) · [remux](https://github.com/lostb1t/remux) · [Kyoo](https://github.com/zoriya/Kyoo) · [Dim](https://github.com/Dusk-Labs/dim) · [Jellyswarrm](https://github.com/LLukas22/Jellyswarrm)

**Adjacent:** [Immich v2.0.0](https://immich.app/blog/v2.0.0-release) · [Immich/FUTO (Linuxiac)](https://linuxiac.com/immich-team-goes-full-time/) · [Navidrome overview](https://www.navidrome.org/docs/overview/) · [Audiobookshelf](https://github.com/advplyr/audiobookshelf)

**Demand and platforms:** [selfh.st 2025 results](https://selfh.st/survey/2025-results/) and [raw JSON](https://github.com/selfhst/cdn/tree/main/assets/surveys/annual) · [Parks Associates via Señal News](https://senalnews.com/en/data/61-of-us-internet-households-use-their-smart-tv-as-the-primary-streaming-device) · [Pixalate Q1 2026](https://www.pixalate.com/blog/pixalates-q1-2026-connected-tv-ctv-device-market-share-report) · [Tailscale install](https://tailscale.com/docs/install) · [Tailscale on Apple TV](https://tailscale.com/docs/install/appletv.md) · [Tailscale pricing](https://tailscale.com/pricing) · [Tailscale "easier building" (tailscale-rs)](https://tailscale.com/blog/easier-building-with-tailscale) · [tailscale/tailscale-rs](https://github.com/tailscale/tailscale-rs)

**Licensing:** [FFmpeg legal](https://www.ffmpeg.org/legal.html) · [GPL FAQ: aggregation](https://www.gnu.org/licenses/gpl-faq.html#MereAggregation) · [FSF on App Store/GPL](https://www.fsf.org/blogs/licensing/more-about-the-app-store-gpl-enforcement) · [TMDB API terms](https://www.themoviedb.org/api-terms-of-use) · [TheTVDB subscribe](https://thetvdb.com/subscribe) · [TheTVDB v4 API](https://github.com/thetvdb/v4-api)

**Local code:** `truffle/Cargo.toml:26`, `truffle/deny.toml:17-35`, `truffle/Package.swift:38-41,67`, `truffle/packages/sidecar-slim/go.mod:7`, `truffle/README.md` ("Requires Tailscale installed and authenticated"), `truffle-rfc025/docs/rfcs/025-login-scoped-nodes.md:3,44-52`, `truffle-rfc025/docs/rfcs/024-truffle-swift.md:47,79`, `mille/Cargo.toml:14`.
