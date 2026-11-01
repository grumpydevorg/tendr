//! The attach client's detach escape: a two-key sequence recognized in the
//! user's keystrokes before they are forwarded to the session.
//!
//! With the default escape, `Ctrl-\` followed by `d` detaches, `Ctrl-\` twice
//! forwards one literal `Ctrl-\`, and `Ctrl-\` followed by any other key
//! forwards both unchanged. Every other key is forwarded as typed. A prefix
//! at the end of one read is held until the next key decides it. `Enter` is
//! not part of the sequence (it would submit an unfinished prompt), and
//! neither is newline-tilde-dot, which an outer `ssh -t` consumes.
//!
//! # Key encodings
//!
//! The child decides how the user's terminal encodes keys, so the escape is
//! recognized in each encoding a terminal can be switched to:
//!
//! - **legacy**: `Ctrl-\` is the byte `0x1c`, `d` is `d`;
//! - **kitty keyboard protocol** (`CSI > flags u`, `CSI = flags ; mode u`):
//!   `CSI key[:shifted[:base]] ; modifiers[:event] ; text u`, so `Ctrl-\` is
//!   `ESC [ 92 ; 5 u`, and with flag 8 `d` is `ESC [ 100 u`
//!   (<https://sw.kovidgoyal.net/kitty/keyboard-protocol/>);
//! - **xterm `modifyOtherKeys`** (`CSI > 4 ; N m`): `CSI 27 ; modifiers ;
//!   code ~`, so `Ctrl-\` is `ESC [ 27 ; 5 ; 92 ~`, and at level 3 `d` is
//!   `ESC [ 27 ; 1 ; 100 ~`; with `formatOtherKeys=1` the form is
//!   `CSI code ; modifiers u`, the same as kitty's
//!   (<https://invisible-island.net/xterm/ctlseqs/ctlseqs.html>, "Alt and
//!   Meta Keys").
//!
//! Modifiers are `1 +` a bitmask (shift 1, alt 2, ctrl 4, super or xterm's
//! meta 8, hyper 16, meta 32, caps lock 64, num lock 128). The prefix is a
//! press of key 92 with ctrl and no other modifier; the detach key is a press
//! of key 100 with no modifier. Caps lock and num lock are ignored
//! in both: they are states, not keys the user is holding.
//!
//! Whatever the encoding, the bytes forwarded are the bytes typed. The one
//! literal `Ctrl-\` a doubled prefix forwards is the second one, in the
//! encoding it arrived in, since the terminal's current mode is what the child
//! expects.
//!
//! # Events that are not keys
//!
//! With kitty's report-event-types flag (2) the terminal also reports key
//! repeats and releases (`:2` and `:3` after the modifiers), and with
//! report-all-keys (8) presses of the modifier keys themselves. Such an event
//! can arrive between the prefix and the key the user types next, so it
//! neither decides nor cancels the escape. It is held with the prefix and
//! keeps its place: another key forwards the prefix, the held events and the
//! key in the order typed. A detach or a doubled prefix discards the
//! swallowed `Ctrl-\` press, so it also discards that key's repeats and
//! releases and forwards the other held events, keeping every press the child
//! saw paired with its release.
//!
//! A repeat always follows a press already decided, so it never starts a
//! prefix: with no prefix held, a `\` repeat belongs to a press the child
//! received, and is forwarded. Without flag 2 a terminal reports a repeat as
//! a press, indistinguishable from typing the key again, so holding `Ctrl-\`
//! down there alternates between a held prefix and one forwarded `Ctrl-\`, as
//! with the legacy byte.
//!
//! # Sequences split across reads
//!
//! A terminal writes each key's sequence at once, but a read can still end
//! inside one. An unfinished `ESC [ …` is held only while it could still
//! become a prefix (or, with a prefix held, any key), and forwarded unchanged
//! as soon as it cannot. A held unfinished sequence is completed by the next
//! read, or forwarded as typed once it has waited [`PARTIAL_HOLD`] (see
//! [`EscapeParser::flush_expired`]), so a lone `Esc` key costs the child at
//! most that much latency. Only a complete prefix waits for the next key
//! without a limit.
//!
//! # Bracketed paste
//!
//! Between a bracketed paste's start (`ESC [ 200 ~`) and its end
//! (`ESC [ 201 ~`) every byte is text, forwarded as is: a pasted `Ctrl-\` is
//! not a prefix, and a pasted `Ctrl-\ d` does not detach. The markers count
//! only while the child has bracketed paste on
//! ([`EscapeParser::set_bracketed_paste`]), which is when a terminal sends
//! them; turning it off ends a paste, so stray marker bytes cannot switch the
//! escape off for good. A read that ends inside what could be the paste's end
//! holds those bytes with no deadline: the next byte decides, and a late end
//! forwarded as text would leave the escape off.

use std::time::{Duration, Instant};

/// `Ctrl-\`.
pub const PREFIX: u8 = 0x1c;
/// The key that detaches after [`PREFIX`].
pub const DETACH_KEY: u8 = b'd';
/// How long an unfinished escape sequence at the end of a read is held for a
/// later read to complete. A terminal writes a key's sequence at once, so the
/// rest normally arrives in the same or the very next read; the bound only
/// matters for a lone `Esc` key, which it delays.
pub const PARTIAL_HOLD: Duration = Duration::from_millis(20);

const ESC: u8 = 0x1b;
/// The longest escape sequence held while undecided; a longer one is
/// forwarded as an ordinary key. Every key encoding fits several times over.
const MAX_SEQUENCE: usize = 64;

/// The unshifted key codes of `\` and `d`.
const KEY_BACKSLASH: u32 = 92;
const KEY_D: u32 = 100;
/// Modifier bits (the encoded value is `1 +` these).
const MOD_CTRL: u32 = 4;
const MOD_LOCKS: u32 = 64 | 128;
/// Kitty event types.
const EVENT_PRESS: u32 = 1;
const EVENT_REPEAT: u32 = 2;
const EVENT_RELEASE: u32 = 3;
/// Bracketed paste markers.
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Which escape the client recognizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeMode {
    /// `Ctrl-\ d` detaches.
    CtrlBackslash,
    /// No escape: every byte is forwarded.
    Disabled,
}

