//! Framing a pipe stream into `output.log` records: a byte reader with
//! bounded records and optional carriage-return framing.
//!
//! A record ends at LF (a preceding CR is dropped with it), at CR under
//! [`LogDelimiter::NewlineOrCr`], at [`MAX_RECORD_BYTES`] (a `fragment`), or
//! at the end of the stream (`eof`). A CR is emitted at once, never held to
//! see whether LF follows; an LF that does follow, even in the next read, is
//! swallowed, so CRLF stays one record. Fragments end on a UTF-8 character
//! boundary where the bytes allow one, so lossy decoding never turns a
//! character split across two records into two replacement characters.

use crate::log::{MAX_RECORD_BYTES, Terminator};
use crate::model::spec::LogDelimiter;

/// One stream's framing state. Holds at most [`MAX_RECORD_BYTES`].
pub(super) struct PipeFramer {
    delimiter: LogDelimiter,
    buf: Vec<u8>,
    /// The last byte seen ended a record with CR: a following LF is part of
    /// that terminator.
    last_was_cr: bool,
}

impl PipeFramer {
    pub(super) fn new(delimiter: LogDelimiter) -> Self {
        Self {
            delimiter,
            buf: Vec::new(),
            last_was_cr: false,
        }
    }

    /// Bytes read that no record holds yet.
    pub(super) fn pending(&self) -> usize {
        self.buf.len()
    }

    /// Frame `bytes`, calling `emit` with each completed record's content
    /// and terminator (`None` for LF). Stops at the first `emit` error.
    pub(super) fn feed<E>(
        &mut self,
        bytes: &[u8],
        emit: &mut impl FnMut(&[u8], Option<Terminator>) -> Result<(), E>,
    ) -> Result<(), E> {
        for &b in bytes {
            if std::mem::take(&mut self.last_was_cr) && b == b'\n' {
                continue;
            }
            match b {
                b'\n' => {
                    if self.buf.last() == Some(&b'\r') {
                        self.buf.pop();
                    }
                    emit(&self.buf, None)?;
                    self.buf.clear();
                }
                b'\r' if self.delimiter == LogDelimiter::NewlineOrCr => {
                    emit(&self.buf, Some(Terminator::Cr))?;
                    self.buf.clear();
                    self.last_was_cr = true;
                }
                _ => {
                    // Cut only once more content comes, so a line of exactly
                    // the cap and its LF stay one record. A CR (newline mode
                    // only reaches here with one) waits for the next byte: a
                    // cap-sized CRLF line is one record too.
                    if b != b'\r' && self.buf.len() >= MAX_RECORD_BYTES {
                        let cut = self.fragment_cut();
                        emit(&self.buf[..cut], Some(Terminator::Fragment))?;
                        self.buf.drain(..cut);
                    }
                    self.buf.push(b);
                }
            }
        }
        Ok(())
    }

    /// The stream closed: emit what is left as an `eof` record.
    pub(super) fn finish<E>(
        &mut self,
        emit: &mut impl FnMut(&[u8], Option<Terminator>) -> Result<(), E>,
    ) -> Result<(), E> {
        if self.buf.is_empty() {
            return Ok(());
        }
        emit(&self.buf, Some(Terminator::Eof))?;
        self.buf.clear();
        Ok(())
    }

