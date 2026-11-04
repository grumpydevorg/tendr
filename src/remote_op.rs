//! Typed requests for the commands `--host` can run remotely: start, status,
//! list, log, push, kill, wait, watch and attach.
//!
//! The parsed command line becomes a [`RemoteOperation`] (`TryFrom<&Commands>`),
//! and [`dispatch`] runs it. Locally that is the whole path. With `--host`
//! the request travels as data in the remote frame ([`crate::remote_frame`])
//! and the remote tendr dispatches it; for `attach` it bridges the attach
//! protocol that follows the request instead.
//!
//! A request holds only what the user typed. Defaults (the namespace) and
//! bindings (`--after` run ids) resolve in [`dispatch`], on the host that runs
//! the command. `exec` is not here: it has its own frame
//! ([`tendr::exec_request`]). Commands that cannot run remotely are
//! unrepresentable: converting them fails with [`LocalOnly`].

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tendr::attach_escape::EscapeMode;
use tendr::model::boundary::{Boundary, BoundaryContext};
use tendr::model::ids::Namespace;
use tendr::model::ready::{ReadyCheck, ReadyWhen};
use tendr::model::spec::{ExecTarget, LogDelimiter, RecordingPolicy};

use crate::{AttachEscape, Commands, commands, parse_optional_namespace, resolve_namespace};

/// One remote-capable command, as typed. One request per process, so the
/// size of its largest variant does not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", content = "params", rename_all = "snake_case")]
pub enum RemoteOperation {
    Start(StartRequest),
    Status(StatusRequest),
    List(ListRequest),
    Log(LogRequest),
    Push(PushRequest),
    Kill(KillRequest),
    Wait(WaitRequest),
    Watch(WatchRequest),
    Attach(AttachRequest),
}

/// `start`: every launch input, unresolved. `env` holds the `KEY=VALUE`
/// entries as typed (validated at launch); `after` and `after_ready` hold
/// session names, bound to run ids at launch. The ready fields are left out
/// when unset, so a request without them reads as it always has; a request
/// with them needs frame version 2 ([`crate::remote_frame`]).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartRequest {
    pub session: String,
    pub namespace: Option<String>,
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<String>,
    pub unset_env: Vec<String>,
    pub stdin: bool,
    pub timeout: Option<u64>,
    pub exec_target: Option<ExecTarget>,
    pub pty: bool,
    pub replace: bool,
    pub on_exit: Vec<String>,
    pub after: Vec<String>,
    pub any_exit: bool,
    pub boundary: Option<Boundary>,
    pub boundary_parent: Vec<Boundary>,
    /// `--record-input`. Left out when false, so a request without it reads
    /// as it always has; a remote that would drop it is never sent it
    /// (`remote_frame::min_frame_version`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub record_input: bool,
    /// `--no-record`, likewise.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_record: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_when_output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_when_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_within_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after_ready: Vec<String>,
    /// Left out at the default, so the frame is one a 0.7 remote reads.
    #[serde(default, skip_serializing_if = "LogDelimiter::is_newline")]
    pub log_delimiter: LogDelimiter,
}

impl StartRequest {
    /// The ready condition the request names, checked as `start` checks it.
    ///
    /// # Errors
    /// Why the ready flags cannot be combined, or why the condition is
    /// invalid.
    pub fn ready_check(&self) -> Result<Option<ReadyCheck>, String> {
        let when = match (&self.ready_when_output, self.ready_when_port) {
            (Some(_), Some(_)) => {
                return Err(
                    "--ready-when-output and --ready-when-port cannot be used together".to_owned(),
                );
            }
            (Some(pattern), None) => ReadyWhen::Output(pattern.clone()),
            (None, Some(port)) => ReadyWhen::Port(port),
            (None, None) if self.ready_within_ms.is_some() => {
                return Err(
                    "--ready-within requires --ready-when-output or --ready-when-port".to_owned(),
                );
            }
            (None, None) => return Ok(None),
        };
        when.validate().map_err(|e| e.to_string())?;
        Ok(Some(ReadyCheck {
            when,
            within_ms: self.ready_within_ms,
        }))
    }
}

