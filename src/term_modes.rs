//! The terminal modes a PTY child's output has turned on.
//!
//! A child such as Claude Code switches the terminal to the alternate screen,
//! turns on mouse reports and a keyboard protocol, and expects them to stay on.
//! A terminal that attaches later never saw those sequences, and a terminal
//! that detaches keeps them: mouse moves and `Ctrl-C` then reach the user's
//! shell as escape sequences. The sidecar keeps a [`TermModes`] fed with the
//! child's output and queues [`TermModes::replay`] ahead of live output for
//! every client it accepts. The client keeps one fed with everything it writes
//! to the terminal and writes [`TermModes::reset`] when it leaves.
//!
//! Tracked, with the DEC private mode numbers:
//! - the alternate screen (47, 1047, 1049);
//! - cursor visibility (25) and autowrap (7), both on by default;
//! - application cursor keys (1) and keypad (`ESC =`);
//! - mouse tracking (9, 1000, 1002, 1003) and its encoding (1005, 1006, 1015,
//!   1016), each one setting as in xterm and Ghostty: the last one set wins,
//!   and resetting any of them turns it off;
//! - alternate scroll (1007), focus events (1004), bracketed paste (2004),
//!   synchronized output (2026), colour-scheme reports (2031) and in-band size
//!   reports (2048);
//! - the kitty keyboard flags, one stack per screen, kept across screen
//!   switches as kitty and Ghostty do
//!   (<https://sw.kovidgoyal.net/kitty/keyboard-protocol/>);
//! - xterm's `modifyOtherKeys`
//!   (<https://invisible-island.net/xterm/ctlseqs/ctlseqs.html>).
//!
//! A full reset (`ESC c`) clears them. A soft reset (`CSI ! p`) shows the
//! cursor and resets the cursor keys, the keypad and `modifyOtherKeys`.
//!
//! Synchronized output is turned off on leaving, so a frame cut short by a
//! detach cannot hold the terminal's display, but never replayed: a new client
//! starts outside any frame.
//!
//! Not tracked: the window title, the scroll region, insert mode, character
//! sets and cursor shape.

/// DEC private modes that are off by default and simply on or off, other than
/// synchronized output.
const DEC_FLAGS: [u16; 6] = [1, 1004, 1007, 2004, 2031, 2048];
const MOUSE_TRACKING: [u16; 4] = [9, 1000, 1002, 1003];
const MOUSE_ENCODING: [u16; 4] = [1005, 1006, 1015, 1016];
const SYNCHRONIZED_OUTPUT: u16 = 2026;
/// The legacy alternate-screen switch: no clear, no cursor save. Used to
/// reach the alternate screen's kitty stack while the main screen is shown.
const ALTERNATE_SWITCH: u16 = 47;
/// The most kitty keyboard entries kept per screen; older ones are dropped.
const KITTY_STACK_LIMIT: usize = 32;
/// The most parameter and intermediate bytes of a CSI sequence kept. A longer
/// sequence is consumed and ignored.
const CSI_LIMIT: usize = 64;
/// The most parameters of one CSI sequence acted on.
const MAX_PARAMS: usize = 16;

/// What a terminal's modes look like after a child's output.
#[derive(Debug, Clone, Default)]
pub struct TermModes {
    parser: Parser,
    /// Bit `i` set: `DEC_FLAGS[i]` is on.
    flags: u8,
    synchronized_output: bool,
    mouse_tracking: Option<u16>,
    mouse_encoding: Option<u16>,
    cursor_hidden: bool,
    autowrap_off: bool,
    application_keypad: bool,
    /// The DEC mode that entered the alternate screen, while it is shown.
    alternate_screen: Option<u16>,
    kitty_main: KittyFlags,
    kitty_alternate: KittyFlags,
    /// The `modifyOtherKeys` level, when above 0.
    modify_other_keys: Option<u32>,
}

impl TermModes {
    pub fn new() -> Self {
        Self::default()
    }