/// What the keystrokes fed so far ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escape {
    /// Keep relaying.
    Continue,
    /// Detach now. Bytes after the detach key are discarded.
    Detach,
}

/// Incremental escape recognizer.
#[derive(Debug, Clone)]
pub struct EscapeParser {
    mode: EscapeMode,
    /// The bytes of a held prefix, in the encoding typed.
    prefix: Option<Vec<u8>>,
    /// Events that are not keys, received since the prefix, in order.
    between: Vec<u8>,
    /// `between` without releases of `Ctrl-\`: what is forwarded if the
    /// prefix is swallowed.
    between_kept: Vec<u8>,
    /// An unfinished escape sequence at the end of the last read.
    partial: Vec<u8>,
    /// When `partial` began to be held.
    partial_since: Option<Instant>,
    /// Inside a bracketed paste.
    pasting: bool,
    /// The child has bracketed paste on, so the terminal marks pastes.
    bracketed_paste: bool,
}

/// One key or event, classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    /// A press of `Ctrl-\`.
    Prefix,
    /// A press of `d`.
    Detach,
    /// A key release, a key repeat of `\`, or a modifier key: neither decides
    /// nor cancels a held prefix, and is forwarded when none is held.
    /// `of_backslash` marks a repeat or release of the `\` key, which goes
    /// with the held prefix's press.
    Incidental { of_backslash: bool },
    /// Anything else.
    Other,
}

/// The next token at the start of the input.
enum Token {
    Complete(usize, Key),
    /// An escape sequence the input ends inside.
    Unfinished,
}

impl EscapeParser {
    #[must_use]
    pub fn new(mode: EscapeMode) -> Self {
        Self {
            mode,
            prefix: None,
            between: Vec::new(),
            between_kept: Vec::new(),
            partial: Vec::new(),
            partial_since: None,
            pasting: false,
            bracketed_paste: false,
        }
    }

    /// Tell the parser whether the child has bracketed paste on. Only then is
    /// `ESC [ 200 ~` a paste's start; turning it off ends a paste.
    pub fn set_bracketed_paste(&mut self, on: bool) {
        self.bracketed_paste = on;
        if !on {
            self.pasting = false;
        }
    }

    /// Feed one read's worth of keystrokes, appending the bytes to forward to
    /// `forward`.
    pub fn feed(&mut self, input: &[u8], forward: &mut Vec<u8>) -> Escape {
        if self.mode == EscapeMode::Disabled {
            forward.extend_from_slice(input);
            return Escape::Continue;
        }
        let mut bytes = std::mem::take(&mut self.partial);
        bytes.extend_from_slice(input);
        let mut at = 0;
        while at < bytes.len() {
            if self.pasting {
                let rest = &bytes[at..];
                if let Some(end) = end_of(rest, PASTE_END) {
                    forward.extend_from_slice(&rest[..end]);
                    self.pasting = false;
                    at += end;
                    continue;
                }
                // A possible end is held without a deadline: the next byte
                // decides it, and forwarding a late end as text would leave
                // the escape off for good.
                let held = partial_end(rest);
                let text = rest.len() - held;
                forward.extend_from_slice(&rest[..text]);
                self.partial_since = None;
                self.partial = rest[text..].to_vec();
                return Escape::Continue;
            }
            let rest = &bytes[at..];
            match token(rest) {
                Token::Complete(len, key) => {
                    if self.decide(&rest[..len], key, forward) == Escape::Detach {
                        self.partial_since = None;
                        return Escape::Detach;
                    }
                    if self.bracketed_paste && &rest[..len] == PASTE_START {
                        self.pasting = true;
                    }
                    at += len;
                }
                Token::Unfinished
                    if self.prefix.is_some()
                        || could_become_prefix(rest)
                        || (self.bracketed_paste && PASTE_START.starts_with(rest)) =>
                {
                    // A sequence continued from the last read keeps its
                    // original deadline.
                    if at > 0 || self.partial_since.is_none() {
                        self.partial_since = Some(Instant::now());
                    }
                    self.partial = rest.to_vec();
                    return Escape::Continue;
                }
                Token::Unfinished => {
                    forward.extend_from_slice(rest);
                    break;
                }
            }
        }
        self.partial_since = None;
        Escape::Continue
    }

    /// When a held unfinished escape sequence is due to be forwarded, if one
    /// is held.
    #[must_use]
    pub fn partial_deadline(&self) -> Option<Instant> {
        self.partial_since.map(|since| since + PARTIAL_HOLD)
    }

    /// Forward a held unfinished escape sequence as typed if it has waited
    /// [`PARTIAL_HOLD`] by `now`. It then counts as an ordinary key, so a
    /// prefix held before it is forwarded too.
    pub fn flush_expired(&mut self, now: Instant, forward: &mut Vec<u8>) {
        if self.partial_deadline().is_none_or(|due| now < due) {
            return;
        }
        self.partial_since = None;
        let partial = std::mem::take(&mut self.partial);
        let decided = self.decide(&partial, Key::Other, forward);
        debug_assert_eq!(decided, Escape::Continue);
    }

    fn decide(&mut self, bytes: &[u8], key: Key, forward: &mut Vec<u8>) -> Escape {
        let Some(prefix) = self.prefix.take() else {
            if key == Key::Prefix {
                self.prefix = Some(bytes.to_vec());
            } else {
                forward.extend_from_slice(bytes);
            }
            return Escape::Continue;
        };
        match key {
            Key::Incidental { of_backslash } => {
                self.between.extend_from_slice(bytes);
                if !of_backslash {
                    self.between_kept.extend_from_slice(bytes);
                }
                self.prefix = Some(prefix);
                return Escape::Continue;
            }
            Key::Detach => {
                forward.append(&mut self.between_kept);
                self.between.clear();
                return Escape::Detach;
            }
            Key::Prefix => {
                forward.append(&mut self.between_kept);
                self.between.clear();
            }
            Key::Other => {
                forward.extend_from_slice(&prefix);
                forward.append(&mut self.between);
                self.between_kept.clear();
            }
        }
        forward.extend_from_slice(bytes);
        Escape::Continue
    }
}

/// The index just past the first `marker` in `input`.
fn end_of(input: &[u8], marker: &[u8]) -> Option<usize> {
    input
        .windows(marker.len())
        .position(|w| w == marker)
        .map(|i| i + marker.len())
}

