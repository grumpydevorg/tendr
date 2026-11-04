//! Test fixture for tests/windows_breakaway.rs.
//!
//! Opens a named Job Object passed via TEST_JOB_NAME, assigns itself to it,
//! then spawns `tendr start <session> -- powershell -NoProfile -Command "Start-Sleep 3600"`,
//! a child that never ends by itself within a test (cleanup kills it).
//! After tendr start returns, runs `tendr status <session>` to read the
//! sidecar PID and writes it to the file path given as the fourth argv.
//! When `tendr start` fails, exits with its code instead.
//!
//! Usage:
//! ```text
//! test_breakaway_parent <tendr_bin> <home_dir> <session> <sidecar_pid_out_path>
//! ```
//!
//! Env:
//!   TEST_JOB_NAME — name of the Job Object created by the parent test
//!   TEST_RUN_SCRIPT — when set, runs `tendr run <script>` in the foreground
//!     instead, and exits with its code (`session` and the out path unused)
#[cfg(windows)]
fn main() {
    use std::process::{Command, exit};
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, OpenJobObjectW};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    // From wincon.h / Win32::System::SystemServices (not in tendr's
    // windows-sys feature set). All we need is "may assign processes".
    const JOB_OBJECT_ASSIGN_PROCESS: u32 = 0x0001;

    let mut args = std::env::args().skip(1);
    let tendr_bin = args.next().unwrap_or_else(|| die("missing tendr_bin arg"));
    let home = args.next().unwrap_or_else(|| die("missing home arg"));
    let session = args.next().unwrap_or_else(|| die("missing session arg"));
    let sidecar_pid_out = args
        .next()
        .unwrap_or_else(|| die("missing sidecar_pid_out arg"));

    let job_name = std::env::var("TEST_JOB_NAME").unwrap_or_else(|_| die("TEST_JOB_NAME not set"));
    let job_name_w: Vec<u16> = job_name.encode_utf16().chain(std::iter::once(0)).collect();

    // SAFETY: name pointer is valid for the duration of the call.
    let job = unsafe { OpenJobObjectW(JOB_OBJECT_ASSIGN_PROCESS, 0, job_name_w.as_ptr()) };
    if job.is_null() {
        die(&format!(
            "OpenJobObjectW({job_name}) failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    // SAFETY: GetCurrentProcess returns a pseudo-handle, always valid.
    let proc = unsafe { GetCurrentProcess() };
    // SAFETY: job and proc are valid handles.
    let ret = unsafe { AssignProcessToJobObject(job, proc) };
    if ret == 0 {
        die(&format!(
            "AssignProcessToJobObject failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    if let Ok(script) = std::env::var("TEST_RUN_SCRIPT") {
        let status = Command::new(&tendr_bin)
            .env("HOME", &home)
            .args(["run", &script])
            .status()
            .unwrap_or_else(|e| die(&format!("tendr run spawn failed: {e}")));
        exit(status.code().unwrap_or(2));
    }

    // Spawn `tendr start` — inherits this process's job assignment.
    let status = Command::new(&tendr_bin)
        .env("HOME", &home)
        .args([
            "start",
            &session,
            "--",
            "powershell",
            "-NoProfile",
            "-Command",
            "Start-Sleep 3600",
        ])
        .status()
        .unwrap_or_else(|e| die(&format!("tendr start spawn failed: {e}")));
    if !status.success() {
        // tendr's own stderr has already passed through; its code is the
        // test's to check.
        exit(status.code().unwrap_or(2));
    }

    // Read sidecar PID from `tendr status <session>` (JSON output).
    let out = Command::new(&tendr_bin)
        .env("HOME", &home)
        .args(["status", &session])
        .output()
        .unwrap_or_else(|e| die(&format!("tendr status spawn failed: {e}")));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| die(&format!("status JSON parse failed: {e}\noutput:\n{stdout}")));
    let sidecar_pid = v["sidecar"]["pid"]
        .as_u64()
        .unwrap_or_else(|| die(&format!("sidecar.pid missing in status:\n{stdout}")));

    std::fs::write(&sidecar_pid_out, sidecar_pid.to_string())
        .unwrap_or_else(|e| die(&format!("write {sidecar_pid_out}: {e}")));

    exit(0);
}

#[cfg(windows)]
fn die(msg: &str) -> ! {
    eprintln!("test_breakaway_parent: {msg}");
    std::process::exit(2);
}

#[cfg(not(windows))]
fn main() {}
