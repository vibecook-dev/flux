//! Hostile input: empty, tiny, truncated, bit-flipped and garbage files must
//! return an error or a result, never panic, and never loop.

mod common;

use common::{features, fixtures, probe_mf, probe_with, MemFile, Rng};
use flux_probe::{probe_file, ContainerHint, ProbeError, Prober, Step};

fn probe_mem(data: &[u8]) -> Result<flux_core::ProbeResult, ProbeError> {
    probe_mf(&MemFile::whole(data))
}

/// Head (2 MiB) and tail (512 KiB) of every feature clip and probe fixture.
fn corpus() -> Vec<(String, MemFile)> {
    let mut out: Vec<(String, MemFile)> = Vec::new();
    for dir in [features(), fixtures()] {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            eprintln!("skipping: {} not present", dir.display());
            continue;
        };
        out.extend(
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| flux_probe::is_media_path(p))
                .filter_map(|p| {
                    Some((
                        p.file_name()?.to_string_lossy().to_string(),
                        MemFile::load(&p, 2 << 20, 512 << 10)?,
                    ))
                }),
        );
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn empty_and_tiny_files() {
    assert!(matches!(probe_mem(&[]), Err(ProbeError::Empty)));
    for n in 1..64 {
        let d = vec![0x47u8; n];
        let _ = probe_mem(&d);
    }
    assert!(probe_mem(&[0x1a, 0x45, 0xdf, 0xa3]).is_err());
    assert!(probe_mem(b"RIFF").is_err());
    assert!(probe_mem(b"\0\0\0\x08ftyp").is_err());
}

#[test]
fn hidden_four_byte_mkv_from_library() {
    let Some(p) = flux_test_media::media("library-tiny/Movies/.hidden.mkv") else {
        return;
    };
    let e = probe_file(&p).unwrap_err();
    assert!(
        matches!(e, ProbeError::Truncated { .. } | ProbeError::Malformed { .. }),
        "{e}"
    );
}

#[test]
fn garbage_is_unknown_format() {
    let mut rng = Rng(0x1234_5678);
    for _ in 0..200 {
        let n = (rng.next() % 300_000) as usize;
        let d: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
        match probe_mem(&d) {
            Err(_) => {}
            Ok(r) => panic!("garbage of {n} bytes parsed as {:?}", r.container),
        }
    }
}

/// Every prefix length class of every corpus file: header-only, mid-header,
/// mid-file. Must not panic; very short prefixes must error.
#[test]
fn truncated_prefixes() {
    let mut rng = Rng(42);
    for (name, f) in corpus() {
        let mut cuts: Vec<u64> = vec![
            0, 1, 4, 8, 12, 16, 31, 64, 100, 188, 192, 512, 1000, 4096, 65_535, 65_536, 65_537,
        ];
        cuts.extend((0..40).map(|_| rng.next() % f.size.min(2 << 20)));
        cuts.push(f.size / 2);
        cuts.push(f.size - 1);
        for cut in cuts {
            let r = probe_mf(&f.prefix(cut));
            if cut < 16 {
                assert!(r.is_err(), "{name} cut at {cut} parsed");
            }
        }
    }
}

/// Bit-flip / byte-stomp fuzzing of the header region of every corpus file
/// (the first 64 KiB, plus occasionally one tail byte).
#[test]
fn mutated_headers() {
    mutate_corpus(7, 150);
}

/// Deeper run of the same fuzzer: `cargo test -p flux-probe --test robustness -- --ignored`.
#[test]
#[ignore]
fn mutated_headers_deep() {
    mutate_corpus(0x5eed, 3000);
}

fn mutate_corpus(seed: u64, rounds: usize) {
    let mut rng = Rng(seed);
    for (_name, f) in corpus() {
        for round in 0..rounds {
            let window = f.head.len().min(if round % 3 == 0 { 512 } else { 64 * 1024 });
            let mut patch = f.head[..window].to_vec();
            let n = 1 + (rng.next() % 16) as usize;
            for _ in 0..n {
                let i = (rng.next() % window as u64) as usize;
                match rng.next() % 4 {
                    0 => patch[i] ^= 1 << (rng.next() % 8),
                    1 => patch[i] = 0xff,
                    2 => patch[i] = 0x00,
                    _ => patch[i] = rng.next() as u8,
                }
            }
            let tail_stomp = (round % 5 == 4 && !f.tail.is_empty())
                .then(|| (f.tail_off + rng.next() % f.tail.len() as u64, rng.next() as u8));
            let _ = probe_with(f.size, |o, l| {
                let mut b = f.read(o, l);
                for (k, x) in b.iter_mut().enumerate() {
                    let at = o + k as u64;
                    if (at as usize) < patch.len() {
                        *x = patch[at as usize];
                    } else if tail_stomp.is_some_and(|(t, _)| t == at) {
                        *x = tail_stomp.map_or(0, |(_, v)| v);
                    }
                }
                b
            });
        }
    }
}

/// The protocol tolerates drip-feeding (short reads) and rejects feeds that
/// do not cover the requested offset.
#[test]
fn sans_io_protocol() {
    let Some((_, f)) = corpus().into_iter().find(|(n, _)| n == "h264_aac_moov_end.mp4") else {
        return;
    };
    // Drip-feed 4 KiB at a time.
    let mut p = Prober::new(f.size, ContainerHint::NONE);
    let mut step = p.start().unwrap();
    let mut feeds = 0;
    let r = loop {
        match step {
            Step::Need { offset, .. } => {
                step = p.feed(offset, &f.read(offset, 4096)).unwrap();
                feeds += 1;
                assert!(feeds < 64);
            }
            Step::Done(r) => break r,
        }
    };
    assert_eq!(r.faststart, Some(false));
    assert_eq!(r.tracks.len(), 2);

    // A feed that misses the requested offset is a driver bug.
    let mut p = Prober::new(f.size, ContainerHint::NONE);
    let Step::Need { offset, .. } = p.start().unwrap() else {
        panic!()
    };
    assert!(matches!(
        p.feed(offset + 10, &[0; 10]),
        Err(ProbeError::BadFeed(_))
    ));
}