    /// Track the modes `bytes` turn on and off. Sequences may be split across
    /// calls.
    pub fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match self.parser.advance(byte) {
                Action::None => {}
                Action::FullReset => self.full_reset(),
                Action::Keypad(application) => self.application_keypad = application,
                Action::Csi(final_byte) => {
                    let sequence = std::mem::take(&mut self.parser.csi);
                    self.csi(&sequence, final_byte);
                    self.parser.csi = sequence;
                }
            }
        }
    }

    /// The sequences that turn the tracked modes on in a terminal in its
    /// default state; empty when none are on.
    pub fn replay(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.kitty_main.replay(&mut out);
        if let Some(mode) = self.alternate_screen {
            dec_mode(&mut out, mode, true);
            self.kitty_alternate.replay(&mut out);
        } else if !self.kitty_alternate.is_empty() {
            dec_mode(&mut out, ALTERNATE_SWITCH, true);
            self.kitty_alternate.replay(&mut out);
            dec_mode(&mut out, ALTERNATE_SWITCH, false);
        }
        for mode in self.dec_modes_on() {
            dec_mode(&mut out, mode, true);
        }
        if self.autowrap_off {
            dec_mode(&mut out, 7, false);
        }
        if self.cursor_hidden {
            dec_mode(&mut out, 25, false);
        }
        if self.application_keypad {
            out.extend_from_slice(b"\x1b=");
        }
        if let Some(level) = self.modify_other_keys {
            out.extend_from_slice(format!("\x1b[>4;{level}m").as_bytes());
        }
        out
    }

    /// The sequences that turn the tracked modes off again; empty when none
    /// are on.
    pub fn reset(&self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.synchronized_output {
            dec_mode(&mut out, SYNCHRONIZED_OUTPUT, false);
        }
        if let Some(mode) = self.alternate_screen {
            self.kitty_alternate.reset(&mut out);
            dec_mode(&mut out, mode, false);
        } else if !self.kitty_alternate.is_empty() {
            dec_mode(&mut out, ALTERNATE_SWITCH, true);
            self.kitty_alternate.reset(&mut out);
            dec_mode(&mut out, ALTERNATE_SWITCH, false);
        }
        self.kitty_main.reset(&mut out);
        for mode in self.dec_modes_on() {
            dec_mode(&mut out, mode, false);
        }
        if self.autowrap_off {
            dec_mode(&mut out, 7, true);
        }
        if self.cursor_hidden {
            dec_mode(&mut out, 25, true);
        }
        if self.application_keypad {
            out.extend_from_slice(b"\x1b>");
        }
        if self.modify_other_keys.is_some() {
            out.extend_from_slice(b"\x1b[>4m");
        }
        out
    }

    /// The on/off DEC modes that are on, in ascending order.
    fn dec_modes_on(&self) -> Vec<u16> {
        let mut on: Vec<u16> = DEC_FLAGS
            .iter()
            .enumerate()
            .filter(|(i, _)| self.flags & (1 << i) != 0)
            .map(|(_, &mode)| mode)
            .chain(self.mouse_tracking)
            .chain(self.mouse_encoding)
            .collect();
        on.sort_unstable();
        on
    }

    fn full_reset(&mut self) {
        *self = Self {
            parser: std::mem::take(&mut self.parser),
            ..Self::default()
        };
    }

    fn kitty(&mut self) -> &mut KittyFlags {
        if self.alternate_screen.is_some() {
            &mut self.kitty_alternate
        } else {
            &mut self.kitty_main
        }
    }

    fn set_dec(&mut self, mode: u16, on: bool) {
        match mode {
            7 => self.autowrap_off = !on,
            25 => self.cursor_hidden = !on,
            // Each screen keeps its own kitty stack across switches.
            47 | 1047 | 1049 => {
                if on && self.alternate_screen.is_none() {
                    self.alternate_screen = Some(mode);
                } else if !on {
                    self.alternate_screen = None;
                }
            }
            SYNCHRONIZED_OUTPUT => self.synchronized_output = on,
            _ if MOUSE_TRACKING.contains(&mode) => {
                self.mouse_tracking = on.then_some(mode);
            }
            _ if MOUSE_ENCODING.contains(&mode) => {
                self.mouse_encoding = on.then_some(mode);
            }
            _ => {
                if let Some(i) = DEC_FLAGS.iter().position(|&m| m == mode) {
                    if on {
                        self.flags |= 1 << i;
                    } else {
                        self.flags &= !(1 << i);
                    }
                }
            }
        }
    }

    /// Apply one complete CSI sequence: `sequence` is its parameter and
    /// intermediate bytes.
    fn csi(&mut self, sequence: &[u8], final_byte: u8) {
        let (prefix, rest) = match sequence.first() {
            Some(&p @ (b'?' | b'>' | b'<' | b'=')) => (Some(p), &sequence[1..]),
            _ => (None, sequence),
        };
        let split = rest
            .iter()
            .position(|b| (0x20..=0x2f).contains(b))
            .unwrap_or(rest.len());
        let (params, intermediates) = rest.split_at(split);
        // Most sequences (SGR, cursor moves) are not modes: decide before
        // parsing their parameters.
        let relevant = matches!(
            (prefix, intermediates, final_byte),
            (Some(b'?'), b"", b'h' | b'l')
                | (Some(b'>' | b'<' | b'='), b"", b'u')
                | (Some(b'>'), b"", b'm' | b'n')
                | (None, b"!", b'p')
        );
        if !relevant {
            return;
        }
        let Some(params) = Params::parse(params) else {
            return;
        };
        let param = |i: usize| params.get(i);
        match (prefix, final_byte) {
            (Some(b'?'), b'h' | b'l') => {
                for mode in params.iter().flatten() {
                    if let Ok(mode) = u16::try_from(mode) {
                        self.set_dec(mode, final_byte == b'h');
                    }
                }
            }
            (Some(b'>'), b'u') => self.kitty().push(param(0).unwrap_or(0)),
            (Some(b'<'), b'u') => self.kitty().pop(param(0).unwrap_or(1)),
            (Some(b'='), b'u') => self
                .kitty()
                .set(param(0).unwrap_or(0), param(1).unwrap_or(1)),
            (Some(b'>'), b'm') => {
                if params.is_empty() {
                    // XTMODKEYS with no resource resets every one.
                    self.modify_other_keys = None;
                } else if param(0) == Some(4) {
                    self.modify_other_keys = param(1).filter(|&level| level > 0);
                }
            }
            (Some(b'>'), b'n') if param(0) == Some(4) => self.modify_other_keys = None,
            (None, b'p') => {
                self.cursor_hidden = false;
                self.set_dec(1, false);
                self.application_keypad = false;
                self.modify_other_keys = None;
            }
            _ => {}
        }
    }
}

