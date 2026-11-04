use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::model::ids::RunId;
use crate::model::pty_control::ControllerEpoch;
use crate::pty_exit::PtyExitCode;
use crate::recording::Geometry;

/// Message types for the attach protocol.
/// Minimal framing: 1 byte type + 4 byte big-endian length + payload.
/// [`Frame`] is the typed form of a frame; [`read_frame`] and [`write_frame`]
/// read and write it.
pub const MSG_DATA: u8 = 0x01;
pub const MSG_RESIZE: u8 = 0x02;
pub const MSG_DETACH: u8 = 0x03;
/// Client → sidecar, first message of every connection: `[version, mode]`.
/// A connection that does not open with a valid hello is closed without
/// gaining control.
pub const MSG_HELLO: u8 = 0x04;
/// Sidecar → client: control granted, in reply to a version 1 hello or a
/// push. Payload: the controller epoch (u64 BE).
pub const MSG_ACCEPTED: u8 = 0x05;
/// Sidecar → client: control refused. Payload: `[class u8][reason UTF-8]`,
/// where the class is a [`RejectClass`] and the reason is for people. The
/// sidecar then closes the connection.
pub const MSG_REJECTED: u8 = 0x06;
/// Sidecar → client: another client took over. Payload: the new epoch (u64 BE).
/// The sidecar then shuts the connection down.
pub const MSG_RETIRED: u8 = 0x07;
/// Sidecar → push client: the push's outcome, then the connection closes.
/// Payload: `[status u8][accepted u64 BE][received u64 BE]`, where `accepted`
/// counts bytes written to the PTY and `received` counts bytes the sidecar read.
/// See [`PushOutcome`].
pub const MSG_INPUT_DONE: u8 = 0x08;
/// Client → sidecar, once after its first resize: redraw the child if the
/// attach did not change the PTY's size. No payload. A sidecar that predates it
/// ignores it as an unknown type, so it needs no version bump.
pub const MSG_REPAINT: u8 = 0x09;
/// Sidecar → viewer: control granted, in reply to a version 2 attach or
/// takeover hello. Payload: `[epoch u64 BE][run id 16 bytes][head u64 BE]`,
/// where the head is the offset of the first output byte the viewer gets
/// stamped (see [`ResumePoint`]). A version 1 client never receives it.
pub const MSG_ACCEPTED_V2: u8 = 0x0a;
/// Sidecar → version 2 viewer: PTY output stamped with its offset. Payload:
/// `[offset u64 BE][bytes]`, where the offset is the count of output bytes
/// the run wrote before these. Each follows the last with no gap. Output that
/// is not the run's own (the mode replay) still goes as unstamped
/// [`MSG_DATA`]. A version 1 client never receives it.
pub const MSG_OUTPUT: u8 = 0x0b;
/// Client → sidecar, in place of the hello: a version 2 viewer whose
/// connection was lost asks to regain the control and output it had.
/// Payload: a [`ResumePoint`], encoded as for [`MSG_ACCEPTED_V2`]: the run,
/// the epoch it held, and the offset of the next output byte it did not
/// get; from version 3, the version byte follows it, as a hello names its
/// version (without it the resume is version 2). A sidecar that predates
/// it, or that version, refuses it with the protocol class, as it refuses
/// anything but a hello.
pub const MSG_RESUME: u8 = 0x0c;
/// Sidecar → resuming viewer: control regained at a new epoch, and its
/// output follows from the offset it asked for, stamped, with no mode
/// replay. Payload: a [`ResumePoint`] with the new epoch and that offset;
/// from version 4, a controller's then carries its input report
/// (`[version u8][written u64 BE]`, see [`Frame::Resumed`]).
pub const MSG_RESUMED: u8 = 0x0d;
/// Sidecar → resuming viewer: control regained at a new epoch, but the
/// output ring no longer holds the offset it asked for. Payload: `[epoch u64
/// BE][run id 16 bytes][head u64 BE][oldest u64 BE]`: the ring holds
/// `oldest` up to `head`. The mode replay follows as unstamped [`MSG_DATA`],
/// then the output from `head`, stamped. From version 4, a controller's
/// then carries its input report, as [`MSG_RESUMED`] does.
pub const MSG_GONE: u8 = 0x0e;
/// Sidecar → watcher: admitted as a read-only viewer, in reply to a
/// [`Mode::Watch`] hello of either version. Payload: a [`WatchPoint`],
/// `[run id 16 bytes][head u64 BE]`. It carries no controller epoch: a
/// watcher holds no control and is never retired by a takeover. A version 2
/// watcher then gets its output stamped from the head; a version 1 watcher
/// gets it plain. Only a client that sends a watch hello receives it.
pub const MSG_WATCHING: u8 = 0x0f;
/// Client → sidecar, in place of the hello: a watcher whose connection was
/// lost asks for the output it had not yet received. Payload: a
/// [`WatchPoint`], encoded as for [`MSG_WATCHING`]: the run and the offset
/// of the next output byte it did not get; from version 3, the version byte
/// follows it, as for [`MSG_RESUME`]. It carries no epoch, so no path
/// can feed a watcher's point into the control rule. The sidecar answers
/// [`MSG_RESUMED`] or [`MSG_GONE`] at epoch zero, which names no control,
/// and the watcher reads only their run and offsets. A sidecar that
/// predates it, or that version, refuses it with the protocol class, as it
/// refuses anything but a hello.
pub const MSG_WATCH_RESUME: u8 = 0x10;
/// Sidecar → version 3 viewer, a controller or a watcher: the run's output
/// has ended, and this viewer has been sent all of it. Payload: the final
/// offset (u64 BE), the count of output bytes the run wrote. Nothing
/// follows it; the sidecar then closes the connection. A viewer that gets
/// to the end of its connection without it was cut off, short of the
/// run's output. Only a version 3 hello or resume asks for it.
pub const MSG_OUTPUT_END: u8 = 0x11;

/// The attach protocol version carried in [`MSG_HELLO`]. The discriminant is
/// the wire byte.
///
/// Version 2 changes only what a viewer (an attach or takeover) receives:
/// [`MSG_ACCEPTED_V2`] instead of [`MSG_ACCEPTED`], and its output as
/// [`MSG_OUTPUT`]; and a viewer may resume ([`MSG_RESUME`]). A push is
/// answered the same in both, and both know the same refusal classes.
///
/// Version 3 adds only the end of a viewer's output: [`MSG_OUTPUT_END`]
/// once it has been sent the run's last byte, so a viewer can tell the
/// run's end from being cut off. Its answers are version 2's, and a resume
/// names it with a byte after its point. A sidecar that predates it refuses
/// a version 3 hello or resume with the protocol class; the client then
/// says it again in version 2. It knows the same refusal classes.
///
/// Version 4 adds only the account of a resuming controller's input: its
/// answer ([`MSG_RESUMED`] or [`MSG_GONE`]) names the version and says how
/// many bytes of input the connection it resumes wrote to the PTY, so the
/// client can say how much of what it sent before the drop arrived. Its
/// other answers are version 3's. A sidecar that predates it refuses a
/// version 4 hello or resume with the protocol class; the client then says
/// it again in version 3. It knows the same refusal classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Version {
    V1 = 1,
    V2 = 2,
    V3 = 3,
    V4 = 4,
}