/// Debug shows the shape of the payload, never its values: argv, cwd, env and
/// callbacks can carry secrets.
impl fmt::Debug for StartRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StartRequest")
            .field("session", &self.session)
            .field("namespace", &self.namespace)
            .field("argv", &Redacted(self.argv.len()))
            .field("cwd", &self.cwd.as_ref().map(|_| Redacted(1)))
            .field("env", &Redacted(self.env.len()))
            .field("unset_env", &Redacted(self.unset_env.len()))
            .field("stdin", &self.stdin)
            .field("timeout", &self.timeout)
            .field("exec_target", &self.exec_target)
            .field("pty", &self.pty)
            .field("replace", &self.replace)
            .field("on_exit", &Redacted(self.on_exit.len()))
            .field("after", &self.after)
            .field("any_exit", &self.any_exit)
            .field("boundary", &self.boundary)
            .field("boundary_parent", &self.boundary_parent)
            .field("record_input", &self.record_input)
            .field("no_record", &self.no_record)
            .field("ready_when_output", &self.ready_when_output)
            .field("ready_when_port", &self.ready_when_port)
            .field("ready_within_ms", &self.ready_within_ms)
            .field("after_ready", &self.after_ready)
            .field("log_delimiter", &self.log_delimiter)
            .finish()
    }
}

/// Stands in for `n` redacted values in Debug output.
struct Redacted(usize);

impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{} redacted>", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusRequest {
    pub session: String,
    pub namespace: Option<String>,
}

/// `list`: no namespace lists every namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListRequest {
    pub namespace: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRequest {
    pub session: String,
    pub namespace: Option<String>,
    pub tail: Option<usize>,
    pub follow: bool,
    pub since: Option<String>,
    pub raw: bool,
    /// An older remote never collapses, so it may ignore this.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all_updates: bool,
}

/// `push`: the bytes come from stdin, not the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushRequest {
    pub session: String,
    pub namespace: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillRequest {
    pub session: String,
    pub namespace: Option<String>,
    pub force: bool,
}

/// `wait`: `ready` waits for the sessions to be ready instead of ended. Left
/// out when false, so a request without it reads as it always has; with it,
/// the request needs frame version 2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitRequest {
    pub sessions: Vec<String>,
    pub namespace: Option<String>,
    pub timeout: Option<u64>,
    pub any: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ready: bool,
}

/// `watch`: no namespace watches every namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchRequest {
    pub namespace: Option<String>,
    pub events: bool,
    pub logs: bool,
    pub annotations: bool,
    pub from_now: bool,
    /// Consumed locally; never crosses the wire.
    #[serde(skip)]
    pub ready_file: Option<PathBuf>,
}

/// `attach`: `escape` is applied by the client's terminal, so over `--host`
/// the remote tendr ignores it. `protocol` is the newest attach protocol
/// version the client speaks; the remote tendr speaks the newest both do
/// ([`tendr::attach_proto::Version::negotiate`]). A request from a client that
/// predates the field has none, which means version 1, and a remote tendr
/// that predates it ignores it.
///
/// `resume` is set when a client whose connection was lost asks to regain
/// its control and output; it is sent only after a version 2 acceptance,
/// and travels again as the frame that replaces the hello.
///
/// `read_only` asks for a read-only viewer, which holds no control. Left out
/// when false, so a request without it reads as it always has. A read-only
/// request with `resume` is a watcher's resume: the remote reads only the
/// run and offset, the client sends epoch zero, and the frame that replaces
/// the hello is a watch resume, which no remote from before it accepts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachRequest {
    pub session: String,
    pub namespace: Option<String>,
    pub takeover: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    pub escape: EscapeMode,
    #[serde(default)]
    pub protocol: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<ResumeRequest>,
}

/// Where a lost attach resumes from ([`tendr::attach_proto::ResumePoint`]):
/// its run, the controller epoch it held (zero for a watcher, which holds
/// none), and the offset of the next output byte it did not get.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeRequest {
    pub run_id: tendr::model::ids::RunId,
    pub epoch: u64,
    pub offset: u64,
}

