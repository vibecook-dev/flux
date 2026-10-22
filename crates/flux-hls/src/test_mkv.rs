//! Small Matroska H.264 files for unit tests, written byte by byte (no
//! ffmpeg): EBML header, Info, one video track, a cluster per GOP, no Cues
//! (the index scans the clusters).

use std::path::PathBuf;

fn vint(out: &mut Vec<u8>, n: u64) {
    let mut len = 1;
    while len < 8 && n >= (1u64 << (7 * len)) - 1 {
        len += 1;
    }
    out.extend_from_slice(&(n | (1u64 << (7 * len))).to_be_bytes()[8 - len..]);
}

fn el(id: u32, body: &[u8]) -> Vec<u8> {
    let b = id.to_be_bytes();
    let skip = b.iter().take_while(|&&x| x == 0).count();
    let mut out = b[skip..].to_vec();
    vint(&mut out, body.len() as u64);
    out.extend_from_slice(body);
    out
}

fn uint(id: u32, v: u64) -> Vec<u8> {
    let b = v.to_be_bytes();
    let skip = b.iter().take_while(|&&x| x == 0).count().min(7);
    el(id, &b[skip..])
}

/// Length-prefixed (4-byte) NAL units.
pub(crate) fn nals(units: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for u in units {
        out.extend_from_slice(&(u.len() as u32).to_be_bytes());
        out.extend_from_slice(u);
    }
    out
}

/// SPS and PPS of a 64×64 High-profile stream from libx264 (synthetic
/// test pattern): progressive, and interlaced (`frame_mbs_only_flag` 0).
pub(crate) const SPS_PROGRESSIVE: &[u8] = &[
    0x67, 0x64, 0x00, 0x0A, 0xAC, 0xD9, 0x44, 0x26, 0xC0, 0x44, 0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00,
    0x03, 0x00, 0xC8, 0x3C, 0x48, 0x96, 0x58,
];
pub(crate) const SPS_INTERLACED: &[u8] = &[
    0x67, 0x64, 0x00, 0x15, 0xAC, 0xD9, 0x44, 0x4D, 0x80, 0x88, 0x00, 0x00, 0x03, 0x00, 0x08, 0x00, 0x00,
    0x03, 0x01, 0x90, 0xF8, 0xA1, 0x4C, 0xB0,
];
pub(crate) const PPS: &[u8] = &[0x68, 0xEB, 0xE3, 0xCB, 0x22, 0xC0];

/// A 25 fps H.264 Matroska file.
pub(crate) struct TestMkv<'a> {
    pub sps: &'a [u8],
    /// The PPS of the track's `avcC` record.
    pub pps: &'a [u8],
    /// Per GOP (1 s, 25 frames, a cluster each): the parameter sets its
    /// keyframe carries in-band (none when empty).
    pub inband: Vec<Vec<&'a [u8]>>,
    /// The video's FlagInterlaced (1 interlaced, 2 progressive), if any.
    pub flag_interlaced: Option<u64>,
    /// The slice NAL units of every keyframe (empty: one IDR slice).
    pub idr: Vec<Vec<u8>>,
}

impl TestMkv<'_> {
    fn avcc(&self) -> Vec<u8> {
        let mut r = vec![1, self.sps[1], self.sps[2], self.sps[3], 0xFF, 0xE1];
        r.extend_from_slice(&(self.sps.len() as u16).to_be_bytes());
        r.extend_from_slice(self.sps);
        r.push(1);
        r.extend_from_slice(&(self.pps.len() as u16).to_be_bytes());
        r.extend_from_slice(self.pps);
        r
    }

    /// Writes the file under the temp dir; returns it and the keyframe
    /// payloads.
    pub(crate) fn write(&self, name: &str) -> (PathBuf, Vec<Vec<u8>>) {
        let mut keyframes = Vec::new();
        let mut clusters = Vec::new();
        for (g, sets) in self.inband.iter().enumerate() {
            let mut c = uint(0xE7, g as u64 * 1000);
            for f in 0..25u16 {
                let key = f == 0;
                let payload = if key {
                    let idr: &[u8] = &[0x65, 0x88, 0x84, g as u8, 0x21];
                    let mut units = sets.clone();
                    if self.idr.is_empty() {
                        units.push(idr);
                    } else {
                        units.extend(self.idr.iter().map(Vec::as_slice));
                    }
                    nals(&units)
                } else {
                    nals(&[&[0x41, 0x9A, f as u8, g as u8]])
                };
                if key {
                    keyframes.push(payload.clone());
                }
                let mut b = vec![0x81];
                b.extend_from_slice(&(f * 40).to_be_bytes());
                b.push(if key { 0x80 } else { 0x00 });
                b.extend_from_slice(&payload);
                c.extend(el(0xA3, &b));
            }
            clusters.extend(el(0x1F43_B675, &c));
        }
        let mut video = uint(0xB0, 64);
        video.extend(uint(0xBA, 64));
        if let Some(f) = self.flag_interlaced {
            video.extend(uint(0x9A, f));
        }
        let mut entry = uint(0xD7, 1);
        entry.extend(uint(0x73C5, 1));
        entry.extend(uint(0x83, 1));
        entry.extend(el(0x86, b"V_MPEG4/ISO/AVC"));
        entry.extend(el(0x63A2, &self.avcc()));
        entry.extend(uint(0x23_E383, 40_000_000));
        entry.extend(el(0xE0, &video));
        let mut info = uint(0x2A_D7B1, 1_000_000);
        info.extend(el(0x4489, &(self.inband.len() as f64 * 1000.0).to_be_bytes()));
        let mut seg = el(0x1549_A966, &info);
        seg.extend(el(0x1654_AE6B, &el(0xAE, &entry)));
        seg.extend(clusters);
        let mut file = el(0x1A45_DFA3, &el(0x4282, b"matroska"));
        file.extend(el(0x1853_8067, &seg));
        let dir = std::env::temp_dir().join(format!("flux-hls-mkv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, file).unwrap();
        (p, keyframes)
    }
}