impl Version {
    /// The newest version this tendr speaks.
    pub const LATEST: Self = Self::V4;

    /// The newest version a watcher speaks: version 4 adds only the report
    /// on a resuming controller's input, and a watcher sends none, so it
    /// would only cost a refused connection to a sidecar from before it.
    pub const WATCH_LATEST: Self = Self::V3;

    /// The version to speak with a peer whose newest is `theirs`: the newer
    /// of the two that both speak. A peer that names none, or one older than
    /// any, gets version 1.
    #[must_use]
    pub fn negotiate(theirs: u8) -> Self {
        match theirs {
            0 | 1 => Self::V1,
            2 => Self::V2,
            3 => Self::V3,
            _ => Self::V4,
        }
    }

    /// The version before this one, to say again to a sidecar that refused
    /// this one; `None` for version 1.
    #[must_use]
    pub fn older(self) -> Option<Self> {
        match self {
            Self::V1 => None,
            Self::V2 => Some(Self::V1),
            Self::V3 => Some(Self::V2),
            Self::V4 => Some(Self::V3),
        }
    }

    /// Whether a viewer speaking it is told where the run's output ends
    /// ([`MSG_OUTPUT_END`]).
    #[must_use]
    pub fn reports_end(self) -> bool {
        self >= Self::V3
    }

    /// Whether a controller resuming in it is told how much of its lost
    /// connection's input was written ([`Frame::Resumed`], [`Frame::Gone`]).
    #[must_use]
    pub fn reports_input(self) -> bool {
        self >= Self::V4
    }
}

impl TryFrom<u8> for Version {
    type Error = ProtocolError;

    fn try_from(byte: u8) -> Result<Self, ProtocolError> {
        match byte {
            1 => Ok(Self::V1),
            2 => Ok(Self::V2),
            3 => Ok(Self::V3),
            4 => Ok(Self::V4),
            _ => Err(ProtocolError::UnsupportedHello),
        }
    }
}

/// A resume's payload, `point`'s encoding then, from version 3, the
/// version byte; version 2 has none, so a sidecar from before version 3
/// still reads it.
fn resume_payload(point: &[u8], version: Version) -> Vec<u8> {
    debug_assert!(version >= Version::V2, "resuming is part of version 2");
    let mut payload = point.to_vec();
    if version > Version::V2 {
        payload.push(version as u8);
    }
    payload
}

/// Split a resume's payload into its point, `len` bytes, and its version:
/// 2 when nothing follows the point, else the byte after it, which must
/// name a later one.
fn split_resume<'a>(
    payload: &'a [u8],
    len: usize,
    what: &'static str,
) -> Result<(&'a [u8], Version), ProtocolError> {
    match payload.len() {
        n if n == len => Ok((payload, Version::V2)),
        n if n == len + 1 => match Version::try_from(payload[len]) {
            Ok(version) if version > Version::V2 => Ok((&payload[..len], version)),
            _ => Err(ProtocolError::Malformed(what)),
        },
        _ => Err(ProtocolError::Malformed(what)),
    }
}

/// The input report a version 4 answer to a resume carries after `body`:
/// the version, then the bytes written (see [`Frame::Resumed`]).
fn with_input(body: &[u8], input_written: Option<u64>) -> Vec<u8> {
    let mut payload = body.to_vec();
    if let Some(written) = input_written {
        payload.push(Version::V4 as u8);
        payload.extend_from_slice(&written.to_be_bytes());
    }
    payload
}

/// Split a resume's answer into its body, `len` bytes, and its input
/// report: none when nothing follows the body (versions 2 and 3), else a
/// version from 4 on and the bytes written.
fn split_input<'a>(
    payload: &'a [u8],
    len: usize,
    what: &'static str,
) -> Result<(&'a [u8], Option<u64>), ProtocolError> {
    match payload.len() {
        n if n == len => Ok((payload, None)),
        n if n == len + 9 => match Version::try_from(payload[len]) {
            Ok(version) if version.reports_input() => Ok((
                &payload[..len],
                Some(u64::from_be_bytes(
                    payload[len + 1..].try_into().expect("8 bytes"),
                )),
            )),
            _ => Err(ProtocolError::Malformed(what)),
        },
        _ => Err(ProtocolError::Malformed(what)),
    }
}

/// Where a version 2 viewer stands in its run: the run, the controller epoch
/// it was granted, and the offset of the next output byte it has not yet
/// received. [`MSG_ACCEPTED_V2`] carries the first, at the head of the
/// output when the viewer was installed; each [`MSG_OUTPUT`] then moves the
/// offset past its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumePoint {
    pub run_id: RunId,
    pub epoch: ControllerEpoch,
    pub offset: u64,
}

/// Where a watcher stands in its run: the run, and the offset of the next
/// output byte it has not yet received. Unlike a [`ResumePoint`] it has no
/// epoch, so nothing a watcher sends can name a controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchPoint {
    pub run_id: RunId,
    pub offset: u64,
}

impl WatchPoint {
    fn encode(self) -> [u8; 24] {
        let mut buf = [0u8; 24];
        buf[0..16].copy_from_slice(self.run_id.as_uuid().as_bytes());
        buf[16..24].copy_from_slice(&self.offset.to_be_bytes());
        buf
    }

    fn decode(payload: &[u8], what: &'static str) -> Result<Self, ProtocolError> {
        let malformed = ProtocolError::Malformed(what);
        let bytes: &[u8; 24] = payload.try_into().map_err(|_| malformed)?;
        let (run_id, offset) = bytes.split_at(16);
        let run_id = uuid::Uuid::from_slice(run_id).map_err(|_| malformed)?;
        Ok(Self {
            run_id: RunId::from_v7(run_id).ok_or(malformed)?,
            offset: u64::from_be_bytes(offset.try_into().expect("8 bytes")),
        })
    }
}

impl ResumePoint {
    fn encode(self) -> [u8; 32] {
        let mut buf = [0u8; 32];
        buf[0..8].copy_from_slice(&self.epoch.get().to_be_bytes());
        buf[8..24].copy_from_slice(self.run_id.as_uuid().as_bytes());
        buf[24..32].copy_from_slice(&self.offset.to_be_bytes());
        buf
    }

    fn decode(payload: &[u8], what: &'static str) -> Result<Self, ProtocolError> {
        let malformed = ProtocolError::Malformed(what);
        let bytes: &[u8; 32] = payload.try_into().map_err(|_| malformed)?;
        let (epoch, rest) = bytes.split_at(8);
        let (run_id, offset) = rest.split_at(16);
        let run_id = uuid::Uuid::from_slice(run_id).map_err(|_| malformed)?;
        Ok(Self {
            run_id: RunId::from_v7(run_id).ok_or(malformed)?,
            epoch: ControllerEpoch::new(u64::from_be_bytes(epoch.try_into().expect("8 bytes"))),
            offset: u64::from_be_bytes(offset.try_into().expect("8 bytes")),
        })
    }
}