impl From<tendr::attach_proto::ResumePoint> for ResumeRequest {
    fn from(point: tendr::attach_proto::ResumePoint) -> Self {
        Self {
            run_id: point.run_id,
            epoch: point.epoch.get(),
            offset: point.offset,
        }
    }
}

impl From<ResumeRequest> for tendr::attach_proto::ResumePoint {
    fn from(request: ResumeRequest) -> Self {
        Self {
            run_id: request.run_id,
            epoch: tendr::model::pty_control::ControllerEpoch::new(request.epoch),
            offset: request.offset,
        }
    }
}

/// The remote operations, as listed when a local-only command is refused.
const SUPPORTED_REMOTE: &str = "start, status, list, log, push, kill, wait, watch, attach";

/// Whether `name` is an operation's name (`op` in the remote frame).
pub fn is_operation(name: &str) -> bool {
    // The list is pinned to the operations by `supported_list_names_every_operation`.
    SUPPORTED_REMOTE.split(", ").any(|op| op == name)
}

/// A command that cannot be a [`RemoteOperation`]: it runs only locally (or,
/// for `exec`, travels on its own frame). `wrap` relies on local process
/// context, `run` reads a local script file, `prune` walks the local
/// session root and `replay` plays a local recording in the local terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "command '{command}' is not supported over SSH (--host).\n\
     Supported remote commands: {SUPPORTED_REMOTE}.\n\
     Some local-only commands rely on local FIFO, process context,\n\
     or filesystem state that cannot tunnel through ssh -T."
)]
pub struct LocalOnly {
    /// The subcommand as typed.
    pub command: &'static str,
}

impl TryFrom<&Commands> for RemoteOperation {
    type Error = LocalOnly;

    fn try_from(command: &Commands) -> Result<Self, LocalOnly> {
        let local = |command| Err(LocalOnly { command });
        Ok(match command {
            Commands::Start {
                name,
                namespace,
                stdin,
                pty,
                log_delimiter,
                exec_target,
                replace,
                timeout,
                cwd,
                env_vars,
                unset_env,
                on_exit,
                after,
                any_exit,
                ready_when_output,
                ready_when_port,
                ready_within,
                after_ready,
                boundary,
                boundary_parent,
                record_input,
                no_record,
                cmd,
            } => Self::Start(StartRequest {
                session: name.clone(),
                namespace: namespace.clone(),
                argv: cmd.clone(),
                cwd: cwd.clone(),
                env: env_vars.clone(),
                unset_env: unset_env.clone(),
                stdin: *stdin,
                timeout: *timeout,
                exec_target: exec_target.clone().map(ExecTarget::from),
                pty: *pty,
                replace: *replace,
                on_exit: on_exit.clone(),
                after: after.clone(),
                any_exit: *any_exit,
                boundary: boundary.clone(),
                boundary_parent: boundary_parent.clone(),
                record_input: *record_input,
                no_record: *no_record,
                ready_when_output: ready_when_output.clone(),
                ready_when_port: *ready_when_port,
                ready_within_ms: ready_within
                    .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
                after_ready: after_ready.clone(),
                log_delimiter: (*log_delimiter).map(LogDelimiter::from).unwrap_or_default(),
            }),
            Commands::Status { name, namespace } => Self::Status(StatusRequest {
                session: name.clone(),
                namespace: namespace.clone(),
            }),
            Commands::List { namespace } => Self::List(ListRequest {
                namespace: namespace.clone(),
            }),
            Commands::Log {
                name,
                namespace,
                tail,
                follow,
                since,
                raw,
                all_updates,
            } => Self::Log(LogRequest {
                session: name.clone(),
                namespace: namespace.clone(),
                tail: *tail,
                follow: *follow,
                since: since.clone(),
                raw: *raw,
                all_updates: *all_updates,
            }),
            Commands::Push { name, namespace } => Self::Push(PushRequest {
                session: name.clone(),
                namespace: namespace.clone(),
            }),
            Commands::Kill {
                name,
                namespace,
                force,
            } => Self::Kill(KillRequest {
                session: name.clone(),
                namespace: namespace.clone(),
                force: *force,
            }),
            Commands::Wait {
                names,
                namespace,
                timeout,
                any,
                ready,
            } => Self::Wait(WaitRequest {
                sessions: names.clone(),
                namespace: namespace.clone(),
                timeout: *timeout,
                any: *any,
                ready: *ready,
            }),
            Commands::Watch {
                namespace,
                events,
                logs,
                annotations,
                from_now,
                ready_file,
            } => Self::Watch(WatchRequest {
                namespace: namespace.clone(),
                events: *events,
                logs: *logs,
                annotations: *annotations,
                from_now: *from_now,
                ready_file: ready_file.clone(),
            }),
            Commands::Attach {
                name,
                namespace,
                takeover,
                read_only,
                escape,
            } => Self::Attach(AttachRequest {
                session: name.clone(),
                namespace: namespace.clone(),
                takeover: *takeover,
                read_only: *read_only,
                escape: match escape {
                    AttachEscape::CtrlBackslash => EscapeMode::CtrlBackslash,
                    AttachEscape::None => EscapeMode::Disabled,
                },
                protocol: tendr::attach_proto::Version::LATEST as u8,
                resume: None,
            }),
            Commands::Run { .. } => return local("run"),
            Commands::Exec { .. } => return local("exec"),
            Commands::Wrap { .. } => return local("wrap"),
            Commands::Events { .. } => return local("events"),
            Commands::Query { .. } => return local("query"),
            Commands::Emit { .. } => return local("emit"),
            Commands::Prune { .. } => return local("prune"),
            Commands::Replay { .. } => return local("replay"),
            Commands::Guide { .. } => return local("guide"),
            Commands::Skill { .. } => return local("skill"),
            Commands::Sidecar { .. } => return local("_sidecar"),
            Commands::Remote => return local("_remote"),
            #[cfg(windows)]
            Commands::CtrlBreak { .. } => return local("_ctrl-break"),
        })
    }
}

