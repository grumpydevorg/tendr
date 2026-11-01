use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

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
/// Sidecar → client: control granted. Payload: the controller epoch (u64 BE).
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

/// The attach protocol version carried in [`MSG_HELLO`].
pub const PROTOCOL_VERSION: u8 = 1;

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
}

impl TryFrom<u8> for Mode {
    type Error = ProtocolError;

    fn try_from(byte: u8) -> Result<Self, ProtocolError> {
        match byte {
            1 => Ok(Self::Attach),
            2 => Ok(Self::Takeover),
            3 => Ok(Self::Push),
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
/// a class means bumping [`PROTOCOL_VERSION`]. A refusal sent before the hello
/// is read (a peer of another user, no free connection slot) may use only
/// classes every supported version knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RejectClass {
    /// The hello was missing, or named a version or mode this sidecar does not
    /// speak.
    Protocol = 1,
    /// Someone else holds the terminal, or the run has ended.
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
    /// Client → sidecar, first frame of every connection. Only
    /// [`PROTOCOL_VERSION`] decodes.
    Hello(Mode),
    /// Terminal bytes: input from a client, output to a viewer.
    Data(Vec<u8>),
    /// Client → sidecar: the client's terminal size.
    Resize(Geometry),
    /// Client → sidecar: the end of this connection's input.
    Detach,
    /// Client → sidecar: make the child redraw after an attach at its size.
    Repaint,
    /// Sidecar → client: control granted at this epoch.
    Accepted(ControllerEpoch),
    /// Sidecar → client: control refused, with why.
    Rejected { class: RejectClass, reason: String },
    /// Sidecar → client: another client took over at this epoch.
    Retired(ControllerEpoch),
    /// Sidecar → push client: the push's outcome.
    InputDone(PushOutcome),
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
                [PROTOCOL_VERSION, mode] => Self::Hello(Mode::try_from(*mode)?),
                _ => return Err(ProtocolError::UnsupportedHello),
            },
            MSG_ACCEPTED => Self::Accepted(epoch("accepted")?),
            MSG_REJECTED => match payload.split_first() {
                Some((class, reason)) => Self::Rejected {
                    class: RejectClass::try_from(*class)?,
                    reason: String::from_utf8_lossy(reason).into_owned(),
                },
                None => return Err(ProtocolError::Malformed("rejected")),
            },
            MSG_RETIRED => Self::Retired(epoch("retired")?),
            MSG_INPUT_DONE => Self::InputDone(PushOutcome::decode(&payload)?),
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
        Frame::Hello(mode) => write_msg(w, MSG_HELLO, &[PROTOCOL_VERSION, *mode as u8]),
        Frame::Data(bytes) => write_msg(w, MSG_DATA, bytes),
        Frame::Resize(size) => write_msg(w, MSG_RESIZE, &resize_payload(*size)),
        Frame::Detach => write_msg(w, MSG_DETACH, &[]),
        Frame::Repaint => write_msg(w, MSG_REPAINT, &[]),
        Frame::Accepted(epoch) => write_msg(w, MSG_ACCEPTED, &epoch.get().to_be_bytes()),
        Frame::Rejected { class, reason } => {
            let mut payload = Vec::with_capacity(1 + reason.len());
            payload.push(*class as u8);
            payload.extend_from_slice(reason.as_bytes());
            write_msg(w, MSG_REJECTED, &payload)
        }
        Frame::Retired(epoch) => write_msg(w, MSG_RETIRED, &epoch.get().to_be_bytes()),
        Frame::InputDone(outcome) => write_msg(w, MSG_INPUT_DONE, &outcome.encode()),
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
        for frame in [
            Frame::Hello(Mode::Attach),
            Frame::Hello(Mode::Takeover),
            Frame::Hello(Mode::Push),
            Frame::Data(b"abc".to_vec()),
            Frame::Resize(Geometry::new(24, 80).unwrap()),
            Frame::Detach,
            Frame::Repaint,
            Frame::Accepted(ControllerEpoch::new(7)),
            Frame::Rejected {
                class: RejectClass::Control,
                reason: "busy".to_owned(),
            },
            Frame::Retired(ControllerEpoch::new(8)),
            Frame::InputDone(PushOutcome::Written { bytes: 9 }),
            Frame::InputDone(PushOutcome::Revoked(progress)),
            Frame::InputDone(PushOutcome::Closed(progress)),
        ] {
            assert_eq!(round_trip(&frame), frame);
        }
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
    fn a_hello_decodes_only_for_this_version_and_a_known_mode() {
        assert_eq!(
            Frame::decode(MSG_HELLO, vec![PROTOCOL_VERSION, 3]),
            Ok(Frame::Hello(Mode::Push))
        );
        for payload in [
            vec![PROTOCOL_VERSION, 0],
            vec![PROTOCOL_VERSION, 4],
            vec![PROTOCOL_VERSION + 1, 1],
            vec![PROTOCOL_VERSION],
            vec![PROTOCOL_VERSION, 1, 0],
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
