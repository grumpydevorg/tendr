/// `print!` that tolerates a reader that has gone away: see [`write_stdout`].
macro_rules! out {
    ($($arg:tt)*) => {
        $crate::commands::write_stdout(format_args!($($arg)*))
    };
}

/// `println!` that tolerates a reader that has gone away: see [`write_stdout`].
macro_rules! outln {
    () => {
        $crate::commands::write_stdout(format_args!("\n"))
    };
    ($($arg:tt)*) => {
        $crate::commands::write_stdout(format_args!("{}\n", format_args!($($arg)*)))
    };
}

mod attach;
mod emit;
mod events;
mod exec;
mod guide;
mod kill;
mod list;
mod log;
mod prune;
mod push;
mod query;
mod run;
mod sidecar;
mod skill;
mod start;
mod status;
mod wait;
mod watch;
mod wrap;

#[cfg(unix)]
pub use attach::cmd_attach_host;
pub use attach::{cmd_attach, cmd_attach_bridge};
pub use emit::{EmitOptions, cmd_emit};
pub use events::{EventsOptions, cmd_events};
pub use exec::{cmd_exec, cmd_exec_frame_from_stdin};
pub use guide::cmd_guide;
pub use kill::cmd_kill;
pub use list::cmd_list;
pub use log::cmd_log;
pub use prune::{cmd_prune, cmd_prune_named};
pub use push::cmd_push;
pub use query::{QueryOptions, cmd_query};
pub use run::cmd_run;
pub use sidecar::cmd_sidecar;
pub use skill::{cmd_skill_install, cmd_skill_path, cmd_skill_print};
pub use start::cmd_start;
pub(crate) use start::{check_unset_env_name, parse_env_entry};
pub use status::cmd_status;
pub use wait::cmd_wait;
pub use watch::cmd_watch;
pub use wrap::cmd_wrap;

/// Write command output to stdout. A reader that exits early
/// (`tendr status x | head -c1`) closes the pipe: the output is dropped and
/// the command carries on, so its exit code still means what it says. Any
/// other write failure ends the process with exit 1. `println!` would panic.
pub(crate) fn write_stdout(args: std::fmt::Arguments<'_>) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    match out.write_fmt(args).and_then(|()| out.flush()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        Err(e) => {
            eprintln!("error: writing to stdout: {e}");
            std::process::exit(1);
        }
    }
}
