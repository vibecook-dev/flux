//! Audio codec configurations and first-frame headers: AAC (ASC, ADTS),
//! AC-3/E-AC-3 (`dac3`/`dec3`, sync frames), DTS core, TrueHD major sync,
//! Opus head, FLAC STREAMINFO, Vorbis identification, MPEG audio, ALAC,
//! Blu-ray LPCM.

use flux_core::Codec;

use super::AudioCfg;
use crate::bytes::{Bits, Rd};

const AAC_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

fn aac_profile(aot: u32) -> Option<&'static str> {
    Some(match aot {
        1 => "Main",
        2 => "LC",
        3 => "SSR",
        4 => "LTP",
        5 => "HE-AAC",
        29 => "HE-AACv2",
        23 => "LD",
        39 => "ELD",
        42 => "xHE-AAC",
        _ => return None,
    })
}

/// AAC channel configuration → channel count (ISO/IEC 14496-3 Table 1.19).
fn aac_channels(cc: u32) -> Option<u8> {
    match cc {
        1..=6 => Some(cc as u8),
        7 => Some(8),
        11 => Some(7),
        12 | 14 => Some(8),
        13 => Some(24),
        _ => None, // 0 = program config element
    }
}

/// `AudioSpecificConfig` (ISO/IEC 14496-3 §1.6.2.1).
pub(crate) fn parse_asc(b: &[u8]) -> Option<AudioCfg> {
    let mut r = Bits::new(b);
    let aot = |r: &mut Bits<'_>| -> Option<u32> {
        let a = r.u(5)?;
        if a == 31 {
            Some(32 + r.u(6)?)
        } else {
            Some(a)
        }
    };
    let freq = |r: &mut Bits<'_>| -> Option<u32> {
        let i = r.u(4)?;
        if i == 0xf {
            r.u(24)
        } else {
            AAC_RATES.get(i as usize).copied()
        }
    };
    let mut object_type = aot(&mut r)?;
    let mut rate = freq(&mut r)?;
    let cc = r.u(4)?;
    let mut profile = aac_profile(object_type);
    if object_type == 5 || object_type == 29 {
        // Explicit hierarchical SBR/PS signalling: the output rate is the
        // extension rate (what players and ffprobe report).
        rate = freq(&mut r)?;
        object_type = aot(&mut r)?;
        let _ = object_type;
    }
    if profile.is_none() {
        profile = aac_profile(object_type);
    }
    Some(AudioCfg {
        codec: Some(Codec::Aac),
        profile: profile.map(str::to_string),
        channels: aac_channels(cc),
        sample_rate: Some(rate),
        ..AudioCfg::default()
    })
}

/// ADTS header (first AAC frame in MPEG-TS).
pub(crate) fn parse_adts(b: &[u8]) -> Option<AudioCfg> {
    let mut r = Bits::new(b);
    if r.u(12)? != 0xfff {
        return None;
    }
    r.skip(1 + 2 + 1)?; // id, layer, protection_absent
    let profile = r.u(2)? + 1; // object type = profile + 1
    let sfi = r.u(4)?;
    r.skip(1)?;
    let cc = r.u(3)?;
    Some(AudioCfg {
        codec: Some(Codec::Aac),
        profile: aac_profile(profile).map(str::to_string),
        channels: aac_channels(cc),
        sample_rate: AAC_RATES.get(sfi as usize).copied(),
        ..AudioCfg::default()
    })
}

const AC3_RATES: [u32; 3] = [48000, 44100, 32000];
/// acmod → full-bandwidth channel count (A/52 Table 5.8).
const AC3_CHANNELS: [u8; 8] = [2, 1, 2, 3, 3, 4, 4, 5];

/// MP4 `dac3` (ETSI TS 102 366 F.4).
pub(crate) fn parse_dac3(b: &[u8]) -> Option<AudioCfg> {
    let mut r = Bits::new(b);
    let fscod = r.u(2)?;
    r.skip(5 + 3)?; // bsid, bsmod
    let acmod = r.u(3)?;
    let lfe = r.u(1)?;
    let rate_code = r.u(5)?;
    const KBPS: [u64; 19] = [
        32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640,
    ];
    Some(AudioCfg {
        codec: Some(Codec::Ac3),
        channels: Some(AC3_CHANNELS[acmod as usize] + lfe as u8),
        sample_rate: AC3_RATES.get(fscod as usize).copied(),
        bitrate: KBPS.get(rate_code as usize).map(|k| k * 1000),
        ..AudioCfg::default()
    })
}