/// One screen's kitty keyboard flags: the flags in force with an empty stack,
/// and the stack of pushed entries above them.
#[derive(Debug, Clone, Default)]
struct KittyFlags {
    base: u32,
    stack: Vec<u32>,
}

impl KittyFlags {
    fn is_empty(&self) -> bool {
        self.base == 0 && self.stack.is_empty()
    }

    fn push(&mut self, flags: u32) {
        if self.stack.len() == KITTY_STACK_LIMIT {
            self.stack.remove(0);
        }
        self.stack.push(flags);
    }

    /// A pop that empties the stack resets all flags.
    fn pop(&mut self, count: u32) {
        if count == 0 {
            return;
        }
        match usize::try_from(count) {
            Ok(count) if count < self.stack.len() => {
                self.stack.truncate(self.stack.len() - count);
            }
            _ => *self = Self::default(),
        }
    }

    /// `CSI = flags ; mode u`: 1 sets the current flags, 2 adds to them, 3
    /// removes from them.
    fn set(&mut self, flags: u32, mode: u32) {
        let current = self.stack.last_mut().unwrap_or(&mut self.base);
        match mode {
            1 => *current = flags,
            2 => *current |= flags,
            3 => *current &= !flags,
            _ => {}
        }
    }

    fn replay(&self, out: &mut Vec<u8>) {
        if self.base != 0 {
            out.extend_from_slice(format!("\x1b[={}u", self.base).as_bytes());
        }
        for flags in &self.stack {
            out.extend_from_slice(format!("\x1b[>{flags}u").as_bytes());
        }
    }

