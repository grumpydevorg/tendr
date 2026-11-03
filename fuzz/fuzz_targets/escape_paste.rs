//! A bracketed paste is forwarded byte for byte, never detaches, and leaves
//! nothing held, however the client's reads cut it.
//!
//! Input: one byte giving the number of read boundaries (mod 32), that many
//! little-endian `u16` boundaries, then the paste's body. The body is made
//! safe (no paste end, nothing at its end that could begin one) and wrapped
//! in the paste markers.

#![no_main]

use std::time::{Duration, Instant};

use libfuzzer_sys::fuzz_target;
use tendr::attach_escape::{Escape, EscapeMode, EscapeParser};

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

fuzz_target!(|data: &[u8]| {
    let Some((&count, rest)) = data.split_first() else {
        return;
    };
    let (cuts, body) = rest.split_at((usize::from(count % 32) * 2).min(rest.len()));
    let input = [PASTE_START, &marker_safe(body.to_vec()), PASTE_END].concat();
    let cuts: Vec<usize> = cuts
        .chunks_exact(2)
        .map(|c| usize::from(u16::from_le_bytes([c[0], c[1]])) % (input.len() + 1))
        .collect();
    check(&input, &reads(&input, &cuts, 0), false);
    // A start marker cut and left past its deadline is typing by design, so
    // the idle run keeps it whole.
    check(&input, &reads(&input, &cuts, PASTE_START.len()), true);
});

/// `body` with every paste end broken (its `~` dropped) and anything at its
/// end that could begin one trimmed.
fn marker_safe(mut body: Vec<u8>) -> Vec<u8> {
    while let Some(at) = body.windows(PASTE_END.len()).position(|w| w == PASTE_END) {
        body.remove(at + PASTE_END.len() - 1);
    }
    while (1..PASTE_END.len()).any(|n| body.ends_with(&PASTE_END[..n])) {
        body.pop();
    }
    body
}

/// `input` cut at `cuts`, except inside its first `whole` bytes.
fn reads<'a>(input: &'a [u8], cuts: &[usize], whole: usize) -> Vec<&'a [u8]> {
    let mut at: Vec<usize> = cuts.iter().copied().filter(|&a| a >= whole).collect();
    at.extend([0, input.len()]);
    at.sort_unstable();
    at.dedup();
    at.windows(2).map(|w| &input[w[0]..w[1]]).collect()
}

/// Feed `reads`, with an idle timeout after each if `idle`, and assert that
/// all of `input` was forwarded and nothing is held.
fn check(input: &[u8], reads: &[&[u8]], idle: bool) {
    let mut parser = EscapeParser::new(EscapeMode::CtrlBackslash);
    parser.set_bracketed_paste(true);
    let mut forward = Vec::new();
    for read in reads {
        assert_eq!(parser.feed(read, &mut forward), Escape::Continue);
        if idle {
            parser.flush_expired(Instant::now() + Duration::from_secs(3600), &mut forward);
        }
    }
    assert!(forward == input, "forwarded other bytes than the paste");
    // Nothing held: no prefix, no unfinished sequence, and the paste is
    // over, so a typed escape detaches and forwards nothing.
    let mut after = Vec::new();
    assert_eq!(parser.feed(b"\x1cd", &mut after), Escape::Detach);
    assert!(after.is_empty(), "something was held: {after:?}");
}
