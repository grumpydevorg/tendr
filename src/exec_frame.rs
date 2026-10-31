use base64::Engine;

/// Build a framed Python exec string for injection into a Python REPL.
///
/// Both the user's code AND the result path are base64-encoded to avoid any
/// escaping issues with backslashes, quotes, or special characters in paths
/// (especially Windows paths containing `\t`, `\U`, etc.).
///
/// The frame wraps execution in try/except, captures stdout/stderr/cwd/traceback,
/// and writes a JSON result file atomically (tmp + rename).
///
/// `result_path` is the absolute path to `{session_dir}/exec-results/{token}.json`.
pub fn python_frame(code: &str, result_path: &str) -> String {
    let encoded_code = base64::engine::general_purpose::STANDARD.encode(code);
    let encoded_path = base64::engine::general_purpose::STANDARD.encode(result_path);
    // The entire frame must be a single line for REPL injection.
    // Python's compile() interprets \n as real newlines inside the string.
    format!(
        "exec(compile('import json,os,sys,contextlib,io,traceback,base64 as _b64;_out,_err,_code,_tb=io.StringIO(),io.StringIO(),0,None;_rp=_b64.b64decode(\"{encoded_path}\").decode()\\ntry:\\n with contextlib.redirect_stdout(_out),contextlib.redirect_stderr(_err):\\n  exec(compile(_b64.b64decode(\"{encoded_code}\").decode(),\"<exec>\",\"exec\"))\\nexcept SystemExit as _e:\\n _code=_e.code if _e.code is not None else 0\\nexcept:\\n _tb=traceback.format_exc();_code=1\\n_tmp=_rp+\".tmp\"\\nwith open(_tmp,\"w\") as _f:\\n json.dump(dict(exit_code=_code,cwd=os.getcwd(),stdout=_out.getvalue(),stderr=_err.getvalue(),traceback=_tb),_f)\\nos.rename(_tmp,_rp)','<tendr-exec>','exec'))\n"
    )
}

/// Build a framed shell command string for Unix shells (bash/sh).
///
/// The command is escaped using shell_words::join, then appended with a sentinel
/// trailer that captures exit code and cwd.
///
/// `TENDR_BLOCK_ID` is exported for exactly the payload's duration
/// (spec §2): set before it, exit code captured first, unset before the
/// sentinel — so a payload spawning `tendr emit` chains to the exec
/// block, and the session shell is not left polluted.
///
/// Token must be hex-only (as produced by `generate_token`); block_id is
/// a UUID (hex + dashes).
/// Printed on stderr after the command and before the stdout sentinel. The
/// sidecar logs each pipe in order, so once this marker is in the log every
/// earlier stderr byte of the command is too (#92).
pub const EXEC_ERR_MARKER: &str = "__TENDR_EXEC_ERR__";

pub fn unix_frame(argv: &[String], token: &str, block_id: &str) -> String {
    debug_assert!(
        token.bytes().all(|b| b.is_ascii_hexdigit()),
        "token must be hex-only, got: {token}"
    );
    debug_assert!(
        block_id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'),
        "block_id must be a uuid, got: {block_id}"
    );
    let cmd = shell_words::join(argv);
    format!(
        "export TENDR_BLOCK_ID='{block_id}'; {cmd}; __tendr_s=$?; unset TENDR_BLOCK_ID; printf '{EXEC_ERR_MARKER} %s\\n' '{token}' >&2; printf '__TENDR_EXEC__ %s %s %s\\n' '{token}' \"$__tendr_s\" \"$(pwd)\"\n"
    )
}