    fn reset(&self, out: &mut Vec<u8>) {
        if !self.stack.is_empty() {
            out.extend_from_slice(format!("\x1b[<{}u", self.stack.len()).as_bytes());
        }
        if self.base != 0 {
            out.extend_from_slice(b"\x1b[=0u");
        }
    }
}

fn dec_mode(out: &mut Vec<u8>, mode: u16, on: bool) {
    let set = if on { 'h' } else { 'l' };
    out.extend_from_slice(format!("\x1b[?{mode}{set}").as_bytes());
}

/// A CSI sequence's parameters, split on `;` with sub-parameters after `:`
/// ignored. An empty parameter is `None`. At most [`MAX_PARAMS`] are kept.
struct Params {
    values: [Option<u32>; MAX_PARAMS],
    len: usize,
}

impl Params {
    /// `None` if a parameter holds anything but digits: a sequence this
    /// module does not act on.
    fn parse(bytes: &[u8]) -> Option<Self> {
        let mut params = Self {
            values: [None; MAX_PARAMS],
            len: 0,
        };
        if bytes.is_empty() {
            return Some(params);
        }
        for param in bytes.split(|&b| b == b';') {
            let main = param.split(|&b| b == b':').next().unwrap_or_default();
            if !main.iter().all(u8::is_ascii_digit) {
                return None;
            }
            if params.len < MAX_PARAMS {
                params.values[params.len] = (!main.is_empty()).then(|| {
                    main.iter().fold(0u32, |n, &d| {
                        n.saturating_mul(10).saturating_add(u32::from(d - b'0'))
                    })
                });
                params.len += 1;
            }
        }
        Some(params)
    }

    fn get(&self, i: usize) -> Option<u32> {
        self.values.get(i).copied().flatten()
    }

    /// No parameter has a value.
    fn is_empty(&self) -> bool {
        self.iter().all(|p| p.is_none())
    }

    fn iter(&self) -> impl Iterator<Item = Option<u32>> + '_ {
        self.values[..self.len].iter().copied()
    }
}

/// What a byte completed.
enum Action {
    None,
    /// `ESC c`.
    FullReset,
    /// `ESC =` (true) or `ESC >` (false).
    Keypad(bool),
    /// A CSI sequence ended with this final byte; its parameter and
    /// intermediate bytes are in [`Parser::csi`].
    Csi(u8),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    /// Inside OSC, DCS, APC, PM or SOS, whose text is not parsed.
    String,
    StringEscape,
}

/// A minimal VT parser (after <https://vt100.net/emu/dec_ansi_parser>): enough
/// to find escape and CSI sequences and to skip the strings that may contain
/// text looking like them. UTF-8 bytes are never C1 controls here.
#[derive(Debug, Clone, Default)]
struct Parser {
    state: State,
    csi: Vec<u8>,
    overflow: bool,
}

const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;
const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

