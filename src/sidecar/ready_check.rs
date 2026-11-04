//! A run's ready condition (`start --ready-when-output`, `--ready-when-port`,
//! `--ready-within`), checked by the sidecar while the run lasts.
//!
//! One thread per run decides the outcome, once: `Ready` when the condition
//! holds, `Missed` when the deadline passes first. It appends `run.ready` or
//! `run.ready_missed`, then patches `meta.json` `ready`, both under
//! [`META_WRITE`], so the guard's whole-meta writes, which fold
//! [`ReadyCheck::outcome`] in under the same lock, never revert it (WAL order,
//! spec §3.6: the event first). A missed deadline only records: the child
//! keeps running and the condition is no longer checked.
//!
//! Output is matched by the capture threads through [`OutputMatcher`], which
//! only notes the first match; the thread records it. [`ReadyCheck::close`]
//! ends the check before the run's terminal record, so nothing is recorded
//! after the run ends.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::{Fact, LifecycleEvents, META_WRITE, lock, patch_meta_file};
use crate::model::ids::EpochTimestamp;
use crate::model::ready::{ReadyCheck as ReadySpec, ReadyState, ReadyWhen, output_regex};

/// How often a port condition is probed.
const PORT_POLL: Duration = Duration::from_millis(200);

/// How long one probe of a port waits for its connection.
const PORT_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// The longest unended output line a PTY matcher keeps: a longer one is
/// matched by its last this many bytes.
const MAX_CARRY: usize = 64 * 1024;

/// A run's ready check. Dropping it does not stop the thread; [`Self::close`]
/// does.
pub(super) struct ReadyCheck {
    shared: Arc<Shared>,
    /// For the capture threads; `None` for a port condition.
    regex: Option<Arc<regex::bytes::Regex>>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    /// Set once the outcome is decided or the run has ended, so capture
    /// stops matching.
    done: AtomicBool,
}

#[derive(Default)]
struct State {
    /// The first output line that matched, and when.
    matched: Option<(Instant, Vec<u8>)>,
    outcome: Option<ReadyState>,
    closed: bool,
    /// The checker's thread has ended; nothing more will be recorded.
    checker_gone: bool,
}

/// `run.ready` data.
#[derive(serde::Serialize)]
struct ReadyData {
    /// Milliseconds from `Running` to the condition holding.
    after_ms: u64,
    /// The output line that matched, lossily as UTF-8.
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
}

/// `run.ready_missed` data.
#[derive(serde::Serialize)]
struct MissedData {
    within_ms: u64,
}

impl ReadyCheck {
    /// Start checking `spec` for a run that is `Running` now. Its facts go
    /// through `facts`; its outcome is patched into `session_dir/meta.json`.
    ///
    /// # Errors
    /// A message for a meta warning when the condition cannot be checked (a
    /// regex that no longer compiles, or no thread for the check).
    pub(super) fn start(
        spec: &ReadySpec,
        facts: LifecycleEvents,
        session_dir: PathBuf,
    ) -> Result<Self, String> {
        let regex = match &spec.when {
            ReadyWhen::Output(pattern) => Some(Arc::new(
                output_regex(pattern).map_err(|e| format!("ready check not run: {e}"))?,
            )),
            ReadyWhen::Port(_) => None,
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            done: AtomicBool::new(false),
        });
        let checker = Checker {
            shared: Arc::clone(&shared),
            port: match spec.when {
                ReadyWhen::Port(port) => Some(port),
                ReadyWhen::Output(_) => None,
            },
            within: spec.within_ms.map(Duration::from_millis),
            running: Instant::now(),
            facts,
            session_dir,
        };
        std::thread::Builder::new()
            .name("ready-check".to_owned())
            .spawn(move || checker.run())
            .map_err(|e| format!("ready check not run: cannot start its thread: {e}"))?;
        Ok(Self { shared, regex })
    }

    /// A matcher for one output stream, if the condition is on output.
    pub(super) fn output_matcher(&self) -> Option<OutputMatcher> {
        Some(OutputMatcher {
            regex: Arc::clone(self.regex.as_ref()?),
            shared: Arc::clone(&self.shared),
            carry: Vec::new(),
        })
    }

    /// The outcome recorded so far. The caller holds [`META_WRITE`], so a
    /// record under way is either finished or not yet started.
    pub(super) fn outcome(&self) -> Option<ReadyState> {
        lock(&self.shared.state).outcome
    }

    /// End the check: nothing is recorded after this returns. A match the
    /// capture threads noted is recorded first, so a child that prints its
    /// ready line and exits at once is still ready; a record under way
    /// finishes first too. The caller holds no [`META_WRITE`], which the
    /// record needs.
    pub(super) fn close(&self) {
        let mut state = lock(&self.shared.state);
        while state.matched.is_some() && state.outcome.is_none() && !state.checker_gone {
            state = self
                .shared
                .changed
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
        state.closed = true;
        self.shared.done.store(true, Ordering::Relaxed);
        self.shared.changed.notify_all();
    }
}

