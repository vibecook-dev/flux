//! Rewrites a Matroska file so its audio frames are laced several to a block,
//! the way some muxers store AC-3 and E-AC-3 (ffmpeg's muxer never laces).
//!
//! Only what ffmpeg writes is handled: an EBML header, one Segment of known
//! size, and Clusters of SimpleBlocks. The rewrite keeps the EBML header,
//! Info, Tracks, Tags and every other level-1 element byte for byte, drops
//! SeekHead, Cues and Void (their positions would be stale; readers rebuild
//! the keyframe list by walking the clusters), and rebuilds each Cluster:
//! its Timestamp, then its blocks in order, with each audio track's frames
//! merged into EBML-laced blocks of `per` frames counted from the track's
//! first frame. Like mkvmerge's, the groups ignore cluster (keyframe)
//! boundaries: a laced block sits where its first frame was, carries that
//! frame's timestamp, and can hold frames that played after the next
//! keyframe.

const EBML_HEADER: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const CUES: u32 = 0x1C53_BB6B;
const VOID: u32 = 0xEC;
const CRC32: u32 = 0xBF;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_TYPE: u32 = 0x83;
const CLUSTER: u32 = 0x1F43_B675;
const CLUSTER_TIMESTAMP: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const BLOCK_GROUP: u32 = 0xA0;

/// One element: (id, header length, body length).
fn element(b: &[u8], p: usize) -> Result<(u32, usize, usize), String> {
    let mut q = p;
    let id = read_id(b, &mut q).ok_or_else(|| format!("bad element ID at {p}"))?;
    let size = read_vint(b, &mut q).ok_or_else(|| format!("bad element size at {p}"))?;
    let size = match size {
        Some(s) => s as usize,
        None => b.len() - q, // unknown size: to the end of the file
    };
    if q + size > b.len() {
        return Err(format!("element {id:#x} at {p} runs past the end"));
    }
    Ok((id, q - p, size))
}

fn read_id(b: &[u8], p: &mut usize) -> Option<u32> {
    let first = *b.get(*p)?;
    let len = first.leading_zeros() as usize + 1;
    if len > 4 {
        return None;
    }
    let bytes = b.get(*p..*p + len)?;
    *p += len;
    Some(bytes.iter().fold(0u32, |a, &x| (a << 8) | x as u32))
}

/// An unsigned EBML vint; `Some(None)` for the reserved all-ones value
/// (an unknown size).
fn read_vint(b: &[u8], p: &mut usize) -> Option<Option<u64>> {
    let first = *b.get(*p)?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 {
        return None;
    }
    let bytes = b.get(*p..*p + len)?;
    *p += len;
    let mask = if len == 8 { 0 } else { 0xFFu8 >> len };
    let v = bytes[1..]
        .iter()
        .fold((first & mask) as u64, |a, &x| (a << 8) | x as u64);
    Some((v != (1u64 << (7 * len)) - 1).then_some(v))
}

fn uint(b: &[u8]) -> u64 {
    b.iter().fold(0, |a, &x| (a << 8) | x as u64)
}

fn write_id(out: &mut Vec<u8>, id: u32) {
    let len = (4 - id.leading_zeros() as usize / 8).max(1);
    out.extend_from_slice(&id.to_be_bytes()[4 - len..]);
}

/// The shortest unsigned EBML vint holding `v` (never the all-ones value).
fn write_vint(out: &mut Vec<u8>, v: u64) {
    let len = (1..=8)
        .find(|&l| v < (1u64 << (7 * l)) - 1)
        .expect("vint too large");
    let marked = v | (1u64 << (7 * len));
    out.extend_from_slice(&marked.to_be_bytes()[8 - len..]);
}

/// A signed EBML-lacing size difference: the shortest vint length L whose
/// range ±(2^(7L−1) − 1) holds `v`, stored as `v + 2^(7L−1) − 1`.
fn write_signed_vint(out: &mut Vec<u8>, v: i64) {
    let len = (1..=8)
        .find(|&l| v.unsigned_abs() < (1u64 << (7 * l - 1)))
        .expect("lace delta too large");
    let biased = (v + (1i64 << (7 * len - 1)) - 1) as u64;
    let marked = biased | (1u64 << (7 * len));
    out.extend_from_slice(&marked.to_be_bytes()[8 - len..]);
}

fn write_element(out: &mut Vec<u8>, id: u32, body: &[u8]) {
    write_id(out, id);
    write_vint(out, body.len() as u64);
    out.extend_from_slice(body);
}