    /// Where a full buffer is cut: before an unfinished UTF-8 sequence at
    /// its end, and before a trailing CR in newline mode (it may be the
    /// start of a CRLF, whose CR is not content). Never zero.
    fn fragment_cut(&self) -> usize {
        let buf = &self.buf;
        let mut cut = buf.len();
        // The last lead byte within three of the end, and its length.
        if let Some(lead) = (buf.len().saturating_sub(3)..buf.len())
            .rev()
            .find(|&i| buf[i] & 0b1100_0000 != 0b1000_0000)
        {
            let need = match buf[lead] {
                b if b >= 0xF0 => 4,
                b if b >= 0xE0 => 3,
                b if b >= 0xC0 => 2,
                _ => 1,
            };
            if lead + need > buf.len() && lead > 0 {
                cut = lead;
            }
        }
        if buf[cut - 1] == b'\r' && cut > 1 {
            cut -= 1;
        }
        cut
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Record = (Vec<u8>, Option<Terminator>);

    fn frame(delimiter: LogDelimiter, reads: &[&[u8]], close: bool) -> (Vec<Record>, usize) {
        let mut framer = PipeFramer::new(delimiter);
        let mut records = Vec::new();
        let mut emit = |bytes: &[u8], t: Option<Terminator>| -> Result<(), ()> {
            records.push((bytes.to_vec(), t));
            Ok(())
        };
        let mut most = 0;
        for read in reads {
            framer.feed(read, &mut emit).unwrap();
            most = most.max(framer.pending());
        }
        if close {
            framer.finish(&mut emit).unwrap();
        }
        (records, most)
    }

    fn rec(bytes: &str, t: Option<Terminator>) -> Record {
        (bytes.as_bytes().to_vec(), t)
    }

    const CR: Option<Terminator> = Some(Terminator::Cr);

    #[test]
    fn a_cr_is_a_record_at_once_even_at_the_end_of_a_read() {
        let (records, _) = frame(LogDelimiter::NewlineOrCr, &[b"10%\r"], false);
        assert_eq!(records, [rec("10%", CR)]);
    }

    #[test]
    fn crlf_is_one_record_also_split_across_reads() {
        for reads in [&[&b"a\r\nb\n"[..]][..], &[b"a\r", b"\nb\n"]] {
            let (records, _) = frame(LogDelimiter::NewlineOrCr, reads, true);
            assert_eq!(records, [rec("a", CR), rec("b", None)], "{reads:?}");
        }
    }

    #[test]
    fn two_crs_split_across_reads_are_two_records_and_cr_keeps_the_next_byte() {
        let (records, _) = frame(LogDelimiter::NewlineOrCr, &[b"a\r", b"\r", b"X\n"], true);
        assert_eq!(records, [rec("a", CR), rec("", CR), rec("X", None)]);
    }

    #[test]
    fn newline_mode_keeps_cr_in_content_and_strips_it_before_lf() {
        let (records, _) = frame(LogDelimiter::Newline, &[b"1%\r2%\r", b"\n"], true);
        assert_eq!(records, [rec("1%\r2%", None)]);
    }

    #[test]
    fn a_line_of_exactly_the_cap_is_one_record() {
        let mut line = vec![b'a'; MAX_RECORD_BYTES];
        line.push(b'\n');
        let (records, _) = frame(LogDelimiter::Newline, &[&line], true);
        assert_eq!(records, [(vec![b'a'; MAX_RECORD_BYTES], None)]);
    }

    #[test]
    fn a_crlf_line_of_exactly_the_cap_is_one_record() {
        let mut line = vec![b'a'; MAX_RECORD_BYTES];
        line.extend_from_slice(b"\r\n");
        for reads in [vec![&line[..]], vec![&line[..MAX_RECORD_BYTES + 1], b"\n"]] {
            let (records, _) = frame(LogDelimiter::Newline, &reads, true);
            assert_eq!(records, [(vec![b'a'; MAX_RECORD_BYTES], None)]);
        }
    }

    #[test]
    fn a_long_span_becomes_fragments_that_concatenate_to_the_original() {
        for delimiter in [LogDelimiter::Newline, LogDelimiter::NewlineOrCr] {
            // Multi-byte characters straddle every cap boundary somewhere.
            let span: Vec<u8> = "aé€😀".repeat(MAX_RECORD_BYTES).into_bytes();
            let reads: Vec<&[u8]> = span.chunks(4093).collect();
            let (records, most) = frame(delimiter, &reads, true);
            assert!(most <= MAX_RECORD_BYTES, "held {most} bytes");
            assert!(records.len() > 2);
            let (last, rest) = records.split_last().unwrap();
            assert_eq!(last.1, Some(Terminator::Eof));
            for (bytes, t) in rest {
                assert_eq!(*t, Some(Terminator::Fragment));
                assert!(bytes.len() <= MAX_RECORD_BYTES);
                assert!(std::str::from_utf8(bytes).is_ok(), "cut inside a character");
            }
            let joined: Vec<u8> = records.iter().flat_map(|r| r.0.clone()).collect();
            assert_eq!(joined, span);
        }
    }

    #[test]
    fn invalid_utf8_still_fragments_and_keeps_every_byte() {
        let span = vec![0xFFu8; MAX_RECORD_BYTES * 2 + 5];
        let (records, _) = frame(LogDelimiter::Newline, &[&span], true);
        let joined: Vec<u8> = records.iter().flat_map(|r| r.0.clone()).collect();
        assert_eq!(joined, span);
    }
}
