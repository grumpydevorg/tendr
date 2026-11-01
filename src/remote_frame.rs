//! The remote frame: how `--host` carries a [`RemoteOperation`] to the remote
//! tendr. The ssh command line is constant, `tendr [--hello] _remote`
//! ([`tendr::ssh::exec_ssh_remote`]); the request travels on ssh's stdin:
//!
//! ```text
//! 4-byte big-endian length N | N bytes of UTF-8 JSON | push only: the body, to EOF
//! ```
//!
//! The JSON is `{"v": 1, "op": <operation>, "params": {…}}`: the request
//! types of [`crate::remote_op`] under a version. Unknown fields are ignored;
//! an unknown version or operation is refused. `attach` is not carried: it
//! runs under `ssh -t` until the attach bridge lands.
//!
//! [`serve`] (`tendr _remote`) reads the frame, checks everything it can
//! before anything runs, then dispatches the request as if typed on this
//! host. A refused request prints `tendr _remote: <why>` and exits 2.

use std::io::{self, Read};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tendr::model::boundary::Boundary;
use tendr::model::ids::{Namespace, SessionName};

use crate::commands::{check_unset_env_name, parse_env_entry};
use crate::remote_op::{self, RemoteOperation, StartRequest};

/// The frame version this binary reads and writes.
pub const FRAME_VERSION: u32 = 1;

/// The largest JSON header accepted, in bytes.
pub const MAX_HEADER_LEN: usize = 1 << 20;

/// The header: a request under its version. Generic so the client can
/// encode a borrowed operation.
#[derive(Serialize, Deserialize)]
struct RemoteFrame<Op> {
    v: u32,
    #[serde(flatten)]
    operation: Op,
}

/// The fields read before the request itself, so a frame of another version
/// or an unknown operation is refused as such.
#[derive(Deserialize)]
struct Head {
    v: u32,
    #[serde(default)]
    op: Option<String>,
}

/// Why a request cannot be sent.
#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("--cwd {0:?} is not valid UTF-8, and a --host request carries UTF-8 text only")]
    NonUtf8Cwd(PathBuf),
    #[error("cannot encode the request: {0}")]
    Json(#[from] serde_json::Error),
    #[error("the request is {0} bytes, over the {MAX_HEADER_LEN}-byte limit")]
    TooLarge(usize),
}

/// Why a received request is refused. Each one is refused before anything
/// runs.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("the request ended before its {0}")]
    Truncated(&'static str),
    #[error("cannot read the request: {0}")]
    Io(io::Error),
    #[error("request header of {0} bytes exceeds the {MAX_HEADER_LEN}-byte limit")]
    TooLarge(u32),
    #[error("invalid remote request: {0}")]
    Parse(#[from] serde_json::Error),
    #[error(
        "unsupported remote request version {0} (this tendr speaks {FRAME_VERSION}): \
         use the same tendr version on both hosts"
    )]
    Version(u32),
    #[error("unknown remote operation '{0}': this tendr may be older than the client")]
    UnknownOperation(String),
    #[error("invalid remote request: {0}")]
    Invalid(String),
}

/// Encode `operation` as a frame: the length, then the JSON header. A cwd
/// that is not UTF-8 is refused rather than altered: JSON carries UTF-8 only.
///
/// # Errors
/// [`EncodeError`] if the request cannot be carried.
pub fn encode(operation: &RemoteOperation) -> Result<Vec<u8>, EncodeError> {
    if let RemoteOperation::Start(StartRequest { cwd: Some(cwd), .. }) = operation {
        if cwd.to_str().is_none() {
            return Err(EncodeError::NonUtf8Cwd(cwd.clone()));
        }
    }
    let header = serde_json::to_vec(&RemoteFrame {
        v: FRAME_VERSION,
        operation,
    })?;
    if header.len() > MAX_HEADER_LEN {
        return Err(EncodeError::TooLarge(header.len()));
    }
    let len = u32::try_from(header.len()).expect("bounded by MAX_HEADER_LEN");
    let mut frame = Vec::with_capacity(4 + header.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&header);
    Ok(frame)
}