/// Track numbers of the audio tracks (TrackType 2) in a Tracks body.
fn audio_tracks(tracks: &[u8]) -> Result<Vec<u64>, String> {
    let mut out = Vec::new();
    let mut p = 0;
    while p < tracks.len() {
        let (id, hl, size) = element(tracks, p)?;
        let body = &tracks[p + hl..p + hl + size];
        if id == TRACK_ENTRY {
            let (mut number, mut kind) = (None, None);
            let mut q = 0;
            while q < body.len() {
                let (cid, chl, csize) = element(body, q)?;
                let v = uint(&body[q + chl..q + chl + csize]);
                match cid {
                    TRACK_NUMBER => number = Some(v),
                    TRACK_TYPE => kind = Some(v),
                    _ => {}
                }
                q += chl + csize;
            }
            if let (Some(n), Some(2)) = (number, kind) {
                out.push(n);
            }
        }
        p += hl + size;
    }
    Ok(out)
}

/// A SimpleBlock body: (track, relative timestamp, flags, payload) when it
/// is unlaced.
fn simple_block(b: &[u8]) -> Option<(u64, i16, u8, &[u8])> {
    let mut p = 0;
    let track = read_vint(b, &mut p)??;
    let rel = i16::from_be_bytes([*b.get(p)?, *b.get(p + 1)?]);
    let flags = *b.get(p + 2)?;
    ((flags & 0x06) == 0).then(|| (track, rel, flags, &b[p + 3..]))
}

/// One EBML-laced SimpleBlock body holding `frames` (all of one track).
fn laced_block(track: u64, rel: i16, flags: u8, frames: &[&[u8]]) -> Vec<u8> {
    let mut b = Vec::new();
    write_vint(&mut b, track);
    b.extend_from_slice(&rel.to_be_bytes());
    if frames.len() == 1 {
        b.push(flags);
        b.extend_from_slice(frames[0]);
        return b;
    }
    b.push(flags | 0x06); // lacing = 0b11: EBML
    b.push((frames.len() - 1) as u8);
    write_vint(&mut b, frames[0].len() as u64);
    for w in frames[..frames.len() - 1].windows(2) {
        write_signed_vint(&mut b, w[1].len() as i64 - w[0].len() as i64);
    }
    for f in frames {
        b.extend_from_slice(f);
    }
    b
}

/// A child of a rebuilt cluster: copied bytes, or an audio frame that is
/// either the head of a lace group (`Some(block)` once the group is
/// complete) or absorbed into an earlier group (`None`).
enum Item<'a> {
    Raw(&'a [u8]),
    Lace(Option<Vec<u8>>),
}

/// An open lace group: where its block goes (cluster, item), its first
/// frame's absolute timestamp and flags, and its frames.
struct Group<'a> {
    at: (usize, usize),
    ts: i64,
    flags: u8,
    frames: Vec<&'a [u8]>,
}

/// One cluster being rebuilt: its timestamp and children.
struct Cluster<'a> {
    ts: i64,
    timestamp: &'a [u8],
    items: Vec<Item<'a>>,
}

/// Writes a group's laced block into its head slot.
fn close(g: Option<Group<'_>>, clusters: &mut [Cluster<'_>], track: u64) -> Result<(), String> {
    let Some(g) = g else { return Ok(()) };
    let c = &mut clusters[g.at.0];
    let rel = i16::try_from(g.ts - c.ts).map_err(|_| "lace group too far from its cluster")?;
    let mut el = Vec::new();
    write_element(
        &mut el,
        SIMPLE_BLOCK,
        &laced_block(track, rel, g.flags, &g.frames),
    );
    c.items[g.at.1] = Item::Lace(Some(el));
    Ok(())
}

/// Rebuilds the Segment's clusters with each audio track's frames laced
/// `per` to a block, counting from the track's first frame (so groups run
/// across cluster boundaries, as with mkvmerge). A group's block goes into
/// the cluster of its first frame, at that frame's place.
fn rebuild_clusters<'a>(bodies: &[&'a [u8]], audio: &[u64], per: usize) -> Result<Vec<Cluster<'a>>, String> {
    let mut clusters: Vec<Cluster<'a>> = Vec::new();
    let mut groups: Vec<Option<Group<'a>>> = audio.iter().map(|_| None).collect();
    for body in bodies {
        let mut cluster = Cluster {
            ts: 0,
            timestamp: &[],
            items: Vec::new(),
        };
        let ci = clusters.len();
        let mut p = 0;
        while p < body.len() {
            let (id, hl, size) = element(body, p)?;
            let child = &body[p..p + hl + size];
            let inner = &body[p + hl..p + hl + size];
            p += hl + size;
            match id {
                CRC32 | VOID => {} // stale after the rewrite
                CLUSTER_TIMESTAMP => {
                    cluster.ts = uint(inner) as i64;
                    cluster.timestamp = child;
                }
                SIMPLE_BLOCK => {
                    let laceable = simple_block(inner)
                        .and_then(|b| audio.iter().position(|&a| a == b.0).map(|i| (i, b)));
                    let Some((i, (_, rel, flags, payload))) = laceable else {
                        cluster.items.push(Item::Raw(child));
                        continue;
                    };
                    let ts = cluster.ts + rel as i64;
                    match &mut groups[i] {
                        Some(g) if g.frames.len() < per => {
                            g.frames.push(payload);
                            cluster.items.push(Item::Lace(None));
                        }
                        open => {
                            // Full (or none yet): this frame heads a new group.
                            if let Some(full) = open.take() {
                                clusters.push(cluster);
                                close(Some(full), &mut clusters, audio[i])?;
                                cluster = clusters.pop().expect("just pushed");
                            }
                            *open = Some(Group {
                                at: (ci, cluster.items.len()),
                                ts,
                                flags,
                                frames: vec![payload],
                            });
                            cluster.items.push(Item::Lace(None));
                        }
                    }
                }
                _ => {
                    if id == BLOCK_GROUP {
                        // Not laced: later frames must not join a group
                        // placed before it.
                        clusters.push(cluster);
                        for (g, &track) in groups.iter_mut().zip(audio) {
                            close(g.take(), &mut clusters, track)?;
                        }
                        cluster = clusters.pop().expect("just pushed");
                    }
                    cluster.items.push(Item::Raw(child));
                }
            }
        }
        clusters.push(cluster);
    }
    for (g, &track) in groups.iter_mut().zip(audio) {
        close(g.take(), &mut clusters, track)?;
    }
    Ok(clusters)
}

