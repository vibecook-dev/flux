/**
 * "Why is it playing like this?" — the plan the server chose, its reasons in
 * plain sentences, the network path, codecs in → out, and live client/server
 * timings. Toggle with `i`. Styled as a scope readout on glass, not a debug dump.
 */
import { channelLayout, codecShort, hdrName, planLabel, planTone } from '../api/explain';
import { formatBitrate, formatBytes, formatCount, formatMs, pathLabel } from '../api/format';
import type { AssetInfo, AudioRef, NetworkPath, PlayResponse, SessionDiagnostics, Track } from '../api/types';
import { videoLabel } from '../api/tracks';
import type { PlayerEngine } from '../caps/engine';
import { renditionKey, type AudioSwitchSample } from './audio';
import { median, type MetricsSnapshot } from './metrics';
import styles from './Player.module.css';

function audioIn(t: Track | undefined): string {
  return t ? `${codecShort(t.codec)} ${channelLayout(t.channels)}`.trim() : '—';
}

export interface StatsProps {
  res: PlayResponse;
  asset: AssetInfo | undefined;
  engine: PlayerEngine | null;
  hlsVersion: string | null;
  path: NetworkPath | undefined;
  metrics: MetricsSnapshot;
  diag: SessionDiagnostics | null;
  /** The audio rendition playing now (HLS), and the last audio switch's timing. */
  audio: AudioRef | null;
  audioSwitch: AudioSwitchSample | null;
  bandwidth: number | null;
  rendition: string | null;
  onClose: () => void;
}