/// `tendr _remote`: read one request from stdin, check it, run it.
///
/// The header is read through `std::io::stdin()`, the process-wide buffered
/// reader, and so is a push's body (`cmd_push`): bytes the header read
/// buffered past its end are the body's first bytes, and are not lost.
pub fn serve() -> anyhow::Result<()> {
    let operation = match read_request(&mut io::stdin().lock()) {
        Ok(operation) => operation,
        Err(e) => {
            eprintln!("tendr _remote: {e}");
            std::process::exit(2);
        }
    };
    remote_op::dispatch(operation)
}

/// Read and check one frame's header from `input`, leaving any body unread.
fn read_request(input: &mut impl Read) -> Result<RemoteOperation, FrameError> {
    let mut len = [0u8; 4];
    read_exact(input, &mut len, "4-byte length")?;
    let len = u32::from_be_bytes(len);
    if len as usize > MAX_HEADER_LEN {
        return Err(FrameError::TooLarge(len));
    }
    let mut header = vec![0; len as usize];
    read_exact(input, &mut header, "header")?;
    decode(&header)
}

fn read_exact(input: &mut impl Read, buf: &mut [u8], what: &'static str) -> Result<(), FrameError> {
    input.read_exact(buf).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => FrameError::Truncated(what),
        _ => FrameError::Io(e),
    })
}

/// Decode and check a JSON header.
fn decode(header: &[u8]) -> Result<RemoteOperation, FrameError> {
    let head: Head = serde_json::from_slice(header)?;
    if head.v != FRAME_VERSION {
        return Err(FrameError::Version(head.v));
    }
    if let Some(op) = head.op.filter(|op| !remote_op::is_operation(op)) {
        return Err(FrameError::UnknownOperation(op));
    }
    let frame: RemoteFrame<RemoteOperation> = serde_json::from_slice(header)?;
    validate(&frame.operation).map_err(FrameError::Invalid)?;
    Ok(frame.operation)
}