/// How many bytes at the end of `input` could begin [`PASTE_END`].
fn partial_end(input: &[u8]) -> usize {
    (1..PASTE_END.len())
        .rev()
        .find(|&n| input.ends_with(&PASTE_END[..n]))
        .unwrap_or(0)
}

fn token(input: &[u8]) -> Token {
    match input[0] {
        PREFIX => Token::Complete(1, Key::Prefix),
        DETACH_KEY => Token::Complete(1, Key::Detach),
        ESC => escape_sequence(input),
        _ => Token::Complete(1, Key::Other),
    }
}

/// Tokenize input starting with `ESC`. Only CSI (`ESC [`) can encode a key
/// the escape cares about; `ESC` followed by anything else is the `Esc` key
/// (or legacy `Alt`) on its own, and the next byte is a token of its own.
fn escape_sequence(input: &[u8]) -> Token {
    match input.get(1) {
        None => return Token::Unfinished,
        Some(b'[') => {}
        Some(_) => return Token::Complete(1, Key::Other),
    }
    let limit = input.len().min(MAX_SEQUENCE);
    for (i, &byte) in input.iter().enumerate().take(limit).skip(2) {
        match byte {
            // Parameter and intermediate bytes.
            0x20..=0x3f => {}
            0x40..=0x7e => return Token::Complete(i + 1, csi_key(&input[2..i], byte)),
            // Not part of a CSI: the sequence ended before this byte.
            _ => return Token::Complete(i, Key::Other),
        }
    }
    if input.len() >= MAX_SEQUENCE {
        Token::Complete(MAX_SEQUENCE, Key::Other)
    } else {
        Token::Unfinished
    }
}

/// Classify a complete CSI from its parameter bytes and final byte.
fn csi_key(params: &[u8], final_byte: u8) -> Key {
    classify_csi(params, final_byte).unwrap_or(Key::Other)
}

fn classify_csi(params: &[u8], final_byte: u8) -> Option<Key> {
    if !params
        .iter()
        .all(|b| b.is_ascii_digit() || *b == b':' || *b == b';')
    {
        return None;
    }
    let fields: Vec<&[u8]> = params.split(|&b| b == b';').collect();
    // xterm modifyOtherKeys: CSI 27 ; modifiers ; code ~ (presses only).
    if final_byte == b'~' && fields.len() == 3 && fields[0] == b"27" {
        return Some(key_event(
            number(fields[2])?,
            number(fields[1])?,
            EVENT_PRESS,
        ));
    }
    // Kitty: CSI code[:shifted[:base]] ; modifiers[:event] ; text u, and the
    // functional keys CSI number ; modifiers[:event] ~ and
    // CSI 1 ; modifiers[:event] {ABCDEFHPQS}, which can report releases too.
    if !matches!(
        final_byte,
        b'u' | b'~' | b'A'..=b'F' | b'H' | b'P' | b'Q' | b'S'
    ) || fields.len() > 3
    {
        return None;
    }
    let mut modifier = fields
        .get(1)
        .copied()
        .unwrap_or_default()
        .split(|&b| b == b':');
    let modifiers = number_or(modifier.next(), 1)?;
    let event = number_or(modifier.next(), EVENT_PRESS)?;
    if modifier.next().is_some() {
        return None;
    }
    let code = fields[0].split(|&b| b == b':').next().and_then(number);
    if event == EVENT_RELEASE {
        return Some(Key::Incidental {
            of_backslash: final_byte == b'u' && code == Some(KEY_BACKSLASH),
        });
    }
    if final_byte != b'u' || !matches!(event, EVENT_PRESS | EVENT_REPEAT) {
        return None;
    }
    let code = code?;
    // A repeat follows a press already decided: with a prefix held it
    // belongs to that press, and with none held the child saw the press.
    if event == EVENT_REPEAT && code == KEY_BACKSLASH {
        return Some(Key::Incidental { of_backslash: true });
    }
    if is_modifier_key(code) {
        return Some(Key::Incidental {
            of_backslash: false,
        });
    }
    Some(key_event(code, modifiers, event))
}

/// A press of `code` (or a repeat of a key other than `\`) with the encoded
/// `modifiers`.
fn key_event(code: u32, modifiers: u32, event: u32) -> Key {
    let Some(held) = modifiers.checked_sub(1).map(|m| m & !MOD_LOCKS) else {
        return Key::Other;
    };
    match (code, held, event) {
        (KEY_BACKSLASH, MOD_CTRL, EVENT_PRESS) => Key::Prefix,
        (KEY_D, 0, EVENT_PRESS) => Key::Detach,
        _ => Key::Other,
    }
}

/// Kitty's codes for the lock keys (`CAPS_LOCK`, `SCROLL_LOCK`, `NUM_LOCK`)
/// and the modifier keys (`LEFT_SHIFT` … `ISO_LEVEL5_SHIFT`), which flag 8
/// reports as keys of their own.
fn is_modifier_key(code: u32) -> bool {
    matches!(code, 57358..=57360 | 57441..=57454)
}