/// Matches one stream's output against the ready regex, for a capture
/// thread. Cheap once the outcome is decided.
pub(super) struct OutputMatcher {
    regex: Arc<regex::bytes::Regex>,
    shared: Arc<Shared>,
    /// A PTY stream's line not yet ended.
    carry: Vec<u8>,
}

impl OutputMatcher {
    /// One whole line, as `output.log` records it (pipe sessions).
    pub(super) fn line(&self, line: &[u8]) {
        if !self.shared.done.load(Ordering::Relaxed) && self.regex.is_match(line) {
            self.matched(line);
        }
    }

    /// Raw terminal output (PTY sessions). Lines end at `\n` or `\r`, as the
    /// terminal shows them: a CRLF or a redraw after `\r` starts a new one.
    /// They are assembled across reads, and the line not yet ended is matched
    /// too, so a prompt that ends without a newline still matches.
    pub(super) fn chunk(&mut self, bytes: &[u8]) {
        if self.shared.done.load(Ordering::Relaxed) {
            return;
        }
        let mut rest = bytes;
        while let Some(end) = rest.iter().position(|&b| b == b'\n' || b == b'\r') {
            self.carry.extend_from_slice(&rest[..end]);
            rest = &rest[end + 1..];
            let line = std::mem::take(&mut self.carry);
            if self.matches_line(&line) {
                return;
            }
        }
        self.carry.extend_from_slice(rest);
        if self.carry.len() > MAX_CARRY {
            self.carry.drain(..self.carry.len() - MAX_CARRY);
        }
        self.matches_line(&self.carry);
    }

    /// Match one PTY line. The empty ones between `\r` and `\n` are not
    /// lines. Returns whether it matched.
    fn matches_line(&self, line: &[u8]) -> bool {
        let hit = !line.is_empty() && self.regex.is_match(line);
        if hit {
            self.matched(line);
        }
        hit
    }

    fn matched(&self, line: &[u8]) {
        let mut state = lock(&self.shared.state);
        if state.matched.is_none() {
            state.matched = Some((Instant::now(), line.to_vec()));
            self.shared.changed.notify_all();
        }
    }
}

/// The check's thread: decides the outcome and records it.
struct Checker {
    shared: Arc<Shared>,
    port: Option<u16>,
    within: Option<Duration>,
    running: Instant,
    facts: LifecycleEvents,
    session_dir: PathBuf,
}

impl Checker {
    fn run(mut self) {
        // However the thread ends, `close` stops waiting for it.
        let _gone = Gone(Arc::clone(&self.shared));
        let deadline = self.within.map(|within| self.running + within);
        loop {
            if let Some(port) = self.port {
                if port_open(port) {
                    let data = ReadyData {
                        after_ms: millis(self.running.elapsed()),
                        line: None,
                        port: Some(port),
                    };
                    self.record(
                        ReadyState::Ready {
                            at: EpochTimestamp::now(),
                        },
                        &data,
                    );
                    return;
                }
            }
            let mut state = lock(&self.shared.state);
            loop {
                // A noted match is recorded even when the run has just ended:
                // `close` waits for it.
                if state.matched.is_some() {
                    drop(state);
                    self.record_match();
                    return;
                }
                if state.closed {
                    return;
                }
                let now = Instant::now();
                if deadline.is_some_and(|deadline| now >= deadline) {
                    drop(state);
                    let data = MissedData {
                        within_ms: self.within.map_or(0, millis),
                    };
                    self.record(
                        ReadyState::Missed {
                            at: EpochTimestamp::now(),
                        },
                        &data,
                    );
                    return;
                }
                let until_deadline = deadline.map(|deadline| deadline - now);
                let wait = match (self.port.map(|_| PORT_POLL), until_deadline) {
                    (Some(poll), Some(left)) => Some(poll.min(left)),
                    (poll, left) => poll.or(left),
                };
                state = match wait {
                    Some(wait) => {
                        let (state, timeout) = self
                            .shared
                            .changed
                            .wait_timeout(state, wait)
                            .unwrap_or_else(|e| e.into_inner());
                        if timeout.timed_out() && self.port.is_some() {
                            break; // probe again
                        }
                        state
                    }
                    None => self
                        .shared
                        .changed
                        .wait(state)
                        .unwrap_or_else(|e| e.into_inner()),
                };
            }
        }
    }

