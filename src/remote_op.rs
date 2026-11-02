//! Typed requests for the commands `--host` can run remotely: start, status,
//! list, log, push, kill, wait, watch and attach.
//!
//! The parsed command line becomes a [`RemoteOperation`] (`TryFrom<&Commands>`),
//! and [`dispatch`] runs it. Locally that is the whole path. With `--host`
//! the request travels as data in the remote frame ([`crate::remote_frame`])
//! and the remote tendr dispatches it; for `attach` it bridges the attach
//! protocol that follows the request instead. A Windows client still runs
//! `attach` under `ssh -t`, with a command line of plain names
//! (`AttachRequest::ssh_argv`).
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
use tendr::model::spec::ExecTarget;

use crate::{AttachEscape, Commands, commands, parse_optional_namespace, resolve_namespace};

/// One remote-capable command, as typed.
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
/// entries as typed (validated at launch); `after` holds session names, bound
/// to run ids at launch.
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitRequest {
    pub sessions: Vec<String>,
    pub namespace: Option<String>,
    pub timeout: Option<u64>,
    pub any: bool,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachRequest {
    pub session: String,
    pub namespace: Option<String>,
    pub takeover: bool,
    pub escape: EscapeMode,
    #[serde(default)]
    pub protocol: u8,
}

#[cfg(windows)]
impl AttachRequest {
    /// The remote tendr argv for `attach --host` from a Windows client, which
    /// has no local console frontend for the attach protocol yet and so runs
    /// `attach` under `ssh -t` as a command line. Only plain names
    /// ([`tendr::ssh::is_plain_name`]) and fixed flag values go in it, so no
    /// remote shell acts on any of it.
    ///
    /// # Errors
    /// [`NotPlainName`] for a session or namespace that is not a plain name.
    pub fn ssh_argv(&self) -> Result<Vec<String>, NotPlainName> {
        let plain = |what, value: &str| {
            if tendr::ssh::is_plain_name(value) {
                Ok(value.to_owned())
            } else {
                Err(NotPlainName {
                    what,
                    value: value.to_owned(),
                })
            }
        };
        let mut args = vec!["attach".to_owned(), plain("session", &self.session)?];
        if let Some(ns) = &self.namespace {
            args.extend(["--namespace".to_owned(), plain("namespace", ns)?]);
        }
        if self.takeover {
            args.push("--takeover".to_owned());
        }
        if self.escape == EscapeMode::Disabled {
            args.extend(["--escape".to_owned(), "none".to_owned()]);
        }
        Ok(args)
    }
}

/// A session or namespace that `attach --host` from a Windows client cannot
/// put on its command line.
#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "attach over --host from Windows takes plain names: \
     {what} {value:?} must match [A-Za-z][A-Za-z0-9_-]{{0,254}}"
)]
pub struct NotPlainName {
    what: &'static str,
    value: String,
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
/// context, `run` reads a local script file and `prune` walks the local
/// session root.
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
                exec_target,
                replace,
                timeout,
                cwd,
                env_vars,
                unset_env,
                on_exit,
                after,
                any_exit,
                boundary,
                boundary_parent,
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
            } => Self::Log(LogRequest {
                session: name.clone(),
                namespace: namespace.clone(),
                tail: *tail,
                follow: *follow,
                since: since.clone(),
                raw: *raw,
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
            } => Self::Wait(WaitRequest {
                sessions: names.clone(),
                namespace: namespace.clone(),
                timeout: *timeout,
                any: *any,
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
                escape,
            } => Self::Attach(AttachRequest {
                session: name.clone(),
                namespace: namespace.clone(),
                takeover: *takeover,
                escape: match escape {
                    AttachEscape::CtrlBackslash => EscapeMode::CtrlBackslash,
                    AttachEscape::None => EscapeMode::Disabled,
                },
                protocol: tendr::attach_proto::Version::LATEST as u8,
            }),
            Commands::Run { .. } => return local("run"),
            Commands::Exec { .. } => return local("exec"),
            Commands::Wrap { .. } => return local("wrap"),
            Commands::Events { .. } => return local("events"),
            Commands::Query { .. } => return local("query"),
            Commands::Emit { .. } => return local("emit"),
            Commands::Prune { .. } => return local("prune"),
            Commands::Guide { .. } => return local("guide"),
            Commands::Skill { .. } => return local("skill"),
            Commands::Sidecar { .. } => return local("_sidecar"),
            Commands::Remote => return local("_remote"),
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
}

/// Resolve a start request on the host that runs it. Only the namespace can
/// fail here, so it is still the first input checked.
fn start_inputs(request: StartRequest) -> anyhow::Result<StartInputs> {
    let StartRequest {
        session,
        namespace,
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
    } = request;
    Ok(StartInputs {
        namespace: resolve_namespace(namespace)?,
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
            commands::cmd_log(&r.session, r.tail, r.follow, r.since, r.raw, &ns)
        }
        RemoteOperation::Push(r) => {
            commands::cmd_push(&r.session, &resolve_namespace(r.namespace)?)
        }
        RemoteOperation::Kill(r) => {
            commands::cmd_kill(&r.session, r.force, &resolve_namespace(r.namespace)?)
        }
        RemoteOperation::Wait(r) => {
            let ns = resolve_namespace(r.namespace)?;
            commands::cmd_wait(&r.sessions, r.timeout, r.any, &ns)
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
            commands::cmd_attach(&r.session, &ns, r.takeover, r.escape)
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
            "--boundary",
            "host:alpha",
            "--boundary-parent",
            "vm:beta",
            "--boundary-parent",
            "pod:gamma",
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
            exec_target,
            replace,
            timeout,
            cwd: parsed_cwd,
            env_vars,
            unset_env,
            on_exit,
            after,
            any_exit,
            boundary: parsed_boundary,
            boundary_parent,
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

    /// A Windows client's `ssh -t` command line for attach: the plain names
    /// and fixed flags, in the order the remote parse expects.
    #[cfg(windows)]
    #[test]
    fn attach_ssh_argv_is_plain() {
        let cases: &[(&[&str], &[&str])] = &[
            (&["attach", "s"], &["attach", "s"]),
            (
                &["attach", "s", "--escape", "ctrl-backslash"],
                &["attach", "s"],
            ),
            (
                &[
                    "attach",
                    "--takeover",
                    "--escape",
                    "none",
                    "--namespace",
                    "n-1",
                    "S_2",
                ],
                &[
                    "attach",
                    "S_2",
                    "--namespace",
                    "n-1",
                    "--takeover",
                    "--escape",
                    "none",
                ],
            ),
        ];
        for (args, expected) in cases {
            let RemoteOperation::Attach(request) = op(args) else {
                panic!("not an attach");
            };
            assert_eq!(request.ssh_argv().unwrap(), strings(expected), "{args:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn attach_ssh_argv_refuses_names_a_shell_acts_on() {
        for args in [
            &["attach", "x&calc"][..],
            &["attach", "a'b"],
            &["attach", "_s"],
            &["attach", "0x1F"],
            &["attach", "--namespace", "n$(id)", "s"],
        ] {
            let RemoteOperation::Attach(request) = op(args) else {
                panic!("not an attach");
            };
            let err = request.ssh_argv().expect_err("not plain");
            assert!(
                err.to_string()
                    .starts_with("attach over --host from Windows takes plain names"),
                "{err}"
            );
        }
    }

    #[test]
    fn local_only_commands_are_unrepresentable() {
        let cases: &[(&[&str], &str)] = &[
            (&["run", "script.sh"], "run"),
            (&["wrap", "--source", "s", "--event", "e", "true"], "wrap"),
            (&["prune", "--all"], "prune"),
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