/// Check what the command line would have checked, and what the command
/// checks only after it has started work: names, `KEY=VALUE` entries, a
/// command to run, and flags that need another. Mirrors clap's rules for
/// the typed command, so a request this tendr's own client sends passes.
fn validate(operation: &RemoteOperation) -> Result<(), String> {
    fn namespace(namespace: &Option<String>) -> Result<(), String> {
        match namespace {
            Some(ns) => Namespace::new(ns).map(drop).map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }
    fn session(name: &str) -> Result<(), String> {
        SessionName::new(name).map(drop).map_err(|e| e.to_string())
    }
    match operation {
        RemoteOperation::Start(r) => {
            namespace(&r.namespace)?;
            session(&r.session)?;
            validate_start(r)
        }
        RemoteOperation::Status(r) => namespace(&r.namespace).and_then(|()| session(&r.session)),
        RemoteOperation::List(r) => namespace(&r.namespace),
        RemoteOperation::Log(r) => namespace(&r.namespace).and_then(|()| session(&r.session)),
        RemoteOperation::Push(r) => namespace(&r.namespace).and_then(|()| session(&r.session)),
        RemoteOperation::Kill(r) => namespace(&r.namespace).and_then(|()| session(&r.session)),
        RemoteOperation::Wait(r) => {
            namespace(&r.namespace)?;
            if r.sessions.is_empty() {
                return Err("wait needs at least one session".to_owned());
            }
            r.sessions.iter().try_for_each(|s| session(s))
        }
        RemoteOperation::Watch(r) => namespace(&r.namespace),
        RemoteOperation::Attach(_) => {
            Err("attach is not carried by the remote frame: it runs over ssh -t".to_owned())
        }
    }
}

fn validate_start(r: &StartRequest) -> Result<(), String> {
    if r.argv.is_empty() {
        return Err("no command specified".to_owned());
    }
    for entry in &r.env {
        parse_env_entry(entry).map_err(|e| e.to_string())?;
    }
    for name in &r.unset_env {
        check_unset_env_name(name).map_err(|e| e.to_string())?;
    }
    for dependency in &r.after {
        SessionName::new(dependency).map_err(|e| e.to_string())?;
    }
    if r.any_exit && r.after.is_empty() {
        return Err("--any-exit requires --after".to_owned());
    }
    if r.boundary.is_none() && !r.boundary_parent.is_empty() {
        return Err("--boundary-parent requires --boundary".to_owned());
    }
    // A boundary is valid when it is what the `KIND:LABEL` parse yields.
    for boundary in r.boundary.iter().chain(&r.boundary_parent) {
        boundary
            .to_string()
            .parse::<Boundary>()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Commands};
    use clap::Parser as _;

    fn op(args: &[&str]) -> RemoteOperation {
        let mut argv = vec!["tendr"];
        argv.extend_from_slice(args);
        let command: Commands = Cli::try_parse_from(argv).expect("parses").command;
        RemoteOperation::try_from(&command).expect("remote-capable")
    }

    fn read(bytes: &[u8]) -> Result<RemoteOperation, FrameError> {
        read_request(&mut &bytes[..])
    }

    /// Every operation the frame carries comes back as it went in, and the
    /// header has the documented shape.
    #[test]
    fn every_framed_operation_round_trips() {
        let hostile = "& | $ ; ( ) \" ' ` \r\n \u{e9} C:\\Program Files\\x";
        let env = format!("K={hostile}");
        let boundary = format!("host:{hostile}");
        let cases: [&[&str]; 8] = [
            &[
                "start",
                "--namespace",
                "n",
                "--stdin",
                "--pty",
                "--replace",
                "--exec-target",
                "python-repl",
                "--timeout",
                "9",
                "--cwd",
                hostile,
                "--env",
                &env,
                "--unset-env",
                "U",
                "--on-exit",
                hostile,
                "--after",
                "dep",
                "--any-exit",
                "--boundary",
                &boundary,
                "--boundary-parent",
                &boundary,
                "s",
                "--",
                hostile,
                "--host",
                "x",
            ],
            &["status", "--namespace", "n", "s"],
            &["list"],
            &["log", "-n", "3", "-f", "-s", hostile, "-r", "s"],
            &["push", "s"],
            &["kill", "-f", "s"],
            &["wait", "-t", "4", "--any", "a", "b"],
            &["watch", "--events", "--logs", "--annotations", "--from-now"],
        ];
        for args in cases {
            let original = op(args);
            let frame = encode(&original).expect("encodes");
            let len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
            assert_eq!(len, frame.len() - 4, "{args:?}");
            let json: serde_json::Value = serde_json::from_slice(&frame[4..]).unwrap();
            assert_eq!(json["v"], 1, "{args:?}");
            assert_eq!(json["op"], args[0], "{args:?}");
            assert!(json["params"].is_object(), "{args:?}");
            assert_eq!(read(&frame).expect("decodes"), original, "{args:?}");
        }
    }

    /// The header documented in the plan and the architecture doc.
    #[test]
    fn start_header_shape() {
        let frame = encode(&op(&[
            "start",
            "--exec-target",
            "powershell",
            "--env",
            "MODE=release",
            "--stdin",
            "build",
            "--",
            "powershell",
            "-NoProfile",
        ]))
        .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&frame[4..]).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "v": 1, "op": "start",
                "params": {
                    "session": "build", "namespace": null,
                    "argv": ["powershell", "-NoProfile"], "cwd": null,
                    "env": ["MODE=release"], "unset_env": [], "stdin": true,
                    "timeout": null, "exec_target": "PowerShell", "pty": false,
                    "replace": false, "on_exit": [], "after": [],
                    "any_exit": false, "boundary": null, "boundary_parent": []
                }
            })
        );
    }

    /// The body after the header is left for push to read.
    #[test]
    fn the_body_is_left_unread() {
        let mut bytes = encode(&op(&["push", "s"])).unwrap();
        bytes.extend_from_slice(b"\x00\xffbody");
        let mut input = &bytes[..];
        assert_eq!(read_request(&mut input).unwrap(), op(&["push", "s"]));
        assert_eq!(input, b"\x00\xffbody");
    }

    #[test]
    fn a_request_is_read_across_partial_reads() {
        /// Hands out one byte per read.
        struct Trickle<'a>(&'a [u8]);
        impl Read for Trickle<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let Some((first, rest)) = self.0.split_first() else {
                    return Ok(0);
                };
                if buf.is_empty() {
                    return Ok(0);
                }
                buf[0] = *first;
                self.0 = rest;
                Ok(1)
            }
        }
        let frame = encode(&op(&["status", "s"])).unwrap();
        assert_eq!(
            read_request(&mut Trickle(&frame)).unwrap(),
            op(&["status", "s"])
        );
    }

    #[test]
    fn truncated_and_oversized_frames_are_refused() {
        let frame = encode(&op(&["status", "s"])).unwrap();
        for cut in [0, 2, 4, frame.len() - 1] {
            assert!(
                matches!(read(&frame[..cut]), Err(FrameError::Truncated(_))),
                "{cut}"
            );
        }
        let oversized = u32::try_from(MAX_HEADER_LEN + 1).unwrap().to_be_bytes();
        assert!(matches!(read(&oversized), Err(FrameError::TooLarge(_))));
        let at_limit = u32::try_from(MAX_HEADER_LEN).unwrap().to_be_bytes();
        assert!(
            matches!(read(&at_limit), Err(FrameError::Truncated("header"))),
            "the limit itself is allowed"
        );
    }

    #[test]
    fn unknown_versions_and_operations_are_refused() {
        assert!(matches!(
            decode(br#"{"v":2,"op":"list","params":{"namespace":null}}"#),
            Err(FrameError::Version(2))
        ));
        assert!(matches!(
            decode(br#"{"v":1,"op":"explode","params":{}}"#),
            Err(FrameError::UnknownOperation(op)) if op == "explode"
        ));
        assert!(matches!(
            decode(br#"{"op":"list"}"#),
            Err(FrameError::Parse(_))
        ));
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let operation =
            decode(br#"{"v":1,"op":"list","params":{"namespace":"n","later":1},"extra":[true]}"#)
                .unwrap();
        assert_eq!(operation, op(&["list", "--namespace", "n"]));
    }

    #[test]
    fn attach_is_not_carried() {
        assert!(matches!(
            decode(
                br#"{"v":1,"op":"attach","params":{"session":"s","namespace":null,"takeover":false,"escape":"none"}}"#
            ),
            Err(FrameError::Invalid(msg)) if msg.contains("ssh -t")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_cwd_is_refused_at_encode() {
        use std::os::unix::ffi::OsStrExt;
        let RemoteOperation::Start(mut request) = op(&["start", "s", "x"]) else {
            panic!("not a start");
        };
        request.cwd = Some(PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/\xff")));
        assert!(matches!(
            encode(&RemoteOperation::Start(request)),
            Err(EncodeError::NonUtf8Cwd(_))
        ));
    }

    #[test]
    fn an_oversized_request_is_refused_at_encode() {
        let RemoteOperation::Start(mut request) = op(&["start", "s", "x"]) else {
            panic!("not a start");
        };
        request.argv = vec!["a".repeat(MAX_HEADER_LEN)];
        assert!(matches!(
            encode(&RemoteOperation::Start(request)),
            Err(EncodeError::TooLarge(_))
        ));
    }
}
