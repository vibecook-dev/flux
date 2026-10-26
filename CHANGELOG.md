# Changelog

## [0.1.0-alpha.4](https://github.com/vibecook-dev/flux/compare/v0.1.0-alpha.3...v0.1.0-alpha.4) (2026-10-26)


### Features

* agents fix what files are, merge items and organise files on disk ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))
* flux for agents: one contract over HTTP, MCP and the flux command line ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))
* HTTPS on the tailnet, with the tailnet's own certificate ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))
* lenses on the GPU, flux for agents, HTTPS on the tailnet, and the website ([#36](https://github.com/vibecook-dev/flux/issues/36)) ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))
* metadata files beside the videos, and collections ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))
* **site:** the product website, with flux's lenses living in the page ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))
* **web:** Agents & changes, an item's details and their sources, lenses agents keep ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))
* **web:** Flow draws with WebGPU ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))
* **web:** lens-based library shell, with the Wall drawn on WebGPU ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))
* **web:** perspective quads and sprite sheets for GPU lenses ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))
* **web:** review file plans, edit details, browse collections as spaces ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))
* **web:** run below a sub-path, and let the product site drive a mock build ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))
* **web:** the Gallery draws with WebGPU, with depth and nothing to z-fight ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))
* **web:** the Shelf draws with WebGPU ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))


### Bug Fixes

