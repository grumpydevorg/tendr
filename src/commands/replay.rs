//! `tendr replay`: play a PTY recording's output in this terminal at the
//! recorded pace, or export it as asciicast v2. Local-only: a recording on
//! another host is copied here first.
//!
//! Playback writes only `OUTPUT` records, never connects to a session and
//! never sends anything anywhere. The terminal is in raw mode throughout
//! ([`super::terminal`]); whatever it sends back, query replies included, is
//! read and dropped, and the terminal is restored however playback ends.
//! Tendr has no screen engine, so geometry is the user's to match: the
//! window is asked for once before playback, and by default a mismatch later
//! is reported once, after playback. `--strict-geometry` pauses at each
//! recorded size change until the window matches. Repaint resizes never
//! prompt; their output is played like any other.
//!
//! The recording is decoded whole before anything is shown, with the library
//! decoder `examples/dump-recording.rs` uses: a damaged recording plays
//! nothing and the error names the file and byte offset. A final segment cut
//! off inside a record, as an active run's is, plays to its last complete
//! record and the cut is reported.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use tendr::recorder::DirectoryStore;
use tendr::recording::{
    Corruption, DecodeError, DecodeErrorKind, DecodedRecording, Geometry, RecordKind, ResizeCause,
    SegmentEnd, Sequence, decode_recording, decode_segment,
};
use tendr::term_modes::TermModes;

use super::terminal::{self, Input, Leave, Output, Raw};

/// How often the window size is checked while waiting for it to match. A
/// resize notice brings the check forward.
const SIZE_POLL: Duration = Duration::from_millis(100);
/// After the last record, input is drained until the terminal has been quiet
/// this long, so replies to the last queries do not reach the shell.
const END_QUIET: Duration = Duration::from_millis(100);
/// The longest the end drain lasts.
const END_DRAIN_MAX: Duration = Duration::from_secs(1);

/// `replay --export`'s formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ExportFormat {
    /// asciicast v2 (asciinema)
    Asciicast,
}

pub struct ReplayOptions {
    pub path: PathBuf,
    pub strict_geometry: bool,
    /// Playback speed; 1 is the recorded pace.
    pub speed: f64,
    /// Play without the recorded pauses.
    pub no_delay: bool,
    pub export: Option<ExportFormat>,
}

pub fn cmd_replay(options: &ReplayOptions) -> anyhow::Result<()> {
    let loaded = load(&options.path)?;
    match options.export {
        Some(ExportFormat::Asciicast) => {
            let cast = asciicast(&loaded.recording)?;
            let mut stdout = std::io::stdout().lock();
            stdout
                .write_all(&cast)
                .and_then(|()| stdout.flush())
                .context("writing the export to stdout")?;
            if let Some(note) = loaded.cut_note() {
                eprintln!("tendr: {note}");
            }
            Ok(())
        }
        None => play(&loaded, options),
    }
}

/// A decoded recording and the segment files it came from.
struct Loaded {
    recording: DecodedRecording,
    segments: Vec<PathBuf>,
}

impl Loaded {
    /// What to say about a final segment cut off inside a record.
    fn cut_note(&self) -> Option<String> {
        let SegmentEnd::Truncated { offset } = self.recording.end else {
            return None;
        };
        let file = self.segments.last().expect("a recording has a segment");
        let played = self.recording.records.last().map_or_else(
            || "no record".to_owned(),
            |r| format!("#{}", r.sequence.get()),
        );
        Some(format!(
            "the recording ends inside a record at {} byte offset {offset} \
             (an active run's recording ends this way): this is up to {played}, \
             its last complete record",
            file.display()
        ))
    }
}

