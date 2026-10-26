/**
 * The player: the picture fills the window and the controls are glass that
 * fades when you stop moving: a back button, a title pill that says how flux is
 * playing the file, and one capsule of transport controls. The explainable stats
 * overlay (i) and keyboard/remote control work throughout. Leaving partway keeps
 * the session in the island as a ring, one tap from where you were.
 *
 * Flow: the Play click already sent POST /play (player/start.ts); here we take
 * that response, attach the engine, autoplay, and report progress + client
 * timings (TTFF, seek latency, stalls, dropped frames) to
 * POST /sessions/{id}/progress.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { back, navigate, type RouteProps } from '../app/router';
import { useTitle } from '../app/useTitle';
import { api, FluxApiError } from '../api/client';
import { resolveUrl } from '../config';
import { planLabel, planTone } from '../api/explain';
import { formatClock } from '../api/format';
import { parentTitle, useItem, useServer } from '../api/queries';
import { audioLabel, subtitleChoices, toBcp47 } from '../api/tracks';
import type { AudioRef, PlayResponse, ProgressReport, SessionDiagnostics } from '../api/types';
import { Icon } from '../components/Icon';
import { audioMenuNote, planAudioSwitch, playingTrack, restartNote, startingRendition, type AudioSwitchSample } from '../player/audio';
import { attachEngine, type AttachedEngine } from '../player/engine';
import { PlaybackMetrics, type MetricsSnapshot } from '../player/metrics';
import { requestPlay, startPlay, takePendingPlay, prefetchUrls, type PlayOptions } from '../player/start';
import { StatsOverlay } from '../player/StatsOverlay';
import styles from '../player/Player.module.css';
import { currentRing, setRing } from '../shell/live';

const EMPTY_METRICS: MetricsSnapshot = { ttffMs: null, lastSeekMs: null, seekSamples: [], stalls: 0, droppedFrames: 0, totalFrames: 0, bufferAheadS: 0, precise: true };

function optionsFromQuery(q: URLSearchParams): PlayOptions {
  const num = (k: string) => (q.get(k) === null ? null : Number(q.get(k)));
  return { assetId: num('asset'), audioTrack: num('audio'), subtitleTrack: num('sub'), startMs: num('start') };
}

/** The subtitle a session draws into its video (a picture format), if any. */
function burnedSubtitle(res: PlayResponse): number | null {
  return res.plan.video.op === 'transcode' ? res.plan.video.burn_subtitle : null;
}

function errorMessage(err: unknown): string {
  if (err instanceof FluxApiError) return err.message;
  if (err instanceof Error) return err.message;
  return 'Something went wrong starting playback.';
}

interface Session {
  res: PlayResponse;
  t0: number;
  options: PlayOptions;
  /** Started by an audio switch that needed a new session (its first frame is the switch time). */
  audioRestart?: boolean;
}

const SHORTCUTS: [string, string][] = [
  ['Space / K', 'Play or pause'],
  ['← / J', 'Back 10 seconds'],
  ['→ / L', 'Forward 10 seconds'],
  ['↑ / ↓', 'Volume'],
  ['M', 'Mute'],
  ['C', 'Subtitles on/off'],
  ['F', 'Full screen'],
  ['I', 'Playback details'],
  ['0–9', 'Jump to 0–90%'],
  ['Esc', 'Back'],
];

/** Ten-second skip icons with the numeral, like a remote's. */
function Skip({ dir }: { dir: 'back' | 'forward' }) {
  const arc = dir === 'back' ? 'M4.5 12a7.5 7.5 0 1 0 2.2-5.3M4.5 4v3.4h3.4' : 'M19.5 12a7.5 7.5 0 1 1-2.2-5.3M19.5 4v3.4h-3.4';
  return (
    <svg viewBox="0 0 24 24" width="24" height="24" fill="none" stroke="currentColor" strokeWidth={1.6} strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d={arc} />
      <text x="12" y="15.6" textAnchor="middle" fontSize="7.4" fontFamily="DM Mono, ui-monospace, monospace" fill="currentColor" stroke="none">
        10
      </text>
    </svg>
  );
}

/** "38 min left", "1:12 left" for the island's ring. */
function remainingLabel(seconds: number): string {
  const min = Math.max(1, Math.round(seconds / 60));
  if (min < 60) return `${min} min left`;
  return `${Math.floor(min / 60)}:${String(min % 60).padStart(2, '0')} left`;
}