fn number(digits: &[u8]) -> Option<u32> {
    if digits.is_empty() {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// A number, or `default` for an absent or empty field.
fn number_or(digits: Option<&[u8]>, default: u32) -> Option<u32> {
    match digits {
        None | Some(b"") => Some(default),
        Some(digits) => number(digits),
    }
}

/// Whether an unfinished sequence could still become a `Ctrl-\` press: a
/// lone `ESC`, or `ESC [` followed by the start of `92[:…][;mods[:event][;text]]`
/// or of `27;mods;92`.
fn could_become_prefix(unfinished: &[u8]) -> bool {
    let Some(rest) = unfinished.strip_prefix(&[ESC]) else {
        return false;
    };
    let Some(params) = rest.strip_prefix(b"[") else {
        return rest.is_empty();
    };
    if !params
        .iter()
        .all(|b| b.is_ascii_digit() || *b == b':' || *b == b';')
    {
        return false;
    }
    let fields: Vec<&[u8]> = params.split(|&b| b == b';').collect();
    let key = fields[0];
    let code = key.split(|&b| b == b':').next().unwrap_or_default();
    let kitty = if fields.len() == 1 && code.len() == key.len() {
        b"92".starts_with(code)
    } else {
        code == b"92"
            && fields.len() <= 3
            && fields
                .get(1)
                .is_none_or(|m| m.iter().filter(|&&b| b == b':').count() <= 1)
    };
    let xterm = if fields.len() == 1 {
        b"27".starts_with(key)
    } else {
        key == b"27"
            && fields.len() <= 3
            && !fields[1].contains(&b':')
            && fields.get(2).is_none_or(|c| b"92".starts_with(c))
    };
    kitty || xterm
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::time::{Duration, Instant};

    /// Feed `chunks` to a parser for a child that has bracketed paste on.
    fn run(mode: EscapeMode, chunks: &[&[u8]]) -> (Vec<u8>, Escape) {
        let mut parser = EscapeParser::new(mode);
        parser.set_bracketed_paste(true);
        let mut forward = Vec::new();
        for chunk in chunks {
            if parser.feed(chunk, &mut forward) == Escape::Detach {
                return (forward, Escape::Detach);
            }
        }
        (forward, Escape::Continue)
    }

    const C: EscapeMode = EscapeMode::CtrlBackslash;

    #[test]
    fn a_bracketed_paste_is_forwarded_whole_and_never_detaches() {
        let paste: &[u8] = b"\x1b[200~a\x1cd\x1c\x1cb\x1b[92;5ud\x1b[201~";
        assert_eq!(run(C, &[paste]), (paste.to_vec(), Escape::Continue));
        // After the paste the escape works again.
        let (forward, decision) = run(C, &[paste, b"x\x1cd"]);
        assert_eq!(decision, Escape::Detach);
        assert_eq!(forward, [paste, b"x"].concat());
    }

    #[test]
    fn a_paste_end_split_across_reads_still_ends_the_paste() {
        let (forward, decision) = run(C, &[b"\x1b[200~a\x1cd\x1b[20", b"1~", b"\x1cd"]);
        assert_eq!(decision, Escape::Detach);
        assert_eq!(forward, b"\x1b[200~a\x1cd\x1b[201~");
        // A paste start split across reads starts the paste.
        let (forward, decision) = run(C, &[b"\x1b[20", b"0~\x1cd", b"\x1b[201~"]);
        assert_eq!(decision, Escape::Continue);
        assert_eq!(forward, b"\x1b[200~\x1cd\x1b[201~");
    }

    #[test]
    fn a_possible_paste_end_is_held_until_the_next_byte_however_late() {
        let mut parser = EscapeParser::new(C);
        parser.set_bracketed_paste(true);
        let mut forward = Vec::new();
        parser.feed(b"\x1b[200~x\x1b[20", &mut forward);
        assert_eq!(forward, b"\x1b[200~x");
        assert_eq!(parser.partial_deadline(), None, "no deadline");
        parser.flush_expired(Instant::now() + Duration::from_secs(3600), &mut forward);
        assert_eq!(forward, b"\x1b[200~x", "still held");
        assert_eq!(parser.feed(b"1~\x1cd", &mut forward), Escape::Detach);
        assert_eq!(forward, b"\x1b[200~x\x1b[201~");

        // Bytes that turn out not to be the end are text, and the paste goes on.
        let (forward, decision) = run(C, &[b"\x1b[200~x\x1b[2", b"y\x1cd"]);
        assert_eq!(decision, Escape::Continue);
        assert_eq!(forward, b"\x1b[200~x\x1b[2y\x1cd");
    }

    #[test]
    fn paste_markers_count_only_while_the_child_has_bracketed_paste_on() {
        let mut parser = EscapeParser::new(C);
        let mut forward = Vec::new();
        assert_eq!(
            parser.feed(b"\x1b[200~a\x1cd", &mut forward),
            Escape::Detach,
            "without bracketed paste a marker is just a key"
        );

        let mut parser = EscapeParser::new(C);
        parser.set_bracketed_paste(true);
        let mut forward = Vec::new();
        parser.feed(b"\x1b[200~a", &mut forward);
        parser.set_bracketed_paste(false);
        assert_eq!(
            parser.feed(b"\x1cd", &mut forward),
            Escape::Detach,
            "turning bracketed paste off ends a paste"
        );
    }

    #[test]
    fn a_prefix_held_before_a_paste_is_forwarded_with_it() {
        let (forward, decision) = run(C, &[b"\x1c", b"\x1b[200~d\x1b[201~"]);
        assert_eq!(decision, Escape::Continue);
        assert_eq!(forward, b"\x1c\x1b[200~d\x1b[201~");
    }

    #[test]
    fn prefix_then_d_detaches_and_forwards_what_came_before() {
        assert_eq!(
            run(EscapeMode::CtrlBackslash, &[b"ls\r\x1cd"]),
            (b"ls\r".to_vec(), Escape::Detach)
        );
    }

    #[test]
    fn bytes_after_the_detach_key_are_discarded() {
        assert_eq!(
            run(EscapeMode::CtrlBackslash, &[b"a\x1cdrm -rf\r"]),
            (b"a".to_vec(), Escape::Detach)
        );
    }

    #[test]
    fn a_doubled_prefix_forwards_one_literal_prefix() {
        assert_eq!(
            run(EscapeMode::CtrlBackslash, &[b"A\x1c\x1cB"]),
            (b"A\x1cB".to_vec(), Escape::Continue)
        );
    }

    #[test]
    fn prefix_then_another_key_forwards_both_unchanged() {
        assert_eq!(
            run(EscapeMode::CtrlBackslash, &[b"\x1cx\x1c\r"]),
            (b"\x1cx\x1c\r".to_vec(), Escape::Continue)
        );
    }

    #[test]
    fn ctrl_right_bracket_passes_through() {
        // Alone, Ctrl-] is an ordinary byte: nothing about it is special to
        // the parser.
        assert_eq!(
            run(EscapeMode::CtrlBackslash, &[b"\x1d"]),
            (b"\x1d".to_vec(), Escape::Continue)
        );

        // As the byte that resolves a pending prefix, it is just an "other"
        // key: both bytes forward unchanged.
        assert_eq!(
            run(EscapeMode::CtrlBackslash, &[b"\x1c\x1d"]),
            (b"\x1c\x1d".to_vec(), Escape::Continue)
        );

        // Right after that resolution, a further Ctrl-] still passes
        // through untouched — no state leaks from the resolved prefix.
        assert_eq!(
            run(EscapeMode::CtrlBackslash, &[b"\x1c\x1d\x1d"]),
            (b"\x1c\x1d\x1d".to_vec(), Escape::Continue)
        );
    }

    #[test]
    fn a_prefix_split_across_reads_is_held_then_decided() {
        assert_eq!(
            run(EscapeMode::CtrlBackslash, &[b"abc\x1c", b"d"]),
            (b"abc".to_vec(), Escape::Detach)
        );
        assert_eq!(
            run(EscapeMode::CtrlBackslash, &[b"abc\x1c", b"\x1c", b"q"]),
            (b"abc\x1cq".to_vec(), Escape::Continue)
        );
        let mut parser = EscapeParser::new(EscapeMode::CtrlBackslash);
        let mut forward = Vec::new();
        assert_eq!(parser.feed(b"z\x1c", &mut forward), Escape::Continue);
        assert_eq!(
            forward, b"z",
            "a trailing prefix is held, not forwarded yet"
        );
    }

    #[test]
    fn disabled_escape_forwards_everything() {
        assert_eq!(
            run(EscapeMode::Disabled, &[b"\x1cd\x1c\x1c"]),
            (b"\x1cd\x1c\x1c".to_vec(), Escape::Continue)
        );
    }

    /// What feeding some chunks forwarded, and what the parser still holds.
    #[derive(Debug, PartialEq, Eq)]
    struct Outcome {
        forward: Vec<u8>,
        escape: Escape,
        prefix: Option<Vec<u8>>,
        between: Vec<u8>,
        between_kept: Vec<u8>,
        partial: Vec<u8>,
        pasting: bool,
    }

    fn run_state(chunks: &[&[u8]]) -> Outcome {
        let mut parser = EscapeParser::new(EscapeMode::CtrlBackslash);
        parser.set_bracketed_paste(true);
        let mut forward = Vec::new();
        let mut escape = Escape::Continue;
        for chunk in chunks {
            escape = parser.feed(chunk, &mut forward);
            if escape == Escape::Detach {
                break;
            }
        }
        Outcome {
            forward,
            escape,
            prefix: parser.prefix,
            between: parser.between,
            between_kept: parser.between_kept,
            partial: parser.partial,
            pasting: parser.pasting,
        }
    }

    /// Keystrokes mixing every encoding, their fragments and arbitrary bytes.
    fn extended_stream() -> impl Strategy<Value = Vec<u8>> {
        let piece = prop_oneof![
            4 => proptest::sample::select(CTRL_BACKSLASH.to_vec()).prop_map(<[u8]>::to_vec),
            4 => proptest::sample::select(DETACH.to_vec()).prop_map(<[u8]>::to_vec),
            3 => proptest::sample::select(vec![
                &b"\x1b[92;5:3u"[..], b"\x1b[92;5:2u", b"\x1b[92;1:2u", b"\x1b[57442;5u", b"\x1b[57442;1:3u", b"\x1b[1;1:3A",
                b"\x1b[A", b"\x1b[1;5A", b"\x1b[97;5u", b"\x1b[100;5u", b"\x1b[200~", b"\x1b[201~",
                b"\x1b", b"\x1b[", b"\x1b[9", b"\x1b[92;", b"\x1b[27;5;", b";", b":", b"u", b"~",
            ]).prop_map(<[u8]>::to_vec),
            2 => "[0-9:;]{1,4}".prop_map(String::into_bytes),
            2 => any::<u8>().prop_map(|b| vec![b]),
        ];
        proptest::collection::vec(piece, 0..24).prop_map(|pieces| pieces.concat())
    }

    /// Complete keys and events that never start the escape.
    fn not_the_prefix() -> impl Strategy<Value = Vec<u8>> {
        prop_oneof![
            proptest::sample::select(vec![
                &b"d"[..],
                b"\x1b[100u",
                b"\x1b[27;1;100~",
                b"\x1b[92;5:3u",
                b"\x1b[92;5:2u",
                b"\x1b[92u",
                b"\x1b[92;7u",
                b"\x1b[27;7;92~",
                b"\x1b[57442;5u",
                b"\x1b[1;1:3A",
                b"\x1b[A",
                b"\x1b[1;5A",
                b"\x1b[97;5u",
                b"\x1b[3~",
                b"\x1b[200~",
                b"\x1bOP",
                b"\x1bx",
                b"\x1b[<0;10;5M",
                b"\x1b[93;5u",
                b"\x1b[92;6u",
            ])
            .prop_map(<[u8]>::to_vec),
            any::<u8>()
                .prop_filter("not the prefix or ESC", |b| *b != PREFIX && *b != ESC)
                .prop_map(|b| vec![b]),
        ]
    }

    #[test]
    fn an_unfinished_sequence_is_forwarded_once_it_has_waited() {
        let mut parser = EscapeParser::new(EscapeMode::CtrlBackslash);
        let mut forward = Vec::new();
        assert_eq!(parser.partial_deadline(), None);
        let before = Instant::now();
        assert_eq!(parser.feed(b"a\x1b", &mut forward), Escape::Continue);
        assert_eq!(forward, b"a");
        let due = parser.partial_deadline().expect("the lone ESC is held");
        assert!(due >= before + PARTIAL_HOLD && due <= Instant::now() + PARTIAL_HOLD);

        parser.flush_expired(due - Duration::from_millis(1), &mut forward);
        assert_eq!(forward, b"a", "not due yet");
        parser.flush_expired(due, &mut forward);
        assert_eq!(forward, b"a\x1b", "the Esc key reaches the child");
        assert_eq!(parser.partial_deadline(), None);
        // The ESC was forwarded, so what follows starts afresh.
        assert_eq!(parser.feed(b"[92;5u", &mut forward), Escape::Continue);
        assert_eq!(forward, b"a\x1b[92;5u");
    }

    #[test]
    fn a_continued_sequence_keeps_its_first_deadline() {
        let mut parser = EscapeParser::new(EscapeMode::CtrlBackslash);
        let mut forward = Vec::new();
        parser.feed(b"\x1b", &mut forward);
        let due = parser.partial_deadline().unwrap();
        std::thread::sleep(Duration::from_millis(2));
        parser.feed(b"[9", &mut forward);
        assert_eq!(parser.partial_deadline(), Some(due));
        // A new sequence after a complete key gets a new deadline.
        parser.feed(b"2;5ux\x1b", &mut forward);
        assert!(parser.partial_deadline().unwrap() > due);
        // Completing it clears the deadline.
        parser.feed(b"[A", &mut forward);
        assert_eq!(parser.partial_deadline(), None);
        assert_eq!(forward, b"\x1b[92;5ux\x1b[A");
    }

    #[test]
    fn an_expired_sequence_after_a_prefix_counts_as_another_key() {
        let mut parser = EscapeParser::new(EscapeMode::CtrlBackslash);
        let mut forward = Vec::new();
        parser.feed(b"\x1b[92;5u\x1b[92;5:3u\x1b", &mut forward);
        assert_eq!(forward, b"");
        let due = parser.partial_deadline().unwrap();
        parser.flush_expired(due, &mut forward);
        assert_eq!(forward, b"\x1b[92;5u\x1b[92;5:3u\x1b");
        assert_eq!(parser.feed(b"d", &mut forward), Escape::Continue);
        assert_eq!(forward, b"\x1b[92;5u\x1b[92;5:3u\x1bd");
    }

    #[test]
    fn a_held_prefix_alone_has_no_deadline() {
        let mut parser = EscapeParser::new(EscapeMode::CtrlBackslash);
        let mut forward = Vec::new();
        parser.feed(b"\x1b[92;5u", &mut forward);
        assert_eq!(parser.partial_deadline(), None);
        parser.flush_expired(Instant::now() + Duration::from_secs(3600), &mut forward);
        assert_eq!(forward, b"");
        assert_eq!(parser.feed(b"d", &mut forward), Escape::Detach);
    }

    #[test]
    fn an_overlong_sequence_is_not_held() {
        let mut long = b"\x1b[92".to_vec();
        long.extend(std::iter::repeat_n(b':', MAX_SEQUENCE));
        let mut parser = EscapeParser::new(EscapeMode::CtrlBackslash);
        let mut forward = Vec::new();
        parser.feed(&long, &mut forward);
        assert_eq!(forward, long);
        assert_eq!(parser.partial_deadline(), None);
    }

    // Extended key encodings. Sources: the kitty keyboard protocol
    // (https://sw.kovidgoyal.net/kitty/keyboard-protocol/) and xterm's
    // modifyOtherKeys (https://invisible-island.net/xterm/ctlseqs/ctlseqs.html,
    // "Alt and Meta Keys"; https://invisible-island.net/xterm/modified-keys.html).

    /// `Ctrl-\` presses (and repeats) in every encoding the parser accepts.
    const CTRL_BACKSLASH: &[&[u8]] = &[
        b"\x1c",                // legacy
        b"\x1b[92;5u",          // kitty, flag 1; xterm formatOtherKeys=1
        b"\x1b[92;5:1u",        // kitty, flag 2: explicit press
        b"\x1b[92::92;5u",      // kitty, flag 4: base layout key only
        b"\x1b[92:124:92;5:1u", // kitty, flag 4: shifted and base layout keys
        b"\x1b[92;69u",         // ctrl + caps_lock
        b"\x1b[92;133u",        // ctrl + num_lock
        b"\x1b[92;197:1u",      // ctrl + caps_lock + num_lock, explicit press
        b"\x1b[27;5;92~",       // xterm modifyOtherKeys=2
        b"\x1b[27;69;92~",      // modifyOtherKeys with a lock bit
    ];

    /// The detach key in every encoding the parser accepts.
    const DETACH: &[&[u8]] = &[
        b"d",
        b"\x1b[100u",        // kitty, flag 8
        b"\x1b[100;1u",      // kitty, flag 8, explicit modifiers
        b"\x1b[100;1:1u",    // kitty, flags 2+8: explicit press
        b"\x1b[100;65u",     // caps_lock
        b"\x1b[100;129u",    // num_lock
        b"\x1b[100;1;100u",  // kitty, flags 8+16: associated text
        b"\x1b[100::100;1u", // kitty, flags 4+8: base layout key
        b"\x1b[27;1;100~",   // xterm modifyOtherKeys=3
    ];

    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn prefix_then_detach_key_detaches_in_every_encoding() {
        for prefix in CTRL_BACKSLASH {
            for key in DETACH {
                let input = cat(&[b"ls", prefix, key, b"rm -rf"]);
                assert_eq!(
                    run(EscapeMode::CtrlBackslash, &[&input]),
                    (b"ls".to_vec(), Escape::Detach),
                    "{input:?}"
                );
            }
        }
    }

    #[test]
    fn a_doubled_prefix_forwards_one_in_the_encoding_typed_second() {
        for first in CTRL_BACKSLASH {
            for second in CTRL_BACKSLASH {
                let input = cat(&[b"A", first, second, b"B"]);
                assert_eq!(
                    run(EscapeMode::CtrlBackslash, &[&input]),
                    (cat(&[b"A", second, b"B"]), Escape::Continue),
                    "{input:?}"
                );
            }
        }
    }

    #[test]
    fn prefix_then_another_key_forwards_both_unchanged_in_every_encoding() {
        let others: &[&[u8]] = &[
            b"x",
            b"D",
            b"\r",
            b"\x04",           // legacy Ctrl-d
            b"\x1b",           // Esc, then a key that is not `[`
            b"\x1b[A",         // up arrow
            b"\x1b[1;5A",      // Ctrl-up
            b"\x1b[97;5u",     // kitty Ctrl-a
            b"\x1b[100;5u",    // kitty Ctrl-d is not `d`
            b"\x1b[100;2u",    // kitty Shift-d
            b"\x1b[100;1:2u",  // a repeat of `d` never started as a press
            b"\x1b[92u",       // `\` without ctrl
            b"\x1b[92;7u",     // Ctrl-Alt-\
            b"\x1b[92;13u",    // Ctrl-Super-\
            b"\x1b[27;7;92~",  // modifyOtherKeys Ctrl-Alt-\
            b"\x1b[27;5;100~", // modifyOtherKeys Ctrl-d
            b"\x1b[200~",      // bracketed paste start
            b"\x1b[<0;10;5M",  // SGR mouse report
            b"\x1bO",          // SS3 introducer (the P follows)
        ];
        for prefix in CTRL_BACKSLASH {
            for other in others {
                let input = cat(&[prefix, other, b"P"]);
                assert_eq!(
                    run(EscapeMode::CtrlBackslash, &[&input]),
                    (input.clone(), Escape::Continue),
                    "{input:?}"
                );
            }
        }
    }

    #[test]
    fn unrelated_sequences_pass_through_untouched() {
        let unrelated: &[&[u8]] = &[
            b"\x1b[A",
            b"\x1b[1;5A",
            b"\x1b[97;5u",
            b"\x1b[3~",
            b"\x1b[3;1:3~",   // kitty Delete release
            b"\x1b[92;5:3u",  // a Ctrl-\ release alone is just forwarded
            b"\x1b[100u",     // kitty `d` with no prefix pending
            b"\x1b[57442;5u", // kitty left-ctrl press
            b"\x1b[?1;2c",    // a DA1 reply
            b"\x1b[I",        // focus in
            b"\x1bOP",        // F1
            b"\x1b\x1b",      // Esc Esc
            b"\x1bx",         // Alt-x
            b"\x1b[200~text\x1b[201~",
        ];
        for bytes in unrelated {
            let input = cat(&[bytes, b"z"]);
            assert_eq!(
                run(EscapeMode::CtrlBackslash, &[&input]),
                (input.clone(), Escape::Continue),
                "{input:?}"
            );
        }
    }

    #[test]
    fn a_release_while_the_prefix_is_held_neither_decides_nor_cancels_it() {
        // Kitty flag 2: Ctrl-\ press, its release, then `d`. The release of
        // the swallowed press is dropped with it.
        assert_eq!(
            run(EscapeMode::CtrlBackslash, &[b"a\x1b[92;5u\x1b[92;5:3ud"]),
            (b"a".to_vec(), Escape::Detach)
        );
        // Kitty flags 2+8: ctrl pressed first, then Ctrl-\, its release, the
        // ctrl release, and `d` as a key event. The ctrl release reaches the
        // child, which saw the ctrl press.
        assert_eq!(
            run(
                EscapeMode::CtrlBackslash,
                &[b"\x1b[57442;5u\x1b[92;5u\x1b[92;5:3u\x1b[57442;1:3u\x1b[100u"]
            ),
            (b"\x1b[57442;5u\x1b[57442;1:3u".to_vec(), Escape::Detach)
        );
        // Released between: another key forwards everything in the order
        // typed.
        assert_eq!(
            run(EscapeMode::CtrlBackslash, &[b"\x1b[92;5u\x1b[92;5:3ux"]),
            (b"\x1b[92;5u\x1b[92;5:3ux".to_vec(), Escape::Continue)
        );
        // Released between: a doubled prefix forwards the second press, and
        // the first press's release goes with the first press.
        assert_eq!(
            run(
                EscapeMode::CtrlBackslash,
                &[b"\x1b[92;5u\x1b[92;5:3u\x1b[92;5u\x1b[92;5:3u"]
            ),
            (b"\x1b[92;5u\x1b[92;5:3u".to_vec(), Escape::Continue)
        );
        // Releases of other keys (arrow keys use the legacy form) are kept.
        assert_eq!(
            run(
                EscapeMode::CtrlBackslash,
                &[b"\x1b[92;5u\x1b[1;1:3A\x1b[97;1:3ud"]
            ),
            (b"\x1b[1;1:3A\x1b[97;1:3u".to_vec(), Escape::Detach)
        );
    }

    #[test]
    fn auto_repeat_of_the_held_prefix_belongs_to_the_swallowed_press() {
        // Kitty flag 2: Ctrl-\ held down, then released, then `d`. Nothing of
        // the `\` key reaches the child, which never saw its press.
        assert_eq!(
            run(
                EscapeMode::CtrlBackslash,
                &[b"a\x1b[92;5u\x1b[92;5:2u\x1b[92;5:2u\x1b[92;5:3ud"]
            ),
            (b"a".to_vec(), Escape::Detach)
        );
        // Released without ctrl still held: the repeats and release of the
        // same physical key carry other modifiers.
        assert_eq!(
            run(
                EscapeMode::CtrlBackslash,
                &[b"\x1b[92;5u\x1b[92;1:2u\x1b[92;1:3u\x1b[100u"]
            ),
            (Vec::new(), Escape::Detach)
        );
        // Held, then another key: everything, in the order typed.
        assert_eq!(
            run(
                EscapeMode::CtrlBackslash,
                &[b"\x1b[92;5u\x1b[92;5:2u\x1b[92;5:2u\x1b[92;5:3ux"]
            ),
            (
                b"\x1b[92;5u\x1b[92;5:2u\x1b[92;5:2u\x1b[92;5:3ux".to_vec(),
                Escape::Continue
            )
        );
        // Held, released, pressed again: one Ctrl-\, the second press.
        assert_eq!(
            run(
                EscapeMode::CtrlBackslash,
                &[b"\x1b[92;5u\x1b[92;5:2u\x1b[92;5:3u\x1b[92;5:1u"]
            ),
            (b"\x1b[92;5:1u".to_vec(), Escape::Continue)
        );
        // Held across a read boundary.
        assert_eq!(
            run(
                EscapeMode::CtrlBackslash,
                &[
                    b"\x1b[92;5u\x1b[92;5:",
                    b"2u\x1b[92;5:2u",
                    b"\x1b[92;5:3u",
                    b"d"
                ]
            ),
            (Vec::new(), Escape::Detach)
        );
    }

    #[test]
    fn a_repeat_with_no_prefix_held_is_an_ordinary_key() {
        // The repeat of a Ctrl-\ already forwarded (the second of a doubled
        // prefix, held down) goes to the child and does not start a prefix.
        assert_eq!(
            run(
                EscapeMode::CtrlBackslash,
                &[b"\x1b[92;5u\x1b[92;5u\x1b[92;5:2u\x1b[92;5:2ud"]
            ),
            (
                b"\x1b[92;5u\x1b[92;5:2u\x1b[92;5:2ud".to_vec(),
                Escape::Continue
            )
        );
        assert_eq!(
            run(EscapeMode::CtrlBackslash, &[b"\x1b[92;5:2ud"]),
            (b"\x1b[92;5:2ud".to_vec(), Escape::Continue)
        );
    }

    #[test]
    fn modifier_key_presses_while_the_prefix_is_held_do_not_cancel_it() {
        // Kitty flag 8 reports modifier keys: tapping Ctrl-\ twice presses
        // ctrl again in between.
        assert_eq!(
            run(
                EscapeMode::CtrlBackslash,
                &[b"\x1b[92;5u\x1b[57442;1:3u\x1b[57442;5u\x1b[92;5u"]
            ),
            (
                b"\x1b[57442;1:3u\x1b[57442;5u\x1b[92;5u".to_vec(),
                Escape::Continue
            )
        );
        // Shift, caps lock and a right-hand modifier held on the way to `d`.
        assert_eq!(
            run(
                EscapeMode::CtrlBackslash,
                &[b"\x1b[92;5u\x1b[57441;2u\x1b[57441;1:3u\x1b[57358;65u\x1b[57448;5:2ud"]
            ),
            (
                b"\x1b[57441;2u\x1b[57441;1:3u\x1b[57358;65u\x1b[57448;5:2u".to_vec(),
                Escape::Detach
            )
        );
    }

    #[test]
    fn every_encoding_split_at_every_byte_is_held_then_decided() {
        for prefix in CTRL_BACKSLASH {
            for key in DETACH.iter().chain(CTRL_BACKSLASH).chain([&&b"x"[..]]) {
                let input = cat(&[b"q", prefix, b"\x1b[92;5:3u", key, b"r"]);
                let whole = run(EscapeMode::CtrlBackslash, &[&input]);
                for cut in 0..=input.len() {
                    let (head, tail) = input.split_at(cut);
                    assert_eq!(
                        run(EscapeMode::CtrlBackslash, &[head, tail]),
                        whole,
                        "{input:?} cut at {cut}"
                    );
                }
                for (a, b) in (0..=input.len()).flat_map(|a| (a..=input.len()).map(move |b| (a, b)))
                {
                    let chunks = [&input[..a], &input[a..b], &input[b..]];
                    assert_eq!(
                        run(EscapeMode::CtrlBackslash, &chunks),
                        whole,
                        "{input:?} cut at {a}, {b}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_sequence_that_cannot_become_a_prefix_is_forwarded_at_once() {
        for partial in [
            &b"\x1b[1;5"[..],
            b"\x1b[97;5",
            b"\x1b[3",
            b"\x1b[?1",
            b"\x1b[93",
            b"\x1b[27;5;93",
        ] {
            let mut parser = EscapeParser::new(EscapeMode::CtrlBackslash);
            let mut forward = Vec::new();
            assert_eq!(parser.feed(partial, &mut forward), Escape::Continue);
            assert_eq!(forward, partial, "{partial:?}");
        }
    }

    #[test]
    fn a_sequence_that_could_become_a_prefix_is_held() {
        for partial in [
            &b"\x1b"[..],
            b"\x1b[",
            b"\x1b[9",
            b"\x1b[92",
            b"\x1b[92;5",
            b"\x1b[92;5:",
            b"\x1b[2",
            b"\x1b[27;5;9",
        ] {
            let mut parser = EscapeParser::new(EscapeMode::CtrlBackslash);
            let mut forward = Vec::new();
            assert_eq!(parser.feed(partial, &mut forward), Escape::Continue);
            assert_eq!(forward, b"", "{partial:?}");
        }
    }

    #[test]
    fn disabled_escape_forwards_extended_encodings_too() {
        let input = b"\x1b[92;5ud\x1b[27;5;92~\x1b[100u\x1b[92;5u\x1b";
        assert_eq!(
            run(EscapeMode::Disabled, &[input]),
            (input.to_vec(), Escape::Continue)
        );
    }

    proptest! {
        #[test]
        fn keystrokes_without_the_prefix_pass_through_unchanged(
            chunks in proptest::collection::vec(
                proptest::collection::vec(
                    any::<u8>().prop_filter("not the prefix or ESC", |b| *b != PREFIX && *b != ESC),
                    0..64,
                ),
                0..16,
            )
        ) {
            let slices: Vec<&[u8]> = chunks.iter().map(Vec::as_slice).collect();
            let (forward, escape) = run(EscapeMode::CtrlBackslash, &slices);
            prop_assert_eq!(escape, Escape::Continue);
            prop_assert_eq!(forward, chunks.concat());
        }

        #[test]
        fn chunking_never_changes_the_result(
            bytes in proptest::collection::vec(prop_oneof![Just(PREFIX), Just(DETACH_KEY), any::<u8>()], 0..64),
            split in any::<proptest::sample::Index>(),
        ) {
            let at = split.index(bytes.len() + 1);
            let whole = run(EscapeMode::CtrlBackslash, &[&bytes]);
            let parts = run(EscapeMode::CtrlBackslash, &[&bytes[..at], &bytes[at..]]);
            prop_assert_eq!(whole, parts);
        }

        #[test]
        fn chunking_never_changes_the_result_with_extended_encodings(
            bytes in extended_stream(),
            cuts in proptest::collection::vec(any::<proptest::sample::Index>(), 0..6),
        ) {
            let mut at: Vec<usize> = cuts.iter().map(|c| c.index(bytes.len() + 1)).collect();
            at.push(0);
            at.push(bytes.len());
            at.sort_unstable();
            let chunks: Vec<&[u8]> = at.windows(2).map(|w| &bytes[w[0]..w[1]]).collect();
            prop_assert_eq!(run_state(&[&bytes]), run_state(&chunks));
        }

        #[test]
        fn complete_keys_that_are_not_the_prefix_pass_through_unchanged(
            keys in proptest::collection::vec(not_the_prefix(), 0..24),
            cuts in proptest::collection::vec(any::<proptest::sample::Index>(), 0..6),
        ) {
            let bytes = keys.concat();
            let mut at: Vec<usize> = cuts.iter().map(|c| c.index(bytes.len() + 1)).collect();
            at.push(0);
            at.push(bytes.len());
            at.sort_unstable();
            let chunks: Vec<&[u8]> = at.windows(2).map(|w| &bytes[w[0]..w[1]]).collect();
            prop_assert_eq!(run(EscapeMode::CtrlBackslash, &chunks), (bytes.clone(), Escape::Continue));
        }
    }
}