/// MP4 `dec3` (ETSI TS 102 366 F.6). Channels include dependent substreams.
pub(crate) fn parse_dec3(b: &[u8]) -> Option<AudioCfg> {
    let mut r = Bits::new(b);
    let data_rate = r.u(13)?;
    let num_ind_sub = r.u(3)? + 1;
    let mut channels = 0u8;
    let mut rate = None;
    for i in 0..num_ind_sub {
        let fscod = r.u(2)?;
        r.skip(5 + 1 + 1 + 3)?; // bsid, reserved, asvc, bsmod
        let acmod = r.u(3)?;
        let lfe = r.u(1)? as u8;
        r.skip(3)?;
        let num_dep_sub = r.u(4)?;
        let mut extra = 0u8;
        if num_dep_sub > 0 {
            extra = chan_loc_count(r.u(9)?);
        } else {
            r.skip(1)?;
        }
        if i == 0 {
            channels = AC3_CHANNELS[acmod as usize] + lfe + extra;
            rate = AC3_RATES.get(fscod as usize).copied();
        }
    }
    Some(AudioCfg {
        codec: Some(Codec::Eac3),
        channels: Some(channels),
        sample_rate: rate,
        bitrate: Some(u64::from(data_rate) * 1000),
        ..AudioCfg::default()
    })
}

/// E-AC-3 `chan_loc` bits (TS 102 366 Table F.6.1): pairs count 2, singles 1.
/// MSB first: Lc/Rc, Lrs/Rrs, Cs, Ts, Lsd/Rsd, Lw/Rw, Lvh/Rvh, Cvh, LFE2.
fn chan_loc_count(loc: u32) -> u8 {
    const WEIGHT: [u8; 9] = [2, 2, 1, 1, 2, 2, 2, 1, 1];
    (0..9)
        .filter(|i| loc & (1 << (8 - i)) != 0)
        .map(|i| WEIGHT[i])
        .sum()
}

/// AC-3 or E-AC-3 sync frame (`0x0B77`). The bsid decides which syntax.
pub(crate) fn parse_ac3_frame(b: &[u8]) -> Option<AudioCfg> {
    let mut r = Bits::new(b);
    if r.u(16)? != 0x0b77 {
        return None;
    }
    // bsid sits at the same bit position (40) in both syntaxes.
    let bsid = (*b.get(5)? >> 3) & 0x1f;
    if bsid <= 10 {
        r.skip(16)?; // crc1
        let fscod = r.u(2)?;
        r.skip(6)?; // frmsizecod
        r.skip(5 + 3)?; // bsid, bsmod
        let acmod = r.u(3)?;
        if acmod & 1 != 0 && acmod != 1 {
            r.skip(2)?;
        }
        if acmod & 4 != 0 {
            r.skip(2)?;
        }
        if acmod == 2 {
            r.skip(2)?;
        }
        let lfe = r.u(1)? as u8;
        Some(AudioCfg {
            codec: Some(Codec::Ac3),
            channels: Some(AC3_CHANNELS[acmod as usize] + lfe),
            sample_rate: AC3_RATES.get(fscod as usize).copied(),
            ..AudioCfg::default()
        })
    } else if bsid <= 16 {
        r.skip(2 + 3 + 11)?; // strmtyp, substreamid, frmsiz
        let fscod = r.u(2)?;
        let rate = if fscod == 3 {
            [24000, 22050, 16000].get(r.u(2)? as usize).copied()
        } else {
            r.skip(2)?; // numblkscod
            AC3_RATES.get(fscod as usize).copied()
        };
        let acmod = r.u(3)?;
        let lfe = r.u(1)? as u8;
        Some(AudioCfg {
            codec: Some(Codec::Eac3),
            channels: Some(AC3_CHANNELS[acmod as usize] + lfe),
            sample_rate: rate,
            ..AudioCfg::default()
        })
    } else {
        None
    }
}

