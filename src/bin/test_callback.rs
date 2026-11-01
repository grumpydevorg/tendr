//! Test fixture: portable helpers for integration tests.
//!
//! Subcommands:
//! ```text
//! touch <path>      — create an empty file at <path>
//! echo-env <path>   — write "$TENDR_SESSION $TENDR_NAMESPACE $TENDR_EXIT_REASON" to <path>
//! copy-stdin <path> — copy stdin to <path> as it arrives, until end of input
//! print-cwd         — print the current working directory to stdout
//! ```

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: test_callback <touch|echo-env|copy-stdin|print-cwd> [<path>]");
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
        other => {
            eprintln!("test_callback: unknown command '{other}'");
            std::process::exit(1);
        }
    }
}