export default function Player({ params, query }: RouteProps) {
  const itemId = Number(params.id);
  const token = query.get('t');
  const item = useItem(itemId);
  const show = parentTitle(item.data);
  const server = useServer();
  const title = item.data ? (show ? `${show} · ${item.data.subtitle ?? item.data.title}` : item.data.title) : '';
  useTitle(title || 'Playing');

  const videoRef = useRef<HTMLVideoElement>(null);
  const rootRef = useRef<HTMLDivElement>(null);
  const engineRef = useRef<AttachedEngine | null>(null);
  const metricsRef = useRef<PlaybackMetrics | null>(null);
  const [session, setSession] = useState<Session | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [engineInfo, setEngineInfo] = useState<{ engine: AttachedEngine['engine'] | null; version: string | null }>({ engine: null, version: null });
  const [metrics, setMetrics] = useState<MetricsSnapshot>(EMPTY_METRICS);
  const [live, setLive] = useState<{ rendition: string | null; bandwidth: number | null }>({ rendition: null, bandwidth: null });
  const [time, setTime] = useState({ current: 0, duration: 0, buffered: 0 });
  const [paused, setPaused] = useState(true);
  const [waiting, setWaiting] = useState(true);
  const [needsGesture, setNeedsGesture] = useState(false);
  const [ended, setEnded] = useState(false);
  const [statsOpen, setStatsOpen] = useState(() => query.get('stats') === '1');
  const [helpOpen, setHelpOpen] = useState(false);
  const [menu, setMenu] = useState<'audio' | 'subs' | null>(null);
  const [chromeVisible, setChromeVisible] = useState(true);
  const [activeSub, setActiveSub] = useState<number | null>(null);
  const [diag, setDiag] = useState<SessionDiagnostics | null>(null);
  const [scrub, setScrub] = useState<number | null>(null);
  const [volume, setVolume] = useState({ level: 1, muted: false });
  // The audio rendition picked in this session (in-place switches); the session's own default otherwise.
  const [audioSel, setAudioSel] = useState<{ sessionId: string; ref: AudioRef } | null>(null);
  const [lastAudioSwitch, setLastAudioSwitch] = useState<AudioSwitchSample | null>(null);
  // When the pending in-place switch was asked for, and every switch so far (for the benchmark hook).
  const pendingAudioSwitch = useRef<number | null>(null);
  const audioLog = useRef<{ rendition: string | null; switches: AudioSwitchSample[] }>({ rendition: null, switches: [] });
  const hideTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [flash, setFlash] = useState<{ icon: 'play' | 'pause'; key: number } | null>(null);
  const [fullscreen, setFullscreen] = useState(false);
  // What the island needs when you leave: refreshed after every render, read in the unmount cleanup.
  const leaving = useRef({ title: '', duration: 0, position: 0, ended: false });
  useEffect(() => {
    leaving.current = { title, duration: time.duration || (session?.res.duration_ms ?? 0) / 1000, position: time.current, ended };
  });

  // 1. The play response: from the click (in flight) or a fresh request.
  useEffect(() => {
    let live = true;
    const pending = takePendingPlay(token);
    const options = pending?.options ?? optionsFromQuery(query);
    const t0 = pending?.t0 ?? performance.now();
    const req = pending?.request ?? requestPlay(itemId, options).then((r) => (prefetchUrls(r.prefetch), r));
    req.then(
      (res) => {
        if (!live) return;
        setSession({ res, t0, options });
        setActiveSub(options.subtitleTrack ?? burnedSubtitle(res) ?? res.subtitles.find((s) => s.forced)?.track ?? null);
      },
      (err: unknown) => live && setError(errorMessage(err)),
    );
    return () => {
      live = false;
    };
    // The query object changes identity every render; token + item identify the request.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [itemId, token]);

  const asset = useMemo(() => item.data?.assets.find((a) => a.id === session?.res.asset_id), [item.data, session]);
  const currentRef = session ? (audioSel?.sessionId === session.res.session_id ? audioSel.ref : startingRendition(session.res)) : null;

  // `video` defaults to the ref; effect cleanups pass the element they captured,
  // because React clears refs before unmount cleanups run (the final report on
  // leaving the player was silently dropped).
  const buildReport = useCallback((video?: HTMLVideoElement | null): ProgressReport | null => {
    const v = video ?? videoRef.current;
    const m = metricsRef.current?.snapshot();
    if (!v || !m) return null;
    return {
      position_ms: Math.round(v.currentTime * 1000),
      paused: v.paused,
      ttff_ms: m.ttffMs,
      last_seek_ms: m.lastSeekMs,
      stalls: m.stalls,
      dropped_frames: m.droppedFrames,
    };
  }, []);

  const report = useCallback(
    (keepalive = false) => {
      const r = buildReport();
      if (r && session) void api.reportProgress(session.res.session_id, r, keepalive).catch(() => undefined);
    },
    [buildReport, session],
  );

  // 2. Attach the engine and measurements.
  useEffect(() => {
    const video = videoRef.current;
    if (!session || !video) return;
    let destroyed = false;
    let firstReported = false;
    const log = audioLog.current;
    log.rendition = startingRendition(session.res)?.rendition ?? null;
    const metrics = new PlaybackMetrics(session.t0, () => {
      const snap = metrics.snapshot();
      setMetrics(snap);
      // Send TTFF as soon as it is known, then every seek.
      if (snap.ttffMs !== null && !firstReported) {
        firstReported = true;
        report();
        // An audio switch that took a new session: its first frame is the switch.
        if (session.audioRestart) {
          const sample: AudioSwitchSample = { ms: snap.ttffMs, mode: 'restart', to: log.rendition ?? `track ${session.options.audioTrack ?? '?'}` };
          log.switches.push(sample);
          setLastAudioSwitch(sample);
        }
      }
    });
    metricsRef.current = metrics;
    metrics.attach(video);
    // Read-only hook for the benchmark harness (scripts/bench-playback.mjs, bench-audio-switch.mjs).
    (window as Window & { __fluxPlayback?: unknown }).__fluxPlayback = {
      sessionId: session.res.session_id,
      plan: session.res.plan,
      metrics: () => metrics.snapshot(),
      audio: () => ({ rendition: log.rendition, switches: [...log.switches] }),
    };
    setEnded(false);
    setWaiting(true);
    const onAudioSwitched = (ref: AudioRef) => {
      const t = pendingAudioSwitch.current;
      if (t === null) return;
      pendingAudioSwitch.current = null;
      const sample: AudioSwitchSample = { ms: Math.round(performance.now() - t), mode: 'in-session', to: ref.rendition };
      log.switches.push(sample);
      setLastAudioSwitch(sample);
    };
    void attachEngine(video, session.res, { onFatal: (m) => setError(m), onAudioSwitched }).then((a) => {
      if (destroyed) {
        a.destroy();
        return;
      }
      engineRef.current = a;
      setEngineInfo({ engine: a.engine, version: a.version });
      video.play().then(
        () => setNeedsGesture(false),
        () => setNeedsGesture(true), // autoplay blocked: show a big Play button
      );
    });
    return () => {
      destroyed = true;
      const r = buildReport(video);
      if (r) void api.reportProgress(session.res.session_id, r, true).catch(() => undefined);
      engineRef.current?.destroy();
      engineRef.current = null;
      metrics.detach();
      metricsRef.current = null;
    };
  }, [session, buildReport, report]);

  // The island keeps one session: a new player replaces it; leaving partway puts this one there.
  useEffect(() => {
    if (currentRing()?.kind === 'session') setRing(null);
    return () => {
      // The last position the video reported: by now the engine's own cleanup may have reset the element.
      const { title: t, duration, position, ended: done } = leaving.current;
      if (done || !duration) return;
      const frac = position / duration;
      if (frac > 0.02 && frac < 0.97) {
        setRing({
          kind: 'session',
          progress: frac,
          label: remainingLabel(duration - position),
          aria: `Resume ${t || 'playback'}`,
          itemId,
          run: () => startPlay(itemId),
        });
      }
    };
  }, [itemId]);

  useEffect(() => {
    if (ended && currentRing()?.kind === 'session' && currentRing()?.itemId === itemId) setRing(null);
  }, [ended, itemId]);

  useEffect(() => {
    const on = () => setFullscreen(Boolean(document.fullscreenElement));
    document.addEventListener('fullscreenchange', on);
    return () => document.removeEventListener('fullscreenchange', on);
  }, []);

  // Controls fade a moment after the last movement while playing (and once playback starts).
  const poke = useCallback(() => {
    setChromeVisible(true);
    if (hideTimer.current) clearTimeout(hideTimer.current);
    hideTimer.current = setTimeout(() => setChromeVisible(false), 2600);
  }, []);
  useEffect(() => () => {
    if (hideTimer.current) clearTimeout(hideTimer.current);
  }, []);

  // 3. Video element state → React (4 Hz timeupdate is plenty for the UI).
  useEffect(() => {
    const v = videoRef.current;
    if (!v) return;
    const sync = () => {
      let buffered = 0;
      for (let i = 0; i < v.buffered.length; i++) if (v.buffered.start(i) <= v.currentTime + 0.25) buffered = Math.max(buffered, v.buffered.end(i));
      setTime({ current: v.currentTime, duration: Number.isFinite(v.duration) ? v.duration : (session?.res.duration_ms ?? 0) / 1000, buffered });
    };
    const handlers: [string, () => void][] = [
      ['timeupdate', sync],
      ['durationchange', sync],
      ['progress', sync],
      ['play', () => setPaused(false)],
      ['pause', () => (setPaused(true), report())],
      ['waiting', () => setWaiting(true)],
      ['playing', () => (setWaiting(false), setNeedsGesture(false), poke())],
      ['canplay', () => setWaiting(false)],
      ['seeked', () => (sync(), report())],
      ['ended', () => (setEnded(true), report())],
      ['volumechange', () => setVolume({ level: v.volume, muted: v.muted })],
    ];
    for (const [e, h] of handlers) v.addEventListener(e, h);
    return () => {
      for (const [e, h] of handlers) v.removeEventListener(e, h);
    };
  }, [session, report, poke]);

  // 4. Periodic progress while playing; metrics refresh for the overlay.
  useEffect(() => {
    if (!session) return;
    const every = setInterval(() => {
      if (videoRef.current && !videoRef.current.paused) report();
    }, 10_000);
    const fast = setInterval(() => {
      if (metricsRef.current) setMetrics(metricsRef.current.snapshot());
      const v = videoRef.current;
      const bw = engineRef.current?.hls?.bandwidthEstimate;
      setLive((prev) => {
        const rendition = v?.videoWidth ? `${v.videoWidth}×${v.videoHeight}` : null;
        const bandwidth = bw !== undefined && Number.isFinite(bw) && bw > 0 ? bw : null;
        return prev.rendition === rendition && prev.bandwidth === bandwidth ? prev : { rendition, bandwidth };
      });
    }, 500);
    const onHide = () => {
      if (document.visibilityState === 'hidden') report(true);
    };
    document.addEventListener('visibilitychange', onHide);
    return () => {
      clearInterval(every);
      clearInterval(fast);
      document.removeEventListener('visibilitychange', onHide);
    };
  }, [session, report]);

  // 5. Server-side diagnostics while the overlay is open.
  useEffect(() => {
    if (!statsOpen || !session) return;
    let live = true;
    const load = () =>
      api.diagnostics(session.res.session_id).then(
        (d) => live && setDiag(d),
        () => undefined,
      );
    void load();
    const t = setInterval(load, 4000);
    return () => {
      live = false;
      clearInterval(t);
    };
  }, [statsOpen, session]);

  // Subtitle tracks: exactly one showing.
  useEffect(() => {
    const v = videoRef.current;
    if (!v || !session) return;
    const apply = () => {
      for (let i = 0; i < v.textTracks.length; i++) {
        const t = v.textTracks[i];
        t.mode = String(activeSub) === t.id ? 'showing' : 'disabled';
      }
    };
    apply();
    v.textTracks.addEventListener('addtrack', apply);
    return () => v.textTracks.removeEventListener('addtrack', apply);
  }, [activeSub, session]);

  const showChrome = chromeVisible || paused || menu !== null || needsGesture || ended || Boolean(error);
  // On a touch screen the first tap on a bare picture only brings the controls back.
  const chromeShown = useRef(showChrome);
  const tapRevealed = useRef(false);
  useEffect(() => {
    chromeShown.current = showChrome;
  });

  const togglePlay = useCallback(() => {
    const v = videoRef.current;
    if (!v) return;
    setFlash({ icon: v.paused ? 'play' : 'pause', key: performance.now() });
    if (v.paused) void v.play().catch(() => setNeedsGesture(true));
    else v.pause();
  }, []);

  const seekBy = useCallback((s: number) => {
    const v = videoRef.current;
    if (!v) return;
    v.currentTime = Math.max(0, Math.min((v.duration || Infinity) - 0.5, v.currentTime + s));
  }, []);

  const toggleFullscreen = useCallback(() => {
    const el = rootRef.current;
    const v = videoRef.current;
    if (document.fullscreenElement) void document.exitFullscreen();
    else if (el?.requestFullscreen) void el.requestFullscreen().catch(() => undefined);
    // iPhone Safari has no element fullscreen, only the video's own.
    else if (v && 'webkitEnterFullscreen' in v && typeof v.webkitEnterFullscreen === 'function') (v.webkitEnterFullscreen as () => void).call(v);
  }, []);

  /** A new session at the current position with `patch` applied (another audio track, a burned-in subtitle). */
  const restartAt = useCallback(
    (patch: Partial<PlayOptions>, audioRestart = false) => {
      const v = videoRef.current;
      if (!session || !v) return;
      const options: PlayOptions = { ...session.options, subtitleTrack: activeSub, startMs: Math.round(v.currentTime * 1000), assetId: session.res.asset_id, ...patch };
      const t0 = performance.now();
      setWaiting(true);
      requestPlay(itemId, options).then(
        (res) => {
          prefetchUrls(res.prefetch);
          setSession({ res, t0, options, audioRestart });
        },
        (err: unknown) => setError(errorMessage(err)),
      );
    },
    [session, activeSub, itemId],
  );

  /**
   * Audio changes switch renditions inside the session when it has one for
   * that track (player/audio.ts); otherwise they restart the stream at the
   * current position with a new plan (direct play, no such rendition, or a
   * FLAC/Opus → other codec change hls.js can't make).
   */
  const switchAudio = useCallback(
    (track: number) => {
      const v = videoRef.current;
      setMenu(null);
      if (!session || !v) return;
      const decision = planAudioSwitch(session.res, currentRef, track, engineRef.current?.engine ?? null);
      if (decision.kind === 'none') return;
      if (decision.kind === 'in-session') {
        pendingAudioSwitch.current = performance.now();
        if (engineRef.current?.selectAudio(decision.ref)) {
          setAudioSel({ sessionId: session.res.session_id, ref: decision.ref });
          audioLog.current.rendition = decision.ref.rendition;
          return;
        }
        // The player can't find that rendition: fall back to a new session.
        pendingAudioSwitch.current = null;
      }
      restartAt({ audioTrack: track }, true);
    },
    [session, currentRef, restartAt],
  );

  /**
   * Subtitles: text ones are WebVTT tracks of the session, switched in the
   * page. A picture subtitle (PGS, VobSub, a `.sup` file) is drawn into the
   * video, so choosing one, or leaving one, restarts playback here with a
   * new plan.
   */
  const chooseSub = useCallback(
    (track: number | null) => {
      setMenu(null);
      if (!session) return;
      const burned = burnedSubtitle(session.res);
      const text = track === null || session.res.subtitles.some((s) => s.track === track);
      if (text && burned === null) {
        setActiveSub(track);
        return;
      }
      if (track === burned) return;
      setActiveSub(track);
      restartAt({ subtitleTrack: track, audioTrack: currentRef?.track ?? session.options.audioTrack });
    },
    [session, currentRef, restartAt],
  );

  const cycleSubs = useCallback(() => {
    const subs = session?.res.subtitles ?? [];
    // A subtitle drawn into the video is changed from the menu (a restart).
    const burned = session ? burnedSubtitle(session.res) : null;
    if (subs.length === 0 || burned !== null) return;
    const i = subs.findIndex((s) => s.track === activeSub);
    setActiveSub(i + 1 >= subs.length ? null : subs[i + 1].track);
  }, [session, activeSub]);

  const leave = useCallback(() => back(`/item/${itemId}`), [itemId]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.metaKey || e.ctrlKey || e.altKey) return;
      const t = e.target as HTMLElement;
      const onControl = t.tagName === 'BUTTON' || t.getAttribute('role') === 'menuitemradio';
      poke();
      switch (e.key) {
        case ' ':
        case 'k':
        case 'K':
          if (onControl && e.key === ' ') return;
          e.preventDefault();
          togglePlay();
          break;
        case 'ArrowLeft':
        case 'j':
        case 'J':
          e.preventDefault();
          seekBy(-10);
          break;
        case 'ArrowRight':
        case 'l':
        case 'L':
          e.preventDefault();
          seekBy(10);
          break;
        case 'ArrowUp':
        case 'ArrowDown': {
          if (menu) return;
          e.preventDefault();
          const v = videoRef.current;
          if (v) v.volume = Math.max(0, Math.min(1, v.volume + (e.key === 'ArrowUp' ? 0.1 : -0.1)));
          break;
        }
        case 'm':
        case 'M':
          if (videoRef.current) videoRef.current.muted = !videoRef.current.muted;
          break;
        case 'f':
        case 'F':
          toggleFullscreen();
          break;
        case 'c':
        case 'C':
          cycleSubs();
          break;
        case 'i':
        case 'I':
          setStatsOpen((o) => !o);
          break;
        case '?':
          setHelpOpen((o) => !o);
          break;
        case 'Escape':
          if (menu) setMenu(null);
          else if (helpOpen) setHelpOpen(false);
          else if (!document.fullscreenElement) leave();
          break;
        default:
          if (/^[0-9]$/.test(e.key)) {
            const v = videoRef.current;
            if (v && Number.isFinite(v.duration)) v.currentTime = (Number(e.key) / 10) * v.duration;
          }
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [togglePlay, seekBy, toggleFullscreen, cycleSubs, leave, poke, menu, helpOpen]);

  const audioTracks = asset?.probe?.tracks.filter((t) => t.kind === 'audio') ?? [];
  const currentAudio = session ? playingTrack(session.res, currentRef) : null;
  const audioNote = session ? audioMenuNote(session.res) : null;
  const subs = session?.res.subtitles ?? [];
  // Every subtitle of this version: its WebVTT renditions, then the picture
  // subtitles that would be drawn into the video.
  const choices = subtitleChoices(asset);
  const burned = session ? burnedSubtitle(session.res) : null;
  const pictureSubs = choices.filter((c) => c.burnIn);
  const subLabel = (track: number, fallback: string) => choices.find((c) => c.track === track)?.label ?? fallback;
  const shownTime = scrub ?? time.current;
  const pct = time.duration > 0 ? (shownTime / time.duration) * 100 : 0;
  const bufPct = time.duration > 0 ? (time.buffered / time.duration) * 100 : 0;
  // The title pill says how flux is playing this file, in its own colours.
  const plan = session ? planLabel(session.res.plan) : null;
  const tone = plan ? planTone(plan) : null;
  const badge = item.data?.badges[0];
  const isEpisode = item.data?.kind === 'episode';
  const pillMeta = isEpisode ? item.data?.subtitle : item.data?.year ? String(item.data.year) : null;
  const chapters = (asset?.probe?.chapters ?? []).filter((c) => c.start_ms > 0 && time.duration > 0 && c.start_ms / 1000 < time.duration - 1);

  return (
    <div
      ref={rootRef}
      className={styles.player}
      data-chrome={showChrome || undefined}
      onPointerMove={poke}
      onPointerDown={(e) => {
        tapRevealed.current = e.pointerType === 'touch' && !chromeShown.current;
        poke();
      }}
    >
      <video
        ref={videoRef}
        className={styles.video}
        playsInline
        preload="auto"
        crossOrigin="anonymous"
        onClick={() => {
          if (tapRevealed.current) tapRevealed.current = false;
          else if (menu) setMenu(null);
          else togglePlay();
        }}
        onDoubleClick={toggleFullscreen}
      >
        {subs.map((s) => (
          <track key={`${session?.res.session_id}-${s.track}`} id={String(s.track)} kind="subtitles" src={resolveUrl(s.url)} srcLang={toBcp47(s.language)} label={s.label} />
        ))}
      </video>

      {(waiting || !session) && !error && !needsGesture && (
        <div className={styles.spinner} role="status" aria-label="Loading">
          <span />
        </div>
      )}

      {needsGesture && !error && (
        <button type="button" className={`${styles.bigPlay} glass glass-btn`} onClick={togglePlay} aria-label="Play">
          <Icon name="play" />
        </button>
      )}

      {flash && !needsGesture && (
        <div key={flash.key} className={styles.flash} aria-hidden="true">
          <Icon name={flash.icon} />
        </div>
      )}

      <header className={styles.top}>
        <button type="button" className={`${styles.round} glass glass-btn`} onClick={leave} aria-label="Back to details" title="Back (esc)">
          <Icon name="back" />
        </button>
        {item.data || plan ? (
          <div className={`${styles.pill} glass`} title={session?.res.explain[0] ?? undefined}>
            {tone && <span className={styles.dot} data-tone={tone} aria-hidden="true" />}
            <span className={styles.pillTitle}>{show ?? item.data?.title ?? ' '}</span>
            {pillMeta && <span className={`${styles.pillMeta} mono`}>{pillMeta}</span>}
            {isEpisode && item.data?.title && <span className={styles.pillEpisode}>{item.data.title}</span>}
            {plan && (
              <span className={styles.pillPath}>
                {plan}
                {badge ? ` · ${badge}` : ''}
              </span>
            )}
          </div>
        ) : (
          <span />
        )}
        <div className={styles.topRight}>
          <button type="button" className={`${styles.round} glass glass-btn`} onClick={() => setHelpOpen((o) => !o)} aria-label="Keyboard shortcuts" aria-pressed={helpOpen} title="Keyboard shortcuts (?)">
            <Icon name="keyboard" />
          </button>
          <button type="button" className={`${styles.round} glass glass-btn`} onClick={() => setStatsOpen((o) => !o)} aria-label="Playback details" aria-pressed={statsOpen} title="Playback details (i)">
            <Icon name="info" />
          </button>
        </div>
      </header>

      {statsOpen && session && (
        <StatsOverlay
          res={session.res}
          asset={asset}
          engine={engineInfo.engine}
          hlsVersion={engineInfo.version}
          path={diag?.path ?? server.data?.path}
          metrics={metrics}
          diag={diag}
          audio={currentRef}
          audioSwitch={lastAudioSwitch}
          bandwidth={live.bandwidth}
          rendition={live.rendition}
          onClose={() => setStatsOpen(false)}
        />
      )}

      {helpOpen && (
        <section className={`${styles.help} glass`} aria-label="Keyboard shortcuts">
          <h2 className="edge">Keyboard</h2>
          <dl>
            {SHORTCUTS.map(([k, d]) => (
              <div key={k}>
                <dt>{k}</dt>
                <dd>{d}</dd>
              </div>
            ))}
          </dl>
        </section>
      )}

      {error && (
        <div className={`${styles.card} glass`} role="alert">
          <h2 className="display-m">Playback stopped</h2>
          <p>{error}</p>
          <div className={styles.cardActions}>
            <button type="button" className="btn btn-primary" onClick={() => navigate(`/play/${itemId}?r=${Date.now()}`, { replace: true })}>
              Try again
            </button>
            <button type="button" className="btn" onClick={leave}>
              Back to details
            </button>
          </div>
        </div>
      )}

      {ended && !error && (
        <div className={`${styles.card} glass`}>
          <p className="display-m">Finished</p>
          <div className={styles.cardActions}>
            <button type="button" className="btn" onClick={() => videoRef.current && ((videoRef.current.currentTime = 0), void videoRef.current.play())}>
              <Icon name="restart" /> Watch again
            </button>
            <button type="button" className="btn btn-primary" onClick={leave}>
              Back to details
            </button>
          </div>
        </div>
      )}

      <footer className={`${styles.bar} glass`}>
        <div className={styles.group}>
          <button type="button" className={styles.btn} onClick={() => seekBy(-10)} aria-label="Back 10 seconds" title="Back 10 seconds (←)">
            <Skip dir="back" />
          </button>
          <button type="button" className={`${styles.btn} ${styles.pp}`} onClick={togglePlay} aria-label={paused ? 'Play' : 'Pause'} title={paused ? 'Play (space)' : 'Pause (space)'}>
            <Icon name={paused ? 'play' : 'pause'} />
          </button>
          <button type="button" className={styles.btn} onClick={() => seekBy(10)} aria-label="Forward 10 seconds" title="Forward 10 seconds (→)">
            <Skip dir="forward" />
          </button>
        </div>

        <div className={styles.scrub}>
          <span className={`${styles.time} mono`}>{formatClock(shownTime * 1000)}</span>
          <div className={styles.scrubber}>
            <span className={styles.trackBg} />
            <span className={styles.trackBuf} style={{ width: `${bufPct}%` }} />
            <span className={styles.trackFill} style={{ width: `${pct}%` }} />
            {chapters.map((c) => (
              <span key={c.start_ms} className={styles.tick} style={{ left: `${(c.start_ms / 1000 / time.duration) * 100}%` }} title={c.title ?? undefined} />
            ))}
            <span className={styles.knob} style={{ left: `${pct}%` }} />
            <input
              type="range"
              min={0}
              max={time.duration || 1}
              step="any"
              value={shownTime}
              aria-label="Seek"
              aria-valuetext={`${formatClock(shownTime * 1000)} of ${formatClock(time.duration * 1000)}`}
              onChange={(e) => setScrub(Number(e.target.value))}
              onPointerUp={() => {
                if (scrub !== null && videoRef.current) videoRef.current.currentTime = scrub;
                setScrub(null);
              }}
              onKeyDown={(e) => e.key.startsWith('Arrow') && e.preventDefault()}
            />
          </div>
          <span className={`${styles.time} mono`}>−{formatClock(Math.max(0, time.duration - shownTime) * 1000)}</span>
        </div>

        <div className={styles.group}>
          {audioTracks.length > 1 && (
            <div className={styles.menuWrap}>
              <button
                type="button"
                className={styles.btn}
                aria-label="Audio"
                aria-haspopup="menu"
                aria-expanded={menu === 'audio'}
                title="Audio"
                onClick={() => setMenu(menu === 'audio' ? null : 'audio')}
              >
                <Icon name="audio" />
              </button>
              {menu === 'audio' && (
                <div className={`${styles.menu} glass`} role="menu" aria-label="Audio" aria-describedby={audioNote ? 'audio-menu-note' : undefined}>
                  {audioTracks.map((t) => {
                    // Say which picks restart the stream (direct play says it once, below).
                    const d = session && !audioNote ? planAudioSwitch(session.res, currentRef, t.index, engineInfo.engine) : null;
                    return (
                      <button key={t.index} type="button" role="menuitemradio" aria-checked={t.index === currentAudio} onClick={() => t.index !== currentAudio && switchAudio(t.index)}>
                        {audioLabel(t)}
                        {d?.kind === 'restart' && <span className={styles.menuHint}>{restartNote(d.reason)}</span>}
                      </button>
                    );
                  })}
                  {audioNote && (
                    <p id="audio-menu-note" className={styles.menuNote}>
                      {audioNote}
                    </p>
                  )}
                </div>
              )}
            </div>
          )}

          {subs.length + pictureSubs.length > 0 && (
            <div className={styles.menuWrap}>
              <button
                type="button"
                className={styles.btn}
                aria-label="Subtitles"
                aria-haspopup="menu"
                aria-expanded={menu === 'subs'}
                title="Subtitles (c)"
                onClick={() => setMenu(menu === 'subs' ? null : 'subs')}
              >
                <Icon name="subtitles" />
              </button>
              {menu === 'subs' && (
                <div className={`${styles.menu} glass`} role="menu" aria-label="Subtitles">
                  <button type="button" role="menuitemradio" aria-checked={activeSub === null && burned === null} onClick={() => chooseSub(null)}>
                    Off
                  </button>
                  {subs.map((s) => (
                    <button key={s.track} type="button" role="menuitemradio" aria-checked={activeSub === s.track && burned === null} onClick={() => chooseSub(s.track)}>
                      {subLabel(s.track, s.label)}
                    </button>
                  ))}
                  {pictureSubs.map((c) => (
                    <button key={c.track} type="button" role="menuitemradio" aria-checked={burned === c.track} onClick={() => chooseSub(c.track)}>
                      {c.label}
                      <span className={styles.menuHint}>drawn into the video (restarts playback)</span>
                    </button>
                  ))}
                </div>
              )}
            </div>
          )}

          <button
            type="button"
            className={styles.btn}
            onClick={() => videoRef.current && (videoRef.current.muted = !videoRef.current.muted)}
            aria-label={volume.muted ? 'Unmute' : 'Mute'}
            title={volume.muted ? 'Unmute (m)' : 'Mute (m)'}
          >
            <Icon name={volume.muted || volume.level === 0 ? 'mute' : 'volume'} />
          </button>
          <button type="button" className={styles.btn} onClick={toggleFullscreen} aria-label="Full screen" title="Full screen (f)">
            <Icon name={fullscreen ? 'exitFullscreen' : 'fullscreen'} />
          </button>
        </div>
      </footer>
    </div>
  );
}