/// Read and decode the recording at `path`: a segment file, a run's
/// recording directory, or a directory holding exactly one run's recording
/// (a session's `recording/`, or the session directory).
fn load(path: &Path) -> anyhow::Result<Loaded> {
    if path.is_file() {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let segments = vec![path.to_path_buf()];
        let segment = decode_segment(&bytes).map_err(|e| decode_failure(&e, &segments))?;
        return Ok(Loaded {
            recording: DecodedRecording {
                header: segment.header,
                segment_count: 1,
                records: segment.records,
                end: segment.end,
            },
            segments,
        });
    }
    let mut runs = DirectoryStore::runs(path)
        .with_context(|| format!("listing recordings at {}", path.display()))?;
    let segments = match runs.len() {
        0 => anyhow::bail!(
            "no recording at {}: give a segment file (seg-*.tndrrec), a run's recording \
             directory, or a session directory",
            path.display()
        ),
        1 => runs.pop().expect("one run").1,
        n => {
            let mut message = format!(
                "{} holds {n} runs' recordings; replay one of them:",
                path.display()
            );
            for (dir, _) in &runs {
                let _ = write!(message, "\n  {}", dir.display());
            }
            anyhow::bail!(message);
        }
    };
    let data = segments
        .iter()
        .map(|p| std::fs::read(p).with_context(|| format!("reading {}", p.display())))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let recording = decode_recording(data.iter().map(Vec::as_slice))
        .map_err(|e| decode_failure(&e, &segments))?;
    Ok(Loaded {
        recording,
        segments,
    })
}

/// A decoding failure, naming the file and byte offset at fault.
fn decode_failure(error: &DecodeError, segments: &[PathBuf]) -> anyhow::Error {
    let file = error
        .segment
        .and_then(|i| segments.get(i as usize))
        .or(segments.first())
        .map_or_else(String::new, |p| p.display().to_string());
    let what = match error.kind {
        DecodeErrorKind::BadMagic => "not a tendr recording".to_owned(),
        DecodeErrorKind::UnsupportedVersion(v) => {
            format!("recording format version {v}, which this tendr cannot read")
        }
        DecodeErrorKind::Corrupt(c) => match c {
            Corruption::HeaderIncomplete => "corrupt: the segment header is incomplete",
            Corruption::HeaderLength => "corrupt: bad segment header length",
            Corruption::HeaderChecksum => "corrupt: segment header checksum mismatch",
            Corruption::LengthCheck => "corrupt: damaged record length",
            Corruption::LengthBounds => "corrupt: record length out of bounds",
            Corruption::RecordChecksum => "corrupt: record checksum mismatch",
            Corruption::TruncatedInteriorSegment => {
                "corrupt: a segment before the last ends inside a record"
            }
        }
        .to_owned(),
        DecodeErrorKind::Invalid(v) => format!("breaks the recording format ({v:?})"),
    };
    let last = error.last_valid_sequence.map_or_else(
        || "no record before it is valid".to_owned(),
        |s| format!("the last valid record before it is #{}", s.get()),
    );
    anyhow::anyhow!(
        "{file}: {what} at byte offset {}; {last}. Nothing was played",
        error.offset
    )
}

// --- playback ---

/// Why playback ended before its last record.
enum Stop {
    /// The user pressed Ctrl-C.
    Cancelled,
    /// A signal, or on Windows a console control event.
    Left(Leave),
    /// The keyboard closed.
    Closed,
}

/// The first recorded size change the window did not match.
struct Mismatch {
    at: Option<Sequence>,
    recorded: Geometry,
    window: Geometry,
}

fn play(loaded: &Loaded, options: &ReplayOptions) -> anyhow::Result<()> {
    let recording = &loaded.recording;
    let terminal = terminal::open("replay")?;
    let raw = terminal.enter()?;
    let output = raw.output();
    if options.strict_geometry && output.size().is_none() {
        anyhow::bail!(
            "--strict-geometry needs the window size, and this terminal does not report it"
        );
    }
    let mut player = Player {
        raw,
        output,
        modes: TermModes::new(),
        buf: vec![0; 4096],
        played: None,
    };
    let ended = player.run(recording, options);
    // Turn off what the recording turned on, then restore the terminal.
    let _ = player.output.write(&player.modes.reset());
    let played = player.played;
    drop(player);

    match ended? {
        Ok(mismatch) => {
            if let Some(m) = mismatch {
                let at =
                    m.at.map_or_else(|| "the start".to_owned(), |s| format!("#{}", s.get()));
                eprintln!(
                    "tendr: from {at} the recording was {} and this window {}: the bytes \
                     played exactly, but the picture may differ (--strict-geometry pauses \
                     until the window matches)",
                    size(m.recorded),
                    size(m.window)
                );
            }
            if let Some(note) = loaded.cut_note() {
                eprintln!("tendr: played to the end; {note}");
            }
            Ok(())
        }
        Err(Stop::Cancelled) => {
            eprintln!("tendr: replay stopped {}", where_stopped(played));
            Ok(())
        }
        Err(Stop::Left(leave)) => anyhow::bail!(
            "tendr: replay ended by {} {}",
            leave.name(),
            where_stopped(played)
        ),
        Err(Stop::Closed) => anyhow::bail!("tendr: the terminal closed {}", where_stopped(played)),
    }
}