export function StatsOverlay({ res, asset, engine, hlsVersion, path, metrics, diag, audio, audioSwitch, bandwidth, rendition, onClose }: StatsProps) {
  const plan = res.plan;
  const label = planLabel(plan);
  const tracks = asset?.probe?.tracks ?? [];
  const vIn = tracks.find((t) => t.index === plan.video_track) ?? tracks.find((t) => t.kind === 'video');
  const video = plan.video;
  const videoOut =
    video.op === 'copy'
      ? `copy${video.fixes.length ? ` + ${video.fixes.map((f) => (f === 'dovi_p7_to81' ? 'DV 7→8.1' : 'parameter sets')).join(', ')}` : ''}`
      : `${codecShort(video.codec)} ${video.width}×${video.height} · ${formatBitrate(video.bitrate)}${video.hardware ? ' · hardware' : ''}${video.tonemap ? ' · tone-mapped' : ''}`;
  const audioRows = plan.audio.map((a) => {
    const src = tracks.find((t) => t.index === a.track);
    const out = a.op.op === 'copy' ? 'copy' : `${codecShort(a.op.codec)} ${channelLayout(a.op.channels)} · ${formatBitrate(a.op.bitrate)}`;
    // Several renditions: mark the one playing (it changes with in-session switches).
    const playing = plan.audio.length > 1 && audio !== null && renditionKey(a) === audio.rendition;
    return [audioIn(src), playing ? `${out} · playing` : out] as const;
  });
  const subRows = plan.subtitles.map((s) => {
    const src = tracks.find((t) => t.index === s.track);
    const out = s.op.op === 'web_vtt' ? 'WebVTT sidecar' : s.op.op === 'client_render' ? 'rendered here' : 'burned into video';
    return [src ? codecShort(src.codec) : `#${s.track}`, out] as const;
  });
  const engineLabel =
    engine === 'hls.js' ? `hls.js ${hlsVersion ?? ''} · MSE` : engine === 'native-hls' ? 'Native HLS' : engine === 'progressive' ? 'Progressive file' : '—';
  const seekP50 = median(metrics.seekSamples);
  const reasons = res.explain.length > 0 ? res.explain : [label === 'Direct play' ? 'Nothing needed changing: this device plays the file as it is.' : 'No reasons given.'];

  return (
    <section className={`${styles.stats} glass`} aria-label="Playback details" data-tone={planTone(label)}>
      <header className={styles.statsHead}>
        <span className={styles.planPill}>{label}</span>
        <button type="button" className={styles.statsClose} onClick={onClose} aria-label="Hide playback details">
          ×
        </button>
      </header>
      <ul className={styles.reasons}>
        {reasons.map((r) => (
          <li key={r}>{r}</li>
        ))}
      </ul>
      <dl className={styles.readout}>
        <dt>Path</dt>
        <dd>{pathLabel(path)}</dd>
        <dt>Player</dt>
        <dd>{engineLabel}</dd>
        <dt>Video</dt>
        <dd>
          {vIn ? `${codecShort(vIn.codec)}${vIn.hdr && vIn.hdr.format !== 'sdr' ? ` ${hdrName(vIn.hdr.format)}` : ''}` : '—'} <span className={styles.arrow}>→</span> {videoOut}
        </dd>
        {audioRows.map(([i, o], k) => (
          <FragmentRow key={`a${k}`} label={k === 0 ? 'Audio' : ''} from={i} to={o} />
        ))}
        {subRows.map(([i, o], k) => (
          <FragmentRow key={`s${k}`} label={k === 0 ? 'Subtitles' : ''} from={i} to={o} />
        ))}
        {vIn && (
          <>
            <dt>Source</dt>
            <dd>{videoLabel(vIn)}</dd>
          </>
        )}
      </dl>
      <dl className={`${styles.readout} ${styles.timings}`}>
        <dt>First frame</dt>
        <dd>{metrics.ttffMs === null ? '…' : `${formatCount(metrics.ttffMs)} ms`}</dd>
        <dt>Last seek</dt>
        <dd>
          {metrics.lastSeekMs === null ? '—' : `${formatCount(metrics.lastSeekMs)} ms`}
          {seekP50 !== null && metrics.seekSamples.length > 1 ? ` · p50 ${seekP50} ms of ${metrics.seekSamples.length}` : ''}
        </dd>
        {audioSwitch && (
          <>
            <dt>Audio switch</dt>
            <dd>
              {formatCount(audioSwitch.ms)} ms · {audioSwitch.mode === 'in-session' ? 'in session' : 'new stream'}
            </dd>
          </>
        )}
        <dt>Buffer</dt>
        <dd>{metrics.bufferAheadS.toFixed(1)} s ahead</dd>
        <dt>Frames</dt>
        <dd>
          {formatCount(metrics.droppedFrames)} dropped of {formatCount(metrics.totalFrames)}
          {metrics.stalls ? ` · ${metrics.stalls} stall${metrics.stalls === 1 ? '' : 's'}` : ''}
        </dd>
        {rendition && (
          <>
            <dt>Picture</dt>
            <dd>{rendition}</dd>
          </>
        )}
        {bandwidth !== null && (
          <>
            <dt>Throughput</dt>
            <dd>{formatBitrate(bandwidth)} (estimate)</dd>
          </>
        )}
        <dt>Server</dt>
        <dd>
          {diag
            ? diag.segments_served > 0
              ? `${diag.segments_served} segments · ${formatBytes(diag.bytes_served)}${diag.segment_ms_p50 !== null ? ` · ${formatMs(diag.segment_ms_p50)} p50` : ''}${diag.segment_ms_p95 !== null ? ` / ${formatMs(diag.segment_ms_p95)} p95` : ''}`
              : 'serving the file directly'
            : '…'}
        </dd>
        <dt>Cost</dt>
        <dd>~{formatCount(plan.est_cpu_ms_per_min)} CPU ms per minute</dd>
      </dl>
      <p className={styles.statsFoot}>
        Session {res.session_id} · {metrics.precise ? 'frame-accurate timings' : 'approximate timings (no requestVideoFrameCallback)'}
      </p>
    </section>
  );
}

function FragmentRow({ label, from, to }: { label: string; from: string; to: string }) {
  return (
    <>
      <dt>{label}</dt>
      <dd>
        {from} <span className={styles.arrow}>→</span> {to}
      </dd>
    </>
  );
}