/// What a connection asks for in its [`MSG_HELLO`]. The discriminant is the
/// wire byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Mode {
    /// Take control only if nobody holds it.
    Attach = 1,
    /// Take control, retiring any current controller.
    Takeover = 2,
    /// An agent push. Claims control only if nobody holds it, streams input as
    /// [`Frame::Data`], ends it with [`Frame::Detach`], and receives a
    /// [`Frame::InputDone`] outcome. No output is sent to a push connection.
    Push = 3,
    /// A read-only viewer: receives the output beside the controller and
    /// never holds or changes control. Answered with [`Frame::Watching`];
    /// the sidecar drops its input, resizes and repaints. A sidecar from
    /// before it refuses the hello with the protocol class.
    Watch = 4,
}

impl TryFrom<u8> for Mode {
    type Error = ProtocolError;

    fn try_from(byte: u8) -> Result<Self, ProtocolError> {
        match byte {
            1 => Ok(Self::Attach),
            2 => Ok(Self::Takeover),
            3 => Ok(Self::Push),
            4 => Ok(Self::Watch),
            _ => Err(ProtocolError::UnsupportedHello),
        }
    }
}

/// Why the sidecar refused a connection ([`MSG_REJECTED`]), so a client can act
/// on a refusal without parsing its reason. The discriminant is the wire byte;
/// a byte this version does not know does not decode.
///
/// Decoding is strict, so the set is part of the protocol version: a sidecar
/// sends only the classes of the version the client's hello names, and adding
/// a class means a new [`Version`]. Versions 1 and 2 know the same four. A
/// refusal sent before the hello is read (a peer of another user, no free
/// connection slot) may use only classes every supported version knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RejectClass {
    /// The hello was missing, or named a version or mode this sidecar does not
    /// speak.
    Protocol = 1,
    /// Someone else holds the terminal, the run has ended, or a resume finds
    /// that control changed while its connection was lost.
    Control = 2,
    /// The sidecar cannot serve the connection: its input writer stopped, or
    /// every connection slot is in use.
    Runtime = 3,
    /// The connecting process is not the run owner's.
    Identity = 4,
}

impl RejectClass {
    /// Every class, in wire order.
    pub const ALL: [Self; 4] = [Self::Protocol, Self::Control, Self::Runtime, Self::Identity];
}

impl TryFrom<u8> for RejectClass {
    type Error = ProtocolError;

    fn try_from(byte: u8) -> Result<Self, ProtocolError> {
        Self::ALL
            .into_iter()
            .find(|class| *class as u8 == byte)
            .ok_or(ProtocolError::UnknownRejectClass(byte))
    }
}

impl From<RejectClass> for PtyExitCode {
    fn from(class: RejectClass) -> Self {
        match class {
            RejectClass::Protocol => Self::Protocol,
            RejectClass::Control => Self::Control,
            RejectClass::Runtime => Self::Runtime,
            RejectClass::Identity => Self::Identity,
        }
    }
}

/// How far a push that stopped early got: `accepted` bytes were written to the
/// PTY and the next `unwritten` bytes the sidecar received were dropped. On the
/// wire it is `accepted` and `received = accepted + unwritten`, so no decoded
/// progress can claim more bytes written than received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub accepted: u64,
    pub unwritten: u64,
}

impl Progress {
    /// Bytes the sidecar received.
    #[must_use]
    pub fn received(self) -> u64 {
        self.accepted.saturating_add(self.unwritten)
    }
}

/// A push's outcome ([`MSG_INPUT_DONE`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// Every received byte was written.
    Written { bytes: u64 },
    /// Control was taken over; the rest was dropped.
    Revoked(Progress),
    /// The PTY stopped accepting input; the rest was dropped.
    Closed(Progress),
}

/// [`MSG_INPUT_DONE`] status bytes.
const INPUT_WRITTEN: u8 = 0;
const INPUT_REVOKED: u8 = 1;
const INPUT_CLOSED: u8 = 2;

impl PushOutcome {
    fn encode(self) -> [u8; 17] {
        let (status, accepted, received) = match self {
            Self::Written { bytes } => (INPUT_WRITTEN, bytes, bytes),
            Self::Revoked(p) => (INPUT_REVOKED, p.accepted, p.received()),
            Self::Closed(p) => (INPUT_CLOSED, p.accepted, p.received()),
        };
        let mut buf = [0u8; 17];
        buf[0] = status;
        buf[1..9].copy_from_slice(&accepted.to_be_bytes());
        buf[9..17].copy_from_slice(&received.to_be_bytes());
        buf
    }

    fn decode(payload: &[u8]) -> Result<Self, ProtocolError> {
        let bytes: &[u8; 17] = payload
            .try_into()
            .map_err(|_| ProtocolError::Malformed("input done"))?;
        let [status, rest @ ..] = bytes;
        let (accepted, received) = rest.split_at(8);
        let accepted = u64::from_be_bytes(accepted.try_into().expect("8 bytes"));
        let received = u64::from_be_bytes(received.try_into().expect("8 bytes"));
        let inconsistent = ProtocolError::InconsistentOutcome { accepted, received };
        let progress = || {
            received
                .checked_sub(accepted)
                .map(|unwritten| Progress {
                    accepted,
                    unwritten,
                })
                .ok_or(inconsistent)
        };
        match *status {
            INPUT_WRITTEN if accepted == received => Ok(Self::Written { bytes: accepted }),
            INPUT_WRITTEN => Err(inconsistent),
            INPUT_REVOKED => progress().map(Self::Revoked),
            INPUT_CLOSED => progress().map(Self::Closed),
            other => Err(ProtocolError::UnknownOutcome(other)),
        }
    }
}