fn where_stopped(played: Option<Sequence>) -> String {
    played.map_or_else(
        || "before the first record".to_owned(),
        |s| format!("after #{}", s.get()),
    )
}

/// A size as `<cols>x<rows>`, the way terminals name it.
fn size(geometry: Geometry) -> String {
    format!("{}x{}", geometry.cols(), geometry.rows())
}

struct Player {
    raw: Box<dyn Raw>,
    output: Arc<dyn Output>,
    /// Fed with everything played, to turn its modes off when leaving.
    modes: TermModes,
    buf: Vec<u8>,
    /// The last record played.
    played: Option<Sequence>,
}

impl Player {
    /// Play `recording`: `Ok(Ok(_))` once every record is played, with the
    /// first size mismatch in default mode.
    fn run(
        &mut self,
        recording: &DecodedRecording,
        options: &ReplayOptions,
    ) -> anyhow::Result<Result<Option<Mismatch>, Stop>> {
        let initial = recording.header.geometry;
        let mut mismatch = None;
        if self.output.size() != Some(initial) {
            let largest = largest(recording);
            let window = self.output.size();
            let ready = if options.strict_geometry {
                self.wait_for_size(initial, |window| {
                    prompt(initial, largest, window, "and it plays")
                })?
            } else if window.is_some() {
                self.wait_for_key(|window| {
                    prompt(initial, largest, window, "then press any key to play")
                })?
            } else {
                Ok(())
            };
            if let Err(stop) = ready {
                return Ok(Err(stop));
            }
            if let Some(window) = self.output.size().filter(|w| *w != initial) {
                mismatch = Some(Mismatch {
                    at: None,
                    recorded: initial,
                    window,
                });
            }
        }
        // The run started on an empty screen.
        self.write(b"\x1b[0m\x1b[H\x1b[2J")?;

        let base = recording.records.first().map_or(0, |r| r.elapsed_ns);
        let mut clock = Instant::now();
        let mut real = initial;
        for record in &recording.records {
            let due = (!options.no_delay).then(|| {
                // Elapsed time never decreases within a recording.
                let offset = record.elapsed_ns.saturating_sub(base) as f64 / 1e9 / options.speed;
                Duration::try_from_secs_f64(offset)
                    .ok()
                    .and_then(|offset| clock.checked_add(offset))
            });
            let waited = match due {
                Some(due) => self.wait_until(due),
                None => self.poll(Duration::ZERO),
            };
            if let Some(stop) = waited {
                return Ok(Err(stop));
            }
            match &record.kind {
                RecordKind::Output(bytes) => {
                    self.write(bytes)?;
                    self.modes.feed(bytes);
                }
                RecordKind::Resize {
                    geometry,
                    cause: ResizeCause::User,
                } if *geometry != real => {
                    real = *geometry;
                    if options.strict_geometry {
                        let paused = Instant::now();
                        if let Err(stop) = self.pause_for(real)? {
                            return Ok(Err(stop));
                        }
                        clock += paused.elapsed();
                    } else if mismatch.is_none() {
                        if let Some(window) = self.output.size().filter(|w| *w != real) {
                            mismatch = Some(Mismatch {
                                at: Some(record.sequence),
                                recorded: real,
                                window,
                            });
                        }
                    }
                }
                // Repaints, a resize to the size in effect, and input play
                // nothing.
                RecordKind::Resize { .. } | RecordKind::Input(_) => {}
            }
            self.played = Some(record.sequence);
        }
        self.drain();
        Ok(Ok(mismatch))
    }