/// What `cmd_start` receives: a [`StartRequest`] with its defaults resolved.
struct StartInputs {
    name: String,
    cmd: Vec<String>,
    stdin: bool,
    replace: bool,
    timeout: Option<u64>,
    cwd: Option<PathBuf>,
    env_vars: Vec<String>,
    unset_env: Vec<String>,
    on_exit: Vec<String>,
    after: Vec<String>,
    any_exit: bool,
    namespace: Namespace,
    pty: bool,
    exec_target: Option<ExecTarget>,
    boundary: Option<BoundaryContext>,
    recording: RecordingPolicy,
    ready: Option<ReadyCheck>,
    after_ready: Vec<String>,
    log_delimiter: LogDelimiter,
}

/// Resolve a start request on the host that runs it. Only the namespace and
/// the ready flags can fail here; the namespace is still the first input
/// checked.
fn start_inputs(request: StartRequest) -> anyhow::Result<StartInputs> {
    let namespace = resolve_namespace(request.namespace.clone())?;
    let ready = request.ready_check().map_err(anyhow::Error::msg)?;
    let StartRequest {
        session,
        namespace: _,
        argv,
        cwd,
        env,
        unset_env,
        stdin,
        timeout,
        exec_target,
        pty,
        replace,
        on_exit,
        after,
        any_exit,
        boundary,
        boundary_parent,
        record_input,
        no_record,
        ready_when_output: _,
        ready_when_port: _,
        ready_within_ms: _,
        after_ready,
        log_delimiter,
    } = request;
    Ok(StartInputs {
        namespace,
        name: session,
        cmd: argv,
        stdin,
        replace,
        timeout,
        cwd,
        env_vars: env,
        unset_env,
        on_exit,
        after,
        any_exit,
        pty,
        exec_target,
        boundary: boundary.map(|current| BoundaryContext {
            current,
            parents: boundary_parent,
        }),
        recording: if no_record {
            RecordingPolicy::Off
        } else if record_input {
            RecordingPolicy::OutputAndInput
        } else {
            RecordingPolicy::Output
        },
        ready,
        after_ready,
        log_delimiter,
    })
}

