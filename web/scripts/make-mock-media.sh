#!/usr/bin/env bash
# Builds local HLS (fMP4/CMAF) renditions of the feature clips for the mock
# backend, laid out like flux-server's /media/{plan}/... routes. Stream copy
# only (no re-encode), so it is quick and the bytes are the clips' own.
#
#   pnpm mock-media            # → $FLUX_MOCK_MEDIA (default $FLUX_WORK/web-mock-media)
#
# The source clips are $FLUX_FEATURES (default $FLUX_TEST_MEDIA/features, made
# by tools/corpus/gen-features.sh); see CONTRIBUTING.md.
#   ONLY=multi pnpm mock-media # just the multi-audio variant (others kept)
#
# vite.config.ts serves the result at /mock-media/hls/* and the original clips
# at /mock-media/features/* (with HTTP Range), so mock playback works offline.
set -euo pipefail

die() { echo "make-mock-media: $* (see CONTRIBUTING.md)" >&2; exit 2; }
if [ -n "${FLUX_FEATURES:-}" ]; then FEATURES="$FLUX_FEATURES"
elif [ -n "${FLUX_TEST_MEDIA:-}" ]; then FEATURES="$FLUX_TEST_MEDIA/features"
else die "set FLUX_TEST_MEDIA (the test corpus) or FLUX_FEATURES (the feature clips)"; fi
if [ -n "${FLUX_MOCK_MEDIA:-}" ]; then OUT="$FLUX_MOCK_MEDIA"
elif [ -n "${FLUX_WORK:-}" ]; then OUT="$FLUX_WORK/web-mock-media"
else die "set FLUX_WORK (a scratch directory) or FLUX_MOCK_MEDIA (the output directory)"; fi
mkdir -p "$OUT/hls"

# variant  source                      CODECS attribute for the multivariant playlist
VARIANTS=(
  "h264|long_2h_360p.mkv|avc1.42C01E,mp4a.40.2|640x360"
  "hevc|hevc_main10_hdr10_hvc1.mp4|hvc1.2.4.L150.B0,mp4a.40.2|3840x2160"
  "av1|av1_opus.webm|av01.0.08M.10,opus|1920x1080"
)

json_variants=()
for v in "${VARIANTS[@]}"; do
  IFS='|' read -r name src codecs res <<<"$v"
  [ -n "${ONLY:-}" ] && [ "$ONLY" != "$name" ] && continue
  dir="$OUT/hls/$name"
  rm -rf "$dir"
  mkdir -p "$dir/v"
  echo "== $name ← $src"
  (
    cd "$dir/v"
    ffmpeg -hide_banner -loglevel error -nostdin -y -i "$FEATURES/$src" \
      -map 0:v:0 -map 0:a:0 -c copy \
      -f hls -hls_time 6 -hls_playlist_type vod -hls_segment_type fmp4 \
      -hls_fmp4_init_filename init.mp4 -hls_segment_filename '%d.m4s' index.m3u8
  )
  bw=$(ffprobe -v error -show_entries format=bit_rate -of csv=p=0 "$FEATURES/$src")
  dur=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$FEATURES/$src")
  cat >"$dir/master.m3u8" <<EOF
#EXTM3U
#EXT-X-VERSION:7
#EXT-X-INDEPENDENT-SEGMENTS
#EXT-X-STREAM-INF:BANDWIDTH=$((bw * 3 / 2)),AVERAGE-BANDWIDTH=$bw,CODECS="$codecs",RESOLUTION=$res
v/index.m3u8
EOF
  first=$(grep -m1 '\.m4s$' "$dir/v/index.m3u8")
  json_variants+=("\"$name\":{\"duration_ms\":$(printf '%.0f' "$(echo "$dur * 1000" | bc)"),\"prefetch\":[\"v/init.mp4\",\"v/$first\"]}")
done

