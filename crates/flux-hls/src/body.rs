//! A segment response body: header bytes plus source byte ranges, streamed
//! with positioned reads so memory per stream stays small (research/03 §4.7).

use std::io;
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use futures::Stream;

use crate::source::Source;

/// One piece of a segment body.
#[derive(Debug, Clone)]
pub enum Part {
    /// Bytes already in memory (box headers, rewritten samples, cached audio).
    Bytes(Bytes),
    /// A byte range of the source file, read when the body is streamed.
    File { offset: u64, len: u64 },
}

/// A complete segment whose exact length is known before any payload is read,
/// so responses can always carry `Content-Length` (research/03 §10).
#[derive(Debug, Clone, Default)]
pub struct SegmentBody {
    parts: Vec<Part>,
    len: u64,
    source: Option<Arc<Source>>,
}

impl SegmentBody {
    pub(crate) fn new() -> Self {
        SegmentBody::default()
    }

    /// A body held entirely in memory.
    pub fn from_bytes(b: impl Into<Bytes>) -> Self {
        let b = b.into();
        SegmentBody {
            len: b.len() as u64,
            parts: vec![Part::Bytes(b)],
            source: None,
        }
    }

    pub(crate) fn with_source(mut self, src: Arc<Source>) -> Self {
        self.source = Some(src);
        self
    }

    /// Appends a part, merging adjacent file ranges.
    pub(crate) fn push(&mut self, p: Part) {
        self.len += match &p {
            Part::Bytes(b) => b.len() as u64,
            Part::File { len, .. } => *len,
        };
        match (&p, self.parts.last_mut()) {
            (Part::File { offset, len }, Some(Part::File { offset: lo, len: ll }))
                if *lo + *ll == *offset =>
            {
                *ll += len;
            }
            _ => self.parts.push(p),
        }
    }

    /// Exact body length in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// True when the body is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The parts, in order.
    pub fn parts(&self) -> &[Part] {
        &self.parts
    }

    /// Bytes that are not yet in memory (still to be read from the source).
    pub fn file_bytes(&self) -> u64 {
        self.parts
            .iter()
            .map(|p| if let Part::File { len, .. } = p { *len } else { 0 })
            .sum()
    }

    /// Concatenates the body when it needs no file reads.
    #[cfg(test)]
    pub(crate) fn read_in_memory(&self) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(self.len as usize);
        for p in &self.parts {
            match p {
                Part::Bytes(b) => out.extend_from_slice(b),
                Part::File { .. } => return None,
            }
        }
        Some(out)
    }

    /// Reads the whole body into memory (blocking). Tools and tests only;
    /// servers should use [`SegmentBody::into_stream`].
    pub fn read_all(&self) -> io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(self.len as usize);
        for p in &self.parts {
            match p {
                Part::Bytes(b) => out.extend_from_slice(b),
                Part::File { offset, len } => {
                    let src = self
                        .source
                        .as_ref()
                        .ok_or_else(|| io::Error::other("file part without source"))?;
                    let start = out.len();
                    out.resize(start + *len as usize, 0);
                    src.read_into(*offset, &mut out[start..])?;
                }
            }
        }
        Ok(out)
    }

    /// Streams the body. File ranges are read on Tokio's blocking pool in
    /// batches of up to `chunk` bytes (several small ranges per syscall batch),
    /// so a 45 MB 4K segment never sits in memory at once.
    pub fn into_stream(self, chunk: usize) -> impl Stream<Item = io::Result<Bytes>> + Send + 'static {
        let chunk = chunk.max(16 << 10);
        let SegmentBody { parts, source, .. } = self;
        let state = (parts.into_iter().peekable(), source, None::<(u64, u64)>);
        futures::stream::unfold(state, move |(mut parts, source, mut pending)| async move {
            // `pending` = remainder of a file range larger than `chunk`.
            if pending.is_none() {
                match parts.peek() {
                    None => return None,
                    Some(Part::Bytes(_)) => {
                        let Some(Part::Bytes(b)) = parts.next() else {
                            return None;
                        };
                        return Some((Ok(b), (parts, source, None)));
                    }
                    Some(Part::File { .. }) => {}
                }
            }
            // Gather a batch of file ranges totalling ≤ chunk bytes.
            let mut batch: Vec<(u64, u64)> = Vec::new();
            let mut total = 0u64;
            if let Some((off, len)) = pending.take() {
                let take = len.min(chunk as u64);
                batch.push((off, take));
                total += take;
                if take < len {
                    pending = Some((off + take, len - take));
                }
            }
            while pending.is_none() && total < chunk as u64 {
                match parts.peek() {
                    Some(Part::File { offset, len }) => {
                        let (off, len) = (*offset, *len);
                        parts.next();
                        let room = chunk as u64 - total;
                        let take = len.min(room);
                        batch.push((off, take));
                        total += take;
                        if take < len {
                            pending = Some((off + take, len - take));
                        }
                    }
                    _ => break,
                }
            }
            let Some(src) = source.clone() else {
                return Some((
                    Err(io::Error::other("file part without source")),
                    (parts, None, None),
                ));
            };
            let read = tokio::task::spawn_blocking(move || -> io::Result<Bytes> {
                let mut buf = BytesMut::zeroed(total as usize);
                let mut at = 0usize;
                for (off, len) in batch {
                    src.read_into(off, &mut buf[at..at + len as usize])?;
                    at += len as usize;
                }
                Ok(buf.freeze())
            })
            .await
            .unwrap_or_else(|e| Err(io::Error::other(e)));
            Some((read, (parts, source, pending)))
        })
    }

    /// Collects the streamed body (async). Tools and tests.
    pub async fn into_bytes(self) -> io::Result<Bytes> {
        use futures::StreamExt;
        let len = self.len as usize;
        let mut out = BytesMut::with_capacity(len);
        let mut s = std::pin::pin!(self.into_stream(1 << 20));
        while let Some(b) = s.next().await {
            out.extend_from_slice(&b?);
        }
        Ok(out.freeze())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_adjacent_ranges() {
        let mut b = SegmentBody::new();
        b.push(Part::File { offset: 10, len: 5 });
        b.push(Part::File { offset: 15, len: 5 });
        b.push(Part::File { offset: 30, len: 1 });
        assert_eq!(b.parts().len(), 2);
        assert_eq!(b.len(), 11);
    }
}