    /// Record the match the capture threads noted: taken from `matched` in
    /// the same critical section that sets the outcome, so `close` sees one
    /// or the other. The fact first, then `meta.json`, under [`META_WRITE`].
    fn record_match(&mut self) {
        let _serialized = lock(&META_WRITE);
        let shared = Arc::clone(&self.shared);
        let mut state = lock(&shared.state);
        let Some((at, line)) = state.matched.take() else {
            return;
        };
        let outcome = ReadyState::Ready {
            at: EpochTimestamp::now(),
        };
        let data = ReadyData {
            after_ms: millis(at.saturating_duration_since(self.running)),
            line: Some(String::from_utf8_lossy(&line).into_owned()),
            port: None,
        };
        self.write(&mut state, outcome, &data);
    }

    /// Record a port or deadline outcome, unless the run has ended: the fact
    /// first, then `meta.json`, under [`META_WRITE`].
    fn record(&mut self, outcome: ReadyState, data: &impl serde::Serialize) {
        let _serialized = lock(&META_WRITE);
        let shared = Arc::clone(&self.shared);
        let mut state = lock(&shared.state);
        if state.closed {
            return;
        }
        self.write(&mut state, outcome, data);
    }

    /// Set and record `outcome`; the caller holds [`META_WRITE`] and the state.
    fn write(&mut self, state: &mut State, outcome: ReadyState, data: &impl serde::Serialize) {
        state.outcome = Some(outcome);
        self.shared.done.store(true, Ordering::Relaxed);
        let fact = match outcome {
            ReadyState::Ready { .. } => Fact::Ready,
            ReadyState::Missed { .. } => Fact::ReadyMissed,
        };
        self.facts.append_fact(fact, data);
        patch_meta_file(&self.session_dir, |meta| meta.set_ready(outcome));
        self.shared.changed.notify_all();
    }
}

/// Marks the checker's thread gone when dropped, even by a panic, so
/// [`ReadyCheck::close`] never waits for a record nobody will make.
struct Gone(Arc<Shared>);

impl Drop for Gone {
    fn drop(&mut self) {
        lock(&self.0.state).checker_gone = true;
        self.0.changed.notify_all();
    }
}

/// Whether something accepts a TCP connection on `port` on this host's
/// loopback, IPv4 or IPv6.
fn port_open(port: u16) -> bool {
    [
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(Ipv6Addr::LOCALHOST),
    ]
    .into_iter()
    .any(|ip| TcpStream::connect_timeout(&SocketAddr::new(ip, port), PORT_CONNECT_TIMEOUT).is_ok())
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(pattern: &str) -> OutputMatcher {
        OutputMatcher {
            regex: Arc::new(output_regex(pattern).unwrap()),
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                changed: Condvar::new(),
                done: AtomicBool::new(false),
            }),
            carry: Vec::new(),
        }
    }

    fn matched(m: &OutputMatcher) -> Option<String> {
        lock(&m.shared.state)
            .matched
            .as_ref()
            .map(|(_, line)| String::from_utf8_lossy(line).into_owned())
    }

    /// A PTY line split across reads still matches, without its `\r`.
    #[test]
    fn a_pty_line_split_across_reads_matches_whole() {
        let mut m = matcher(r"^listening on \d+$");
        m.chunk(b"boot\r\nlisten");
        assert_eq!(matched(&m), None);
        m.chunk(b"ing on 8080\r\nmore\r\n");
        assert_eq!(matched(&m).as_deref(), Some("listening on 8080"));
    }

    /// A prompt that never ends its line matches.
    #[test]
    fn an_unended_pty_prompt_matches() {
        let mut m = matcher(r"^ready> $");
        m.chunk(b"loading\r\nready> ");
        assert_eq!(matched(&m).as_deref(), Some("ready> "));
    }

    /// A cooked child's CRLF reaches the PTY as `\r\r\n`, and a prompt
    /// redrawn after `\r` replaces what was on its line: each is a line as
    /// the terminal shows it.
    #[test]
    fn pty_lines_end_at_carriage_returns_too() {
        let mut m = matcher(r"^ok$");
        m.chunk(b"ok\r\r\n");
        assert_eq!(matched(&m).as_deref(), Some("ok"));

        let mut m = matcher(r"^ready> $");
        m.chunk(b"loading 10%\rloading 90%\rready> ");
        assert_eq!(matched(&m).as_deref(), Some("ready> "));
    }

    /// Once decided, output is not matched, and the first match is kept.
    #[test]
    fn only_the_first_match_counts() {
        let m = matcher("up");
        m.line(b"up 1");
        m.line(b"up 2");
        assert_eq!(matched(&m).as_deref(), Some("up 1"));
        m.shared.done.store(true, Ordering::Relaxed);
        lock(&m.shared.state).matched = None;
        m.line(b"up 3");
        assert_eq!(matched(&m), None);
    }
}