impl Parser {
    fn advance(&mut self, byte: u8) -> Action {
        match self.state {
            State::Ground => {
                if byte == ESC {
                    self.state = State::Escape;
                }
                Action::None
            }
            State::Escape => self.escape(byte),
            State::EscapeIntermediate => {
                match byte {
                    ESC => self.state = State::Escape,
                    CAN | SUB | 0x30..=0x7e => self.state = State::Ground,
                    _ => {}
                }
                Action::None
            }
            State::Csi => match byte {
                0x20..=0x3f => {
                    if self.csi.len() < CSI_LIMIT {
                        self.csi.push(byte);
                    } else {
                        self.overflow = true;
                    }
                    Action::None
                }
                0x40..=0x7e => {
                    self.state = State::Ground;
                    if self.overflow {
                        Action::None
                    } else {
                        Action::Csi(byte)
                    }
                }
                ESC => {
                    self.state = State::Escape;
                    Action::None
                }
                CAN | SUB | 0x80..=0xff => {
                    self.state = State::Ground;
                    Action::None
                }
                // Other C0 controls act within the sequence; DEL is ignored.
                _ => Action::None,
            },
            State::String => {
                match byte {
                    BEL | CAN | SUB => self.state = State::Ground,
                    ESC => self.state = State::StringEscape,
                    _ => {}
                }
                Action::None
            }
            State::StringEscape => {
                if byte == b'\\' {
                    self.state = State::Ground;
                    Action::None
                } else {
                    // ESC followed by anything else ends the string and starts
                    // a new sequence.
                    self.escape(byte)
                }
            }
        }
    }