/// One attach protocol frame, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Client → sidecar, first frame of every connection: the protocol
    /// version the client speaks on it, and what it asks for.
    Hello { version: Version, mode: Mode },
    /// Terminal bytes: input from a client, output to a viewer.
    Data(Vec<u8>),
    /// Client → sidecar: the client's terminal size.
    Resize(Geometry),
    /// Client → sidecar: the end of this connection's input.
    Detach,
    /// Client → sidecar: make the child redraw after an attach at its size.
    Repaint,
    /// Sidecar → client: control granted at this epoch (version 1, or a push).
    Accepted(ControllerEpoch),
    /// Sidecar → version 2 viewer: control granted, at this point of the run.
    AcceptedV2(ResumePoint),
    /// Sidecar → version 2 viewer: the run's output from `offset` on.
    Output { offset: u64, bytes: Vec<u8> },
    /// Client → sidecar, in place of the hello: resume from this point,
    /// speaking `version` (2 or later) from here on.
    Resume {
        point: ResumePoint,
        version: Version,
    },
    /// Sidecar → resuming viewer: control regained at `point.epoch`; its
    /// output follows from `point.offset`, the offset it asked for. In
    /// version 4 a controller's `input_written` says how many bytes of input
    /// the connection it resumes wrote to the PTY under the epoch it held;
    /// `None` before version 4, and for a watcher.
    Resumed {
        point: ResumePoint,
        input_written: Option<u64>,
    },
    /// Sidecar → resuming viewer: control regained at `point.epoch`, but
    /// only `oldest` up to `point.offset`, the head, is still held; the mode
    /// replay follows, then the output from the head. `input_written` as
    /// for [`Frame::Resumed`].
    Gone {
        point: ResumePoint,
        oldest: u64,
        input_written: Option<u64>,
    },
    /// Sidecar → client: control refused, with why.
    Rejected { class: RejectClass, reason: String },
    /// Sidecar → client: another client took over at this epoch.
    Retired(ControllerEpoch),
    /// Sidecar → push client: the push's outcome.
    InputDone(PushOutcome),
    /// Sidecar → watcher: admitted read-only, at this point of the run.
    Watching(WatchPoint),
    /// Client → sidecar, in place of the hello: a watcher resumes from this
    /// point, speaking `version` (2 or later) from here on.
    WatchResume { point: WatchPoint, version: Version },
    /// Sidecar → version 3 viewer: the run's output ended at this offset,
    /// and the viewer has been sent all of it.
    OutputEnd(u64),
}

/// A frame whose payload does not fit its type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("unknown attach message type {0}")]
    UnknownType(u8),
    #[error("unsupported attach protocol version or mode")]
    UnsupportedHello,
    #[error("malformed {0} frame")]
    Malformed(&'static str),
    #[error("unknown attach refusal class {0}")]
    UnknownRejectClass(u8),
    #[error("unknown push outcome status {0}")]
    UnknownOutcome(u8),
    #[error("push outcome reports {accepted} bytes written of {received} received")]
    InconsistentOutcome { accepted: u64, received: u64 },
}

impl Frame {
    /// Decode a frame read by [`read_msg`].
    ///
    /// # Errors
    ///
    /// A payload that does not fit its message type, or a message type this
    /// version does not know ([`ProtocolError::UnknownType`]).
    pub fn decode(msg_type: u8, payload: Vec<u8>) -> Result<Self, ProtocolError> {
        let epoch = |what| {
            <[u8; 8]>::try_from(payload.as_slice())
                .map(|b| ControllerEpoch::new(u64::from_be_bytes(b)))
                .map_err(|_| ProtocolError::Malformed(what))
        };
        Ok(match msg_type {
            MSG_DATA => Self::Data(payload),
            MSG_RESIZE => {
                Self::Resize(parse_resize(&payload).ok_or(ProtocolError::Malformed("resize"))?)
            }
            MSG_DETACH => Self::Detach,
            MSG_REPAINT if payload.is_empty() => Self::Repaint,
            MSG_REPAINT => return Err(ProtocolError::Malformed("repaint")),
            MSG_HELLO => match payload.as_slice() {
                [version, mode] => Self::Hello {
                    version: Version::try_from(*version)?,
                    mode: Mode::try_from(*mode)?,
                },
                _ => return Err(ProtocolError::UnsupportedHello),
            },
            MSG_ACCEPTED => Self::Accepted(epoch("accepted")?),
            MSG_ACCEPTED_V2 => Self::AcceptedV2(ResumePoint::decode(&payload, "accepted v2")?),
            MSG_RESUME => {
                let (point, version) = split_resume(&payload, 32, "resume")?;
                Self::Resume {
                    point: ResumePoint::decode(point, "resume")?,
                    version,
                }
            }
            MSG_RESUMED => {
                let (point, input_written) = split_input(&payload, 32, "resumed")?;
                Self::Resumed {
                    point: ResumePoint::decode(point, "resumed")?,
                    input_written,
                }
            }
            MSG_GONE => {
                let (body, input_written) = split_input(&payload, 40, "gone")?;
                let point = ResumePoint::decode(&body[..32], "gone")?;
                let oldest = u64::from_be_bytes(body[32..].try_into().expect("8 bytes"));
                if oldest > point.offset {
                    return Err(ProtocolError::Malformed("gone"));
                }
                Self::Gone {
                    point,
                    oldest,
                    input_written,
                }
            }
            MSG_OUTPUT if payload.len() >= 8 => {
                let offset = u64::from_be_bytes(payload[..8].try_into().expect("8 bytes"));
                let mut bytes = payload;
                bytes.drain(..8);
                Self::Output { offset, bytes }
            }
            MSG_OUTPUT => return Err(ProtocolError::Malformed("output")),
            MSG_REJECTED => match payload.split_first() {
                Some((class, reason)) => Self::Rejected {
                    class: RejectClass::try_from(*class)?,
                    reason: String::from_utf8_lossy(reason).into_owned(),
                },
                None => return Err(ProtocolError::Malformed("rejected")),
            },
            MSG_RETIRED => Self::Retired(epoch("retired")?),
            MSG_INPUT_DONE => Self::InputDone(PushOutcome::decode(&payload)?),
            MSG_WATCHING => Self::Watching(WatchPoint::decode(&payload, "watching")?),
            MSG_WATCH_RESUME => {
                let (point, version) = split_resume(&payload, 24, "watch resume")?;
                Self::WatchResume {
                    point: WatchPoint::decode(point, "watch resume")?,
                    version,
                }
            }
            MSG_OUTPUT_END => Self::OutputEnd(u64::from_be_bytes(
                payload
                    .as_slice()
                    .try_into()
                    .map_err(|_| ProtocolError::Malformed("output end"))?,
            )),
            other => return Err(ProtocolError::UnknownType(other)),
        })
    }
}