    fn write(&self, bytes: &[u8]) -> anyhow::Result<()> {
        self.output.write(bytes).context("writing to the terminal")
    }

    /// Wait up to `timeout` for the keyboard, dropping what it sends.
    fn poll(&mut self, timeout: Duration) -> Option<Stop> {
        match self.raw.input(&mut self.buf, timeout) {
            Input::Bytes(n) => is_cancel(&self.buf[..n]).then_some(Stop::Cancelled),
            Input::Leave(leave) => Some(Stop::Left(leave)),
            Input::Closed => Some(Stop::Closed),
            Input::SizeDue | Input::Wake => None,
        }
    }

    /// Drop input until `due`; `None` waits for a stop alone.
    fn wait_until(&mut self, due: Option<Instant>) -> Option<Stop> {
        loop {
            let timeout = match due {
                Some(due) => due.saturating_duration_since(Instant::now()),
                None => Duration::from_secs(3600),
            };
            if let Some(stop) = self.poll(timeout) {
                return Some(stop);
            }
            if due.is_some_and(|due| Instant::now() >= due) {
                return None;
            }
        }
    }

    /// Show `line` for the window's size until a key other than Ctrl-C.
    fn wait_for_key(
        &mut self,
        line: impl Fn(Option<Geometry>) -> String,
    ) -> anyhow::Result<Result<(), Stop>> {
        let mut shown = self.output.size();
        self.write(line(shown).as_bytes())?;
        loop {
            match self.raw.input(&mut self.buf, SIZE_POLL) {
                Input::Bytes(n) if is_cancel(&self.buf[..n]) => return Ok(Err(Stop::Cancelled)),
                Input::Bytes(_) => return Ok(Ok(())),
                Input::Leave(leave) => return Ok(Err(Stop::Left(leave))),
                Input::Closed => return Ok(Err(Stop::Closed)),
                Input::SizeDue | Input::Wake => {}
            }
            let window = self.output.size();
            if window != shown {
                shown = window;
                self.write(line(shown).as_bytes())?;
            }
        }
    }

    /// Show `line` for the window's size until the window is `wanted`.
    fn wait_for_size(
        &mut self,
        wanted: Geometry,
        line: impl Fn(Option<Geometry>) -> String,
    ) -> anyhow::Result<Result<(), Stop>> {
        let mut shown = None;
        loop {
            let window = self.output.size();
            if window == Some(wanted) {
                return Ok(Ok(()));
            }
            if shown != Some(window) {
                shown = Some(window);
                self.write(line(window).as_bytes())?;
            }
            if let Some(stop) = self.poll(SIZE_POLL) {
                return Ok(Err(stop));
            }
        }
    }

    /// `--strict-geometry` at a recorded size change: a note on the bottom
    /// line until the window is `wanted`, then the note erased. The program
    /// recorded redraws after a resize, which covers what is left of it.
    fn pause_for(&mut self, wanted: Geometry) -> anyhow::Result<Result<(), Stop>> {
        let Some(window) = self.output.size() else {
            return Ok(Ok(()));
        };
        if window == wanted {
            return Ok(Ok(()));
        }
        let mut note = format!(
            " tendr replay: the recording resizes to {} here; size this window to match. \
             Ctrl-C stops. ",
            size(wanted)
        );
        note.truncate(usize::from(window.cols()).saturating_sub(1));
        let row = window.rows();
        self.write(
            format!("\x1b7\x1b[{row};1H\x1b[0m\x1b[7m\x1b[2K{note}\x1b[0m\x1b8").as_bytes(),
        )?;
        let ready = self.wait_for_size(wanted, |_| String::new())?;
        if ready.is_ok() {
            self.write(format!("\x1b7\x1b[{row};1H\x1b[2K\x1b8").as_bytes())?;
        }
        Ok(ready)
    }

