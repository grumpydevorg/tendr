use tendr::log::{LogQuery, follow_log, parse_since, query_log};
use tendr::model::ids::{Namespace, SessionName};
use tendr::session::{self, SessionRoot};

pub fn cmd_log(
    name: &str,
    tail: Option<usize>,
    follow: bool,
    since: Option<String>,
    raw: bool,
    namespace: &Namespace,
) -> anyhow::Result<()> {
    let session_name = SessionName::new(name)?;
    let root = SessionRoot::default_path()?;

    let session = session::open(&root, namespace, &session_name)?
        .ok_or_else(|| anyhow::anyhow!("session not found: {name}"))?;

    let since_us = match since {
        Some(ref v) => Some(parse_since(v).map_err(|e| anyhow::anyhow!("invalid --since: {e}"))?),
        None => None,
    };

    let query = LogQuery {
        tail,
        since_us,
        raw,
    };

    let log_path = session.path().join("output.log");

    let written = if follow {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        follow_log(&log_path, &query, &mut out, || {
            session::read_meta(&session)
                .map(|m| m.status().is_terminal())
                .unwrap_or(false)
        })
    } else if !log_path.exists() {
        return Ok(());
    } else {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        query_log(&log_path, &query, &mut out).map(drop)
    };
    match written {
        // Only stdout can break the pipe: the reader exited early.
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => Ok(other?),
    }
}
