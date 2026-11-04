//! Test fixture: portable helpers for integration tests.
//!
//! Subcommands:
//! ```text
//! touch <path>      — create an empty file at <path>
//! echo-env <path>   — write "$TENDR_SESSION $TENDR_NAMESPACE $TENDR_EXIT_REASON" to <path>
//! copy-stdin <path> — copy stdin to <path> as it arrives, until end of input
//! print-cwd         — print the current working directory to stdout
//! ndjson-echo       — a line-oriented JSON process (see `ndjson_echo`)
//! hook-agent <tendr> — a scripted agent whose hooks emit events (see `hook_agent`)
//! ```

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!(
            "usage: test_callback <touch|echo-env|copy-stdin|print-cwd|ndjson-echo> [<path>]"
        );
        std::process::exit(1);
    }

    let cmd = &args[1];

    match cmd.as_str() {
        "touch" | "echo-env" | "copy-stdin" => {
            if args.len() < 3 {
                eprintln!("usage: test_callback {cmd} <path>");
                std::process::exit(1);
            }
            let path = &args[2];
            match cmd.as_str() {
                "touch" => {
                    std::fs::write(path, "").unwrap_or_else(|e| {
                        eprintln!("test_callback touch: {e}");
                        std::process::exit(1);
                    });
                }
                "echo-env" => {
                    let session = std::env::var("TENDR_SESSION").unwrap_or_default();
                    let namespace = std::env::var("TENDR_NAMESPACE").unwrap_or_default();
                    let exit_reason = std::env::var("TENDR_EXIT_REASON").unwrap_or_default();
                    let content = format!("{session} {namespace} {exit_reason}\n");
                    std::fs::write(path, content).unwrap_or_else(|e| {
                        eprintln!("test_callback echo-env: {e}");
                        std::process::exit(1);
                    });
                }
                "copy-stdin" => {
                    // An unbuffered File: each chunk is on disk as soon as
                    // it is read, so a test can watch the bytes arrive.
                    let copied = std::fs::File::create(path).and_then(|mut file| {
                        std::io::copy(&mut std::io::stdin().lock(), &mut file)
                    });
                    if let Err(e) = copied {
                        eprintln!("test_callback copy-stdin: {e}");
                        std::process::exit(1);
                    }
                }
                _ => unreachable!(),
            }
        }
        "print-cwd" => {
            let cwd = std::env::current_dir().unwrap_or_else(|e| {
                eprintln!("test_callback print-cwd: {e}");
                std::process::exit(1);
            });
            println!("{}", cwd.display());
        }
        "ndjson-echo" => ndjson_echo(),
        "hook-agent" => match args.get(2) {
            Some(tendr) => hook_agent(tendr),
            None => {
                eprintln!("usage: test_callback hook-agent <tendr>");
                std::process::exit(1);
            }
        },
        other => {
            eprintln!("test_callback: unknown command '{other}'");
            std::process::exit(1);
        }
    }
}

/// A stand-in for an interactive agent with lifecycle hooks: each stdin line
/// `<hook> <json>` fires that hook as the guide's recipe does, running
/// `<tendr> emit --best-effort --source claude.hook --kind hook.<hook>
/// --data-stdin` with `<json>` on its stdin. Like a real hook it is the
/// agent's child, so the event finds its session through the inherited
/// `TENDR_*` variables. Hooks fire in line order; end of input ends it.
fn hook_agent(tendr: &str) {
    use std::io::{BufRead, Write};
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { return };
        let Some((hook, data)) = line.split_once(' ') else {
            continue;
        };
        let mut emit = std::process::Command::new(tendr)
            .args(["emit", "--best-effort", "--source", "claude.hook", "--kind"])
            .arg(format!("hook.{hook}"))
            .arg("--data-stdin")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| {
                eprintln!("test_callback hook-agent: {e}");
                std::process::exit(1);
            });
        let mut stdin = emit.stdin.take().expect("piped stdin");
        stdin.write_all(data.as_bytes()).unwrap();
        drop(stdin);
        emit.wait().unwrap();
    }
}

/// Read one JSON object per stdin line and answer each in order with one
/// line `{"id": <request id>, "ok": <request ok, default true>, "cmd": <request
/// cmd>}`, plus the request's `error` member if it has one. Before answering,
/// a request's members act: `stderr` (string) is written to stderr, `push`
/// (string) is written to stdout as a line of its own, `sleep_ms` (number)
/// delays, `wait_path` (string) blocks until that path exists (no deadline:
/// the test creates it), `hold_for_next` (true) reads the next request line
/// first, answering this one only then and handling that one after (ending
/// at end of input), and `exit` (number) exits with that code without
/// answering.
fn ndjson_echo() {
    use std::io::{BufRead, Write};
    let mut out = std::io::stdout().lock();
    let mut lines = std::io::stdin().lock().lines();
    let mut held = None;
    loop {
        let line = match held.take() {
            Some(line) => line,
            None => match lines.next() {
                Some(Ok(line)) => line,
                _ => return,
            },
        };
        let request: serde_json::Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(e) => {
                let reply = serde_json::json!({"ok": false, "error": e.to_string()});
                writeln!(out, "{reply}").unwrap();
                out.flush().unwrap();
                continue;
            }
        };
        if let Some(text) = request["stderr"].as_str() {
            eprintln!("{text}");
        }
        if let Some(text) = request["push"].as_str() {
            writeln!(out, "{text}").unwrap();
            out.flush().unwrap();
        }
        if let Some(ms) = request["sleep_ms"].as_u64() {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        if let Some(path) = request["wait_path"].as_str() {
            while !std::path::Path::new(path).exists() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        if request["hold_for_next"].as_bool() == Some(true) {
            match lines.next() {
                Some(Ok(next)) => held = Some(next),
                _ => return,
            }
        }
        if let Some(code) = request["exit"].as_i64() {
            std::process::exit(code as i32);
        }
        let mut reply = serde_json::json!({
            "id": request["id"],
            "ok": request.get("ok").cloned().unwrap_or(serde_json::Value::Bool(true)),
            "cmd": request["cmd"],
        });
        if let Some(error) = request.get("error") {
            reply["error"] = error.clone();
        }
        writeln!(out, "{reply}").unwrap();
        out.flush().unwrap();
    }
}
