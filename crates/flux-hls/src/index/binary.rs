//! Compact, versioned binary encoding of a [`PlaybackIndex`] for the
//! catalog's per-asset BLOB (`asset.play_index`).
//!
//! JSON is fine for everything except the per-sample arrays: a 2 h MP4's
//! sample tables are ~0.5 M sizes and offsets, ~12 MB as JSON. Here the
//! index is split in two:
//!
//! ```text
//! "FXIX" | format u8 | INDEX_VERSION u16 LE
//! varint len | JSON of the index with the big arrays emptied
//! keyframes:      count, then per keyframe zigzag Δpts_ns, zigzag Δraw_ts,
//!                 zigzag Δpos, rel (0 = none, else rel + 1)
//! per MP4 track:  stts (count, delta)…, ctts (count, zigzag offset)…,
//!                 sizes as zigzag Δ, stsc (Δfirst_chunk, samples)…,
//!                 chunk offsets as zigzag Δ, sync (0 = all, else n + 1) as Δ
//! ```
//!
//! All integers are LEB128 varints. Struct changes ride in the JSON part,
//! so only array layout changes need a new format byte; `INDEX_VERSION`
//! still guards derivation changes exactly as [`PlaybackIndex::from_json`].

use super::{Keyframe, Layout, PlaybackIndex, INDEX_VERSION};
use crate::error::{HlsError, Result};

const MAGIC: &[u8; 4] = b"FXIX";
/// Bump when the binary array layout changes.
const FORMAT: u8 = 1;

/// Format tag of [`PlaybackIndex::to_bytes`] output, for callers that store
/// the bytes next to a tag (flux-catalog's `PlayIndex.format`). Changes
/// whenever the binary layout byte does.
pub const INDEX_FORMAT_BINARY: &str = "fxix1";

fn bad(detail: impl Into<String>) -> HlsError {
    HlsError::Malformed {
        container: "binary PlaybackIndex",
        detail: detail.into(),
    }
}