/// Write one frame.
///
/// # Errors
///
/// The writer's error.
pub fn write_frame(w: &mut impl Write, frame: &Frame) -> io::Result<()> {
    match frame {
        Frame::Hello { version, mode } => write_msg(w, MSG_HELLO, &[*version as u8, *mode as u8]),
        Frame::Data(bytes) => write_msg(w, MSG_DATA, bytes),
        Frame::Resize(size) => write_msg(w, MSG_RESIZE, &resize_payload(*size)),
        Frame::Detach => write_msg(w, MSG_DETACH, &[]),
        Frame::Repaint => write_msg(w, MSG_REPAINT, &[]),
        Frame::Accepted(epoch) => write_msg(w, MSG_ACCEPTED, &epoch.get().to_be_bytes()),
        Frame::AcceptedV2(point) => write_msg(w, MSG_ACCEPTED_V2, &point.encode()),
        Frame::Output { offset, bytes } => write_output(w, *offset, bytes),
        Frame::Resume { point, version } => {
            write_msg(w, MSG_RESUME, &resume_payload(&point.encode(), *version))
        }
        Frame::Resumed {
            point,
            input_written,
        } => write_msg(w, MSG_RESUMED, &with_input(&point.encode(), *input_written)),
        Frame::Gone {
            point,
            oldest,
            input_written,
        } => {
            let mut body = [0u8; 40];
            body[..32].copy_from_slice(&point.encode());
            body[32..].copy_from_slice(&oldest.to_be_bytes());
            write_msg(w, MSG_GONE, &with_input(&body, *input_written))
        }
        Frame::Rejected { class, reason } => {
            let mut payload = Vec::with_capacity(1 + reason.len());
            payload.push(*class as u8);
            payload.extend_from_slice(reason.as_bytes());
            write_msg(w, MSG_REJECTED, &payload)
        }
        Frame::Retired(epoch) => write_msg(w, MSG_RETIRED, &epoch.get().to_be_bytes()),
        Frame::InputDone(outcome) => write_msg(w, MSG_INPUT_DONE, &outcome.encode()),
        Frame::Watching(point) => write_msg(w, MSG_WATCHING, &point.encode()),
        Frame::WatchResume { point, version } => write_msg(
            w,
            MSG_WATCH_RESUME,
            &resume_payload(&point.encode(), *version),
        ),
        Frame::OutputEnd(offset) => write_msg(w, MSG_OUTPUT_END, &offset.to_be_bytes()),
    }
}

/// Why [`read_frame`] returned no frame.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// The connection failed or closed, or a frame declared an oversized
    /// payload. Nothing further can be read.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// A whole frame was read, but its payload does not fit its type. The
    /// stream is still in sync.
    #[error(transparent)]
    Malformed(ProtocolError),
    /// A whole frame of a type this version does not know. The stream is
    /// still in sync; both ends ignore it, so a later version can add types.
    #[error("unknown attach message type {0}")]
    Unknown(u8),
}

/// Read and decode one frame.
///
/// # Errors
///
/// See [`FrameError`].
pub fn read_frame(r: &mut impl Read) -> Result<Frame, FrameError> {
    let (msg_type, payload) = read_msg(r)?;
    Frame::decode(msg_type, payload).map_err(|e| match e {
        ProtocolError::UnknownType(msg_type) => FrameError::Unknown(msg_type),
        e => FrameError::Malformed(e),
    })
}

/// How long the sidecar waits for a connection's hello.
pub const HELLO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Largest payload any attach frame may carry. [`read_msg`] rejects a larger
/// length before allocating for it.
pub const MAX_FRAME_PAYLOAD: usize = 64 * 1024;

pub fn write_msg(w: &mut impl Write, msg_type: u8, payload: &[u8]) -> io::Result<()> {
    let len = payload.len() as u32;
    w.write_all(&[msg_type])?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Write a [`MSG_OUTPUT`] frame: the type, length and offset in one write,
/// then the bytes, without joining them into one buffer.
fn write_output(w: &mut impl Write, offset: u64, bytes: &[u8]) -> io::Result<()> {
    let mut header = [0u8; 13];
    header[0] = MSG_OUTPUT;
    header[1..5].copy_from_slice(&((8 + bytes.len()) as u32).to_be_bytes());
    header[5..13].copy_from_slice(&offset.to_be_bytes());
    w.write_all(&header)?;
    w.write_all(bytes)?;
    w.flush()
}

pub fn read_msg(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    r.read_exact(&mut header)?;
    let msg_type = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("attach frame declares {len} bytes; the limit is {MAX_FRAME_PAYLOAD}"),
        ));
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut payload)?;
    }
    Ok((msg_type, payload))
}

/// Encode a [`MSG_RESIZE`] payload: `[rows u16 BE][cols u16 BE]`.
#[must_use]
pub fn resize_payload(size: Geometry) -> [u8; 4] {
    let mut buf = [0u8; 4];
    buf[0..2].copy_from_slice(&size.rows().to_be_bytes());
    buf[2..4].copy_from_slice(&size.cols().to_be_bytes());
    buf
}

/// Decode a [`MSG_RESIZE`] payload. `None` if it is short or either dimension
/// is zero: no terminal program can draw into such a size, and it cannot be
/// recorded, so it is refused here rather than at the PTY.
#[must_use]
pub fn parse_resize(payload: &[u8]) -> Option<Geometry> {
    if payload.len() < 4 {
        return None;
    }
    let rows = u16::from_be_bytes([payload[0], payload[1]]);
    let cols = u16::from_be_bytes([payload[2], payload[3]]);
    Geometry::new(rows, cols)
}

/// Read the attach socket path from the session's breadcrumb (`a.sock.path`),
/// which the sidecar publishes atomically after binding its private socket.
/// There is no fallback location: a session without a breadcrumb has no
/// attachable listener.
pub fn read_sock_path(session_dir: &Path) -> Option<PathBuf> {
    read_breadcrumb(session_dir).filter(|path| path.exists())
}