/// Build a framed PowerShell exec string for injection into a PowerShell session.
///
/// Both the user's code AND the result path are base64-encoded to avoid any
/// escaping issues with backslashes, quotes, or special characters in paths
/// (especially Windows paths containing `\t`, `\U`, etc.) and in the user
/// payload (single quotes, here-strings, etc.).
///
/// The frame runs the decoded code via `[scriptblock]::Create($code)` (which
/// makes arbitrary expressions / pipelines / multi-statement snippets work),
/// partitions the merged success+error stream by object type to separate
/// stderr from stdout, and writes a JSON result file atomically (tmp + Move).
///
/// `result_path` is the absolute path to `{session_dir}/exec-results/{token}.json`.
///
/// The frame is one logical line terminated with **two** newlines. The blank
/// line is required: PowerShell's interactive REPL (both Windows PowerShell 5.1
/// and PS Core 7+) buffers complex multi-statement input on a sustained stdin
/// pipe and waits for a blank line to flush the parser before executing —
/// even when the syntax is already complete. With a single `\n` the line is
/// echoed but never runs. See PowerShell/PowerShell#3223 for the upstream
/// discussion of this pseudo-interactive behavior.
pub fn powershell_frame(code: &str, result_path: &str) -> String {
    let encoded_code = base64::engine::general_purpose::STANDARD.encode(code);
    let encoded_path = base64::engine::general_purpose::STANDARD.encode(result_path);
    format!(
        "$_b = [Convert]::FromBase64String('{encoded_code}'); $_code = [System.Text.Encoding]::UTF8.GetString($_b); $_rp = [System.Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded_path}')); $_tmp = \"$_rp.tmp\"; $_outBuf = New-Object System.Text.StringBuilder; $_errBuf = New-Object System.Text.StringBuilder; $_exit = 0; $LASTEXITCODE = $null; try {{ & ([scriptblock]::Create($_code)) 2>&1 | ForEach-Object {{ if ($_ -is [System.Management.Automation.ErrorRecord]) {{ [void]$_errBuf.AppendLine($_.ToString()) }} else {{ [void]$_outBuf.AppendLine(($_ | Out-String).TrimEnd()) }} }}; if ($null -ne $LASTEXITCODE -and $LASTEXITCODE -ne 0) {{ $_exit = $LASTEXITCODE }} elseif ($_errBuf.Length -gt 0) {{ $_exit = 1 }} }} catch {{ [void]$_errBuf.AppendLine($_.Exception.Message); $_exit = 1 }}; $_payload = @{{ exit_code = $_exit; cwd = (Get-Location).Path; stdout = $_outBuf.ToString(); stderr = $_errBuf.ToString() }} | ConvertTo-Json -Compress; [System.IO.File]::WriteAllText($_tmp, $_payload); [System.IO.File]::Move($_tmp, $_rp)\n\n"
    )
}

/// Build a framed SQL exec string for injection into a DuckDB session.
///
/// Results flow through stdout (`.mode json`) and are captured in the output
/// log, just like shell exec. No `.output` file redirection — that dot-command
/// has path-escaping fragility that breaks on Windows backslashes and paths
/// with spaces.
///
/// We deliberately do NOT use `.bail on` — it would exit the DuckDB process
/// on error, killing the session. Errors are detected from stderr lines in
/// the output log. The sentinel always fires with exit code 0; the caller
/// must check stderr to detect failures.
///
/// Token must be hex-only (as produced by `generate_token`).
pub fn duckdb_frame(sql: &str, token: &str) -> String {
    debug_assert!(
        token.bytes().all(|b| b.is_ascii_hexdigit()),
        "token must be hex-only, got: {token}"
    );
    format!(".mode json\n.nullvalue null\n{sql}\n.print __TENDR_EXEC__ {token} 0 .\n")
}

/// Parse a sentinel line into `(residual, exit code, cwd)`.
///
/// The sentinel may end a line rather than start one: a command whose output
/// lacks a trailing newline leaves its last bytes in front of it (#95). Those
/// bytes are returned as `residual`, which is the command's own output.
/// Returns None if the line holds no sentinel for `expected_token`.
pub fn parse_sentinel<'a>(line: &'a str, expected_token: &str) -> Option<(&'a str, i32, String)> {
    let needle = format!("__TENDR_EXEC__ {expected_token} ");
    let at = line.rfind(&needle)?;
    let (code_str, cwd) = line[at + needle.len()..].split_once(' ')?;
    let code: i32 = code_str.parse().ok()?;
    Some((&line[..at], code, cwd.to_owned()))
}

/// Parse the stderr end marker, returning the bytes in front of it: the
/// command's unterminated last stderr line, or `""`. Returns None if the line
/// does not end with the marker for `expected_token`.
pub fn parse_err_marker<'a>(line: &'a str, expected_token: &str) -> Option<&'a str> {
    line.strip_suffix(expected_token)?
        .strip_suffix(' ')?
        .strip_suffix(EXEC_ERR_MARKER)
}