    /// Drop what the terminal sends until it has been quiet for
    /// [`END_QUIET`], at most [`END_DRAIN_MAX`]: replies to the last
    /// queries played must not reach the shell after the terminal is
    /// restored.
    fn drain(&mut self) {
        let end = Instant::now() + END_DRAIN_MAX;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            match self.raw.input(&mut self.buf, END_QUIET.min(left)) {
                Input::Bytes(_) | Input::SizeDue => {}
                Input::Wake | Input::Leave(_) | Input::Closed => return,
            }
        }
    }
}

/// The prompt before playback, drawn whole from the top of a cleared screen
/// on each change, in lines short enough not to wrap at any size (none is
/// over 70 columns), so a redraw never leaves a stale line behind.
fn prompt(initial: Geometry, largest: Geometry, window: Option<Geometry>, then: &str) -> String {
    let now = window.map_or_else(|| "unknown".to_owned(), size);
    let largest = if largest == initial {
        String::new()
    } else {
        format!("Its largest size is {}.\r\n", size(largest))
    };
    format!(
        "\x1b[H\x1b[2Jtendr replay: the recording starts at {} (cols x rows).\r\n\
         {largest}Size this window to {} (now {now}),\r\n{then}. Ctrl-C stops.",
        size(initial),
        size(initial),
    )
}

/// The largest rows and columns the recording's user sizes reach.
fn largest(recording: &DecodedRecording) -> Geometry {
    let (rows, cols) = recording
        .records
        .iter()
        .filter_map(|r| match r.kind {
            RecordKind::Resize {
                geometry,
                cause: ResizeCause::User,
            } => Some(geometry),
            _ => None,
        })
        .fold(
            (
                recording.header.geometry.rows(),
                recording.header.geometry.cols(),
            ),
            |(rows, cols), g| (rows.max(g.rows()), cols.max(g.cols())),
        );
    Geometry::new(rows, cols).expect("recorded sizes are nonzero")
}

/// Whether the keyboard sent Ctrl-C: the legacy byte, or the forms a
/// keyboard protocol the recording turned on encodes it in (kitty's
/// `CSI 99 ; 5 u`, xterm's `modifyOtherKeys` `CSI 27 ; 5 ; 99 ~`). Lock
/// states are ignored, and so is a key release.
fn is_cancel(bytes: &[u8]) -> bool {
    if bytes.contains(&0x03) {
        return true;
    }
    let ctrl = |mods: &[u8]| {
        std::str::from_utf8(mods)
            .ok()
            .and_then(|m| m.parse::<u16>().ok())
            .is_some_and(|m| m >= 1 && (m - 1) & !(64 | 128) == 4)
    };
    let mut rest = bytes;
    while let Some(at) = rest.windows(2).position(|w| w == b"\x1b[") {
        rest = &rest[at + 2..];
        let end = rest
            .iter()
            .position(|b| (0x40..=0x7e).contains(b))
            .unwrap_or(rest.len());
        let Some(&last) = rest.get(end) else {
            break;
        };
        let params: Vec<&[u8]> = rest[..end].split(|&b| b == b';').collect();
        let hit = match (last, params.as_slice()) {
            // CSI 99 ; mods[:event] u, the key code perhaps with alternates.
            (b'u', [key, mods, ..]) => {
                let key = key.split(|&b| b == b':').next().unwrap_or_default();
                let mut mods = mods.split(|&b| b == b':');
                let modifiers = mods.next().unwrap_or_default();
                let released = mods.next() == Some(b"3");
                key == b"99" && ctrl(modifiers) && !released
            }
            (b'~', [b"27", mods, b"99"]) => ctrl(mods),
            _ => false,
        };
        if hit {
            return true;
        }
        rest = &rest[end..];
    }
    false
}

// --- export ---