# multi: the multi-audio clip with demuxed audio renditions laid out as
# flux-hls names them (a/{rendition}/), so the player's in-session audio
# switching has real renditions to switch between: track 1 (English AAC, copied)
# and track 2 (Japanese AC-3 5.1 → AAC 5.1, as the mock planner plans it for Chrome).
# One master per starting rendition (master-{rendition}.m3u8), since the
# session's first rendition is the playlist DEFAULT.
if [ -z "${ONLY:-}" ] || [ "$ONLY" = multi ]; then
  src="$FEATURES/h264_multi_audio_subs.mkv"
  dir="$OUT/hls/multi"
  rm -rf "$dir"
  mkdir -p "$dir/v" "$dir/a/1" "$dir/a/2-aac6"
  echo "== multi ← h264_multi_audio_subs.mkv (demuxed audio renditions)"
  hls() {
    local into="$1"
    shift
    (cd "$into" && ffmpeg -hide_banner -loglevel error -nostdin -y "$@" \
      -f hls -hls_time 6 -hls_playlist_type vod -hls_segment_type fmp4 \
      -hls_fmp4_init_filename init.mp4 -hls_segment_filename '%d.m4s' index.m3u8)
  }
  hls "$dir/v" -i "$src" -map 0:v:0 -c copy
  hls "$dir/a/1" -i "$src" -map 0:a:0 -c copy
  hls "$dir/a/2-aac6" -i "$src" -map 0:a:1 -c:a aac -ac 6 -b:a 640k
  bw=$(ffprobe -v error -show_entries format=bit_rate -of csv=p=0 "$src")
  dur=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$src")
  for def in 1 2-aac6; do
    d1=NO
    d2=NO
    [ "$def" = 1 ] && d1=YES
    [ "$def" = 2-aac6 ] && d2=YES
    {
      echo '#EXTM3U'
      echo '#EXT-X-VERSION:7'
      echo '#EXT-X-INDEPENDENT-SEGMENTS'
      echo "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a0\",NAME=\"English (AAC Stereo)\",LANGUAGE=\"en\",DEFAULT=$d1,AUTOSELECT=YES,CHANNELS=\"2\",URI=\"a/1/index.m3u8\""
      echo "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a0\",NAME=\"Japanese 5.1 (AAC 5.1)\",LANGUAGE=\"ja\",DEFAULT=$d2,AUTOSELECT=YES,CHANNELS=\"6\",URI=\"a/2-aac6/index.m3u8\""
      echo "#EXT-X-STREAM-INF:BANDWIDTH=$((bw * 3 / 2)),AVERAGE-BANDWIDTH=$bw,CODECS=\"avc1.640028,mp4a.40.2\",RESOLUTION=1920x1080,AUDIO=\"a0\""
      echo 'v/index.m3u8'
    } >"$dir/master-$def.m3u8"
  done
  json_variants+=("\"multi\":{\"duration_ms\":$(printf '%.0f' "$(echo "$dur * 1000" | bc)"),\"prefetch\":[\"v/init.mp4\",\"v/0.m4s\"],\"audio\":[\"1\",\"2-aac6\"]}")
fi

features_json=$(cd "$FEATURES" && ls *.mp4 *.webm *.mkv 2>/dev/null | sed 's/.*/"&"/' | paste -sd, -)
# Merge into an existing index so ONLY= keeps the other variants.
node -e '
  const fs = require("fs");
  const [file, variants, features] = process.argv.slice(1);
  let idx = { hls: {}, features: [] };
  try { idx = JSON.parse(fs.readFileSync(file, "utf8")); } catch {}
  Object.assign(idx.hls, JSON.parse(`{${variants}}`));
  idx.features = JSON.parse(`[${features}]`);
  fs.writeFileSync(file, JSON.stringify(idx) + "\n");
' "$OUT/index.json" "$(IFS=,; echo "${json_variants[*]}")" "$features_json"
echo "wrote $OUT/index.json"
du -sh "$OUT"
