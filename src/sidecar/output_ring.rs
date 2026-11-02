//! The run's PTY output ring: the latest output, shared by every viewer.
//!
//! Every output byte has an offset: how many bytes the run wrote before it.
//! Capture appends each chunk under the attach sink's lock, so offsets are
//! assigned in one place, in capture's order. A viewer is a [`Cursor`], the
//! offset of the next byte it sends; its own sender thread copies the bytes
//! at its cursor and writes them, so capture never waits for a viewer. A
//! viewer that falls a whole ring behind is evicted: the bytes it would send
//! next are gone.
//!
//! The ring keeps output only from the first cursor on, so a run nobody
//! views holds none. The offsets count from the run's start regardless.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};

use super::lock;

/// Output the ring keeps: how far a viewer may fall behind before it is
/// disconnected.
const OUTPUT_RING_BYTES: usize = 32 << 20;

/// The latest `capacity` bytes of a run's PTY output, by offset.
pub(super) struct OutputRing {
    state: Mutex<RingState>,
    /// Signalled when output is appended or a cursor is closed.
    ready: Condvar,
}

struct RingState {
    /// The output from offset `head - bytes.len()` up to `head`.
    bytes: VecDeque<u8>,
    capacity: usize,
    /// The offset of the next byte capture appends: all output so far.
    head: u64,
    /// Whether appended output is kept; set by the first cursor.
    retaining: bool,
}

impl RingState {
    fn span(&self) -> Span {
        Span {
            oldest: self.head - self.bytes.len() as u64,
            head: self.head,
        }
    }
}

/// The offsets a ring holds: from `oldest` up to, but not including, `head`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Span {
    pub(super) oldest: u64,
    pub(super) head: u64,
}

/// One viewer's place in the ring: the offset of the next byte it sends.
pub(super) struct Cursor {
    next: AtomicU64,
    closed: AtomicBool,
}

impl Cursor {
    /// Whether this cursor's viewer has to go: its sender closed it, or the
    /// ring, now holding `span`, no longer has the byte it sends next.
    pub(super) fn is_lost(&self, span: Span) -> bool {
        self.closed.load(Ordering::SeqCst) || self.next.load(Ordering::SeqCst) < span.oldest
    }
}

/// What [`OutputRing::next`] found at a cursor.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Next {
    /// The bytes from the cursor on, which start at `offset`; the cursor now
    /// follows them.
    Bytes { offset: u64, bytes: Vec<u8> },
    /// The ring no longer holds the cursor's offset.
    Evicted,
    /// The cursor was closed.
    Closed,
}

impl Default for OutputRing {
    fn default() -> Self {
        Self::with_capacity(OUTPUT_RING_BYTES)
    }
}