/// Run an operation on this host.
pub fn dispatch(operation: RemoteOperation) -> anyhow::Result<()> {
    match operation {
        RemoteOperation::Start(request) => {
            let i = start_inputs(request)?;
            commands::cmd_start(
                &i.name,
                i.cmd,
                i.stdin,
                i.replace,
                i.timeout,
                i.cwd.as_deref(),
                &i.env_vars,
                &i.unset_env,
                &i.on_exit,
                &i.after,
                i.any_exit,
                &i.namespace,
                i.pty,
                i.exec_target,
                i.boundary,
                i.recording,
                i.ready,
                &i.after_ready,
                i.log_delimiter,
            )
        }
        RemoteOperation::Status(r) => {
            commands::cmd_status(&r.session, &resolve_namespace(r.namespace)?)
        }
        RemoteOperation::List(r) => {
            commands::cmd_list(parse_optional_namespace(r.namespace)?.as_ref())
        }
        RemoteOperation::Log(r) => {
            let ns = resolve_namespace(r.namespace)?;
            commands::cmd_log(
                &r.session,
                r.tail,
                r.follow,
                r.since,
                r.raw,
                r.all_updates,
                &ns,
            )
        }
        RemoteOperation::Push(r) => {
            commands::cmd_push(&r.session, &resolve_namespace(r.namespace)?)
        }
        RemoteOperation::Kill(r) => {
            commands::cmd_kill(&r.session, r.force, &resolve_namespace(r.namespace)?)
        }
        RemoteOperation::Wait(r) => {
            let ns = resolve_namespace(r.namespace)?;
            commands::cmd_wait(&r.sessions, r.timeout, r.any, r.ready, &ns)
        }
        RemoteOperation::Watch(r) => {
            let ns = parse_optional_namespace(r.namespace)?;
            commands::cmd_watch(
                ns.as_ref(),
                r.events,
                r.logs,
                r.annotations,
                r.from_now,
                r.ready_file,
            )
        }
        RemoteOperation::Attach(r) => {
            let ns = resolve_namespace(r.namespace)?;
            commands::cmd_attach(&r.session, &ns, r.takeover, r.read_only, r.escape)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Commands};
    use clap::Parser as _;
    use std::path::PathBuf;
    use tendr::model::boundary::{Boundary, BoundaryContext, BoundaryKind};
    use tendr::model::ids::Namespace;
    use tendr::model::spec::ExecTarget;

    fn parse(args: &[&str]) -> Commands {
        let mut argv = vec!["tendr"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv).expect("parses").command
    }

    fn op(args: &[&str]) -> RemoteOperation {
        RemoteOperation::try_from(&parse(args)).expect("remote-capable")
    }

    fn round_trip(op: &RemoteOperation) -> RemoteOperation {
        let json = serde_json::to_string(op).expect("encodes");
        serde_json::from_str(&json).expect("decodes")
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_owned()).collect()
    }

    fn boundary(kind: BoundaryKind, label: &str) -> Boundary {
        Boundary {
            kind,
            label: label.to_owned(),
        }
    }

    /// Every `start` flag survives Commands → StartRequest → JSON → decode →
    /// the inputs dispatch hands to `cmd_start`. Both destructurings are
    /// exhaustive: a new `start` flag or input fails to compile here until
    /// it is carried through.
    #[test]
    fn every_start_flag_survives_the_round_trip() {
        let cwd = r"C:\Users\o'neil\a b&c";
        let command = parse(&[
            "start",
            "--namespace",
            "ns-1",
            "--stdin",
            "--pty",
            "--exec-target",
            "powershell",
            "--replace",
            "--timeout",
            "42",
            "--cwd",
            cwd,
            "--env",
            "A=$(id)",
            "--env",
            "B=x=y;z",
            "--unset-env",
            "DROP",
            "--on-exit",
            "notify 'done' | tee",
            "--after",
            "dep-1",
            "--after",
            "dep-2",
            "--any-exit",
            "--ready-when-output",
            r"^listening on \d+$",
            "--ready-within",
            "1m 500ms",
            "--after-ready",
            "db",
            "--boundary",
            "host:alpha",
            "--boundary-parent",
            "vm:beta",
            "--boundary-parent",
            "pod:gamma",
            "--record-input",
            "x&calc",
            "--",
            "prog",
            "--flag",
            "a b\r\n`é`",
        ]);
        let Commands::Start {
            name,
            namespace,
            stdin,
            pty,
            log_delimiter: _, // conflicts with --pty; its trip: remote_frame's version test
            exec_target,
            replace,
            timeout,
            cwd: parsed_cwd,
            env_vars,
            unset_env,
            on_exit,
            after,
            any_exit,
            ready_when_output,
            ready_when_port,
            ready_within,
            after_ready,
            boundary: parsed_boundary,
            boundary_parent,
            record_input,
            no_record,
            cmd,
        } = &command
        else {
            panic!("not a start");
        };

        let RemoteOperation::Start(request) =
            round_trip(&RemoteOperation::try_from(&command).expect("remote-capable"))
        else {
            panic!("not a start request");
        };
        let StartInputs {
            name: in_name,
            cmd: in_cmd,
            stdin: in_stdin,
            replace: in_replace,
            timeout: in_timeout,
            cwd: in_cwd,
            env_vars: in_env,
            unset_env: in_unset,
            on_exit: in_on_exit,
            after: in_after,
            any_exit: in_any_exit,
            namespace: in_namespace,
            pty: in_pty,
            exec_target: in_exec_target,
            boundary: in_boundary,
            recording: in_recording,
            ready: in_ready,
            after_ready: in_after_ready,
            log_delimiter: _,
        } = start_inputs(request).expect("resolves");

        assert_eq!(name, "x&calc");
        assert_eq!(&in_name, name);
        assert_eq!(namespace.as_deref(), Some("ns-1"));
        assert_eq!(in_namespace, Namespace::new("ns-1").unwrap());
        assert!(*stdin && in_stdin);
        assert!(*pty && in_pty);
        assert!(exec_target.is_some());
        assert_eq!(in_exec_target, Some(ExecTarget::PowerShell));
        assert!(*replace && in_replace);
        assert_eq!((*timeout, in_timeout), (Some(42), Some(42)));
        assert_eq!(parsed_cwd.as_deref(), Some(std::path::Path::new(cwd)));
        assert_eq!(in_cwd, Some(PathBuf::from(cwd)));
        assert_eq!(env_vars, &strings(&["A=$(id)", "B=x=y;z"]));
        assert_eq!(&in_env, env_vars);
        assert_eq!(unset_env, &strings(&["DROP"]));
        assert_eq!(&in_unset, unset_env);
        assert_eq!(on_exit, &strings(&["notify 'done' | tee"]));
        assert_eq!(&in_on_exit, on_exit);
        assert_eq!(after, &strings(&["dep-1", "dep-2"]));
        assert_eq!(&in_after, after);
        assert!(*any_exit && in_any_exit);
        assert_eq!(ready_when_output.as_deref(), Some(r"^listening on \d+$"));
        assert_eq!(*ready_when_port, None);
        assert_eq!(
            *ready_within,
            Some(std::time::Duration::from_millis(60_500))
        );
        assert_eq!(
            in_ready,
            Some(ReadyCheck {
                when: ReadyWhen::Output(r"^listening on \d+$".to_owned()),
                within_ms: Some(60_500),
            })
        );
        assert_eq!(after_ready, &strings(&["db"]));
        assert_eq!(&in_after_ready, after_ready);
        assert_eq!(
            parsed_boundary.as_ref(),
            Some(&boundary(BoundaryKind::Host, "alpha"))
        );
        assert_eq!(
            boundary_parent,
            &vec![
                boundary(BoundaryKind::Vm, "beta"),
                boundary(BoundaryKind::Pod, "gamma")
            ]
        );
        assert_eq!(
            in_boundary,
            Some(BoundaryContext {
                current: boundary(BoundaryKind::Host, "alpha"),
                parents: boundary_parent.clone(),
            })
        );
        assert_eq!(cmd, &strings(&["prog", "--flag", "a b\r\n`é`"]));
        assert_eq!(&in_cmd, cmd);
        assert!(*record_input && !*no_record);
        assert_eq!(in_recording, RecordingPolicy::OutputAndInput);
    }

    /// `--no-record` (which conflicts with `--record-input`) arrives, and
    /// neither flag means the default recording.
    #[test]
    fn recording_flags_resolve_on_the_target() {
        for (flags, expected) in [
            (&["--pty", "--no-record"][..], RecordingPolicy::Off),
            (&["--pty"][..], RecordingPolicy::Output),
        ] {
            let mut args = vec!["start"];
            args.extend_from_slice(flags);
            args.extend(["s", "--", "true"]);
            let RemoteOperation::Start(request) = round_trip(&op(&args)) else {
                panic!("not a start request");
            };
            assert_eq!(
                start_inputs(request).unwrap().recording,
                expected,
                "{flags:?}"
            );
        }
    }

    /// The port condition travels too, and the flags that cannot combine are
    /// refused by the command line.
    #[test]
    fn the_port_condition_survives_and_conflicts_are_refused() {
        let RemoteOperation::Start(request) = round_trip(&op(&[
            "start",
            "--ready-when-port",
            "8080",
            "s",
            "--",
            "srv",
        ])) else {
            panic!("not a start request");
        };
        assert_eq!(
            start_inputs(request).expect("resolves").ready,
            Some(ReadyCheck {
                when: ReadyWhen::Port(8080),
                within_ms: None,
            })
        );
        for args in [
            &[
                "start",
                "--ready-when-port",
                "1",
                "--ready-when-output",
                "x",
                "s",
                "--",
                "t",
            ][..],
            &["start", "--ready-within", "1s", "s", "--", "t"],
            &["start", "--ready-when-port", "0", "s", "--", "t"],
            &["start", "--ready-when-output", "(", "s", "--", "t"],
        ] {
            let mut argv = vec!["tendr"];
            argv.extend_from_slice(args);
            assert!(Cli::try_parse_from(argv).is_err(), "{args:?}");
        }
    }

    #[test]
    fn start_defaults_resolve_on_the_target() {
        let RemoteOperation::Start(request) = round_trip(&op(&["start", "s", "--", "true"])) else {
            panic!("not a start request");
        };
        assert_eq!(request.namespace, None, "the wire carries what was typed");
        let inputs = start_inputs(request).expect("resolves");
        assert_eq!(inputs.namespace, Namespace::default_namespace());
        assert_eq!(inputs.exec_target, None, "inferred by the launch, not here");
        assert_eq!(inputs.boundary, None);
        assert_eq!(inputs.cwd, None);
    }

    #[test]
    fn wire_shape_is_op_and_params() {
        let json = serde_json::to_value(op(&["kill", "--force", "s"])).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "op": "kill",
                "params": {"session": "s", "namespace": null, "force": true}
            })
        );
        let json = serde_json::to_value(op(&["list"])).unwrap();
        assert_eq!(json["op"], "list");
    }

    /// The escape travels as the `--escape` value the user typed.
    #[test]
    fn escape_uses_the_cli_spelling() {
        for value in ["ctrl-backslash", "none"] {
            let original = op(&["attach", "--escape", value, "s"]);
            let json = serde_json::to_value(&original).unwrap();
            assert_eq!(json["params"]["escape"], value);
            assert_eq!(round_trip(&original), original);
        }
    }

    #[test]
    fn every_operation_round_trips() {
        let cases: &[&[&str]] = &[
            &["status", "--namespace", "n", "s"],
            &["list", "--namespace", "n"],
            &[
                "log",
                "--namespace",
                "n",
                "-n",
                "5",
                "-f",
                "-s",
                "5m; rm",
                "-r",
                "s",
            ],
            &["push", "--namespace", "n", "s"],
            &["kill", "-f", "s"],
            &["wait", "--namespace", "n", "-t", "9", "--any", "a", "b"],
            &[
                "watch",
                "--namespace",
                "n",
                "--events",
                "--logs",
                "--annotations",
                "--from-now",
                "--ready-file",
                "/tmp/ready",
            ],
            &[
                "attach",
                "--namespace",
                "n",
                "--takeover",
                "--escape",
                "none",
                "s",
            ],
            &["attach", "--read-only", "s"],
        ];
        for args in cases {
            let original = op(args);
            let mut expected = original.clone();
            if let RemoteOperation::Watch(w) = &mut expected {
                assert!(w.ready_file.is_some(), "the local request keeps it");
                // Consumed locally; never crosses the wire.
                w.ready_file = None;
            }
            assert_eq!(round_trip(&original), expected, "{args:?}");
        }
    }

    /// Each start bool lands in its own input: set one at a time.
    #[test]
    fn start_bools_do_not_swap() {
        let flags = ["--stdin", "--pty", "--replace"];
        for flag in flags {
            let RemoteOperation::Start(request) = round_trip(&op(&["start", flag, "s", "x"]))
            else {
                panic!("not a start request");
            };
            let i = start_inputs(request).expect("resolves");
            assert_eq!(
                [i.stdin, i.pty, i.replace],
                flags.map(|f| f == flag),
                "{flag}"
            );
            assert!(!i.any_exit, "{flag}");
        }
        let RemoteOperation::Start(request) = round_trip(&op(&["start", "--after", "d", "s", "x"]))
        else {
            panic!("not a start request");
        };
        let i = start_inputs(request).expect("resolves");
        assert!(!(i.stdin || i.pty || i.replace || i.any_exit));
    }

    #[test]
    fn local_only_commands_are_unrepresentable() {
        let cases: &[(&[&str], &str)] = &[
            (&["run", "script.sh"], "run"),
            (&["wrap", "--source", "s", "--event", "e", "true"], "wrap"),
            (&["prune", "--all"], "prune"),
            (&["replay", "rec"], "replay"),
            (&["query", "select 1"], "query"),
            (&["guide"], "guide"),
            (&["skill", "print"], "skill"),
            (&["emit", "--kind", "k.x"], "emit"),
            (&["events"], "events"),
            (&["exec", "s", "true"], "exec"),
            (&["_sidecar", "--session-dir", "/d"], "_sidecar"),
            (&["_remote"], "_remote"),
        ];
        for (args, name) in cases {
            let err = RemoteOperation::try_from(&parse(args)).expect_err("local-only");
            assert_eq!(err, LocalOnly { command: name }, "{args:?}");
            assert!(err.to_string().contains(&format!("'{name}'")), "{err}");
        }
    }

    /// The generic rejection names every remote operation, and only those,
    /// as the frame's `op` spells them.
    #[test]
    fn supported_list_names_every_operation() {
        let names: Vec<String> = [
            op(&["start", "s", "x"]),
            op(&["status", "s"]),
            op(&["list"]),
            op(&["log", "s"]),
            op(&["push", "s"]),
            op(&["kill", "s"]),
            op(&["wait", "s"]),
            op(&["watch"]),
            op(&["attach", "s"]),
        ]
        .iter()
        .map(|o| {
            serde_json::to_value(o).unwrap()["op"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
        assert_eq!(SUPPORTED_REMOTE, names.join(", "));
        assert!(names.iter().all(|name| is_operation(name)));
        assert!(!is_operation("exec") && !is_operation("st"));
    }

    #[test]
    fn debug_redacts_payloads() {
        let start = op(&[
            "start",
            "--env",
            "TOKEN=hunter2-secret",
            "--on-exit",
            "curl callback-secret",
            "--cwd",
            "/home/cwd-secret",
            "s",
            "--",
            "prog",
            "argv-secret",
        ]);
        let rendered = format!("{start:?} {start:#?}");
        for secret in [
            "hunter2",
            "TOKEN",
            "callback-secret",
            "cwd-secret",
            "argv-secret",
            "prog",
        ] {
            assert!(!rendered.contains(secret), "{secret} leaked: {rendered}");
        }
        assert!(rendered.contains("Start"), "{rendered}");
    }
}
