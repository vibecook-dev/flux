# Phase D review: findings and their status

*2026-10-20.* An adversarial review of `main` at `1fbcb63` verified **37 defects** in four areas: security (7), robustness (7), playback (14) and integrity (9). Each was reproduced or proven from the code by a reviewer, then re-checked by a verifier, who also found new ones (the `-V`/`-NEW-` ids and SEC-5..7). Refuted findings are left out. The raw record (triggers, logs, repro commands, both reviewers' notes) is `$FLUX_WORK/review/findings-d1.md`; the reviewers' repro code is in the detached worktrees `$FLUX_WORK/wt/review-{security,robust,playback,integrity}`.

Phase D fixed them on five branches (`phase-d/{scancat,hls-correct,hls-perf,server}` and the web agent's commits on `main`) plus Mille's and Truffle's `flux/dogfood-d`. The merge that brought them together, and the seams it wired, are in [fixes.md](fixes.md).

**Status on `main` after the merge: all 37 are fixed.** Each has a regression test that fails without its fix (checked by each fix agent with the fix reverted). Several have a known gap, listed with the finding. The status below is the merged one: where a branch reported "partly" because the rest belonged to another component, the merge wired the rest (ROB-1, PB-7, PB-11, SEC-1/SEC-7's web half).

## Status

| ID | Severity | Finding | Status | Fixed in | Regression test (main) |
|---|---|---|---|---|---|
| SEC-1 | critical | No Host/Origin check; localhost = Owner, so DNS rebinding gives a web page the Owner API | fixed | server, web | `flux-server/tests/guard.rs::rebinding_hosts_get_421_before_any_handler`, `tests/socket.rs::real_socket_refuses_foreign_hosts` |
| SEC-2 | medium | A symlink under a root serves any file the server can read | fixed | scancat, server, merge | `flux-server/tests/play.rs::files_outside_the_library_are_not_served`, `flux-scan/tests/integrity.rs::sec2_links_leaving_the_roots_follow_the_policy` |
| SEC-3 | medium | No connection or SSE caps: a few hundred idle sockets wedge the server | fixed | server (flux-net) | `flux-net/tests/serving.rs::connections_over_the_per_ip_limit_are_refused_at_once`, `flux-server/tests/guard.rs::event_streams_are_capped_per_caller_and_in_total` |
| SEC-4 | medium | `POST /play` with varied caps grows `<data>/plans` without bound | fixed | server | `flux-server/tests/play.rs::varied_caps_do_not_mint_a_plan_per_request` |
| SEC-5 | medium | Forged session ids on a GET add sessions without limit (O(n) under a global lock) | fixed | server | `flux-server/tests/play.rs::forged_session_ids_create_no_sessions` |
| SEC-6 | medium | No write timeout: a client that stops reading holds ~2 MiB and a connection | fixed | server (flux-net) | `flux-net/tests/serving.rs::a_client_that_stops_reading_is_dropped_after_the_write_timeout` |
| SEC-7 | low | Body-less cross-site `POST …/scan` runs an Owner-only scan | fixed | server, web | `flux-server/tests/guard.rs::cross_site_writes_are_refused` |
| ROB-1 | high | Play-time ffprobe index scan has no timeout or kill; wedges `/play` and shutdown | fixed | hls-correct, merge | `flux-hls/tests/phase_d.rs::hung_ffprobe_index_scan_is_killed`, `flux-hls/tests/merge_seams.rs::engine_shutdown_kills_a_hung_index_scan` |
| ROB-2 | high | MP4 `stsz`/`ctts` counts trusted: a 1.6 MB file allocates GBs | fixed | hls-correct | `flux-hls/tests/phase_d.rs::mp4_sample_table_bombs` |
| ROB-3 | medium | The scan's ffprobe fallback ignores the server's ffprobe | fixed | scancat, merge | `flux-scan/tests/integrity.rs::rob3_the_fallback_runs_the_configured_ffprobe`, `flux-server/tests/seams.rs::the_scan_fallback_runs_the_servers_ffprobe` |
| ROB-4 | low | Shutdown overruns its grace on a hung scan-time ffprobe | fixed | scancat | `flux-scan/tests/integrity.rs::rob4_shutdown_kills_a_hung_ffprobe_within_the_grace` |
| ROB-5 | low | Truncated MP4: 200 with a Content-Length it can't fulfil | fixed | hls-correct, merge | `flux-hls/tests/phase_d.rs::truncated_mp4_fails_before_the_response` |
| ROB-V1 | medium | A failed probe is never retried | fixed | scancat | `flux-scan/tests/integrity.rs::robv1_a_failed_fallback_heals_when_ffprobe_works`, `tests/scan.rs::deep_scan_selects_outdated_versions_and_unknown_library_errors` |
| ROB-V2 | medium | A corrupt `moof` size makes `/play` allocate ~2× the file | fixed | hls-correct | `flux-hls/tests/phase_d.rs::moof_size_zero_is_rejected` |
| PB-1 | high | Matroska VFR video snapped to the DefaultDuration grid: frames collide | fixed | hls-correct | `flux-hls/tests/phase_d.rs::vfr_matroska_frames_keep_unique_timestamps` |
| PB-2 | high | Transcode of a track without DefaultDuration assumes 24 fps | fixed | hls-correct, merge | `flux-hls/tests/phase_d.rs::transcode_without_default_duration_counts_real_frames`, `merge_seams.rs::predicted_pieces_start_on_their_idr` |
| PB-3 | medium | WebVTT cached by (path, track): a replaced film keeps old subtitles | fixed | hls-perf | `flux-hls engine::tests::webvtt_cache_follows_the_file_identity` |
| PB-4 | medium | Up to 64 media files held open after playback | fixed | hls-perf | `flux-hls source::tests::deleted_file_is_closed_and_its_space_freed` |
| PB-5 | medium | Seeking away never cancels an encoder run | fixed | hls-correct | `flux-hls/tests/phase_d.rs::abandoned_seeks_cancel_their_runs` |
| PB-6 | low | `/play` warms every audio rendition | fixed | hls-perf, web | `flux-hls engine::tests::warm_produces_only_the_default_audio`, `web/src/player/audio.test.ts` |
| PB-7 | medium | Turning on a subtitle reads the whole file before the first cue | fixed | hls-correct, merge | `flux-hls/tests/phase_d.rs::subtitle_extraction_reads_little`, `merge_seams.rs::webvtt_streams_cue_by_cue_then_comes_from_the_cache`, `flux-server/tests/seams.rs::subtitle_sidecars_stream_then_come_with_a_length` |
| PB-9 | medium | Display rotation dropped: portrait video plays sideways | fixed | hls-correct | `flux-hls/tests/phase_d.rs::display_rotation_survives_copy_and_transcode` |
| PB-10 | low | Matroska BANDWIDTH counts unsent tracks | fixed | hls-correct | `flux-hls/tests/phase_d.rs::bandwidth_counts_only_sent_bytes` |
| PB-11 | low | Transcoded segments depend on request history yet get a strong ETag | fixed | hls-perf, merge | `flux-hls engine::tests::transcoded_audio_bytes_do_not_depend_on_prefetch`, `flux-server/tests/seams.rs::video_transcodes_get_weak_etags` |
| PB-12 | low | Transcode level always 4.1, even for 1080p60 | fixed | hls-correct | `flux-hls/tests/phase_d.rs::transcode_level_matches_size_and_rate` |
| PB-NEW-1 | medium | Transcode starts at the wrong time when start_time > 0 | fixed | hls-correct | `flux-hls/tests/phase_d.rs::transcode_honours_a_late_start_time` |
| PB-NEW-2 | low | hls-validate passes a transcode that lost 58 of 288 frames | fixed | hls-correct | `hls_validate tests::transcode_frame_count_tolerance` |
| PB-NEW-3 | low | hls-validate treats `--help` as a file and overwrites shared results | fixed | hls-correct | `hls_validate tests::help_and_unknown_flags`, `shared/cli.rs tests::results_are_never_overwritten_silently` |
| INT-1 | high | A title-changing rename orphans watch state, resume, pins and match | fixed | scancat | `flux-scan/tests/integrity.rs::int1_a_title_fixing_rename_keeps_watch_state_and_the_pin`, `flux-catalog/tests/phase_d.rs::int1_*` |
| INT-2 | medium | A partly unavailable root (unmounted sub-volume, dangling links) is tombstoned | fixed | scancat | `flux-scan/tests/integrity.rs::int2_a_dangling_file_link_is_unread_not_missing` (+3) |
| INT-3 | medium | "Scan now" during a running scan loses changes in walked folders | fixed | scancat | `flux-scan/tests/integrity.rs::int3_scan_now_during_a_running_scan_sees_changes_in_listed_dirs` |
| INT-4 | medium | A stale snapshot tombstones a file a watcher batch just moved | fixed | scancat | `flux-scan/tests/integrity.rs::int4_a_stale_snapshot_does_not_tombstone_a_concurrently_moved_file` |
| INT-5 | medium | A folder symlink inside a root is walked depending on listing order | fixed | Mille, scancat | `mille-core scan::tests::smart_link_decisions_do_not_depend_on_which_listing_answers_first`, `flux-scan/tests/integrity.rs::int5_an_in_root_folder_alias_is_never_walked` |
| INT-6 | low | A transient open failure wipes playback facts and is never retried | fixed | scancat | `flux-scan/tests/integrity.rs::int6_a_transient_open_failure_is_retried` |
| INT-7 | low | Overlapping roots in one library catalog files twice | fixed | scancat, merge | `flux-catalog/tests/phase_d.rs::int7_overlapping_roots_in_one_library_are_refused`, `flux-server/tests/seams.rs::overlapping_roots_are_a_bad_request` |
| INT-8 | low | `merge_items` unpins the user's art when the target has no pin | fixed | scancat | `flux-catalog/tests/phase_d.rs::int8_merge_keeps_the_users_pin_when_the_target_has_none` |
| INT-V1 | low | Edited in place, then renamed: stale probe facts for good | fixed | scancat | `flux-scan/tests/integrity.rs::intv1_an_edited_then_renamed_file_is_reprobed` |

Paths are relative to `crates/` unless they start with `web/`. "merge" means the Phase D merge finished or wired the fix ([fixes.md](fixes.md) §2).

## Security

### SEC-1 (critical): DNS rebinding reaches the Owner API

- **What was wrong.** flux-net gives every loopback caller the Owner identity, and nothing checked `Host`, `Origin` or `Sec-Fetch-*`. A page that rebinds its own hostname to 127.0.0.1 is same-origin for the browser, so `fetch('http://attacker.example:8096/api/v1/libraries', {method: 'POST', …})` created an Owner-only library rooted anywhere. The verifier traced it further: `POST /tailnet/connect {"mode":"embedded"}` then hands the attacker a login URL, and the attacker's tailnet becomes the owner. Proven with curl: `Host: evil.example:18421` → 200 `"role":"owner"`.
- **Fix.** One axum layer (`flux-server/src/guard.rs`) wraps every route, the media routes and the web fallback, so it covers every listener. A `Host` that isn't one of the server's names gets **421** before any handler runs. Allowed: loopback names and addresses, the host name and `<host>.local`, interface addresses (re-read at most every 5 s on a miss), the MagicDNS name and its IPs while the tailnet runs, and `--allow-host`/`FLUX_ALLOW_HOSTS`. A request with no `Host` passes (browsers always send one). `--localhost-role` (including `none`) and `--owner-login` make the documented multi-user mitigation configurable. The web client sends `X-Flux-Request: 1` on every `/api` call and explains 421/403 to the user.
- **Merge smoke (curl).** Forged `Host` → 421 on GET and on the `POST /libraries` repro, and no library is created. `Host: localhost:PORT` and `[::1]:PORT` → 200.

### SEC-2 (medium): a symlink under a root escapes it

- **What was wrong.** The scanner follows file links, and `/media/{asset}/file` opened the stored path without containment, so `Leak (2021).mkv -> ~/.ssh/id_ed25519` was served verbatim to any caller. The verifier showed a scan-time check isn't enough (re-point the link after the scan), and that refusing every target outside the roots would break real setups (rclone, debrid, *arr layouts).
- **Fix, scan side (scancat).** Links inside the library's roots are aliases and never followed. Links leaving them are followed once per target, and a *new* file reached that way is cataloged only if it probes as media (the ssh key is not). `AssetLocation` now carries the identity the scan recorded (`dev`, `ino`, `mtime_ns`, `root_path`).
- **Fix, serve side (server + merge).** The server branch made serving strictly in-root (open the file, then canonicalize, check containment, and compare the opened fd's `(dev, ino)` with the resolved path). That contradicted the scan's supported external links, so the merge combined the two: a file inside the library is served as before; a file outside it is served only when the opened file is exactly the one the scan recorded (inode, size, nanosecond mtime; the device is left out because a remounted NAS gets a new number). A link re-pointed after the scan names another file and gets 404.
- **Gap.** HLS checks when the prepared asset is (re)loaded, not per segment (the 10 s size/mtime re-check catches a changed file). A file swapped in place with the same inode, size and mtime is served; that requires write access to the target, which reads nothing new.

### SEC-3 (medium): no connection or SSE caps

- **What was wrong.** No admission control in `accept_loop`, unlimited SSE streams, and no `RLIMIT_NOFILE` raise. Under the macOS GUI soft limit (256), 245 SSE connections from one client made every request fail with ECONNRESET, and the outage outlasted the attack by ~21 s.
- **Fix.** flux-net admits connections at accept time: 512 in total from non-loopback addresses, 64 per IP, loopback with its own budget; over a limit, an immediate 503 and close. `RLIMIT_NOFILE` is raised at startup (macOS: capped by `kern.maxfilesperproc`) and the limits shrink to fit. Event streams: 32 in total (503 beyond), 8 per caller, where a ninth ends that caller's oldest.
- **Measured (server agent, `ulimit -n 256`).** 245 SSE connections → 31 streams and 214 immediate 503s, 54 server fds, no EMFILE, and a new request succeeds at once after the flood.
- **Gap.** A local flood can use up loopback's 64 slots for up to the 30 s header timeout. Streams of vanished clients count until a keep-alive write fails (≤ ~30 s with `half_close`).

### SEC-4 (medium): unbounded plan files

- **Fix.** The caller's `max_bitrate` and `max_width`/`max_height` are rounded down to standard rungs before planning, so an identical recipe reuses its plan. `<data>/plans` is capped at 4,096 files (least recently used deleted first, not on memory eviction, which the verifier showed was harmful), and a used plan's mtime is refreshed hourly, so the 14-day prune removes *unused* plans.
- **Measured.** The verifier's 300-play flood: 1 plan file (was 300).

### SEC-5 (medium): unbounded sessions

- **Fix.** At most 1,024 sessions, oldest dropped first, O(1) per insert. `GET …/diagnostics` of an unknown id answers from a transient session and stores nothing; `POST …/progress` still rebuilds a session forgotten by a restart.
- **Measured.** 20,000 forged ids: RSS 26.9 → 27.0 MB (was +9 MB and climbing).
- **Gap.** Session nonces are not HMAC-authenticated; the cap bounds memory.

### SEC-6 (medium): no write timeout

- **Fix.** Every connection's IO is wrapped (`flux-net limits::WriteTimeout`): a write that makes no progress for 60 s fails, hyper ends the connection, and the body's buffers are freed. Any progress resets the timer; responses waiting on their body (SSE) aren't timed.
- **Measured.** 100 stalled readers: 32 admitted, the rest 503, the stalled ones dropped at 60 s (fds 79 → 24).
- **Gap.** A reader that keeps reading a few KB a minute is bounded only by the per-IP cap.

### SEC-7 (low): body-less cross-site POST

- **Fix.** Every unsafe `/api/` call needs `X-Flux-Request: 1` (a header a cross-site page can't send without a preflight flux never grants), and a present `Origin` must name one of the server's hosts (`null` included); otherwise 403. Roles are still enforced behind the guard. flux-core exports the header name (`api::REQUEST_HEADER`).
- **Merge smoke (curl).** The verifier's repro (`Origin: https://evil.example`, `Sec-Fetch-Site: cross-site`, no body) → 403; foreign or `null` Origin with the header → 403; the correct request → 200.

## Robustness

### ROB-1 (high): the play-time ffprobe scan can hang forever

- **What was wrong.** `external::run` used `Command::output()`: no timeout, no kill. A hung ffprobe (fake `sleep 3600`, or a stalled NAS) kept `/play` waiting, and SIGTERM never exited (Runtime drop waits for the blocking thread).
- **Fix (hls-correct).** ffprobe runs in its own process group with stdout/stderr drained on threads; it is killed after 20 s without output or after 30 s + 30 s/GB (`ScanLimits`), and `kill_running()` kills every running scan.
- **Fix (merge).** `HlsEngine::shutdown` calls `kill_running()`; `EngineConfig::index_scan_limits` carries the limits, which the server's own index build uses; `main` ends with `rt.shutdown_timeout(2 s)`.
- **Merge smoke.** Release server with a hung fake ffprobe, `/play` of an AVI in flight, then SIGTERM: exit after 5.1 s (the 5 s grace for in-flight requests), no `sleep` child left. Before: > 35 s and still running, ffprobe left behind.

### ROB-2 (high): MP4 sample-table bombs

- **Fix.** A sample count must be ≤ the file size and ≤ 32 M per track (PCM exempt, expanded per chunk), checked before any per-sample allocation; `ctts`/`stts` runs are clamped; tables are expanded once per play instead of about four times.
- **Measured.** The `stsz` bomb gives a clean error; the `ctts` bomb takes 1.7 ms (was ~5.5 s of CPU).

### ROB-3 (medium): the scan fallback ignores `--ffprobe`

- **Fix.** `ScanConfig::ffprobe` (scancat), and the server passes the ffprobe it resolved (`--ffprobe`, the config file, next to ffmpeg, Homebrew) into it (merge).
- **Gap.** `AssetInfo` still has no `probe_error`, so the API doesn't say why a file failed to probe (a flux-core DTO change, proposed). The catalog stores it and `AssetLocation` exposes it.

### ROB-4 (low): shutdown waits out a hung scan-time ffprobe

- **Fix.** `ffprobe::run` polls the job's cancel flag and kills the child. `shutdown(1 s)` returns in under 4 s with no child left (was ~20 s).

### ROB-5 (low): a truncated MP4 breaks a 200 mid-body

- **Fix.** `Asset::checked` verifies every file range against the source length when the segment is built, so a truncated file errors before any status or Content-Length. Growing files whose `moov` describes more than is present still index, and the present part plays. The server now logs a body that fails mid-stream (merge).

### ROB-V1 (medium): probe failures are permanent

- **Fix.** Catalog schema v4 stores `probe_attempts` and `probe_retry_at` (1 min doubling to 1 day). A Quick scan retries once the backoff has passed; "Scan now" (Interactive) and Deep retry at once. A failed re-probe keeps the existing facts. Deep scans now select by `probe_version`/`parse_version` instead of re-probing everything.
- **Gap.** As ROB-3: the reason isn't in the API yet.

### ROB-V2 (medium): unbounded `moof` read

- **Fix.** A `moof` with size 0 (to end of file) or over 64 MiB is rejected; fragmented tracks over the sample cap are rejected during the scan. The 128 MiB sparse repro now gives an index error.

## Playback

### PB-1 (high): VFR Matroska frames collide

- **Fix.** Matroska timestamps snap to the DefaultDuration grid only within 1 ms of it (`IndexTrack::grid_point`); an off-grid frame keeps its own time. `vfr_section.mkv`: 58 colliding PTS → 0, DTS strictly increasing, 288/288 frames.

### PB-2 (high): transcode of a track without DefaultDuration assumes 24 fps

- **Fix (hls-correct).** Such a track gets its frame rate from its own timestamps, and each transcode run takes its frame counts, trim point, forced-IDR times and sample durations from the index's real frame times (Matroska block headers, MP4 sample tables). The first sample of a piece is no longer *marked* sync when it isn't one. `vfr_hevc_short.mkv`: 230/288 → 288/288.
- **Fix (merge).** Sources whose frames only ffprobe or the TS scan know (AVI, rejected TS, in-process TS) still predict counts from the frame grid. The merge's smoke run found that a frame missing from such a source (`mpeg4_mp3.avi`, at 0.04 s) put the IDRs forced at the first run's piece boundaries one sample before pieces 1–3, so a seek into them began on a P frame (main had this too, hidden by the forced sync flag). A predicted piece now ends at the forced IDR within 3 frames of its count, and a run's last piece leaves out a frame read past the run: every piece starts on an IDR, 750/750 frames (was 751/750).
- **Gap.** Predicted pieces are still cut by count, not by the encoder's timestamps (the verifier's fuller proposal).

### PB-3 (medium): stale WebVTT after a file is replaced

- **Fix.** The subtitle cache is keyed by `(path:size:mtime, track)`, like the engine's other caches.

### PB-4 (medium): files held open after playback

- **Fix.** flux-hls `Source` opens lazily and a reaper closes its fd after 30 s idle (`EngineConfig::source_idle_timeout`), at once when the file is unlinked, and when its path names another inode (checked every 2 s). The next read reopens and refuses a changed file. The server's 64-entry asset LRU no longer pins files.

### PB-5 (medium): abandoned encoder runs keep the permits

- **Fix.** Each run has its own cancellation token and a count of waiting requests; when the last waiter goes away before its piece is ready, ffmpeg is killed and the permit freed. Requests already served, `warm()` and prefetch runs are left alone. Repro: 1 encoder run left going → 0, within 39 ms.

### PB-6 (low): `/play` warms every audio rendition

- **Fix.** `warm()` produces the video and the renditions of the first audio track only (`HlsEngine::warm_renditions`), and keeps what it produced in a 64 MB LRU that `serve()` hits. The web client switches audio inside the session (hls.js `setAudioOption`, Safari `audioTracks`) instead of POSTing `/play` again: 19 ms p50 against 173 ms for a new session (dev mock).

### PB-7 (medium): a subtitle reads the whole file first

- **Fix (hls-correct).** Subtitle blocks are read through the track's cue points (49.5 MB → 25 KB read; mkvmerge and ffmpeg both index subtitle blocks). Without cue points, a header-only walk reads 15 % of the file.
- **Fix (merge).** The engine streams the sidecar (`HlsEngine::webvtt_body`): the header at once, then each cue as its block is read, chunked; the completed document is cached and later requests get it with a length, byte for byte the same. The extraction holds a read permit like a segment read.

### PB-9 (medium): rotation dropped

- **Fix.** The `tkhd` matrix is read and written into the copy's init; transcodes use `-noautorotate` and rotate explicitly (`transpose_vt`/`transpose`), with the upright size in the init and `RESOLUTION`. Chrome and AVPlayer: 640×360 sideways → 360×640, for copy and transcode.
- **Gap.** Matroska rotation (`Projection`) isn't read.

### PB-10 (low): BANDWIDTH counts unsent tracks

- **Fix.** For Matroska the segment plan subtracts the other tracks' bitrates (codec headers, exact for PCM, else measured on the first blocks); MP4 sizes come from the sample tables. `h264_truehd71` on Chrome: 8.65 → 6.07 Mb/s signalled, against 6.00 served.

### PB-11 (low): history-dependent bytes under a strong ETag

- **Fix.** Audio transcodes are now deterministic (prefetch uses the same part partition as on-demand). Video transcodes are equivalent but not bit-exact across VideoToolbox runs, so `HlsEngine::is_deterministic` says so, and the server sends `ETag: W/"…"` for a video transcode's init and segments (merge); `If-None-Match` compares weakly.

### PB-12 (low): level 4.1 forced

- **Fix.** `h264_level(w, h, fps)` follows Table A-1 (at least 4.1), used for `-level:v` and `CODECS` alike. 1080p60: `avc1.64002a`.

### PB-NEW-1 (medium): late start time

- **Fix.** Matroska/MP4 transcodes pass `-seek_timestamp 1`. `start1_hevc.mkv`: 120/144 → 144/144 frames.

### PB-NEW-2 / PB-NEW-3 (low): hls-validate

- **Fix.** A transcode with fewer frames than the source fails; a surplus is allowed only once per real source hole. The validator decodes at 90 kHz (the null muxer's 1/fps base made VFR sources fail themselves). `--help` prints usage, unknown flags and presets are errors, results go to `bench/results/flux-hls-validate-<UTC>.json` and are never overwritten without `--force`.

## Integrity

### INT-1 (high): a title-changing rename orphans user state

- **Fix.** When a rename or re-parse leaves a work with no assets, it is carried into the work its file moved to: watch state (watched wins, newer position wins), pins, missing ids and a locked match (an automatic match is not carried: the rename may be fixing it). Same library and kind: merged like `merge_items`; another library or kind: state copied.

### INT-2 (medium): unreachable below the root treated as deleted

- **Fix.** A dangling file or folder link, and an empty directory whose cataloged files were on another device (an unmounted volume), are *unread*, never missing. A renumbered device (a remount seen elsewhere in the same walk) doesn't count.
- **Gap.** The real-mount test needs `RUN_MOUNT=1` and `hdiutil attach` (refused in the sandbox); the device rule is tested by rewriting the catalog's devices.

### INT-3 (medium): "Scan now" joins a running scan and loses changes

- **Fix.** A request that joins a running library scan sets a flag; the job then runs one follow-up Quick pass, so the request completes after a pass that started after it.

### INT-4 (medium): a stale snapshot tombstones a moved file

- **Fix.** `ScanBatch.expect` carries the snapshot row each `missing` id and move was decided from; the catalog applies them only while the row still matches.

### INT-5 (medium): folder symlinks depend on listing order

- **Fix (Mille, `flux/dogfood-d`).** Smart links are held until nothing is pending, then decided in path order against everything walked. **flux-scan** adds its own deterministic policy (above, SEC-2). The reviewer's repro: 2 writer commands per unchanged rescan and a flipping asset count → 0 writes and 1 asset.

### INT-6 (low): a transient open failure wipes facts

- **Fix.** As ROB-V1: retried with backoff, and a failed re-probe keeps the stored facts.

### INT-7 (low): overlapping roots

- **Fix.** `create_library`/`add_root` return `CatalogError::OverlappingRoots` (whole-component comparison, so `/m/Movies2` doesn't overlap `/m/Movies`); the server answers 400 with a message (merge). Different libraries may share a root.

### INT-8 (low): a merge drops the user's pin

- **Fix.** The art update in `merge_items` keeps the merged work's pin when the target has none.

### INT-V1 (low): edited in place, then renamed

- **Fix.** An inode move whose mtime differs from the snapshot row is applied as a move (identity kept) and re-probed; a stat change without a new fingerprint clears the stale one.

## What the reviewers checked and found sound

Kept from the review for the record; nothing here needed a fix.

- **Security.** Static-file path traversal, plan-id validation, HLS resource parsing, Range parsing, the React UI (text rendering, fixed link templates, strict CSP), WebVTT/ASS escaping, TMDB secret handling, admin authorization (the gap was localhost = Owner), the 30 s header timeout against slowloris, the 415 on non-JSON POSTs.
- **Robustness.** 372 malformed inputs through flux-probe and 269 through the index build (0 panics, bounded memory); 166 corrupt files served through the release server; filesystem edge cases (loops, FIFOs, `/dev/zero` links, a full disk: clean 500s, no corruption); direct-file streaming; every other subprocess has a timeout and a kill; panics unwind to a 5xx.
- **Playback and integrity.** See `findings-d1.md` (the reviewers' `checked_ok` lists).

## Reproducing

The reviewers' repros, as committed tests or scripts:

- `wt/review-security`: `tmp/r-security/scripts/{sse_flood,idle}.py`, `tmp/r-security/verify/scripts/{play_flood,session_flood,stall}.py`.
- `wt/review-robust/tools/review-robust/*.py` (ROB-*), with `verify/` for the verifier's.
- `wt/review-playback/crates/flux-hls/tests/review_playback.rs` (PB-1, PB-7, PB-5; ported into `tests/phase_d.rs`).
- `wt/review-integrity/crates/flux-scan/tests/integrity.rs` and `verify_integrity.rs` (ported into `crates/flux-scan/tests/integrity.rs`).