fn put(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn put_i(out: &mut Vec<u8>, v: i64) {
    put(out, ((v << 1) ^ (v >> 63)) as u64);
}

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

impl Reader<'_> {
    fn u(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self.b.get(self.p).ok_or_else(|| bad("truncated"))?;
            self.p += 1;
            v |= ((byte & 0x7F) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(bad("varint too long"))
    }
    fn i(&mut self) -> Result<i64> {
        let z = self.u()?;
        Ok(((z >> 1) as i64) ^ -((z & 1) as i64))
    }
    fn len(&mut self) -> Result<usize> {
        let n = self.u()? as usize;
        // Every element takes at least one byte.
        if n > self.b.len() - self.p.min(self.b.len()) + 1 && n > 0 {
            return Err(bad("array longer than the blob"));
        }
        Ok(n)
    }
    fn bytes(&mut self, n: usize) -> Result<&[u8]> {
        let s = self.b.get(self.p..self.p + n).ok_or_else(|| bad("truncated"))?;
        self.p += n;
        Ok(s)
    }
}

impl PlaybackIndex {
    /// Compact binary form for catalog storage (see the module docs).
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut head = self.clone();
        head.keyframes = Vec::new();
        if let Layout::Mp4(l) = &mut head.layout {
            for t in &mut l.tracks {
                t.stts = Vec::new();
                t.ctts = Vec::new();
                t.sizes = Vec::new();
                t.stsc = Vec::new();
                t.chunk_offsets = Vec::new();
                t.sync = None;
            }
        }
        let json = serde_json::to_vec(&head)?;
        let mut out = Vec::with_capacity(json.len() + self.keyframes.len() * 8 + 64);
        out.extend_from_slice(MAGIC);
        out.push(FORMAT);
        out.extend_from_slice(&INDEX_VERSION.to_le_bytes());
        put(&mut out, json.len() as u64);
        out.extend_from_slice(&json);

        put(&mut out, self.keyframes.len() as u64);
        let (mut pts, mut raw, mut pos) = (0i64, 0i64, 0i64);
        for k in &self.keyframes {
            put_i(&mut out, k.pts_ns - pts);
            put_i(&mut out, k.raw_ts - raw);
            put_i(&mut out, k.pos as i64 - pos);
            put(&mut out, k.rel.map(|r| r as u64 + 1).unwrap_or(0));
            (pts, raw, pos) = (k.pts_ns, k.raw_ts, k.pos as i64);
        }
        if let Layout::Mp4(l) = &self.layout {
            for t in &l.tracks {
                put(&mut out, t.stts.len() as u64);
                for &(c, d) in &t.stts {
                    put(&mut out, c as u64);
                    put(&mut out, d as u64);
                }
                put(&mut out, t.ctts.len() as u64);
                for &(c, o) in &t.ctts {
                    put(&mut out, c as u64);
                    put_i(&mut out, o);
                }
                put(&mut out, t.sizes.len() as u64);
                let mut prev = 0i64;
                for &s in &t.sizes {
                    put_i(&mut out, s as i64 - prev);
                    prev = s as i64;
                }
                put(&mut out, t.stsc.len() as u64);
                let mut prev = 0u32;
                for &(first, n) in &t.stsc {
                    put(&mut out, first.wrapping_sub(prev) as u64);
                    put(&mut out, n as u64);
                    prev = first;
                }
                put(&mut out, t.chunk_offsets.len() as u64);
                let mut prev = 0i64;
                for &o in &t.chunk_offsets {
                    put_i(&mut out, o as i64 - prev);
                    prev = o as i64;
                }
                match &t.sync {
                    None => put(&mut out, 0),
                    Some(s) => {
                        put(&mut out, s.len() as u64 + 1);
                        let mut prev = 0u32;
                        for &k in s {
                            put(&mut out, k.wrapping_sub(prev) as u64);
                            prev = k;
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// Decodes [`PlaybackIndex::to_bytes`]; rejects other format bytes and
    /// index versions (the caller rebuilds the index then).
    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        if b.len() < 7 || &b[..4] != MAGIC {
            return Err(bad("not a binary PlaybackIndex"));
        }
        if b[4] != FORMAT {
            return Err(HlsError::NotFound(format!(
                "binary index format {} != {FORMAT}",
                b[4]
            )));
        }
        let version = u16::from_le_bytes([b[5], b[6]]);
        if version != INDEX_VERSION {
            return Err(HlsError::NotFound(format!(
                "index version {version} != {INDEX_VERSION}"
            )));
        }
        let mut r = Reader { b, p: 7 };
        let n = r.len()?;
        let mut idx: PlaybackIndex = serde_json::from_slice(r.bytes(n)?)?;
        let n = r.len()?;
        let mut kf = Vec::with_capacity(n);
        let (mut pts, mut raw, mut pos) = (0i64, 0i64, 0i64);
        for _ in 0..n {
            pts += r.i()?;
            raw += r.i()?;
            pos += r.i()?;
            let rel = r.u()?;
            kf.push(Keyframe {
                pts_ns: pts,
                raw_ts: raw,
                pos: pos as u64,
                rel: (rel > 0).then(|| (rel - 1) as u32),
            });
        }
        idx.keyframes = kf;
        if let Layout::Mp4(l) = &mut idx.layout {
            for t in &mut l.tracks {
                let n = r.len()?;
                t.stts = (0..n)
                    .map(|_| Ok((r.u()? as u32, r.u()? as u32)))
                    .collect::<Result<_>>()?;
                let n = r.len()?;
                t.ctts = (0..n)
                    .map(|_| Ok((r.u()? as u32, r.i()?)))
                    .collect::<Result<_>>()?;
                let n = r.len()?;
                let mut prev = 0i64;
                t.sizes = (0..n)
                    .map(|_| {
                        prev += r.i()?;
                        Ok(prev as u32)
                    })
                    .collect::<Result<_>>()?;
                let n = r.len()?;
                let mut prev = 0u32;
                t.stsc = (0..n)
                    .map(|_| {
                        prev = prev.wrapping_add(r.u()? as u32);
                        Ok((prev, r.u()? as u32))
                    })
                    .collect::<Result<_>>()?;
                let n = r.len()?;
                let mut prev = 0i64;
                t.chunk_offsets = (0..n)
                    .map(|_| {
                        prev += r.i()?;
                        Ok(prev as u64)
                    })
                    .collect::<Result<_>>()?;
                t.sync = match r.u()? {
                    0 => None,
                    m => {
                        let n = (m - 1) as usize;
                        if n > b.len() {
                            return Err(bad("sync table longer than the blob"));
                        }
                        let mut prev = 0u32;
                        Some(
                            (0..n)
                                .map(|_| {
                                    prev = prev.wrapping_add(r.u()? as u32);
                                    Ok(prev)
                                })
                                .collect::<Result<_>>()?,
                        )
                    }
                };
            }
        }
        if r.p != b.len() {
            return Err(bad(format!("{} trailing bytes", b.len() - r.p)));
        }
        Ok(idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_roundtrip() {
        let mut v = Vec::new();
        for x in [0i64, 1, -1, 63, -64, 1 << 40, -(1 << 40), i64::MAX, i64::MIN] {
            put_i(&mut v, x);
        }
        let mut r = Reader { b: &v, p: 0 };
        for x in [0i64, 1, -1, 63, -64, 1 << 40, -(1 << 40), i64::MAX, i64::MIN] {
            assert_eq!(r.i().unwrap(), x);
        }
    }

    #[test]
    fn format_tag_follows_the_layout_byte() {
        assert_eq!(INDEX_FORMAT_BINARY, format!("fxix{FORMAT}"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(PlaybackIndex::from_bytes(b"nope").is_err());
        assert!(PlaybackIndex::from_bytes(b"FXIX\x01\x02\x00\xff\xff").is_err());
    }
}
