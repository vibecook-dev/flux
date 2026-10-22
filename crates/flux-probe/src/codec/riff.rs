//! Windows multimedia structures shared by AVI and Matroska's VfW/ACM modes:
//! `BITMAPINFOHEADER` (video FourCC) and `WAVEFORMATEX(TENSIBLE)` (audio).

use flux_core::Codec;

use super::AudioCfg;
use crate::bytes::Rd;

/// Video FourCC → codec (case-insensitive).
pub(crate) fn video_fourcc(f: [u8; 4]) -> Codec {
    let up = f.map(|c| c.to_ascii_uppercase());
    match &up {
        b"XVID" | b"DIVX" | b"DX50" | b"FMP4" | b"MP4V" | b"M4S2" | b"3IV2" | b"DIV5" | b"DIV6" | b"XVIX"
        | b"MP4S" => Codec::Mpeg4Part2,
        b"H264" | b"AVC1" | b"X264" | b"DAVC" | b"VSSH" => Codec::H264,
        b"HEVC" | b"H265" | b"HVC1" | b"HEV1" | b"X265" => Codec::Hevc,
        b"AV01" => Codec::Av1,
        b"VP80" => Codec::Vp8,
        b"VP90" => Codec::Vp9,
        b"MJPG" | b"AVRN" | b"LJPG" | b"JPGL" | b"MJLS" => Codec::Mjpeg,
        b"MPG2" | b"MPEG" | b"MX5P" | b"HDV1" | b"HDV2" | b"HDV3" => Codec::Mpeg2Video,
        b"WVC1" | b"WMV3" => Codec::Vc1,
        b"APCN" | b"APCH" | b"APCS" | b"APCO" | b"AP4H" | b"AP4X" => Codec::ProRes,
        b"WMV1" => Codec::Wmv1,
        b"WMV2" => Codec::Wmv2,
        // Microsoft MPEG-4 v1–v3 and the DivX 3 hacks of v3.
        b"MPG4" | b"MP41" | b"DIV1" | b"MP42" | b"DIV2" | b"MP43" | b"DIV3" | b"DIV4" | b"AP41" | b"COL1"
        | b"DVX3" => Codec::Msmpeg4,
        b"FLV1" => Codec::Flv1,
        b"VP60" | b"VP61" | b"VP62" | b"VP6F" | b"VP6A" => Codec::Vp6,
        b"MPG1" => Codec::Mpeg1Video,
        _ => Codec::Other,
    }
}

/// `WAVEFORMATEX.wFormatTag` → codec.
pub(crate) fn wave_format(tag: u16) -> Codec {
    match tag {
        0x0001 | 0x0003 => Codec::Pcm,
        0x0050 => Codec::Mp2,
        0x0055 => Codec::Mp3,
        0x00ff | 0x1600 | 0x1610 | 0x706d | 0x4143 | 0xa106 => Codec::Aac,
        0x2000 => Codec::Ac3,
        0x2001 => Codec::Dts,
        0x6771 | 0x674f | 0x6750 | 0x6751 => Codec::Vorbis,
        0xf1ac => Codec::Flac,
        0x704f => Codec::Opus,
        0x0160 | 0x0161 => Codec::Wma,
        0x0162 => Codec::WmaPro,
        _ => Codec::Other,
    }
}

/// `BITMAPINFOHEADER`: (fourcc, width, height, bit count).
pub(crate) fn bitmap_info(b: &[u8]) -> Option<([u8; 4], u32, u32, u16)> {
    let mut r = Rd::new(b);
    r.le32()?; // biSize
    let w = r.le32()? as i32;
    let h = r.le32()? as i32;
    r.le16()?; // biPlanes
    let bits = r.le16()?;
    let fourcc = r.fourcc()?;
    Some((fourcc, w.unsigned_abs(), h.unsigned_abs(), bits))
}

/// `WAVEFORMATEX` (+ `WAVEFORMATEXTENSIBLE` sub-format). Returns the format
/// tag (resolved from the sub-format GUID for 0xFFFE) and the audio facts.
pub(crate) fn wave_format_ex(b: &[u8]) -> Option<(u16, AudioCfg)> {
    let mut r = Rd::new(b);
    let mut tag = r.le16()?;
    let ch = r.le16()?;
    let rate = r.le32()?;
    let avg_bytes = r.le32()?;
    r.le16()?; // nBlockAlign
    let bits = r.le16().unwrap_or(0);
    if tag == 0xfffe {
        // cbSize, wValidBitsPerSample, dwChannelMask, SubFormat GUID (first 2 bytes = tag)
        if let (Some(_), Some(_), Some(_), Some(sub)) = (r.le16(), r.le16(), r.le32(), r.le16()) {
            tag = sub;
        }
    }
    let codec = wave_format(tag);
    Some((
        tag,
        AudioCfg {
            codec: Some(codec),
            channels: u8::try_from(ch).ok().filter(|c| *c > 0),
            sample_rate: Some(rate).filter(|r| *r > 0),
            bit_depth: if codec == Codec::Pcm {
                u8::try_from(bits).ok().filter(|b| *b > 0)
            } else {
                None
            },
            bitrate: Some(u64::from(avg_bytes) * 8).filter(|b| *b > 0),
            ..AudioCfg::default()
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourccs() {
        assert_eq!(video_fourcc(*b"xvid"), Codec::Mpeg4Part2);
        assert_eq!(video_fourcc(*b"H264"), Codec::H264);
        assert_eq!(video_fourcc(*b"????"), Codec::Other);
        assert_eq!(wave_format(0x55), Codec::Mp3);
        // Old Windows codecs are named, not "Other".
        assert_eq!(video_fourcc(*b"DIV3"), Codec::Msmpeg4);
        assert_eq!(video_fourcc(*b"wmv2"), Codec::Wmv2);
        assert_eq!(wave_format(0x0161), Codec::Wma);
        assert_eq!(wave_format(0x0162), Codec::WmaPro);
    }
}
