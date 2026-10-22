//! Rewriting the `moov` of an MP4/MOV clip, for tests that need a file
//! shaped like a broken one from the wild (text in a legacy code page, a
//! junk tail, signed composition offsets in a version-0 `ctts`, …).
//!
//! [`with_moov`] turns the clip's `moov` into a `free` box of the same size
//! and appends the edited `moov` at the end of the file, so `mdat` and every
//! sample offset stay where they were whatever the edit does to its size.

/// Box types whose bodies are child boxes and nothing else.
const CONTAINERS: [&[u8; 4]; 10] = [
    b"moov", b"trak", b"mdia", b"minf", b"stbl", b"edts", b"dinf", b"mvex", b"udta", b"tref",
];

/// One box of a `moov` tree: a container with its children, or a leaf with
/// its body. `raw` bytes are written as they are, with no header (junk).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bx {
    pub kind: [u8; 4],
    /// The body of a leaf (empty for a container).
    pub body: Vec<u8>,
    /// The children of a container.
    pub kids: Option<Vec<Bx>>,
    /// Bytes written verbatim instead of a box.
    pub raw: Option<Vec<u8>>,
}

impl Bx {
    /// A leaf box.
    pub fn leaf(kind: &[u8; 4], body: impl Into<Vec<u8>>) -> Bx {
        Bx {
            kind: *kind,
            body: body.into(),
            kids: None,
            raw: None,
        }
    }

    /// A container box.
    pub fn container(kind: &[u8; 4], kids: Vec<Bx>) -> Bx {
        Bx {
            kind: *kind,
            body: Vec::new(),
            kids: Some(kids),
            raw: None,
        }
    }

    /// Bytes written as they are (no header).
    pub fn raw(bytes: impl Into<Vec<u8>>) -> Bx {
        Bx {
            kind: [0; 4],
            body: Vec::new(),
            kids: None,
            raw: Some(bytes.into()),
        }
    }

    /// Parses the box at the start of `b` (its 32- or 64-bit size must fit `b`).
    pub fn parse(b: &[u8]) -> Bx {
        let (size, header) = extent(b).expect("a whole box");
        let kind: [u8; 4] = b[4..8].try_into().unwrap();
        let body = &b[header..size];
        if CONTAINERS.contains(&&kind) {
            let mut kids = Vec::new();
            let mut p = 0;
            while p < body.len() {
                match extent(&body[p..]) {
                    Some((s, _)) => {
                        kids.push(Bx::parse(&body[p..p + s]));
                        p += s;
                    }
                    None => {
                        kids.push(Bx::raw(&body[p..]));
                        break;
                    }
                }
            }
            Bx::container(&kind, kids)
        } else {
            Bx::leaf(&kind, body)
        }
    }

    /// The box as bytes (32-bit sizes).
    pub fn to_bytes(&self) -> Vec<u8> {
        if let Some(r) = &self.raw {
            return r.clone();
        }
        let body: Vec<u8> = match &self.kids {
            Some(kids) => kids.iter().flat_map(|k| k.to_bytes()).collect(),
            None => self.body.clone(),
        };
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(&self.kind);
        out.extend(body);
        out
    }

    /// Calls `f` on every box below this one, depth first, with the types of
    /// its ancestors (this box's first).
    pub fn visit(&mut self, f: &mut impl FnMut(&[[u8; 4]], &mut Bx)) {
        let mut path = vec![self.kind];
        self.visit_kids(&mut path, f);
    }

    fn visit_kids(&mut self, path: &mut Vec<[u8; 4]>, f: &mut impl FnMut(&[[u8; 4]], &mut Bx)) {
        if let Some(kids) = &mut self.kids {
            for k in kids {
                f(path, k);
                path.push(k.kind);
                k.visit_kids(path, f);
                path.pop();
            }
        }
    }

    /// The `trak` children whose `mdia/hdlr` handler type is `handler`
    /// (`vide`, `soun`).
    pub fn traks_mut(&mut self, handler: &[u8; 4]) -> Vec<&mut Bx> {
        self.kids
            .iter_mut()
            .flatten()
            .filter(|t| &t.kind == b"trak" && t.handler() == Some(*handler))
            .collect()
    }

    /// A `trak`'s handler type.
    pub fn handler(&self) -> Option<[u8; 4]> {
        let mdia = self.kids.as_ref()?.iter().find(|k| &k.kind == b"mdia")?;
        let hdlr = mdia.kids.as_ref()?.iter().find(|k| &k.kind == b"hdlr")?;
        hdlr.body.get(8..12)?.try_into().ok()
    }