/// See the module docs. `per` is the number of frames per laced block.
pub(crate) fn lace_audio(src: &[u8], per: usize) -> Result<Vec<u8>, String> {
    let per = per.clamp(1, 256);
    let mut out = Vec::with_capacity(src.len());
    let mut p = 0;
    while p < src.len() {
        let (id, hl, size) = element(src, p)?;
        match id {
            EBML_HEADER => out.extend_from_slice(&src[p..p + hl + size]),
            SEGMENT => {
                let seg = &src[p + hl..p + hl + size];
                // Level-1 elements in order; clusters are rebuilt together.
                let mut parts: Vec<Result<&[u8], usize>> = Vec::new();
                let mut bodies = Vec::new();
                let mut audio = Vec::new();
                let mut q = 0;
                while q < seg.len() {
                    let (cid, chl, csize) = element(seg, q)?;
                    let el = &seg[q..q + chl + csize];
                    let inner = &seg[q + chl..q + chl + csize];
                    q += chl + csize;
                    match cid {
                        SEEK_HEAD | CUES | VOID => {}
                        CLUSTER => {
                            parts.push(Err(bodies.len()));
                            bodies.push(inner);
                        }
                        TRACKS => {
                            audio = audio_tracks(inner)?;
                            parts.push(Ok(el));
                        }
                        _ => parts.push(Ok(el)),
                    }
                }
                if audio.is_empty() {
                    return Err("no audio track to lace".into());
                }
                let clusters = rebuild_clusters(&bodies, &audio, per)?;
                let mut body = Vec::with_capacity(seg.len());
                for part in parts {
                    match part {
                        Ok(el) => body.extend_from_slice(el),
                        Err(i) => {
                            let c = &clusters[i];
                            let mut cb = c.timestamp.to_vec();
                            for item in &c.items {
                                match item {
                                    Item::Raw(b) => cb.extend_from_slice(b),
                                    Item::Lace(Some(b)) => cb.extend_from_slice(b),
                                    Item::Lace(None) => {}
                                }
                            }
                            write_element(&mut body, CLUSTER, &cb);
                        }
                    }
                }
                write_element(&mut out, SEGMENT, &body);
            }
            _ => {}
        }
        p += hl + size;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vints_round_trip() {
        for v in [0u64, 1, 126, 127, 128, 16_382, 16_383, 1 << 40] {
            let mut b = Vec::new();
            write_vint(&mut b, v);
            let mut p = 0;
            assert_eq!(read_vint(&b, &mut p), Some(Some(v)), "{v}");
            assert_eq!(p, b.len());
        }
        // 127 is the 1-byte all-ones (unknown size) pattern: two bytes.
        let mut b = Vec::new();
        write_vint(&mut b, 127);
        assert_eq!(b, [0x40, 0x7F]);
        let mut b = Vec::new();
        write_id(&mut b, CLUSTER);
        write_id(&mut b, SIMPLE_BLOCK);
        assert_eq!(b, [0x1F, 0x43, 0xB6, 0x75, 0xA3]);
    }

    #[test]
    fn signed_lace_deltas() {
        let enc = |v: i64| {
            let mut b = Vec::new();
            write_signed_vint(&mut b, v);
            b
        };
        // Matroska spec: 1 byte holds −63..=63, stored with a bias of 63.
        assert_eq!(enc(0), [0xBF]);
        assert_eq!(enc(-63), [0x80]);
        assert_eq!(enc(63), [0xFE]);
        assert_eq!(enc(64), [0x60, 0x3F]); // 64 + 8191 = 0x203F, 2 bytes
    }

    fn block(track: u64, rel: i16, payload: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        write_vint(&mut b, track);
        b.extend_from_slice(&rel.to_be_bytes());
        b.push(0x80);
        b.extend_from_slice(payload);
        let mut el = Vec::new();
        write_element(&mut el, SIMPLE_BLOCK, &b);
        el
    }

    /// A file with video track 1, audio track 2 and these clusters
    /// (timestamp, SimpleBlocks), plus Cues to be dropped.
    fn file(clusters: &[(u8, Vec<Vec<u8>>)]) -> Vec<u8> {
        let mut entry = Vec::new();
        write_element(&mut entry, TRACK_NUMBER, &[2]);
        write_element(&mut entry, TRACK_TYPE, &[2]);
        let mut tracks = Vec::new();
        write_element(&mut tracks, TRACK_ENTRY, &entry);
        let mut seg = Vec::new();
        write_element(&mut seg, TRACKS, &tracks);
        write_element(&mut seg, CUES, &[]);
        for (ts, blocks) in clusters {
            let mut c = Vec::new();
            write_element(&mut c, CLUSTER_TIMESTAMP, &[*ts]);
            c.extend(blocks.concat());
            write_element(&mut seg, CLUSTER, &c);
        }
        let mut file = Vec::new();
        write_element(&mut file, EBML_HEADER, &[]);
        write_element(&mut file, SEGMENT, &seg);
        file
    }

    /// The block bodies of each cluster of a rewritten file (after its
    /// Timestamp), checking that only Tracks and Clusters remain.
    fn clusters_of(out: &[u8]) -> Vec<Vec<Vec<u8>>> {
        let (_, hl, _) = element(out, 0).unwrap();
        let (id, shl, ssize) = element(out, hl).unwrap();
        assert_eq!(id, SEGMENT);
        let seg = &out[hl + shl..hl + shl + ssize];
        let mut clusters = Vec::new();
        let mut p = 0;
        while p < seg.len() {
            let (id, hl, size) = element(seg, p).unwrap();
            let body = &seg[p + hl..p + hl + size];
            p += hl + size;
            if id == TRACKS {
                continue;
            }
            assert_eq!(id, CLUSTER);
            let mut blocks = Vec::new();
            let mut q = 0;
            while q < body.len() {
                let (cid, chl, csize) = element(body, q).unwrap();
                assert_eq!(cid == CLUSTER_TIMESTAMP, q == 0);
                if cid == SIMPLE_BLOCK {
                    blocks.push(body[q + chl..q + chl + csize].to_vec());
                }
                q += chl + csize;
            }
            clusters.push(blocks);
        }
        clusters
    }

    /// One cluster: video, 3 audio frames, video, 2 audio frames; laced 4
    /// to a block.
    #[test]
    fn laces_consecutive_audio_frames() {
        let input = file(&[(
            0,
            vec![
                block(1, 0, b"V0"),
                block(2, 0, b"a0"),
                block(2, 32, b"a1x"),
                block(2, 64, b"a2"),
                block(1, 40, b"V1"),
                block(2, 96, b"a3"),
                block(2, 128, b"a4"),
            ],
        )]);
        let out = clusters_of(&lace_audio(&input, 4).unwrap());
        // V0, [a0 a1x a2 a3] at a0's place and time, V1, [a4].
        assert_eq!(
            out,
            [vec![
                vec![0x81, 0, 0, 0x80, b'V', b'0'],
                vec![
                    0x82, 0, 0, 0x86, 3, 0x82, 0xC0, 0xBE, b'a', b'0', b'a', b'1', b'x', b'a', b'2', b'a',
                    b'3'
                ],
                vec![0x81, 0, 40, 0x80, b'V', b'1'],
                vec![0x82, 0, 128, 0x80, b'a', b'4'],
            ]]
        );
    }

    /// A group started before a keyframe's cluster takes the frames after
    /// it; the block stays in the first cluster.
    #[test]
    fn groups_run_across_clusters() {
        let input = file(&[
            (0, vec![block(2, 0, b"a0"), block(2, 32, b"a1")]),
            (
                64,
                vec![block(1, 0, b"V"), block(2, 0, b"a2"), block(2, 32, b"a3")],
            ),
        ]);
        let out = clusters_of(&lace_audio(&input, 3).unwrap());
        assert_eq!(
            out,
            [
                vec![vec![
                    0x82, 0, 0, 0x86, 2, 0x82, 0xBF, b'a', b'0', b'a', b'1', b'a', b'2'
                ]],
                vec![vec![0x81, 0, 0, 0x80, b'V'], vec![0x82, 0, 32, 0x80, b'a', b'3']],
            ]
        );
    }
}