/// The recording as asciicast v2: a header line, then one event per output,
/// input and resize record, with its time since the recording's origin.
/// Output and input are decoded as UTF-8 across records, each stream on its
/// own, since a character can be split between two; invalid UTF-8 refuses
/// the export. Every applied resize is an `r` event, repaints included.
fn asciicast(recording: &DecodedRecording) -> anyhow::Result<Vec<u8>> {
    let header = &recording.header;
    let mut out = format!(
        "{{\"version\": 2, \"width\": {}, \"height\": {}, \"timestamp\": {}",
        header.geometry.cols(),
        header.geometry.rows(),
        header.origin_unix_ns / 1_000_000_000
    );
    if !header.term.as_str().is_empty() {
        let term = serde_json::to_string(header.term.as_str()).expect("a string serializes");
        let _ = write!(out, ", \"env\": {{\"TERM\": {term}}}");
    }
    out.push_str("}\n");

    let mut output = Utf8Stream::default();
    let mut input = Utf8Stream::default();
    for record in &recording.records {
        let time = Seconds(record.elapsed_ns);
        let (code, data) = match &record.kind {
            RecordKind::Output(bytes) => ("o", output.decode(record.sequence, bytes)?),
            RecordKind::Input(bytes) => ("i", input.decode(record.sequence, bytes)?),
            RecordKind::Resize { geometry, .. } => ("r", size(*geometry)),
        };
        if data.is_empty() {
            continue;
        }
        let data = serde_json::to_string(&data).expect("a string serializes");
        let _ = writeln!(out, "[{time}, \"{code}\", {data}]");
    }
    for (stream, what) in [(&output, "output"), (&input, "input")] {
        if let Some(from) = stream.pending_from {
            match recording.end {
                // The rest of the character was cut off with the record.
                SegmentEnd::Truncated { .. } => {}
                SegmentEnd::Clean => anyhow::bail!(
                    "the recording's {what} ends inside a UTF-8 character begun in record \
                     #{}; asciicast carries text only",
                    from.get()
                ),
            }
        }
    }
    Ok(out.into_bytes())
}

/// Nanoseconds as seconds with six decimals, as asciicast writes time.
struct Seconds(u64);

impl std::fmt::Display for Seconds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}.{:06}",
            self.0 / 1_000_000_000,
            self.0 % 1_000_000_000 / 1_000
        )
    }
}

/// Incremental UTF-8 decoding of one byte stream fed record by record.
#[derive(Default)]
struct Utf8Stream {
    /// The start of a character the last record ended inside.
    pending: Vec<u8>,
    /// The record that began it.
    pending_from: Option<Sequence>,
}

impl Utf8Stream {
    /// The text `bytes` complete, holding back a character they end inside.
    fn decode(&mut self, sequence: Sequence, bytes: &[u8]) -> anyhow::Result<String> {
        let carried = self.pending.len();
        let mut joined = std::mem::take(&mut self.pending);
        joined.extend_from_slice(bytes);
        let from = self.pending_from.take().unwrap_or(sequence);
        match std::str::from_utf8(&joined) {
            Ok(text) => Ok(text.to_owned()),
            Err(e) if e.error_len().is_none() => {
                let valid = e.valid_up_to();
                self.pending = joined[valid..].to_vec();
                self.pending_from = Some(if valid < carried { from } else { sequence });
                Ok(String::from_utf8(joined[..valid].to_vec()).expect("validated prefix"))
            }
            Err(e) => {
                let valid = e.valid_up_to();
                let place = if valid < carried {
                    format!(
                        "the end of record #{}, continued in #{}",
                        from.get(),
                        sequence.get()
                    )
                } else {
                    format!("byte {} of record #{}", valid - carried, sequence.get())
                };
                anyhow::bail!(
                    "invalid UTF-8 at {place}: asciicast carries text only (the recording \
                     itself stays the exact source)"
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_c_is_recognized_in_each_keyboard_encoding() {
        for (bytes, cancel) in [
            (&b"\x03"[..], true),
            (b"x\x1b[99;5u", true),
            (b"\x1b[99;5:1u", true),
            (b"\x1b[99;69u", true),   // caps lock on
            (b"\x1b[99:67;5u", true), // with an alternate key
            (b"\x1b[27;5;99~", true),
            (b"\x1b[99;5:3u", false), // a release
            (b"\x1b[99;7u", false),   // ctrl-alt-c
            (b"\x1b[99u", false),
            (b"\x1b[6;1R", false), // a cursor position reply
            (b"\x1b]11;rgb:0000/0000/0000\x07", false),
            (b"c", false),
        ] {
            assert_eq!(is_cancel(bytes), cancel, "{bytes:?}");
        }
    }
}