/// The socket path the session's breadcrumb names, whether or not it exists.
pub fn read_breadcrumb(session_dir: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(session_dir.join("a.sock.path")).ok()?;
    Some(PathBuf::from(content.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(msg_type: u8, declared_len: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![msg_type];
        out.extend_from_slice(&declared_len.to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn a_resize_round_trips_and_a_zero_dimension_is_refused_at_parse() {
        let size = Geometry::new(40, 120).unwrap();
        assert_eq!(parse_resize(&resize_payload(size)), Some(size));
        assert_eq!(parse_resize(&[0, 0, 0, 100]), None, "zero rows");
        assert_eq!(parse_resize(&[0, 24, 0, 0]), None, "zero columns");
        assert_eq!(parse_resize(&[0, 24, 0]), None, "short");
    }

    fn round_trip(frame: &Frame) -> Frame {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, frame).unwrap();
        read_frame(&mut bytes.as_slice()).unwrap()
    }

    #[test]
    fn every_frame_round_trips() {
        let progress = Progress {
            accepted: 3,
            unwritten: 5,
        };
        let point = ResumePoint {
            run_id: RunId::new(),
            epoch: ControllerEpoch::new(6),
            offset: 1 << 40,
        };
        for frame in [
            Frame::Hello {
                version: Version::V1,
                mode: Mode::Attach,
            },
            Frame::Hello {
                version: Version::V2,
                mode: Mode::Takeover,
            },
            Frame::Hello {
                version: Version::V2,
                mode: Mode::Push,
            },
            Frame::Data(b"abc".to_vec()),
            Frame::Resize(Geometry::new(24, 80).unwrap()),
            Frame::Detach,
            Frame::Repaint,
            Frame::Accepted(ControllerEpoch::new(7)),
            Frame::AcceptedV2(point),
            Frame::Output {
                offset: 3,
                bytes: b"xyz".to_vec(),
            },
            Frame::Output {
                offset: u64::MAX,
                bytes: Vec::new(),
            },
            Frame::Rejected {
                class: RejectClass::Control,
                reason: "busy".to_owned(),
            },
            Frame::Retired(ControllerEpoch::new(8)),
            Frame::InputDone(PushOutcome::Written { bytes: 9 }),
            Frame::InputDone(PushOutcome::Revoked(progress)),
            Frame::InputDone(PushOutcome::Closed(progress)),
            Frame::Resume {
                point,
                version: Version::V2,
            },
            Frame::Resume {
                point,
                version: Version::V3,
            },
            Frame::Resume {
                point,
                version: Version::V4,
            },
            Frame::Resumed {
                point,
                input_written: None,
            },
            Frame::Resumed {
                point,
                input_written: Some(1 << 44),
            },
            Frame::Gone {
                point,
                oldest: 1 << 39,
                input_written: None,
            },
            Frame::Gone {
                point,
                oldest: point.offset,
                input_written: Some(0),
            },
            Frame::Hello {
                version: Version::V1,
                mode: Mode::Watch,
            },
            Frame::Watching(WatchPoint {
                run_id: point.run_id,
                offset: 1 << 41,
            }),
            Frame::WatchResume {
                point: WatchPoint {
                    run_id: point.run_id,
                    offset: 1 << 42,
                },
                version: Version::V2,
            },
            Frame::WatchResume {
                point: WatchPoint {
                    run_id: point.run_id,
                    offset: 1 << 42,
                },
                version: Version::V3,
            },
            Frame::Hello {
                version: Version::V3,
                mode: Mode::Watch,
            },
            Frame::OutputEnd(1 << 43),
            Frame::OutputEnd(0),
        ] {
            assert_eq!(round_trip(&frame), frame);
        }
    }

    /// The watch accept and the watch resume are `[run id][offset]`,
    /// big-endian, with no epoch; a payload of another length, or whose run
    /// id is not a v7 UUID, does not decode.
    #[test]
    fn the_watch_frames_keep_their_wire_format() {
        let run_id = RunId::new();
        let point = WatchPoint {
            run_id,
            offset: 0x0304,
        };
        let mut payload = run_id.as_uuid().as_bytes().to_vec();
        payload.extend_from_slice(&[0, 0, 0, 0, 0, 0, 3, 4]);
        for (sent, msg_type, what) in [
            (Frame::Watching(point), 0x0f, "watching"),
            (
                Frame::WatchResume {
                    point,
                    version: Version::V2,
                },
                0x10,
                "watch resume",
            ),
        ] {
            let mut bytes = Vec::new();
            write_frame(&mut bytes, &sent).unwrap();
            assert_eq!(bytes, frame(msg_type, 24, &payload));

            let mut not_v7 = payload.clone();
            not_v7[..16].copy_from_slice(uuid::Uuid::from_u128(1).as_bytes());
            for bad in [
                payload[..23].to_vec(),
                [payload.clone(), vec![0; 8]].concat(),
                not_v7,
            ] {
                assert_eq!(
                    Frame::decode(msg_type, bad),
                    Err(ProtocolError::Malformed(what))
                );
            }
        }
    }

    /// The v2 frames' wire formats: the accept is `[epoch][run id][head]`,
    /// stamped output `[offset][bytes]`, all integers big-endian.
    #[test]
    fn v2_frames_keep_their_wire_format() {
        let run_id = RunId::new();
        let mut accept = Vec::new();
        write_frame(
            &mut accept,
            &Frame::AcceptedV2(ResumePoint {
                run_id,
                epoch: ControllerEpoch::new(0x0102),
                offset: 0x0304,
            }),
        )
        .unwrap();
        let mut payload = vec![0, 0, 0, 0, 0, 0, 1, 2];
        payload.extend_from_slice(run_id.as_uuid().as_bytes());
        payload.extend_from_slice(&[0, 0, 0, 0, 0, 0, 3, 4]);
        assert_eq!(accept, frame(0x0a, 32, &payload));

        let mut output = Vec::new();
        write_frame(
            &mut output,
            &Frame::Output {
                offset: 5,
                bytes: b"hi".to_vec(),
            },
        )
        .unwrap();
        assert_eq!(output, frame(0x0b, 10, b"\0\0\0\0\0\0\0\x05hi"));
    }

    /// The resume frames' wire formats: the resume and its answer are the
    /// v2 accept's `[epoch][run id][offset]`; gone adds `[oldest]`.
    #[test]
    fn resume_frames_keep_their_wire_format() {
        let run_id = RunId::new();
        let point = ResumePoint {
            run_id,
            epoch: ControllerEpoch::new(0x0102),
            offset: 0x0304,
        };
        let mut payload = vec![0, 0, 0, 0, 0, 0, 1, 2];
        payload.extend_from_slice(run_id.as_uuid().as_bytes());
        payload.extend_from_slice(&[0, 0, 0, 0, 0, 0, 3, 4]);
        let resume = Frame::Resume {
            point,
            version: Version::V2,
        };
        let resumed = Frame::Resumed {
            point,
            input_written: None,
        };
        for (sent, msg_type) in [(resume, 0x0c), (resumed, 0x0d)] {
            let mut bytes = Vec::new();
            write_frame(&mut bytes, &sent).unwrap();
            assert_eq!(bytes, frame(msg_type, 32, &payload));
        }
        let mut gone = Vec::new();
        write_frame(
            &mut gone,
            &Frame::Gone {
                point,
                oldest: 0x0203,
                input_written: None,
            },
        )
        .unwrap();
        payload.extend_from_slice(&[0, 0, 0, 0, 0, 0, 2, 3]);
        assert_eq!(gone, frame(0x0e, 40, &payload));
    }

    /// Version 4's answers to a resume: version 2's, then the version byte
    /// and the bytes of input written, big-endian. A report with a version
    /// before 4, or of another length, does not decode.
    #[test]
    fn v4_resume_answers_keep_their_wire_format() {
        let point = ResumePoint {
            run_id: RunId::new(),
            epoch: ControllerEpoch::new(0x0102),
            offset: 0x0304,
        };
        let report = [4, 0, 0, 0, 0, 0, 0, 0x05, 0x06];
        let gone_body = [point.encode().as_slice(), &0x0203u64.to_be_bytes()].concat();
        for (sent, msg_type, body, what) in [
            (
                Frame::Resumed {
                    point,
                    input_written: Some(0x0506),
                },
                MSG_RESUMED,
                point.encode().to_vec(),
                "resumed",
            ),
            (
                Frame::Gone {
                    point,
                    oldest: 0x0203,
                    input_written: Some(0x0506),
                },
                MSG_GONE,
                gone_body,
                "gone",
            ),
        ] {
            let mut bytes = Vec::new();
            write_frame(&mut bytes, &sent).unwrap();
            let payload = [body.as_slice(), &report].concat();
            assert_eq!(bytes, frame(msg_type, payload.len() as u32, &payload));
            for bad in [
                [body.as_slice(), &[3, 0, 0, 0, 0, 0, 0, 0, 1]].concat(),
                [body.as_slice(), &[0, 0, 0, 0, 0, 0, 0, 0, 1]].concat(),
                [body.as_slice(), &report[..8]].concat(),
                [body.as_slice(), &report, &[0]].concat(),
            ] {
                assert_eq!(
                    Frame::decode(msg_type, bad),
                    Err(ProtocolError::Malformed(what))
                );
            }
        }
    }

    /// A gone answer whose oldest offset is past its head, or of the wrong
    /// length, does not decode.
    #[test]
    fn a_malformed_gone_does_not_decode() {
        let point = ResumePoint {
            run_id: RunId::new(),
            epoch: ControllerEpoch::new(2),
            offset: 10,
        };
        let gone = |oldest: u64| [point.encode().as_slice(), &oldest.to_be_bytes()].concat();
        assert_eq!(
            Frame::decode(MSG_GONE, gone(10)),
            Ok(Frame::Gone {
                point,
                oldest: 10,
                input_written: None
            })
        );
        for bad in [gone(11), gone(0)[..39].to_vec(), point.encode().to_vec()] {
            assert_eq!(
                Frame::decode(MSG_GONE, bad),
                Err(ProtocolError::Malformed("gone"))
            );
        }
        assert_eq!(
            Frame::decode(MSG_RESUME, vec![0; 31]),
            Err(ProtocolError::Malformed("resume"))
        );
    }

    #[test]
    fn a_malformed_v2_frame_does_not_decode() {
        let mut payload = vec![0; 8];
        payload.extend_from_slice(RunId::new().as_uuid().as_bytes());
        payload.extend_from_slice(&[0; 8]);
        assert!(Frame::decode(MSG_ACCEPTED_V2, payload.clone()).is_ok());
        let mut not_v7 = payload.clone();
        not_v7[8..24].copy_from_slice(uuid::Uuid::from_u128(1).as_bytes());
        for bad in [payload[..31].to_vec(), [payload, vec![0]].concat(), not_v7] {
            assert_eq!(
                Frame::decode(MSG_ACCEPTED_V2, bad),
                Err(ProtocolError::Malformed("accepted v2"))
            );
        }
        assert_eq!(
            Frame::decode(MSG_OUTPUT, vec![0; 7]),
            Err(ProtocolError::Malformed("output"))
        );
    }

    /// Version 3's frames: a resume is version 2's point, then the version
    /// byte, which must name a version after 2; the end of output is the
    /// final offset, big-endian.
    #[test]
    fn v3_frames_keep_their_wire_format() {
        let run_id = RunId::new();
        let point = ResumePoint {
            run_id,
            epoch: ControllerEpoch::new(0x0102),
            offset: 0x0304,
        };
        let watch = WatchPoint {
            run_id,
            offset: 0x0304,
        };
        for (sent, msg_type, encoded) in [
            (
                Frame::Resume {
                    point,
                    version: Version::V3,
                },
                MSG_RESUME,
                point.encode().to_vec(),
            ),
            (
                Frame::WatchResume {
                    point: watch,
                    version: Version::V3,
                },
                MSG_WATCH_RESUME,
                watch.encode().to_vec(),
            ),
        ] {
            let mut bytes = Vec::new();
            write_frame(&mut bytes, &sent).unwrap();
            let payload = [encoded.as_slice(), &[3]].concat();
            assert_eq!(bytes, frame(msg_type, payload.len() as u32, &payload));
            for version in [0, 1, 2, 5] {
                let bad = [encoded.as_slice(), &[version]].concat();
                assert!(
                    matches!(
                        Frame::decode(msg_type, bad),
                        Err(ProtocolError::Malformed(_))
                    ),
                    "version byte {version}"
                );
            }
        }

        let mut end = Vec::new();
        write_frame(&mut end, &Frame::OutputEnd(0x0506)).unwrap();
        assert_eq!(end, frame(0x11, 8, &[0, 0, 0, 0, 0, 0, 5, 6]));
        for bad in [vec![0; 7], vec![0; 9], Vec::new()] {
            assert_eq!(
                Frame::decode(MSG_OUTPUT_END, bad),
                Err(ProtocolError::Malformed("output end"))
            );
        }
    }

    #[test]
    fn the_negotiated_version_is_the_newest_both_speak() {
        assert_eq!(Version::negotiate(0), Version::V1);
        assert_eq!(Version::negotiate(1), Version::V1);
        assert_eq!(Version::negotiate(2), Version::V2);
        assert_eq!(Version::negotiate(3), Version::V3);
        assert_eq!(Version::negotiate(4), Version::V4);
        assert_eq!(Version::negotiate(9), Version::V4);
    }

    /// A refused version steps down one at a time; from version 3 a viewer
    /// is told where the output ends, and from version 4 a resume how much
    /// input arrived.
    #[test]
    fn each_version_steps_down_to_the_one_before() {
        assert_eq!(Version::V4.older(), Some(Version::V3));
        assert_eq!(Version::V3.older(), Some(Version::V2));
        assert_eq!(Version::V2.older(), Some(Version::V1));
        assert_eq!(Version::V1.older(), None);
        assert!(Version::V3.reports_end());
        assert!(!Version::V2.reports_end());
        assert!(!Version::V1.reports_end());
        assert!(Version::V4.reports_input());
        assert!(!Version::V3.reports_input());
    }

    /// The typed outcomes keep the v1 wire format: `[status][accepted][received]`.
    #[test]
    fn push_outcomes_keep_their_wire_format() {
        let payload = |status: u8, accepted: u64, received: u64| {
            let mut p = vec![status];
            p.extend_from_slice(&accepted.to_be_bytes());
            p.extend_from_slice(&received.to_be_bytes());
            p
        };
        assert_eq!(
            PushOutcome::Written { bytes: 5 }.encode().to_vec(),
            payload(0, 5, 5)
        );
        let progress = Progress {
            accepted: 3,
            unwritten: 2,
        };
        assert_eq!(
            PushOutcome::Revoked(progress).encode().to_vec(),
            payload(1, 3, 5)
        );
        assert_eq!(
            PushOutcome::Closed(progress).encode().to_vec(),
            payload(2, 3, 5)
        );
        assert_eq!(
            Frame::decode(MSG_INPUT_DONE, payload(1, 3, 5)),
            Ok(Frame::InputDone(PushOutcome::Revoked(progress)))
        );
    }

    /// Outcomes that cannot be true are refused where they are decoded.
    #[test]
    fn an_impossible_push_outcome_does_not_decode() {
        let payload = |status: u8, accepted: u64, received: u64| {
            let mut p = vec![status];
            p.extend_from_slice(&accepted.to_be_bytes());
            p.extend_from_slice(&received.to_be_bytes());
            p
        };
        assert_eq!(
            Frame::decode(MSG_INPUT_DONE, payload(0, 3, 5)),
            Err(ProtocolError::InconsistentOutcome {
                accepted: 3,
                received: 5
            }),
            "written, with bytes left unwritten"
        );
        assert_eq!(
            Frame::decode(MSG_INPUT_DONE, payload(1, 6, 5)),
            Err(ProtocolError::InconsistentOutcome {
                accepted: 6,
                received: 5
            }),
            "more written than received"
        );
        assert_eq!(
            Frame::decode(MSG_INPUT_DONE, payload(3, 5, 5)),
            Err(ProtocolError::UnknownOutcome(3))
        );
        assert_eq!(
            Frame::decode(MSG_INPUT_DONE, vec![0; 16]),
            Err(ProtocolError::Malformed("input done"))
        );
    }

    #[test]
    fn every_reject_class_round_trips() {
        for class in RejectClass::ALL {
            let frame = Frame::Rejected {
                class,
                reason: format!("{class:?}"),
            };
            assert_eq!(round_trip(&frame), frame);
        }
    }

    /// A refusal is `[class u8][reason UTF-8]`; the reason may be empty.
    #[test]
    fn a_refusal_keeps_its_wire_format() {
        let mut bytes = Vec::new();
        let refusal = Frame::Rejected {
            class: RejectClass::Identity,
            reason: "peer".to_owned(),
        };
        write_frame(&mut bytes, &refusal).unwrap();
        assert_eq!(bytes, frame(MSG_REJECTED, 5, b"\x04peer"));
        for (byte, class) in [
            (1, RejectClass::Protocol),
            (2, RejectClass::Control),
            (3, RejectClass::Runtime),
            (4, RejectClass::Identity),
        ] {
            assert_eq!(
                Frame::decode(MSG_REJECTED, vec![byte]),
                Ok(Frame::Rejected {
                    class,
                    reason: String::new()
                })
            );
        }
    }

    /// A class this version does not know is a malformed frame, never a
    /// guess: that includes the free-text refusal an unreleased v1 sidecar
    /// sent, whose first byte is the reason's.
    #[test]
    fn a_refusal_without_a_known_class_does_not_decode() {
        for class in [0u8, 5, 0xff, b'b'] {
            assert_eq!(
                Frame::decode(MSG_REJECTED, vec![class, b'x']),
                Err(ProtocolError::UnknownRejectClass(class)),
                "{class}"
            );
        }
        assert_eq!(
            Frame::decode(MSG_REJECTED, Vec::new()),
            Err(ProtocolError::Malformed("rejected"))
        );
        let mut bytes = Vec::new();
        write_msg(&mut bytes, MSG_REJECTED, b"busy").unwrap();
        assert!(matches!(
            read_frame(&mut bytes.as_slice()),
            Err(FrameError::Malformed(ProtocolError::UnknownRejectClass(
                b'b'
            )))
        ));
    }

    #[test]
    fn each_reject_class_maps_to_its_exit_code() {
        let codes: Vec<i32> = RejectClass::ALL
            .into_iter()
            .map(|class| PtyExitCode::from(class).code())
            .collect();
        assert_eq!(codes, [80, 81, 84, 85]);
    }

    #[test]
    fn a_hello_decodes_only_for_a_known_version_and_mode() {
        assert_eq!(
            Frame::decode(MSG_HELLO, vec![1, 3]),
            Ok(Frame::Hello {
                version: Version::V1,
                mode: Mode::Push
            })
        );
        assert_eq!(
            Frame::decode(MSG_HELLO, vec![2, 1]),
            Ok(Frame::Hello {
                version: Version::V2,
                mode: Mode::Attach
            })
        );
        // The watch hello is the byte a sidecar from before it refuses.
        assert_eq!(
            Frame::decode(MSG_HELLO, vec![2, 4]),
            Ok(Frame::Hello {
                version: Version::V2,
                mode: Mode::Watch
            })
        );
        // Version 3 is the byte a sidecar from before the end of output
        // refuses.
        assert_eq!(
            Frame::decode(MSG_HELLO, vec![3, 4]),
            Ok(Frame::Hello {
                version: Version::V3,
                mode: Mode::Watch
            })
        );
        // Version 4 is the byte a sidecar from before the input report
        // refuses.
        assert_eq!(
            Frame::decode(MSG_HELLO, vec![4, 1]),
            Ok(Frame::Hello {
                version: Version::V4,
                mode: Mode::Attach
            })
        );
        for payload in [
            vec![1, 0],
            vec![2, 5],
            vec![0, 1],
            vec![5, 1],
            vec![1],
            vec![1, 1, 0],
        ] {
            assert_eq!(
                Frame::decode(MSG_HELLO, payload.clone()),
                Err(ProtocolError::UnsupportedHello),
                "{payload:?}"
            );
        }
    }

    #[test]
    fn an_unknown_type_and_a_malformed_payload_do_not_decode() {
        assert_eq!(
            Frame::decode(0x7f, vec![1, 2]),
            Err(ProtocolError::UnknownType(0x7f))
        );
        assert_eq!(
            Frame::decode(MSG_ACCEPTED, vec![0; 7]),
            Err(ProtocolError::Malformed("accepted"))
        );
        assert_eq!(
            Frame::decode(MSG_RESIZE, vec![0, 0, 0, 80]),
            Err(ProtocolError::Malformed("resize"))
        );
        assert_eq!(
            Frame::decode(MSG_REPAINT, vec![1]),
            Err(ProtocolError::Malformed("repaint"))
        );
        // Unknown and malformed frames leave the stream in sync: the next
        // frame reads.
        let mut bytes = Vec::new();
        write_msg(&mut bytes, 0x7f, &[1, 2]).unwrap();
        write_msg(&mut bytes, MSG_RESIZE, &[0, 0, 0, 80]).unwrap();
        write_frame(&mut bytes, &Frame::Detach).unwrap();
        let mut reader = bytes.as_slice();
        assert!(matches!(
            read_frame(&mut reader),
            Err(FrameError::Unknown(0x7f))
        ));
        assert!(matches!(
            read_frame(&mut reader),
            Err(FrameError::Malformed(ProtocolError::Malformed("resize")))
        ));
        assert_eq!(read_frame(&mut reader).unwrap(), Frame::Detach);
    }

    #[test]
    fn a_frame_at_the_payload_limit_is_read() {
        let payload = vec![7u8; MAX_FRAME_PAYLOAD];
        let bytes = frame(MSG_DATA, MAX_FRAME_PAYLOAD as u32, &payload);
        let (msg_type, got) = read_msg(&mut bytes.as_slice()).unwrap();
        assert_eq!(msg_type, MSG_DATA);
        assert_eq!(got.len(), MAX_FRAME_PAYLOAD);
    }

    #[test]
    fn an_oversized_frame_is_rejected_before_allocation() {
        // A 4 GiB declared length with no payload behind it: reading must fail
        // on the length alone, not try to allocate or wait for the bytes.
        let bytes = frame(MSG_DATA, u32::MAX, &[]);
        let err = read_msg(&mut bytes.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let just_over = frame(MSG_DATA, MAX_FRAME_PAYLOAD as u32 + 1, &[]);
        let err = read_msg(&mut just_over.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