    /// For an `stsd` leaf: replaces the body of the first `kind` box found
    /// in it (by its type, inside the first sample entry) with `edit`'s
    /// output, fixing the sizes of the box and of the entry.
    pub fn edit_entry_child(&mut self, kind: &[u8; 4], edit: impl FnOnce(&[u8]) -> Vec<u8>) {
        assert_eq!(&self.kind, b"stsd");
        let b = &mut self.body;
        let at = b.windows(4).position(|w| w == kind).expect("the box") - 4;
        let size = u32::from_be_bytes(b[at..at + 4].try_into().unwrap()) as usize;
        let new = edit(&b[at + 8..at + size]);
        let grow = |b: &mut [u8], p: usize, by: usize, less: usize| {
            let v = u32::from_be_bytes(b[p..p + 4].try_into().unwrap()) as usize + by - less;
            b[p..p + 4].copy_from_slice(&(v as u32).to_be_bytes());
        };
        let (by, less) = (new.len(), size - 8);
        b.splice(at + 8..at + size, new);
        grow(b, at, by, less);
        // stsd body: version/flags, entry_count, then the first entry.
        grow(b, 8, by, less);
    }

    /// The first box of type `kind` below this one (depth first).
    pub fn find_mut(&mut self, kind: &[u8; 4]) -> Option<&mut Bx> {
        for k in self.kids.iter_mut().flatten() {
            if &k.kind == kind {
                return Some(k);
            }
            if let Some(x) = k.find_mut(kind) {
                return Some(x);
            }
        }
        None
    }
}

/// (size, header length) of the box at the start of `b`, if it fits.
fn extent(b: &[u8]) -> Option<(usize, usize)> {
    let size = u32::from_be_bytes(b.get(..4)?.try_into().ok()?) as u64;
    b.get(4..8)?;
    let (size, header) = match size {
        0 => (b.len() as u64, 8),
        1 => (u64::from_be_bytes(b.get(8..16)?.try_into().ok()?), 16),
        s => (s, 8),
    };
    (size >= header as u64 && size <= b.len() as u64).then_some((size as usize, header))
}

/// Offset and size of the top-level `moov` of an MP4/MOV file.
pub fn find_moov(file: &[u8]) -> Option<(usize, usize)> {
    let mut p = 0;
    while let Some((size, _)) = extent(&file[p..]) {
        if &file[p + 4..p + 8] == b"moov" {
            return Some((p, size));
        }
        p += size;
    }
    None
}

/// `file` with its `moov` replaced by `edit`'s output (given the `moov`
/// box's bytes): the old `moov` becomes a `free` box and the new one is
/// appended, so sample offsets hold. Panics without a `moov`.
pub fn with_moov_bytes(file: &[u8], edit: impl FnOnce(Vec<u8>) -> Vec<u8>) -> Vec<u8> {
    let (at, size) = find_moov(file).expect("a moov box");
    let moov = file[at..at + size].to_vec();
    let mut out = file.to_vec();
    out[at + 4..at + 8].copy_from_slice(b"free");
    out.extend(edit(moov));
    out
}

/// [`with_moov_bytes`] through a parsed tree ([`Bx`]).
pub fn with_moov(file: &[u8], edit: impl FnOnce(&mut Bx)) -> Vec<u8> {
    with_moov_bytes(file, |b| {
        let mut moov = Bx::parse(&b);
        edit(&mut moov);
        moov.to_bytes()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_moov_round_trips_and_moves_to_the_end() {
        let hdlr = |h: &[u8; 4]| {
            let mut b = vec![0; 8];
            b.extend_from_slice(h);
            b.extend_from_slice(&[0; 12]);
            b.extend_from_slice(b"Handler\0");
            Bx::leaf(b"hdlr", b)
        };
        let trak = |h: &[u8; 4]| Bx::container(b"trak", vec![Bx::container(b"mdia", vec![hdlr(h)])]);
        let moov = Bx::container(
            b"moov",
            vec![Bx::leaf(b"mvhd", [0; 4]), trak(b"vide"), trak(b"soun")],
        );
        let mut file = Bx::leaf(b"ftyp", *b"isom").to_bytes();
        file.extend(moov.to_bytes());
        file.extend(Bx::leaf(b"mdat", [7; 5]).to_bytes());
        assert_eq!(Bx::parse(&moov.to_bytes()), moov);

        let out = with_moov(&file, |m| {
            assert_eq!(m.traks_mut(b"soun").len(), 1);
            m.kids.as_mut().unwrap().push(Bx::raw([1, 2, 3]));
        });
        // The old moov is a free box of the same size; mdat did not move.
        assert_eq!(&out[16..20], b"free");
        assert_eq!(&out[..16], &file[..16]);
        assert_eq!(&out[file.len() - 13..file.len()], &file[file.len() - 13..]);
        let (at, size) = find_moov(&out[file.len()..]).unwrap();
        assert_eq!((at, size), (0, moov.to_bytes().len() + 3));
        assert_eq!(&out[out.len() - 3..], &[1, 2, 3]);
    }
}
