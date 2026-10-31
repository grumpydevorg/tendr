//! Exit codes for the PTY operations: `attach`, and `push` to a PTY session.
//!
//! A failure that belongs to one of these classes is returned as a
//! [`PtyExit`] inside the command's `anyhow::Error`; the binary's `main` finds
//! it with [`exit_code`] and exits with its code instead of 1. Every other
//! failure keeps exit 1, and the legacy command-specific codes (`wait`, `run`,
//! `exec`, `events`, …) are unchanged. The command-scoped table is in
//! `docs/guide.md` ("Exit codes").
//!
//! 82 (deadline exceeded) is used outside this module by the `--host` connect
//! deadline (`ssh::SshError::ConnectDeadline`); a remote command's own 82
//! passes through `--host` unchanged, so the stderr message tells them apart.
//! 83 is reserved and not used.

use std::io;

/// The class of a PTY operation failure, which fixes its exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtyExitCode {
    /// 80: request validation or protocol-version failure after CLI parsing,
    /// such as a handshake with an older or newer sidecar.
    Protocol,
    /// 81: controller conflict, stale epoch or stale run: someone else holds
    /// the terminal, took it over, or the run it targeted has ended.
    Control,
    /// 84: runtime, transport or storage failure, including a push that ended
    /// with bytes unwritten.
    Runtime,
    /// 85: peer identity or permission rejection.
    Identity,
}

impl PtyExitCode {
    /// The process exit code.
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            Self::Protocol => 80,
            Self::Control => 81,
            Self::Runtime => 84,
            Self::Identity => 85,
        }
    }

    /// `error`, marked to exit with this code.
    pub fn error(self, error: impl Into<anyhow::Error>) -> anyhow::Error {
        PtyExit {
            code: self,
            error: error.into(),
        }
        .into()
    }

    /// The class of a failed peer check on the attach socket: a peer of
    /// another user (`PermissionDenied`) is [`Identity`](Self::Identity); a
    /// credential query that failed is [`Runtime`](Self::Runtime).
    #[must_use]
    pub fn of_peer_check(error: &io::Error) -> Self {
        if error.kind() == io::ErrorKind::PermissionDenied {
            Self::Identity
        } else {
            Self::Runtime
        }
    }
}

/// A PTY operation failure and the code the process exits with.
///
/// It displays as the wrapped error with its causes, so `main`'s
/// `{error:#}` prints the same text it would without the code.
#[derive(Debug, thiserror::Error)]
#[error("{error:#}")]
pub struct PtyExit {
    pub code: PtyExitCode,
    error: anyhow::Error,
}

/// The exit code for a command's failure: the code of the first [`PtyExit`]
/// in its chain, or 1.
#[must_use]
pub fn exit_code(error: &anyhow::Error) -> i32 {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<PtyExit>())
        .map_or(1, |exit| exit.code.code())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;

    #[test]
    fn codes_are_the_reserved_pty_range() {
        assert_eq!(PtyExitCode::Protocol.code(), 80);
        assert_eq!(PtyExitCode::Control.code(), 81);
        assert_eq!(PtyExitCode::Runtime.code(), 84);
        assert_eq!(PtyExitCode::Identity.code(), 85);
    }

    #[test]
    fn a_marked_error_exits_with_its_code_and_anything_else_with_1() {
        let marked = PtyExitCode::Control.error(anyhow::anyhow!("busy"));
        assert_eq!(exit_code(&marked), 81);
        assert_eq!(marked.to_string(), "busy");

        let wrapped = Err::<(), _>(PtyExitCode::Runtime.error(io::Error::other("reset")))
            .context("push")
            .unwrap_err();
        assert_eq!(exit_code(&wrapped), 84, "context on top keeps the code");

        assert_eq!(exit_code(&anyhow::anyhow!("session not found: x")), 1);
    }

    #[test]
    fn a_marked_error_prints_its_causes_once() {
        let inner = Err::<(), _>(io::Error::other("connection reset"))
            .context("attach socket")
            .unwrap_err();
        let marked = PtyExitCode::Runtime.error(inner);
        assert_eq!(format!("{marked:#}"), "attach socket: connection reset");
    }

    /// Peer uid mismatch cannot be produced deterministically in a test (it
    /// needs a second user), so the mapping `verify_peer`'s error takes is
    /// checked here.
    #[test]
    fn a_peer_of_another_user_is_an_identity_failure() {
        let other_user = io::Error::new(io::ErrorKind::PermissionDenied, "user 501, not 0");
        assert_eq!(
            PtyExitCode::of_peer_check(&other_user),
            PtyExitCode::Identity
        );
        let query_failed = io::Error::other("getpeereid failed");
        assert_eq!(
            PtyExitCode::of_peer_check(&query_failed),
            PtyExitCode::Runtime
        );
    }
}