/// DTS core sync frame (big-endian 16-bit, `0x7FFE8001`), plus a DTS-HD
/// extension substream check right after the core frame for the profile.
pub(crate) fn parse_dts_frame(b: &[u8]) -> Option<AudioCfg> {
    let mut r = Bits::new(b);
    if r.u(32)? != 0x7ffe_8001 {
        return None;
    }
    r.skip(1 + 5 + 1 + 7)?; // ftype, short, cpf, nblks
    let fsize = r.u(14)? as usize + 1;
    let amode = r.u(6)?;
    let sfreq = r.u(4)?;
    r.skip(5 + 1 + 1 + 1 + 1 + 1 + 3 + 1 + 1)?; // rate … aspf
    let lff = r.u(2)?;
    const CH: [u8; 16] = [1, 2, 2, 2, 2, 3, 3, 4, 4, 5, 6, 6, 6, 7, 8, 8];
    const SR: [u32; 16] = [
        0, 8000, 16000, 32000, 0, 0, 11025, 22050, 44100, 0, 0, 12000, 24000, 48000, 0, 0,
    ];
    let lfe = u8::from(lff == 1 || lff == 2);
    let profile = match b.get(fsize..fsize + 4) {
        Some([0x64, 0x58, 0x20, 0x25]) => {
            // Substream header found: lossless (XLL) if its sync appears soon after.
            let tail = b.get(fsize..).unwrap_or(&[]);
            let xll = tail.windows(4).take(4096).any(|w| w == [0x41, 0xa2, 0x95, 0x47]);
            if xll {
                "DTS-HD MA"
            } else {
                "DTS-HD HRA"
            }
        }
        _ => "DTS",
    };
    Some(AudioCfg {
        codec: Some(Codec::Dts),
        profile: Some(profile.to_string()),
        channels: CH.get(amode as usize).map(|c| c + lfe),
        sample_rate: SR.get(sfreq as usize).copied().filter(|r| *r > 0),
        ..AudioCfg::default()
    })
}

/// TrueHD access unit with a major sync (`0xF8726FBA`), FFmpeg `mlp_parse.c`.
pub(crate) fn parse_truehd_frame(b: &[u8]) -> Option<AudioCfg> {
    // Search a little: in Blu-ray TS the TrueHD units are interleaved with an AC-3 core.
    let pos = b
        .windows(4)
        .take(64 * 1024)
        .position(|w| w == [0xf8, 0x72, 0x6f, 0xba])?;
    let mut r = Bits::new(b.get(pos + 4..)?);
    let ratebits = r.u(4)?;
    r.skip(4 + 2 + 2)?;
    let ch1 = truehd_channels(r.u(5)?);
    r.skip(2)?;
    let ch2 = truehd_channels(r.u(13)?);
    let rate = if ratebits == 0xf {
        0
    } else {
        (if ratebits & 8 != 0 { 44100 } else { 48000 }) << (ratebits & 7)
    };
    Some(AudioCfg {
        codec: Some(Codec::TrueHd),
        channels: Some(if ch2 > 0 { ch2 } else { ch1 }).filter(|c| *c > 0),
        sample_rate: Some(rate).filter(|r| *r > 0),
        ..AudioCfg::default()
    })
}

fn truehd_channels(arrangement: u32) -> u8 {
    const COUNT: [u8; 13] = [2, 1, 1, 2, 2, 2, 2, 1, 1, 2, 2, 1, 1];
    (0..13)
        .filter(|i| arrangement & (1 << i) != 0)
        .map(|i| COUNT[i])
        .sum()
}

/// Matroska `OpusHead` (RFC 7845 §5.1, little-endian). Opus always decodes
/// at 48 kHz, which is what ffprobe reports.
pub(crate) fn parse_opus_head(b: &[u8]) -> Option<AudioCfg> {
    let mut r = Rd::new(b);
    if r.take(8)? != b"OpusHead" {
        return None;
    }
    r.u8()?;
    let ch = r.u8()?;
    Some(AudioCfg {
        codec: Some(Codec::Opus),
        channels: Some(ch),
        sample_rate: Some(48000),
        ..AudioCfg::default()
    })
}