impl OutputRing {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            state: Mutex::new(RingState {
                bytes: VecDeque::new(),
                capacity,
                head: 0,
                retaining: false,
            }),
            ready: Condvar::new(),
        }
    }

    /// Append the next chunk of output, evicting the oldest bytes past the
    /// capacity. Returns what the ring then holds. Never blocks on a viewer.
    pub(super) fn append(&self, chunk: &[u8]) -> Span {
        let mut state = lock(&self.state);
        state.head += chunk.len() as u64;
        if !state.retaining {
            return state.span();
        }
        let capacity = state.capacity;
        let keep = &chunk[chunk.len().saturating_sub(capacity)..];
        let evict = (state.bytes.len() + keep.len()).saturating_sub(capacity);
        state.bytes.drain(..evict);
        // Grow towards the capacity, never past it: a doubling growth could
        // otherwise allocate up to twice the ring's bound.
        let needed = state.bytes.len() + keep.len();
        if needed > state.bytes.capacity() {
            let target = needed.max(state.bytes.capacity() * 2).min(capacity);
            let additional = target - state.bytes.len();
            state.bytes.reserve_exact(additional);
        }
        state.bytes.extend(keep);
        let span = state.span();
        drop(state);
        self.ready.notify_all();
        span
    }

    /// What the ring holds now. Under the attach sink's lock, which capture
    /// holds while it appends, it stays so until that lock is released.
    #[cfg(unix)]
    pub(super) fn span(&self) -> Span {
        lock(&self.state).span()
    }

    /// A cursor at `offset`, which must be one the ring holds or its head;
    /// otherwise the ring's span. The ring keeps output from now on.
    #[cfg(unix)]
    pub(super) fn cursor_at(&self, offset: u64) -> Result<std::sync::Arc<Cursor>, Span> {
        let mut state = lock(&self.state);
        let span = state.span();
        if offset < span.oldest || offset > span.head {
            return Err(span);
        }
        state.retaining = true;
        Ok(std::sync::Arc::new(Cursor {
            next: AtomicU64::new(offset),
            closed: AtomicBool::new(false),
        }))
    }

    /// Wait for output at `cursor`, then take up to `max` bytes of it and
    /// move the cursor past them.
    #[cfg(unix)]
    pub(super) fn next(&self, cursor: &Cursor, max: usize) -> Next {
        let mut state = lock(&self.state);
        loop {
            if cursor.closed.load(Ordering::SeqCst) {
                return Next::Closed;
            }
            let at = cursor.next.load(Ordering::SeqCst);
            let span = state.span();
            if at < span.oldest {
                return Next::Evicted;
            }
            if at < span.head {
                // Both fit in usize: they are within `bytes`.
                let start = (at - span.oldest) as usize;
                let end = start + max.min((span.head - at) as usize);
                let (front, back) = state.bytes.as_slices();
                let mut out = Vec::with_capacity(end - start);
                if start < front.len() {
                    out.extend_from_slice(&front[start..end.min(front.len())]);
                }
                if end > front.len() {
                    out.extend_from_slice(
                        &back[start.saturating_sub(front.len())..end - front.len()],
                    );
                }
                cursor.next.store(at + out.len() as u64, Ordering::SeqCst);
                return Next::Bytes {
                    offset: at,
                    bytes: out,
                };
            }
            state = self.ready.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Close `cursor`, waking its sender if it waits for output.
    #[cfg(unix)]
    pub(super) fn close(&self, cursor: &Cursor) {
        cursor.closed.store(true, Ordering::SeqCst);
        // Taking the lock orders the store before a waiting sender's check,
        // so the notification cannot fall between its check and its wait.
        drop(lock(&self.state));
        self.ready.notify_all();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::sync::Arc;
    use std::time::Duration;

    /// Read every byte a cursor has, in reads of at most `max`, checking that
    /// each read starts where the last one ended.
    fn drain(ring: &OutputRing, cursor: &Cursor, max: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut at = cursor.next.load(Ordering::SeqCst);
        while cursor.next.load(Ordering::SeqCst) < ring.span().head {
            match ring.next(cursor, max) {
                Next::Bytes { offset, bytes } => {
                    assert_eq!(offset, at, "a read starts where the last ended");
                    at += bytes.len() as u64;
                    out.extend(bytes);
                }
                other => panic!("{other:?}"),
            }
        }
        out
    }

    #[test]
    fn a_ring_nobody_views_keeps_nothing_but_counts_offsets() {
        let ring = OutputRing::with_capacity(16);
        assert_eq!(ring.append(b"hello"), Span { oldest: 5, head: 5 });
        assert_eq!(
            ring.append(b", world"),
            Span {
                oldest: 12,
                head: 12
            }
        );
        assert_eq!(
            ring.cursor_at(11).err(),
            Some(Span {
                oldest: 12,
                head: 12
            })
        );

        let cursor = ring.cursor_at(12).unwrap();
        ring.append(b"!");
        assert_eq!(
            ring.next(&cursor, 64),
            Next::Bytes {
                offset: 12,
                bytes: b"!".to_vec()
            }
        );
    }

    #[test]
    fn a_cursor_at_the_head_waits_for_output() {
        let ring = Arc::new(OutputRing::with_capacity(16));
        ring.append(b"before");
        let cursor = ring.cursor_at(6).unwrap();
        let (got, read) = std::sync::mpsc::channel();
        let reader = {
            let (ring, cursor) = (Arc::clone(&ring), Arc::clone(&cursor));
            std::thread::spawn(move || got.send(ring.next(&cursor, 64)).unwrap())
        };
        assert!(
            read.recv_timeout(Duration::from_millis(100)).is_err(),
            "a cursor at the head returned without output"
        );
        ring.append(b"after");
        assert_eq!(
            read.recv_timeout(Duration::from_secs(10)).unwrap(),
            Next::Bytes {
                offset: 6,
                bytes: b"after".to_vec()
            }
        );
        reader.join().unwrap();
    }

    #[test]
    fn closing_a_waiting_cursor_wakes_it() {
        let ring = Arc::new(OutputRing::with_capacity(16));
        let cursor = ring.cursor_at(0).unwrap();
        let reader = {
            let (ring, cursor) = (Arc::clone(&ring), Arc::clone(&cursor));
            std::thread::spawn(move || ring.next(&cursor, 64))
        };
        std::thread::sleep(Duration::from_millis(50));
        ring.close(&cursor);
        assert_eq!(reader.join().unwrap(), Next::Closed);
        assert!(cursor.is_lost(ring.span()));
    }

    #[test]
    fn a_cursor_a_whole_ring_behind_is_evicted() {
        let ring = OutputRing::with_capacity(8);
        let cursor = ring.cursor_at(0).unwrap();
        // Exactly full: the cursor's byte is still held.
        let span = ring.append(b"01234567");
        assert!(!cursor.is_lost(span));
        // One more byte evicts it.
        let span = ring.append(b"8");
        assert_eq!(span, Span { oldest: 1, head: 9 });
        assert!(cursor.is_lost(span));
        assert_eq!(ring.next(&cursor, 64), Next::Evicted);
    }

    proptest! {
        /// Whatever the chunking, the ring holds the last `capacity` bytes
        /// of the output at their offsets, and a cursor anywhere in it reads
        /// exactly the output from there to the head, in reads of any size.
        #[test]
        fn offsets_are_contiguous_across_chunks(
            capacity in 1usize..64,
            before in proptest::collection::vec(any::<u8>(), 0..32),
            chunks in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..24), 0..12),
            at in any::<proptest::sample::Index>(),
            max in 1usize..40,
        ) {
            let ring = OutputRing::with_capacity(capacity);
            ring.append(&before);
            let first = ring.cursor_at(before.len() as u64).unwrap();
            for chunk in &chunks {
                ring.append(chunk);
            }
            let output: Vec<u8> = chunks.concat();
            let total = (before.len() + output.len()) as u64;
            let oldest = total - output.len().min(capacity) as u64;
            let span = ring.span();
            prop_assert_eq!(span, Span { oldest, head: total });
            prop_assert_eq!(first.is_lost(span), output.len() > capacity);

            let offset = oldest + at.index((total - oldest) as usize + 1) as u64;
            let cursor = ring.cursor_at(offset).unwrap();
            let from = (offset - before.len() as u64) as usize;
            prop_assert_eq!(drain(&ring, &cursor, max), &output[from..]);
            if oldest > 0 {
                prop_assert_eq!(ring.cursor_at(oldest - 1).err(), Some(span));
            }
            prop_assert_eq!(ring.cursor_at(total + 1).err(), Some(span));
        }
    }
}