    fn escape(&mut self, byte: u8) -> Action {
        self.state = State::Ground;
        match byte {
            b'[' => {
                self.state = State::Csi;
                self.csi.clear();
                self.overflow = false;
                Action::None
            }
            b']' | b'P' | b'_' | b'^' | b'X' => {
                self.state = State::String;
                Action::None
            }
            b'c' => Action::FullReset,
            b'=' => Action::Keypad(true),
            b'>' => Action::Keypad(false),
            ESC => {
                self.state = State::Escape;
                Action::None
            }
            0x20..=0x2f => {
                self.state = State::EscapeIntermediate;
                Action::None
            }
            // Other C0 controls act within the sequence.
            0x00..=0x1f if byte != CAN && byte != SUB => {
                self.state = State::Escape;
                Action::None
            }
            _ => Action::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fed(chunks: &[&[u8]]) -> TermModes {
        let mut modes = TermModes::new();
        for chunk in chunks {
            modes.feed(chunk);
        }
        modes
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).replace('\x1b', "^[")
    }

    #[test]
    fn nothing_tracked_means_nothing_to_replay_or_reset() {
        let modes = fed(&[b"plain text\r\n\x1b[1;31mred\x1b[0m\x1b[2J\x1b[H"]);
        assert_eq!(text(&modes.replay()), "");
        assert_eq!(text(&modes.reset()), "");
    }

    /// Claude Code 2.1.316's start-up under `TERM=xterm-ghostty`, as recorded
    /// on the cloud PTY VM (plan 00, step 5).
    const CLAUDE_START: &[u8] = b"\x1b7\x1b[r\x1b8\x1b[?25h\
        \x1b[?2004h\x1b[?2031h\x1b[?1004h\x1b[<u\x1b[>5u\x1b[>4;2m\
        \x1b]0;\xe2\x9c\xb3 Claude Code\x07\
        \x1b[?1049h\x1b[2J\x1b[H\x1b[<u\x1b[>5u\x1b[>4;2m\
        \x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?25l\x1b[?25l\x1b[?2026h\x1b[H\r\
        \x1b[>0q\x1b[?u\x1b]7501;?\x07\x1b[c\x1b[?2026$p";

    #[test]
    fn claude_start_up_is_replayed_and_reset_in_order() {
        let modes = fed(&[CLAUDE_START]);
        assert_eq!(
            text(&modes.replay()),
            "^[[>5u^[[?1049h^[[>5u\
             ^[[?1003h^[[?1004h^[[?1006h^[[?2004h^[[?2031h\
             ^[[?25l^[[>4;2m"
        );
        assert_eq!(
            text(&modes.reset()),
            "^[[?2026l^[[<1u^[[?1049l^[[<1u\
             ^[[?1003l^[[?1004l^[[?1006l^[[?2004l^[[?2031l\
             ^[[?25h^[[>4m"
        );
    }

    #[test]
    fn split_feeds_inside_string_terminators_and_short_escapes() {
        let input: &[u8] = b"\x1b]0;t\x1b\\\x1b[?1004h\x1b=\x1b[?7l";
        let whole = fed(&[input]);
        assert_eq!(text(&whole.replay()), "^[[?1004h^[[?7l^[=");
        for at in 1..input.len() {
            let split = fed(&[&input[..at], &input[at..]]);
            assert_eq!(split.replay(), whole.replay(), "split at {at}");
        }
    }

    #[test]
    fn sequences_split_across_feeds_are_tracked() {
        let whole = fed(&[CLAUDE_START]);
        for at in 1..CLAUDE_START.len() {
            let split = fed(&[&CLAUDE_START[..at], &CLAUDE_START[at..]]);
            assert_eq!(split.replay(), whole.replay(), "split at {at}");
        }
        let bytewise: Vec<&[u8]> = CLAUDE_START.chunks(1).collect();
        assert_eq!(fed(&bytewise).reset(), whole.reset());
    }

    #[test]
    fn modes_turned_off_again_are_not_tracked() {
        let modes = fed(&[b"\x1b[?1049h\x1b[?1003;1006h\x1b[?25l\x1b=\x1b[>4;1m\
            \x1b[?1003;1006l\x1b[?1049l\x1b[?25h\x1b>\x1b[>4m"]);
        assert_eq!(text(&modes.replay()), "");
        assert_eq!(text(&modes.reset()), "");
    }

    #[test]
    fn kitty_flags_are_restored_per_screen() {
        // Main screen: push 1, then 3; set the top to 3|8. Alternate screen:
        // a pop on its empty stack, then push 5 and 7, pop one.
        let modes = fed(&[b"\x1b[>1u\x1b[>3u\x1b[=8;2u\
            \x1b[?1049h\x1b[<u\x1b[>5u\x1b[>7u\x1b[<1u"]);
        assert_eq!(text(&modes.replay()), "^[[>1u^[[>11u^[[?1049h^[[>5u");
        assert_eq!(text(&modes.reset()), "^[[<1u^[[?1049l^[[<2u");

        // Leaving the alternate screen keeps its stack, as kitty and Ghostty
        // do: it is reached with the plain switch, which neither clears nor
        // moves the cursor.
        let mut left = modes.clone();
        left.feed(b"\x1b[?1049l");
        assert_eq!(text(&left.replay()), "^[[>1u^[[>11u^[[?47h^[[>5u^[[?47l");
        assert_eq!(text(&left.reset()), "^[[?47h^[[<1u^[[?47l^[[<2u");
        // Entering it again pushes onto the same stack.
        left.feed(b"\x1b[?1049h\x1b[>5u");
        assert_eq!(text(&left.replay()), "^[[>1u^[[>11u^[[?1049h^[[>5u^[[>5u");
    }

    #[test]
    fn kitty_flags_set_without_a_push_are_reset_to_zero() {
        let modes = fed(&[b"\x1b[=5u"]);
        assert_eq!(text(&modes.replay()), "^[[=5u");
        assert_eq!(text(&modes.reset()), "^[[=0u");
        let cleared = fed(&[b"\x1b[=5u\x1b[=5;3u"]);
        assert_eq!(text(&cleared.replay()), "");
    }

    #[test]
    fn a_kitty_pop_that_empties_the_stack_resets_all_flags() {
        assert_eq!(text(&fed(&[b"\x1b[=5u\x1b[>1u\x1b[<2u"]).replay()), "");
        assert_eq!(text(&fed(&[b"\x1b[=5u\x1b[<u"]).replay()), "");
        // A pop of nothing pops nothing.
        assert_eq!(text(&fed(&[b"\x1b[>1u\x1b[<0u"]).replay()), "^[[>1u");
    }

    #[test]
    fn a_reset_from_the_child_clears_tracked_modes() {
        let mut modes = fed(&[CLAUDE_START, b"\x1b=\x1b[?1h"]);
        modes.feed(b"\x1bc");
        assert_eq!(text(&modes.replay()), "");
        assert_eq!(text(&modes.reset()), "");
    }

    #[test]
    fn a_soft_reset_shows_the_cursor_and_resets_the_keys() {
        let modes = fed(&[b"\x1b[?25l\x1b[?1h\x1b=\x1b[>4;2m\x1b[?1003h\x1b[!p"]);
        assert_eq!(text(&modes.replay()), "^[[?1003h");
    }

    #[test]
    fn one_mouse_tracking_mode_and_one_encoding_at_a_time() {
        // The last mode set wins; resetting any of them turns tracking off.
        let modes = fed(&[b"\x1b[?1000h\x1b[?1002h\x1b[?1005h\x1b[?1006h"]);
        assert_eq!(text(&modes.replay()), "^[[?1002h^[[?1006h");
        assert_eq!(text(&modes.reset()), "^[[?1002l^[[?1006l");
        let off = fed(&[b"\x1b[?1003h\x1b[?1000l\x1b[?1006h\x1b[?1015l"]);
        assert_eq!(text(&off.replay()), "");
        let x10 = fed(&[b"\x1b[?9h"]);
        assert_eq!(text(&x10.replay()), "^[[?9h");
    }

    #[test]
    fn autowrap_and_size_reports_are_tracked() {
        let modes = fed(&[b"\x1b[?7l\x1b[?2048h"]);
        assert_eq!(text(&modes.replay()), "^[[?2048h^[[?7l");
        assert_eq!(text(&modes.reset()), "^[[?2048l^[[?7h");
    }

    #[test]
    fn modify_other_keys_forms() {
        assert_eq!(text(&fed(&[b"\x1b[>4;2m"]).replay()), "^[[>4;2m");
        assert_eq!(text(&fed(&[b"\x1b[>4;2m\x1b[>4;0m"]).replay()), "");
        assert_eq!(text(&fed(&[b"\x1b[>4;2m\x1b[>4n"]).replay()), "");
        assert_eq!(text(&fed(&[b"\x1b[>4;2m\x1b[>m"]).replay()), "");
        // Other XTMODKEYS resources are not tracked.
        assert_eq!(text(&fed(&[b"\x1b[>1;2m"]).replay()), "");
    }

    #[test]
    fn mode_text_inside_strings_is_not_a_mode() {
        let modes = fed(&[
            b"\x1b]0;[?1049h title\x07",
            b"\x1bPq[?1003h\x1b\\",
            b"\x1b_Ga=q;[?2004h\x1b\\",
            b"\x1b]2;bell-less\x1b\\",
        ]);
        assert_eq!(text(&modes.replay()), "");
    }

    #[test]
    fn a_cancelled_or_overlong_sequence_does_nothing() {
        let mut long = b"\x1b[?".to_vec();
        long.extend(std::iter::repeat_n(b'1', 200));
        long.extend_from_slice(b";1049h");
        let modes = fed(&[
            b"\x1b[?10\x1849h",
            b"\x1b[?10\x1a49h",
            b"\x1b[?10\x1b[0m49h",
            b"\x1b[?10\xc3\xa949h",
            &long,
        ]);
        assert_eq!(text(&modes.replay()), "");
    }

    #[test]
    fn untracked_and_query_sequences_are_ignored() {
        let modes = fed(&[b"\x1b[?12h\x1b[?u\x1b[?1049$p\x1b[4h\x1b[1049h\x1b[u\x1b[?3;4l"]);
        assert_eq!(text(&modes.replay()), "");
    }

    #[test]
    fn a_kitty_stack_is_bounded() {
        let mut modes = TermModes::new();
        for _ in 0..10_000 {
            modes.feed(b"\x1b[>1u");
        }
        assert!(modes.replay().len() < 1024);
    }
}
