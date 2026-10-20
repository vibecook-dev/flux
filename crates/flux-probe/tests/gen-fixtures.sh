#!/usr/bin/env bash
# Edge-case fixtures for flux-probe (short synthetic clips, all lavfi sources).
#
# Usage: crates/flux-probe/tests/gen-fixtures.sh [OUT_DIR]
# Default OUT_DIR: $FLUX_TEST_MEDIA/probe-fixtures. Items 13 and 14 read
# corpus files from $FLUX_TEST_MEDIA/features and are skipped without it.
# Re-running skips files that exist. Used by tests/fixtures.rs and probe-diff.
set -euo pipefail

MEDIA="${FLUX_TEST_MEDIA:-}"
if [[ $# -eq 0 && -z "$MEDIA" ]]; then
  echo "usage: $0 OUT_DIR (or set FLUX_TEST_MEDIA)" >&2
  exit 2
fi
OUT="${1:-$MEDIA/probe-fixtures}"
mkdir -p "$OUT"
cd "$OUT"
FF=(ffmpeg -hide_banner -loglevel error -nostdin -y)
V=(-f lavfi -i "testsrc2=size=1280x720:rate=24")
A=(-f lavfi -i "sine=frequency=440:sample_rate=48000")
A51=(-f lavfi -i "aevalsrc=0.1*sin(440*2*PI*t)|0.1*sin(550*2*PI*t)|0.1*sin(660*2*PI*t)|0.1*sin(50*2*PI*t)|0.1*sin(770*2*PI*t)|0.1*sin(880*2*PI*t):c=5.1:s=48000")
MD="G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1)"
have() { [[ -s "$1" ]]; }

# 1. Live-style Matroska written to a pipe: unknown-size Segment and
#    Clusters, no Cues, no Duration in Info.
have live_unknown_size.mkv || "${FF[@]}" "${V[@]}" "${A[@]}" -t 4 -c:v libx264 -preset ultrafast -g 24 \
  -c:a aac -f matroska pipe:1 > live_unknown_size.mkv

# 2. H.264 10-bit PQ: mastering/CLL only as SEI in the first access unit
#    (avcC has no SEI), so the header pass must ask for the keyframe pass.
have h264_pq_sei.mkv || "${FF[@]}" "${V[@]}" -t 2 -c:v libx264 -preset ultrafast -pix_fmt yuv420p10le \
  -x264-params "colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:mastering-display=$MD:cll=1000,400" \
  h264_pq_sei.mkv
have h264_pq_sei.mp4 || "${FF[@]}" -i h264_pq_sei.mkv -c copy -movflags +faststart h264_pq_sei.mp4

# 3. HEVC HLG (no mastering metadata at all).
have hevc_hlg.mkv || "${FF[@]}" "${V[@]}" -t 2 -c:v libx265 -preset ultrafast -pix_fmt yuv420p10le \
  -x265-params "colorprim=bt2020:transfer=arib-std-b67:colormatrix=bt2020nc:log-level=error" hevc_hlg.mkv

# 4. Fragmented MP4 (moov + mvex, samples in moof).
have fragmented.mp4 || "${FF[@]}" "${V[@]}" "${A[@]}" -t 4 -c:v libx264 -preset ultrafast -g 24 -c:a aac \
  -movflags frag_keyframe+empty_moov+default_base_moof fragmented.mp4

# 5. QuickTime .mov (major brand 'qt  ') with 5.1 AC-3 and an English title.
have quicktime_ac3.mov || "${FF[@]}" "${V[@]}" "${A51[@]}" -t 2 -c:v libx264 -preset ultrafast -c:a ac3 \
  -metadata title="QuickTime Title" -metadata:s:a:0 language=fre quicktime_ac3.mov

# 6. MP4 with E-AC-3 5.1, Nero chapters (chpl) and a title.
printf ';FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=1000\ntitle=One\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=1000\nEND=2000\ntitle=Two\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=2000\nEND=3000\ntitle=Three\n' > chapters3.ffmeta
have mp4_eac3_chapters.mp4 || "${FF[@]}" "${V[@]}" "${A51[@]}" -i chapters3.ffmeta -t 3 -map 0:v -map 1:a \
  -map_chapters 2 -c:v libx264 -preset ultrafast -c:a eac3 -metadata title="Chaptered" -movflags +faststart \
  mp4_eac3_chapters.mp4

# 7. VP9 + Vorbis WebM.
have vp9_vorbis.webm || "${FF[@]}" "${V[@]}" "${A[@]}" -t 2 -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -b:v 1M \
  -c:a libvorbis vp9_vorbis.webm 2>/dev/null || "${FF[@]}" "${V[@]}" "${A[@]}" -t 2 -c:v libvpx-vp9 \
  -deadline realtime -cpu-used 8 -b:v 1M -c:a libopus vp9_vorbis.webm

# 8. MPEG-TS with HEVC + MP2 + E-AC-3, with ISO-639 languages.
have hevc_mp2_eac3.ts || "${FF[@]}" "${V[@]}" "${A[@]}" "${A51[@]}" -t 3 -map 0:v -map 1:a -map 2:a \
  -c:v libx265 -preset ultrafast -x265-params log-level=error -c:a:0 mp2 -c:a:1 eac3 \
  -metadata:s:a:0 language=deu -metadata:s:a:1 language=spa hevc_mp2_eac3.ts

# 9. Blu-ray style M2TS with LPCM.
have h264_lpcm.m2ts || "${FF[@]}" "${V[@]}" "${A51[@]}" -t 3 -c:v libx264 -preset ultrafast -c:a pcm_bluray \
  -mpegts_m2ts_mode 1 h264_lpcm.m2ts

# 10. AVI with H.264 + AC-3 and PCM.
have h264_ac3.avi || "${FF[@]}" "${V[@]}" "${A51[@]}" -t 2 -c:v libx264 -preset ultrafast -c:a ac3 h264_ac3.avi
have mjpeg_pcm.avi || "${FF[@]}" "${V[@]}" "${A[@]}" -t 2 -c:v mjpeg -q:v 5 -c:a pcm_s16le mjpeg_pcm.avi

# 11. Unknown-to-flux containers for the ffprobe fallback.
have flv_h264.flv || "${FF[@]}" "${V[@]}" "${A[@]}" -t 2 -c:v libx264 -preset ultrafast -c:a aac flv_h264.flv
have mpeg_ps.mpg || "${FF[@]}" "${V[@]}" "${A[@]}" -t 2 -c:v mpeg2video -c:a mp2 -f vob mpeg_ps.mpg

# 12. A .mkv that is really an MP4 (extensions lie).
have mp4_named.mkv || cp -c mp4_eac3_chapters.mp4 mp4_named.mkv 2>/dev/null || cp mp4_eac3_chapters.mp4 mp4_named.mkv

# 13. 2-hour MP4s by stream copy: a multi-MB moov (piecewise parsing, ~1 read
#     per track), moov at the end and at the front.
LONG="$MEDIA/features/long_2h_360p.mkv"
if [[ -n "$MEDIA" && -f "$LONG" ]]; then
  have long_2h_moov_end.mp4 || "${FF[@]}" -i "$LONG" -c copy long_2h_moov_end.mp4
  have long_2h_faststart.mp4 || "${FF[@]}" -i "$LONG" -c copy -movflags +faststart long_2h_faststart.mp4
fi

# 14. Matroska with ASS subtitles and two font attachments (fansub style).
FONT_DIR=/System/Library/Fonts/Supplemental
ASS="$MEDIA/features/subs_en_styled.ass"
if [[ -n "$MEDIA" && -f "$ASS" && -f "$FONT_DIR/Arial.ttf" && -f "$FONT_DIR/Georgia.ttf" ]]; then
  have mkv_fonts.mkv || "${FF[@]}" "${V[@]}" -i "$ASS" -t 2 \
    -map 0:v -map 1:s -c:v libx264 -preset ultrafast -c:s ass \
    -attach "$FONT_DIR/Arial.ttf" -attach "$FONT_DIR/Georgia.ttf" \
    -metadata:s:t:0 mimetype=font/ttf -metadata:s:t:1 mimetype=font/ttf mkv_fonts.mkv
fi

# 15. Anamorphic video (non-square pixels; Track::sample_aspect_ratio). NTSC
#     widescreen 720x480 at SAR 32:27 (displays as 853x480) in every place a
#     SAR can live, plus PAL 4:3 (SAR 16:15) through the HEVC VUI only:
#     - MKV: ffmpeg writes DisplayWidth/Height as a 16:9 DAR (DisplayUnit 3), plus the H.264 VUI;
#     - WebM: DisplayWidth/Height in pixels (853x480), VP9 has no VUI SAR;
#     - MP4: `pasp` plus the H.264 VUI;
#     - MPEG-TS: codec only (HEVC VUI; MPEG-2 sequence header aspect code 3 = 16:9 DAR);
#     - AVI: OpenDML `vprp` (plus the MPEG-4 Part 2 VOL header).
SD=(-f lavfi -i "testsrc2=size=720x480:rate=30000/1001")
PAL=(-f lavfi -i "testsrc2=size=720x576:rate=25")
have anamorphic_h264.mkv || "${FF[@]}" "${SD[@]}" -t 2 -vf setsar=32/27 -c:v libx264 -preset ultrafast \
  anamorphic_h264.mkv
have anamorphic_h264.mp4 || "${FF[@]}" -i anamorphic_h264.mkv -c copy -movflags +faststart anamorphic_h264.mp4
have anamorphic_vp9.webm || "${FF[@]}" "${SD[@]}" -t 2 -vf setsar=32/27 -c:v libvpx-vp9 -deadline realtime \
  -cpu-used 8 -b:v 500k anamorphic_vp9.webm
have anamorphic_hevc_pal.ts || "${FF[@]}" "${PAL[@]}" -t 2 -vf setsar=16/15 -c:v libx265 -preset ultrafast \
  -x265-params log-level=error anamorphic_hevc_pal.ts
have anamorphic_mpeg2.ts || "${FF[@]}" "${SD[@]}" -t 2 -c:v mpeg2video -aspect 16:9 -b:v 2M anamorphic_mpeg2.ts
have anamorphic_mpeg4.avi || "${FF[@]}" "${SD[@]}" -t 2 -vf setsar=32/27 -c:v mpeg4 -q:v 5 anamorphic_mpeg4.avi
#     Precedence: the 32:27 VUI with the container saying square (`-aspect 3:2`).
#     MKV: no Display* elements = square by default, which wins (ffprobe: 1:1).
#     MP4: the `pasp` box renamed to `free` (moov is first), so the VUI applies (32:27).
have anamorphic_vui_only.mkv || "${FF[@]}" -i anamorphic_h264.mkv -c copy -aspect 3:2 anamorphic_vui_only.mkv
have anamorphic_vui_only.mp4 || { "${FF[@]}" -i anamorphic_h264.mkv -c copy -aspect 3:2 -movflags +faststart \
  pasp_tmp.mp4 && perl -0777 -pe 's/pasp/free/' < pasp_tmp.mp4 > anamorphic_vui_only.mp4 && rm pasp_tmp.mp4; }

ls -la "$OUT"
