#!/usr/bin/env bash
# Generate the flux "feature corpus": short synthetic clips that cover the
# container / codec / audio / subtitle / HDR cases a media server must handle.
#
# Usage: tools/corpus/gen-features.sh [OUT_DIR]
# Default OUT_DIR: $FLUX_TEST_MEDIA/features (the test corpus; see CONTRIBUTING.md)
#
# Every clip is generated from lavfi test sources, so the corpus is fully
# reproducible and free of copyrighted material. Re-running skips files that
# already exist; delete a file to regenerate it.
set -euo pipefail

if [[ $# -ge 1 ]]; then
  OUT="$1"
elif [[ -n "${FLUX_TEST_MEDIA:-}" ]]; then
  OUT="$FLUX_TEST_MEDIA/features"
else
  echo "usage: $0 OUT_DIR (or set FLUX_TEST_MEDIA to the test corpus directory; see CONTRIBUTING.md)" >&2
  exit 2
fi
mkdir -p "$OUT"
cd "$OUT"

FF=(ffmpeg -hide_banner -loglevel error -nostdin -y)
FPS="24000/1001"

log() { printf '[corpus] %s\n' "$*"; }
have() { [[ -s "$1" ]] && { log "skip $1 (exists)"; return 0; }; return 1; }

# ---- shared inputs ----------------------------------------------------------
V1080=(-f lavfi -i "testsrc2=size=1920x1080:rate=$FPS")
V2160=(-f lavfi -i "testsrc2=size=3840x2160:rate=$FPS")
V360=(-f lavfi -i "testsrc2=size=640x360:rate=$FPS")
A_STEREO=(-f lavfi -i "sine=frequency=440:sample_rate=48000")
A_51=(-f lavfi -i "aevalsrc=0.2*sin(440*2*PI*t)|0.2*sin(550*2*PI*t)|0.2*sin(660*2*PI*t)|0.1*sin(50*2*PI*t)|0.2*sin(770*2*PI*t)|0.2*sin(880*2*PI*t):c=5.1:s=48000")
A_71=(-f lavfi -i "aevalsrc=0.2*sin(440*2*PI*t)|0.2*sin(550*2*PI*t)|0.2*sin(660*2*PI*t)|0.1*sin(50*2*PI*t)|0.2*sin(770*2*PI*t)|0.2*sin(880*2*PI*t)|0.2*sin(990*2*PI*t)|0.2*sin(1100*2*PI*t):c=7.1:s=48000")

# Sidecar subtitle sources (also used as embedded tracks).
cat > subs_en.srt <<'EOF'
1
00:00:01,000 --> 00:00:04,000
Hello from flux.

2
00:00:05,000 --> 00:00:09,500
This line has <i>italics</i> and
two lines.

3
00:00:30,000 --> 00:00:34,000
Half a minute in.
EOF

cat > subs_en_styled.ass <<'EOF'
[Script Info]
ScriptType: v4.00+
PlayResX: 1920
PlayResY: 1080

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Arial,64,&H00FFFFFF,&H000000FF,&H00000000,&H64000000,0,0,0,0,100,100,0,0,1,3,1,2,40,40,60,1
Style: Sign,Georgia,80,&H0000FFFF,&H000000FF,&H00000000,&H00000000,1,0,0,0,100,100,0,0,1,2,0,8,40,40,80,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:04.00,Default,,0,0,0,,Styled dialogue line
Dialogue: 0,0:00:02.00,0:00:06.00,Sign,,0,0,0,,{\pos(960,200)\fad(300,300)}A typeset sign
Dialogue: 0,0:00:10.00,0:00:14.00,Default,,0,0,0,,{\i1}Italic{\i0} and {\c&H00FF00&}green{\c} text
EOF

cat > chapters.ffmeta <<'EOF'
;FFMETADATA1
title=flux feature corpus
[CHAPTER]
TIMEBASE=1/1000
START=0
END=20000
title=Opening
[CHAPTER]
TIMEBASE=1/1000
START=20000
END=40000
title=Middle
[CHAPTER]
TIMEBASE=1/1000
START=40000
END=60000
title=End
EOF

H264=(-c:v libx264 -preset veryfast -profile:v high -pix_fmt yuv420p -g 48 -keyint_min 48 -sc_threshold 0 -b:v 6M -maxrate 8M -bufsize 12M)
HDR10_X265="hdr10=1:repeat-headers=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):max-cll=1000,400:keyint=48:min-keyint=48:scenecut=0"
HDR_TAGS=(-color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc -color_range tv)

# ---- 1. MP4, H.264 + AAC, moov at front (direct-play baseline) -------------
have h264_aac_faststart.mp4 || { log "h264_aac_faststart.mp4";
  "${FF[@]}" "${V1080[@]}" "${A_STEREO[@]}" -t 60 "${H264[@]}" -c:a aac -b:a 160k -movflags +faststart h264_aac_faststart.mp4; }

# ---- 2. MP4 with moov at the END (probe must range-read the tail) ----------
have h264_aac_moov_end.mp4 || { log "h264_aac_moov_end.mp4";
  "${FF[@]}" "${V1080[@]}" "${A_STEREO[@]}" -t 60 "${H264[@]}" -c:a aac -b:a 160k h264_aac_moov_end.mp4; }

# ---- 3. MKV: H.264 + AAC(eng) + AC3 5.1(jpn) + SRT + ASS + chapters --------
have h264_multi_audio_subs.mkv || { log "h264_multi_audio_subs.mkv";
  "${FF[@]}" "${V1080[@]}" "${A_STEREO[@]}" "${A_51[@]}" -i subs_en.srt -i subs_en_styled.ass -i chapters.ffmeta -t 60 \
    -map 0:v -map 1:a -map 2:a -map 3:s -map 4:s -map_metadata 5 -map_chapters 5 \
    "${H264[@]}" -c:a:0 aac -b:a:0 160k -c:a:1 ac3 -b:a:1 448k -c:s:0 srt -c:s:1 ass \
    -metadata:s:a:0 language=eng -metadata:s:a:0 title="Stereo" \
    -metadata:s:a:1 language=jpn -metadata:s:a:1 title="Japanese 5.1" \
    -metadata:s:s:0 language=eng -metadata:s:s:1 language=eng -metadata:s:s:1 title="Signs & Songs" \
    -disposition:s:1 forced h264_multi_audio_subs.mkv; }

# ---- 4. MKV: HEVC Main10 HDR10 + E-AC-3 5.1 ---------------------------------
have hevc_main10_hdr10_eac3.mkv || { log "hevc_main10_hdr10_eac3.mkv";
  "${FF[@]}" "${V2160[@]}" "${A_51[@]}" -t 30 \
    -c:v libx265 -preset ultrafast -pix_fmt yuv420p10le -b:v 15M -x265-params "$HDR10_X265:log-level=error" "${HDR_TAGS[@]}" \
    -c:a eac3 -b:a 640k hevc_main10_hdr10_eac3.mkv; }

# ---- 5. MP4 (hvc1-tagged, Apple-friendly): HEVC Main10 HDR10 + AAC ----------
have hevc_main10_hdr10_hvc1.mp4 || { log "hevc_main10_hdr10_hvc1.mp4";
  "${FF[@]}" -i hevc_main10_hdr10_eac3.mkv "${A_STEREO[@]}" -map 0:v -map 1:a -t 30 -c:v copy -tag:v hvc1 -c:a aac -b:a 160k -movflags +faststart hevc_main10_hdr10_hvc1.mp4; }

# ---- 6/7. AV1 10-bit + Opus in MKV and WebM ---------------------------------
have av1_opus.mkv || { log "av1_opus.mkv";
  "${FF[@]}" "${V1080[@]}" "${A_STEREO[@]}" -t 30 -c:v libsvtav1 -preset 10 -pix_fmt yuv420p10le -g 48 -b:v 3M -c:a libopus -b:a 128k av1_opus.mkv; }
have av1_opus.webm || { log "av1_opus.webm";
  "${FF[@]}" -i av1_opus.mkv -c copy av1_opus.webm; }

# ---- 8/9/10. Lossless / premium audio that browsers cannot play -------------
have h264_truehd71.mkv || { log "h264_truehd71.mkv";
  "${FF[@]}" "${V1080[@]}" "${A_71[@]}" -t 30 "${H264[@]}" -c:a truehd -strict -2 h264_truehd71.mkv; }
have h264_dts51.mkv || { log "h264_dts51.mkv";
  "${FF[@]}" "${V1080[@]}" "${A_51[@]}" -t 30 "${H264[@]}" -c:a dca -strict -2 -b:a 1509k h264_dts51.mkv; }
have h264_flac.mkv || { log "h264_flac.mkv";
  "${FF[@]}" "${V1080[@]}" "${A_STEREO[@]}" -t 30 "${H264[@]}" -c:a flac h264_flac.mkv; }

# ---- 11/12. MPEG-TS (DVR recordings / Blu-ray-style m2ts) -------------------
have mpeg2_ac3.ts || { log "mpeg2_ac3.ts";
  "${FF[@]}" -f lavfi -i "testsrc2=size=1920x1080:rate=30000/1001" "${A_51[@]}" -t 30 -c:v mpeg2video -b:v 12M -g 15 -c:a ac3 -b:a 384k mpeg2_ac3.ts; }
have h264_aac.m2ts || { log "h264_aac.m2ts";
  "${FF[@]}" "${V1080[@]}" "${A_STEREO[@]}" -t 30 "${H264[@]}" -c:a aac -f mpegts -mpegts_m2ts_mode 1 h264_aac.m2ts; }

# ---- 13. Long GOP (10 s keyframe interval) + open GOP: segmenter stress -----
have h264_long_gop.mkv || { log "h264_long_gop.mkv";
  "${FF[@]}" "${V1080[@]}" "${A_STEREO[@]}" -t 60 -c:v libx264 -preset veryfast -pix_fmt yuv420p -g 240 -keyint_min 240 -x264-params open-gop=1 -b:v 5M -c:a aac h264_long_gop.mkv; }

# ---- 14. Legacy AVI: MPEG-4 Part 2 + MP3 ------------------------------------
have mpeg4_mp3.avi || { log "mpeg4_mp3.avi";
  "${FF[@]}" -f lavfi -i "testsrc2=size=720x400:rate=25" "${A_STEREO[@]}" -t 30 -c:v mpeg4 -vtag xvid -q:v 4 -c:a libmp3lame -b:a 128k mpeg4_mp3.avi 2>/dev/null \
  || "${FF[@]}" -f lavfi -i "testsrc2=size=720x400:rate=25" "${A_STEREO[@]}" -t 30 -c:v mpeg4 -vtag xvid -q:v 4 -c:a mp3 -b:a 128k mpeg4_mp3.avi \
  || log "WARN: no MP3 encoder, skipping AVI"; }

# ---- 15. Two-hour low-res file for seek-latency tests -----------------------
have long_2h_360p.mkv || { log "long_2h_360p.mkv (takes a few minutes)";
  "${FF[@]}" "${V360[@]}" "${A_STEREO[@]}" -t 7200 -c:v libx264 -preset ultrafast -tune zerolatency -g 48 -keyint_min 48 -sc_threshold 0 -b:v 400k -c:a aac -b:a 64k long_2h_360p.mkv; }

# ---- 16. 4K HEVC at ~60 Mbps: throughput test (UHD remux-class bitrate) -----
have hevc_4k_60mbps.mkv || { log "hevc_4k_60mbps.mkv";
  "${FF[@]}" -f lavfi -i "testsrc2=size=3840x2160:rate=$FPS,noise=alls=40:allf=t" "${A_51[@]}" -t 60 \
    -c:v hevc_videotoolbox -b:v 60M -maxrate 80M -bufsize 120M -g 48 -tag:v hvc1 -c:a eac3 -b:a 640k hevc_4k_60mbps.mkv; }

# ---- 17. "Episodes" sharing an intro at different offsets (intro detection) -
# Each episode = unique cold open + identical 20 s intro + unique body.
have intro_shared.mkv || { log "intro_shared.mkv";
  "${FF[@]}" -f lavfi -i "mandelbrot=size=1280x720:rate=$FPS" \
    -f lavfi -i "aevalsrc=0.3*sin(2*PI*(220+110*floor(mod(t*2\,4)))*t):s=48000" -t 20 \
    -c:v libx264 -preset veryfast -pix_fmt yuv420p -g 48 -c:a aac -b:a 128k intro_shared.mkv; }
for ep in 1 2 3; do
  f="show_e0${ep}_with_intro.mkv"
  have "$f" && continue
  log "$f"
  cold=$((15 * ep + 10))
  "${FF[@]}" -f lavfi -i "testsrc2=size=1280x720:rate=$FPS" -f lavfi -i "sine=frequency=$((300 + 100 * ep)):sample_rate=48000" -t "$cold" \
    -c:v libx264 -preset veryfast -pix_fmt yuv420p -g 48 -c:a aac -b:a 128k "cold_$ep.mkv"
  "${FF[@]}" -f lavfi -i "smptehdbars=size=1280x720:rate=$FPS" -f lavfi -i "anoisesrc=color=pink:amplitude=0.2:sample_rate=48000:seed=$ep" -t 60 \
    -c:v libx264 -preset veryfast -pix_fmt yuv420p -g 48 -c:a aac -b:a 128k "body_$ep.mkv"
  printf "file 'cold_%s.mkv'\nfile 'intro_shared.mkv'\nfile 'body_%s.mkv'\n" "$ep" "$ep" > "concat_$ep.txt"
  "${FF[@]}" -f concat -safe 0 -i "concat_$ep.txt" -c copy "$f"
  rm -f "cold_$ep.mkv" "body_$ep.mkv" "concat_$ep.txt"
done

# ---- manifest ---------------------------------------------------------------
log "writing manifest.json"
{
  echo "["
  first=1
  for f in *.mp4 *.mkv *.webm *.ts *.m2ts *.avi; do
    [[ -e "$f" ]] || continue
    [[ $first -eq 1 ]] || echo ","
    first=0
    printf '  {"file": "%s", "bytes": %s, "ffprobe": ' "$f" "$(stat -f %z "$f")"
    ffprobe -v error -show_entries format=format_name,duration,bit_rate:stream=index,codec_type,codec_name,profile,pix_fmt,width,height,channels,color_transfer:stream_tags=language -of json "$f" | tr -d '\n'
    printf '}'
  done
  echo
  echo "]"
} > manifest.json
log "done: $(ls -1 | wc -l | tr -d ' ') files in $OUT ($(du -sh . | cut -f1))"