/// MP4 `dOps` (Opus-in-ISOBMFF §4.3.2, big-endian).
pub(crate) fn parse_dops(b: &[u8]) -> Option<AudioCfg> {
    let mut r = Rd::new(b);
    r.u8()?; // version
    let ch = r.u8()?;
    Some(AudioCfg {
        codec: Some(Codec::Opus),
        channels: Some(ch),
        sample_rate: Some(48000),
        ..AudioCfg::default()
    })
}

/// FLAC metadata blocks (after the `fLaC` marker or inside `dfLa`): STREAMINFO.
pub(crate) fn parse_flac_blocks(b: &[u8]) -> Option<AudioCfg> {
    let mut r = Rd::new(b);
    let hdr = r.u8()?;
    if hdr & 0x7f != 0 {
        return None; // first block must be STREAMINFO
    }
    let len = r.be24()? as usize;
    let si = r.take(len.min(34))?;
    let mut bits = Bits::new(si);
    bits.skip(16 + 16 + 24 + 24)?;
    let rate = bits.u(20)?;
    let ch = bits.u8(3)? + 1;
    let bps = bits.u8(5)? + 1;
    Some(AudioCfg {
        codec: Some(Codec::Flac),
        channels: Some(ch),
        sample_rate: Some(rate),
        bit_depth: Some(bps),
        ..AudioCfg::default()
    })
}

/// Matroska `A_FLAC` CodecPrivate: `fLaC` + metadata blocks.
pub(crate) fn parse_flac_private(b: &[u8]) -> Option<AudioCfg> {
    parse_flac_blocks(b.strip_prefix(b"fLaC")?)
}

/// Matroska `A_VORBIS` CodecPrivate: Xiph-laced headers; the first is the
/// identification header.
pub(crate) fn parse_vorbis_private(b: &[u8]) -> Option<AudioCfg> {
    let mut r = Rd::new(b);
    let n = r.u8()?;
    if n < 1 {
        return None;
    }
    // Xiph lacing size of the first packet.
    let mut first = 0usize;
    loop {
        let v = r.u8()?;
        first += usize::from(v);
        if v != 255 {
            break;
        }
    }
    for _ in 1..n {
        loop {
            if r.u8()? != 255 {
                break;
            }
        }
    }
    let id = r.take(first)?;
    let mut ir = Rd::new(id);
    if ir.u8()? != 1 || ir.take(6)? != b"vorbis" {
        return None;
    }
    ir.le32()?;
    let ch = ir.u8()?;
    let rate = ir.le32()?;
    Some(AudioCfg {
        codec: Some(Codec::Vorbis),
        channels: Some(ch),
        sample_rate: Some(rate),
        ..AudioCfg::default()
    })
}

/// MPEG-1/2/2.5 audio frame header (layer I/II/III).
pub(crate) fn parse_mpa_frame(b: &[u8]) -> Option<AudioCfg> {
    let mut r = Bits::new(b);
    if r.u(11)? != 0x7ff {
        return None;
    }
    let version = r.u(2)?; // 0 = 2.5, 2 = 2, 3 = 1
    let layer = r.u(2)?; // 1 = III, 2 = II, 3 = I
    r.skip(1 + 4)?;
    let sri = r.u(2)? as usize;
    r.skip(2)?;
    let mode = r.u(2)?;
    const SR: [[u32; 3]; 4] = [
        [11025, 12000, 8000],
        [0, 0, 0],
        [22050, 24000, 16000],
        [44100, 48000, 32000],
    ];
    let rate = *SR.get(version as usize)?.get(sri)?;
    if rate == 0 || layer == 0 {
        return None;
    }
    Some(AudioCfg {
        codec: Some(match layer {
            1 => Codec::Mp3,
            2 => Codec::Mp2,
            _ => Codec::Other,
        }),
        channels: Some(if mode == 3 { 1 } else { 2 }),
        sample_rate: Some(rate),
        ..AudioCfg::default()
    })
}