* **catalog:** a file merged into a work never renames it ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))
* **cli:** flux mcp writes \u escapes on Windows too ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))
* **cli:** JSON that survives Windows PowerShell pipes ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))
* **deps:** ctutils 0.4.6, for RUSTSEC-2026-0385 ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))
* **scan:** hold a library's watcher while flux moves its files ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))
* **server:** a plan lists a work's files in reading order ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))
* **server:** MCP clients find no OAuth, and get plain tool schemas ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))
* **site:** one line for the tailnet's label, and phone maps that route round captions ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))
* **web:** an item an agent identified says so; quieter source marks ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))
* **web:** GPU lenses decode posters off the main thread, sharper at an angle ([3cef403](https://github.com/vibecook-dev/flux/commit/3cef403f8494c5c59e112d429743f68242e8275e))

## [0.1.0-alpha.3](https://github.com/vibecook-dev/flux/compare/v0.1.0-alpha.2...v0.1.0-alpha.3) (2026-10-22)


### Features

* **hls:** play subtitle files and MP4 text subtitles, and burn in picture subtitles ([#26](https://github.com/vibecook-dev/flux/issues/26)) ([62bd007](https://github.com/vibecook-dev/flux/commit/62bd0079cce73ecad3b31413dac812fbd5455183))
* **release:** support Windows x86_64 with a CI job, a release zip and install docs ([#27](https://github.com/vibecook-dev/flux/issues/27)) ([87aded0](https://github.com/vibecook-dev/flux/commit/87aded02415fe079f80612bb6984149ce1c33fd3))
* videos libraries, show evidence in mixed libraries, no code files as videos ([#32](https://github.com/vibecook-dev/flux/issues/32)) ([d5e94e1](https://github.com/vibecook-dev/flux/commit/d5e94e115edc155390d4355f64782513462611c5))


### Bug Fixes

* **catalog:** version subfolders, word-named extras, duplicate roots and a no-metadata notice ([#22](https://github.com/vibecook-dev/flux/issues/22)) ([c52e4a4](https://github.com/vibecook-dev/flux/commit/c52e4a48b55ee7cfea41732b2d08add700fbebc9))
* heal upgraded catalogs and keep art when the data dir moves ([#33](https://github.com/vibecook-dev/flux/issues/33)) ([55a95e8](https://github.com/vibecook-dev/flux/commit/55a95e80460438dff5cfc69f3a8a884c83794bba))
* **hls:** cut VFR MP4 transcodes on a constant-rate grid so every piece starts with an IDR ([#34](https://github.com/vibecook-dev/flux/issues/34)) ([0aec75c](https://github.com/vibecook-dev/flux/commit/0aec75c61f7b9703f5d3a836cb09eb99b1a84282))
* **hls:** play Dolby Vision and hev1 MP4s correctly for each client ([#19](https://github.com/vibecook-dev/flux/issues/19)) ([ef62d91](https://github.com/vibecook-dev/flux/commit/ef62d9193ea2df5cf848e23e78f3c3b68859a7bf))
* **hls:** remux parameter sets, transcode pieces, deinterlacing and transcode concurrency ([#23](https://github.com/vibecook-dev/flux/issues/23)) ([2600bc1](https://github.com/vibecook-dev/flux/commit/2600bc1955b698498d44eeadce59fde6bba20e8e))
* **hls:** seamless AAC from laced AC-3/E-AC-3 in Matroska ([#25](https://github.com/vibecook-dev/flux/issues/25)) ([2ee5856](https://github.com/vibecook-dev/flux/commit/2ee585659500f1c9aa4962516ccbf1cee6ea4641))
* **hls:** stop burn-in runs of a subtitle track waiting for an end ffmpeg can drop ([#35](https://github.com/vibecook-dev/flux/issues/35)) ([2da7cf0](https://github.com/vibecook-dev/flux/commit/2da7cf050ca41bdb999d44c2b7b9ba73b83f7966))
* index MP4/MOV files with legacy text, padded boxes, junk or negative timings ([#30](https://github.com/vibecook-dev/flux/issues/30)) ([e787ee1](https://github.com/vibecook-dev/flux/commit/e787ee10d3e09548ab0fa18134d14e7dcb3b4b25))
* **names:** read Chinese release names, stack A/B parts and keep bilingual titles ([#20](https://github.com/vibecook-dev/flux/issues/20)) ([7884b30](https://github.com/vibecook-dev/flux/commit/7884b30c79a20c33a1dfca16ec280e6781f4cde0))
* play ASF, FLV, RealMedia, MPEG-PS and AVI without a packet scan, and name their formats ([#31](https://github.com/vibecook-dev/flux/issues/31)) ([9a19887](https://github.com/vibecook-dev/flux/commit/9a19887d44c7f151d6c9f11dcfabb08cd9fa1525))
* play files behind junctions, classify drives and clean root paths on Windows; bring back roots that answer again ([#21](https://github.com/vibecook-dev/flux/issues/21)) ([a6a5245](https://github.com/vibecook-dev/flux/commit/a6a5245db23033234187fe33eb9fbac0644e4f34))
* play H.264 Baseline and VP9 without a video transcode, and give idle cache memory back ([#29](https://github.com/vibecook-dev/flux/issues/29)) ([3769ba3](https://github.com/vibecook-dev/flux/commit/3769ba363c719d31749cc81623ebbaf0350e36f1))
* **server:** run cleanly on Windows: graceful stops, helper processes, data dir, host allowlist, CJK posters ([#24](https://github.com/vibecook-dev/flux/issues/24)) ([43309e2](https://github.com/vibecook-dev/flux/commit/43309e2387f92a8a280bb81d5989b0695ac3b256))

## [0.1.0-alpha.2](https://github.com/vibecook-dev/flux/compare/v0.1.0-alpha.1...v0.1.0-alpha.2) (2026-10-21)


### Bug Fixes

* complete THIRD_PARTY_LICENSES, and colour the log only on a terminal ([#13](https://github.com/vibecook-dev/flux/issues/13)) ([d2d766e](https://github.com/vibecook-dev/flux/commit/d2d766e2d93dae325eeebf964555ec07df881470))
* **server:** exit after "flux stopped" instead of hanging in the catalog's drop ([#17](https://github.com/vibecook-dev/flux/issues/17)) ([b5ff2b6](https://github.com/vibecook-dev/flux/commit/b5ff2b63b6f734408d6239479396634f6b780c92))

## 0.1.0-alpha.1 (2026-10-21)


### Features

* initial public release of flux ([7af3200](https://github.com/vibecook-dev/flux/commit/7af3200b278d1aa0109c385a2edfbb4fda44e908))


### Bug Fixes

* **deps:** take truffle-core 0.7.20 from crates.io and bundle its sidecar ([#6](https://github.com/vibecook-dev/flux/issues/6)) ([23558e1](https://github.com/vibecook-dev/flux/commit/23558e16181b82307a84beddbc6ff2b02a00ba57))
* **net:** point the missing-sidecar error at release packages, not a dev worktree ([#10](https://github.com/vibecook-dev/flux/issues/10)) ([7cd02b2](https://github.com/vibecook-dev/flux/commit/7cd02b23c68f49dfd51cd5540d8d95698ecb8198))
* **server:** find the web client and sidecar next to a symlinked flux-server ([#11](https://github.com/vibecook-dev/flux/issues/11)) ([73b6d12](https://github.com/vibecook-dev/flux/commit/73b6d121c596ff9de2573384068c540ba9180e92))