/// Generate a unique token for sentinel matching.
pub fn generate_token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}{:x}", std::process::id(), nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: &str = "01981f32-5550-7abc-8def-111122223333";

    #[test]
    fn unix_frame_simple_command() {
        let frame = unix_frame(&["echo".into(), "hello".into()], "a1b2c3", BLOCK);
        assert!(frame.contains("echo hello"));
        assert!(frame.contains("__TENDR_EXEC__ %s %s %s"));
        assert!(frame.contains("a1b2c3"));
        assert!(frame.ends_with('\n'));
    }

    #[test]
    fn unix_frame_command_with_special_chars() {
        let frame = unix_frame(&["echo".into(), "it's a \"test\"".into()], "a1b2c3", BLOCK);
        assert!(frame.contains("__TENDR_EXEC__"));
        assert!(frame.contains("a1b2c3"));
    }

    #[test]
    fn unix_frame_exports_block_id_around_payload() {
        // Spec §2: set before the payload, capture the exit
        // code first, unset before the sentinel — the session shell is not
        // left polluted.
        let frame = unix_frame(&["echo".into(), "hi".into()], "a1b2c3", BLOCK);
        let export = frame
            .find(&format!("export TENDR_BLOCK_ID='{BLOCK}'"))
            .expect("export present");
        let cmd = frame.find("echo hi").expect("payload present");
        let status = frame.find("__tendr_s=$?").expect("exit capture present");
        let unset = frame.find("unset TENDR_BLOCK_ID").expect("unset present");
        let sentinel = frame.find("printf").expect("sentinel present");
        assert!(export < cmd, "export precedes payload");
        assert!(cmd < status, "exit captured after payload");
        assert!(status < unset, "unset after exit capture");
        assert!(unset < sentinel, "unset before the sentinel prints");
    }

    #[test]
    fn parse_sentinel_valid() {
        let result = parse_sentinel("__TENDR_EXEC__ a1b2c3 0 /home/user", "a1b2c3");
        let (residual, exit_code, cwd) = result.unwrap();
        assert_eq!(residual, "");
        assert_eq!(exit_code, 0);
        assert_eq!(cwd, "/home/user");
    }

    #[test]
    fn parse_sentinel_nonzero_exit() {
        let result = parse_sentinel("__TENDR_EXEC__ a1b2c3 42 /tmp", "a1b2c3");
        let (_, exit_code, cwd) = result.unwrap();
        assert_eq!(exit_code, 42);
        assert_eq!(cwd, "/tmp");
    }

    #[test]
    fn parse_sentinel_cwd_with_spaces() {
        let result = parse_sentinel("__TENDR_EXEC__ a1b2c3 0 /home/user/my project", "a1b2c3");
        let (_, _, cwd) = result.unwrap();
        assert_eq!(cwd, "/home/user/my project");
    }

    #[test]
    fn parse_sentinel_wrong_token() {
        let result = parse_sentinel("__TENDR_EXEC__ deadbeef 0 /home", "a1b2c3");
        assert!(result.is_none());
    }

    #[test]
    fn parse_sentinel_not_sentinel() {
        let result = parse_sentinel("hello world", "a1b2c3");
        assert!(result.is_none());
    }

    /// Output without a trailing newline leaves its last bytes in front of the
    /// sentinel on the same line (#95).
    #[test]
    fn parse_sentinel_keeps_a_glued_residual_as_output() {
        let (residual, code, cwd) =
            parse_sentinel("foo__TENDR_EXEC__ a1b2c3 0 /a b", "a1b2c3").unwrap();
        assert_eq!(residual, "foo");
        assert_eq!(code, 0);
        assert_eq!(cwd, "/a b");
    }

    #[test]
    fn parse_err_marker_exact() {
        assert_eq!(
            parse_err_marker("__TENDR_EXEC_ERR__ a1b2c3", "a1b2c3"),
            Some("")
        );
    }

    #[test]
    fn parse_err_marker_keeps_a_glued_residual() {
        assert_eq!(
            parse_err_marker("y__TENDR_EXEC_ERR__ a1b2c3", "a1b2c3"),
            Some("y")
        );
    }

    #[test]
    fn parse_err_marker_rejects_another_token_and_plain_lines() {
        assert_eq!(
            parse_err_marker("__TENDR_EXEC_ERR__ deadbeef", "a1b2c3"),
            None
        );
        assert_eq!(parse_err_marker("error: something", "a1b2c3"), None);
    }

    #[test]
    fn unix_frame_prints_the_stderr_marker_before_the_stdout_sentinel() {
        let frame = unix_frame(&["true".to_string()], "abc123", BLOCK);
        let err = frame
            .find("printf '__TENDR_EXEC_ERR__ %s\\n' 'abc123' >&2")
            .expect("stderr marker printed to fd 2");
        let out = frame.find("printf '__TENDR_EXEC__ %s %s %s").unwrap();
        assert!(err < out, "stderr marker must come first: {frame}");
    }

    #[test]
    fn powershell_side_channel_frame_encodes_code_and_path() {
        let frame = powershell_frame("$x = 1; $x + 1", "/tmp/result.json");
        assert!(frame.contains("FromBase64String"));
        // Both code and path are base64-encoded — neither appears raw
        assert!(!frame.contains("$x = 1; $x + 1"));
        assert!(!frame.contains("/tmp/result.json"));
        let encoded_code = base64::engine::general_purpose::STANDARD.encode("$x = 1; $x + 1");
        let encoded_path = base64::engine::general_purpose::STANDARD.encode("/tmp/result.json");
        assert!(frame.contains(&encoded_code));
        assert!(frame.contains(&encoded_path));
    }

    #[test]
    fn powershell_side_channel_frame_handles_special_chars() {
        // Code with quotes, here-strings, backticks, $variables, multiline —
        // all handled by base64.
        let code = "$x = 'hello'; @\"\nvalue: $x\n\"@; `n";
        let frame = powershell_frame(code, "/tmp/result.json");
        assert!(frame.contains("FromBase64String"));
        // Raw payload must not leak — these chars would break single-quoted PS strings.
        assert!(!frame.contains("@\""));
        assert!(!frame.contains("`n"));
    }

    #[test]
    fn powershell_side_channel_frame_handles_windows_paths() {
        // Windows path with backslashes and `\U` (which Python r-strings hate)
        // round-trips because we base64-encode the path.
        let frame = powershell_frame("$x + 1", r"C:\Users\rick\exec-results\abc.json");
        assert!(!frame.contains(r"C:\Users")); // path must not appear raw
        assert!(frame.contains("FromBase64String"));
        let encoded_path = base64::engine::general_purpose::STANDARD
            .encode(r"C:\Users\rick\exec-results\abc.json");
        assert!(frame.contains(&encoded_path));
    }

    #[test]
    fn powershell_frame_is_double_newline_terminated() {
        // The frame MUST end with `\n\n`. A single `\n` leaves the PS REPL
        // waiting for more input; the blank line forces it to flush the
        // parser and execute. See PowerShell/PowerShell#3223.
        let frame = powershell_frame("$x + 1", "/tmp/r.json");
        assert!(frame.ends_with("\n\n"), "frame must end with two newlines");
    }

    #[test]
    fn python_frame_encodes_code_and_path() {
        let frame = python_frame("print('hello')", "/tmp/result.json");
        assert!(frame.contains("b64decode"));
        assert!(frame.ends_with('\n'));
        // Both code and path are base64-encoded — neither appears raw
        assert!(!frame.contains("/tmp/result.json"));
        let encoded_code = base64::engine::general_purpose::STANDARD.encode("print('hello')");
        let encoded_path = base64::engine::general_purpose::STANDARD.encode("/tmp/result.json");
        assert!(frame.contains(&encoded_code));
        assert!(frame.contains(&encoded_path));
    }

    #[test]
    fn python_frame_handles_special_chars() {
        // Code with quotes, newlines, backslashes — all handled by base64
        let code = "x = 'hello'\nprint(f\"value: {x}\\n\")";
        let frame = python_frame(code, "/tmp/result.json");
        assert!(frame.contains("b64decode"));
        // Frame is a single line
        assert_eq!(frame.matches('\n').count(), 1); // trailing newline only
    }

    #[test]
    fn python_frame_escapes_windows_paths() {
        // Windows path with backslashes that would be problematic if not b64-encoded
        let frame = python_frame("print(1)", r"C:\Users\test\exec-results\abc.json");
        assert!(!frame.contains(r"C:\Users")); // path must not appear raw
        assert!(frame.contains("b64decode"));
    }

    #[test]
    fn duckdb_frame_basic_query() {
        let frame = duckdb_frame("SELECT 42 as answer;", "abc123");
        assert!(frame.starts_with(".mode json\n"));
        assert!(
            !frame.contains(".bail on"),
            "frame must not use .bail on — it kills the session"
        );
        assert!(
            !frame.contains(".output"),
            "frame must not use .output — path escaping is fragile"
        );
        assert!(frame.contains(".nullvalue null\n"));
        assert!(frame.contains("SELECT 42 as answer;\n"));
        assert!(frame.contains("__TENDR_EXEC__ abc123 0 .\n"));
        assert!(frame.ends_with('\n'));
    }

    #[test]
    fn duckdb_frame_multi_statement() {
        let sql = "SELECT 1;\nSELECT 2;";
        let frame = duckdb_frame(sql, "def456");
        assert!(frame.contains("SELECT 1;\nSELECT 2;\n"));
        assert!(frame.contains("__TENDR_EXEC__ def456 0 ."));
    }

    #[test]
    fn duckdb_sentinel_parses_with_dot_cwd() {
        let result = parse_sentinel("__TENDR_EXEC__ abc123 0 .", "abc123");
        let (residual, exit_code, cwd) = result.unwrap();
        assert_eq!(residual, "");
        assert_eq!(exit_code, 0);
        assert_eq!(cwd, ".");
    }

    #[test]
    fn generate_token_is_unique() {
        let t1 = generate_token();
        std::thread::sleep(std::time::Duration::from_millis(1));
        let t2 = generate_token();
        assert_ne!(t1, t2);
    }
}