/// MP4 `alac` magic cookie (ALACSpecificConfig), after the full-box header.
pub(crate) fn parse_alac(b: &[u8]) -> Option<AudioCfg> {
    let mut r = Rd::new(b);
    r.skip(4 + 1)?; // frameLength, compatibleVersion
    let depth = r.u8()?;
    r.skip(3)?;
    let ch = r.u8()?;
    r.skip(2 + 4)?;
    let avg = r.be32()?;
    let rate = r.be32()?;
    Some(AudioCfg {
        codec: Some(Codec::Alac),
        channels: Some(ch),
        sample_rate: Some(rate),
        bit_depth: Some(depth),
        bitrate: Some(u64::from(avg)).filter(|b| *b > 0),
        ..AudioCfg::default()
    })
}

/// Blu-ray LPCM 4-byte header (stream type 0x80 under HDMV).
pub(crate) fn parse_bd_lpcm(b: &[u8]) -> Option<AudioCfg> {
    let mut r = Rd::new(b);
    r.be16()?; // payload size
    let x = r.u8()?;
    let y = r.u8()?;
    const CH: [u8; 16] = [0, 1, 0, 2, 3, 3, 4, 4, 5, 6, 7, 8, 0, 0, 0, 0];
    let rate = match x & 0xf {
        1 => 48000,
        4 => 96000,
        5 => 192000,
        _ => 0,
    };
    let depth = match y >> 6 {
        1 => 16,
        2 => 20,
        3 => 24,
        _ => 0,
    };
    Some(AudioCfg {
        codec: Some(Codec::Pcm),
        channels: Some(CH[usize::from(x >> 4)]).filter(|c| *c > 0),
        sample_rate: Some(rate).filter(|r| *r > 0),
        bit_depth: Some(depth).filter(|d| *d > 0),
        ..AudioCfg::default()
    })
}

/// Sniff an elementary audio payload of unknown type (MPEG-TS stream type
/// 0x06 without a descriptor): ADTS, AC-3/E-AC-3, DTS, MPEG audio.
pub(crate) fn sniff_frame(b: &[u8]) -> Option<AudioCfg> {
    match b {
        [0x0b, 0x77, ..] => parse_ac3_frame(b),
        [0x7f, 0xfe, 0x80, 0x01, ..] => parse_dts_frame(b),
        [0xff, x, ..] if x & 0xf6 == 0xf0 => parse_adts(b),
        [0xff, x, ..] if x & 0xe0 == 0xe0 => parse_mpa_frame(b),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asc_lc_stereo_48k() {
        let c = parse_asc(&[0x11, 0x90]).unwrap();
        assert_eq!(c.profile.as_deref(), Some("LC"));
        assert_eq!((c.channels, c.sample_rate), (Some(2), Some(48000)));
        // HE-AAC explicit: AOT 5, 24 kHz core, ext 48 kHz, underlying LC.
        let c = parse_asc(&[0x2b, 0x11, 0x88, 0x00]).unwrap();
        assert_eq!(c.profile.as_deref(), Some("HE-AAC"));
        assert_eq!(c.sample_rate, Some(48000));
    }

    #[test]
    fn ac3_frame_51() {
        // 0B77 | crc1 | fscod=00 frmsizecod=011100 | bsid=01000 bsmod=000 |
        // acmod=111 cmixlev=00 surmixlev=00 lfeon=1 …
        let f = [0x0b, 0x77, 0, 0, 0x1c, 0x40, 0b1110_0001, 0];
        let c = parse_ac3_frame(&f).unwrap();
        assert_eq!(c.codec, Some(Codec::Ac3));
        assert_eq!(c.sample_rate, Some(48000));
        assert_eq!(c.channels, Some(6));
        // Same header with bsid=16 parses as E-AC-3.
        let e = [0x0b, 0x77, 0x00, 0x00, 0b0011_1111, 0x80, 0, 0];
        assert_eq!(parse_ac3_frame(&e).and_then(|c| c.codec), Some(Codec::Eac3));
    }

    #[test]
    fn junk_is_none() {
        for f in [
            parse_asc as fn(&[u8]) -> Option<AudioCfg>,
            parse_adts,
            parse_dac3,
            parse_dec3,
            parse_ac3_frame,
            parse_dts_frame,
            parse_truehd_frame,
            parse_opus_head,
            parse_flac_private,
            parse_vorbis_private,
            parse_mpa_frame,
            parse_alac,
            sniff_frame,
        ] {
            assert!(f(&[]).is_none());
            let _ = f(&[0xff; 3]);
        }
    }
}
