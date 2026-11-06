mod harness;
use harness::{Home, tendr, wait_running, wait_terminal};

#[cfg(unix)]
#[test]
fn clean_passes_only_selected_values_without_persisting_them() {
    let home = Home::new();
    let result = home.path().join("observed");
    let out = tendr(&home).env("TENDR_ENV_SELECTED", "private-selected-value")
        .env("TENDR_ENV_OMITTED", "private-omitted-value")
        .args(["start", "clean", "--env-mode", "clean", "--pass-env", "TENDR_ENV_SELECTED", "--", "sh", "-c", "printf '%s|%s|%s' \"$TENDR_ENV_SELECTED\" \"${TENDR_ENV_OMITTED-unset}\" \"$TENDR_ENV_EXPLICIT\" > \"$1\"", "sh"])
        .arg(&result).env("TENDR_ENV_EXPLICIT", "not-passed").output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_terminal(&home, "clean");
    assert_eq!(
        std::fs::read_to_string(result).unwrap(),
        "private-selected-value|unset|"
    );
    let dir = home.path().join(".tendr/sessions/default/clean");
    for name in ["meta.json", "output.log"] {
        let data = std::fs::read_to_string(dir.join(name)).unwrap();
        assert!(
            !data.contains("private-selected-value"),
            "leaked into {name}"
        );
    }
    let status = tendr(&home).args(["status", "clean"]).output().unwrap();
    assert!(!String::from_utf8_lossy(&status.stdout).contains("private-selected-value"));
    assert!(
        !serde_json::to_string(&harness::read_events(&home, "clean"))
            .unwrap()
            .contains("private-selected-value")
    );
}

#[cfg(unix)]
#[test]
fn inherit_and_idempotency_preserve_original_environment() {
    let home = Home::new();
    let result = home.path().join("observed");
    let script = "printf '%s' \"$TENDR_ENV_SELECTED\" > \"$1\"; sleep 3600";
    let launch = |value: &str, mode: &str, extra: &[&str]| {
        let mut cmd = tendr(&home);
        cmd.env("TENDR_ENV_SELECTED", value)
            .args(["start", "held", "--env-mode", mode])
            .args(extra)
            .args(["--", "sh", "-c", script, "sh"])
            .arg(&result);
        cmd.output().unwrap()
    };
    let first = launch("first", "clean", &["--pass-env", "TENDR_ENV_SELECTED"]);
    assert!(first.status.success());
    wait_running(&home, "held");
    let second = launch(
        "second",
        "clean",
        &[
            "--pass-env",
            "TENDR_ENV_SELECTED",
            "--pass-env",
            "TENDR_ENV_SELECTED",
        ],
    );
    assert!(second.status.success());
    let a: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    let b: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(a["run_id"], b["run_id"]);
    harness::poll_until(harness::HANG, std::time::Duration::from_millis(10), || {
        if std::fs::read_to_string(&result).is_ok_and(|s| !s.is_empty()) {
            harness::Observation::Ready(())
        } else {
            harness::Observation::Pending("worker output".into())
        }
    })
    .unwrap();
    assert_eq!(std::fs::read_to_string(&result).unwrap(), "first");
    assert!(!launch("second", "inherit", &[]).status.success());
    assert!(!launch("second", "clean", &[]).status.success());
}

#[cfg(unix)]
#[test]
fn missing_pass_env_cannot_replace_running_session() {
    let home = Home::new();
    let first = tendr(&home)
        .args(["start", "held", "sleep", "3600"])
        .output()
        .unwrap();
    assert!(first.status.success());
    wait_running(&home, "held");
    let failed = tendr(&home)
        .env_remove("TENDR_ENV_MISSING")
        .args([
            "start",
            "held",
            "--replace",
            "--env-mode",
            "clean",
            "--pass-env",
            "TENDR_ENV_MISSING",
            "--",
            "sleep",
            "3600",
        ])
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(2));
    let status = tendr(&home).args(["status", "held"]).output().unwrap();
    let before: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    let after: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(before["run_id"], after["run_id"]);
}

#[cfg(unix)]
#[test]
fn inherit_keeps_unspecified_values() {
    let home = Home::new();
    let result = home.path().join("observed");
    let output = tendr(&home)
        .env("TENDR_ENV_MARKER", "inherited")
        .args([
            "start",
            "inherit",
            "--",
            "sh",
            "-c",
            "printf %s \"$TENDR_ENV_MARKER\" > \"$1\"",
            "sh",
        ])
        .arg(&result)
        .output()
        .unwrap();
    assert!(output.status.success());
    wait_terminal(&home, "inherit");
    assert_eq!(std::fs::read_to_string(result).unwrap(), "inherited");
}

#[cfg(unix)]
#[test]
fn clean_worker_overrides_and_callback_inherits_sidecar_environment() {
    let home = Home::new();
    let worker = home.path().join("worker");
    let callback = home.path().join("callback");
    let callback_script = format!(
        "printf '%s|%s' \"$TENDR_ENV_SELECTED\" \"${{TENDR_ENV_OMITTED-unset}}\" > {}",
        shell_words::quote(callback.to_str().unwrap())
    );
    let callback_cmd = format!("sh -c {}", shell_words::quote(&callback_script));
    let output = tendr(&home)
        .env("TENDR_ENV_SELECTED", "passed")
        .env("TENDR_ENV_OMITTED", "discarded")
        .args([
            "start",
            "callback",
            "--env-mode",
            "clean",
            "--pass-env",
            "TENDR_ENV_SELECTED",
            "--unset-env",
            "TENDR_ENV_SELECTED",
            "--env",
            "TENDR_ENV_EXPLICIT=explicit",
            "--on-exit",
            &callback_cmd,
            "--",
            "sh",
            "-c",
            "printf '%s|%s' \"${TENDR_ENV_SELECTED-unset}\" \"$TENDR_ENV_EXPLICIT\" > \"$1\"",
            "sh",
        ])
        .arg(&worker)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    wait_terminal(&home, "callback");
    harness::poll_until(harness::HANG, std::time::Duration::from_millis(10), || {
        if std::fs::read_to_string(&callback).is_ok_and(|s| s == "passed|unset") {
            harness::Observation::Ready(())
        } else {
            harness::Observation::Pending("callback output".into())
        }
    })
    .unwrap();
    assert_eq!(std::fs::read_to_string(worker).unwrap(), "unset|explicit");
    assert_eq!(std::fs::read_to_string(callback).unwrap(), "passed|unset");
}

#[cfg(unix)]
#[test]
fn invalid_pass_env_is_usage_error_before_creation() {
    let home = Home::new();
    for options in [
        vec!["--pass-env", "HOME"],
        vec!["--env-mode", "clean", "--pass-env", "BAD=NAME"],
        vec!["--env-mode", "clean", "--pass-env", ""],
    ] {
        let output = tendr(&home)
            .args(["start", "invalid"])
            .args(options)
            .args(["--", "true"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(2),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!home.path().join(".tendr/sessions/default/invalid").exists());
    }
}

#[cfg(windows)]
const PYTHON: &str = "python";
#[cfg(not(windows))]
const PYTHON: &str = "python3";

#[test]
fn portable_clean_environment_reaches_worker_but_not_records() {
    let home = Home::new();
    let secret = "nonpersisted-secret-3127";
    let output = tendr(&home)
        .env("TENDR_TEST_SELECTED_VALUE", secret)
        .env("TENDR_TEST_OMITTED_VALUE", "not-in-clean")
        .args([
            "start",
            "portable-clean",
            "--env-mode",
            "clean",
            "--pass-env",
            "TENDR_TEST_SELECTED_VALUE",
            "--",
            &harness::test_callback_bin(),
            "env-probe",
            "TENDR_TEST_SELECTED_VALUE",
            "TENDR_TEST_OMITTED_VALUE",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    wait_terminal(&home, "portable-clean");
    let log = tendr(&home)
        .args(["log", "portable-clean", "--raw"])
        .output()
        .unwrap();
    assert!(log.status.success());
    let probe: serde_json::Value = serde_json::from_slice(&log.stdout).unwrap();
    assert_eq!(probe["selected_present"], true);
    assert_eq!(probe["selected_len"], secret.len());
    assert_eq!(
        probe["selected_sha256"],
        format!(
            "{:x}",
            <sha2::Sha256 as sha2::Digest>::digest(secret.as_bytes())
        )
    );
    assert_eq!(probe["omitted_present"], false);
    let status = tendr(&home)
        .args(["status", "portable-clean"])
        .output()
        .unwrap();
    assert!(status.status.success());
    for bytes in [
        &output.stdout,
        &output.stderr,
        &status.stdout,
        &status.stderr,
        &log.stdout,
        &log.stderr,
    ] {
        assert!(!String::from_utf8_lossy(bytes).contains(secret));
    }
    let dir = home.path().join(".tendr/sessions/default/portable-clean");
    for file in ["meta.json", "output.log"] {
        assert!(
            !std::fs::read_to_string(dir.join(file))
                .unwrap()
                .contains(secret)
        );
    }
    assert!(
        !serde_json::to_string(&harness::read_events(&home, "portable-clean"))
            .unwrap()
            .contains(secret)
    );
}

#[test]
fn portable_missing_pass_env_does_not_replace_running_session() {
    let home = Home::new();
    let first = tendr(&home)
        .args([
            "start",
            "portable-held",
            "--",
            PYTHON,
            "-c",
            "import time; time.sleep(3600)",
        ])
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    wait_running(&home, "portable-held");
    let failed = tendr(&home)
        .env_remove("TENDR_TEST_ABSENT_VALUE")
        .args([
            "start",
            "portable-held",
            "--replace",
            "--env-mode",
            "clean",
            "--pass-env",
            "TENDR_TEST_ABSENT_VALUE",
            "--",
            PYTHON,
            "-c",
            "pass",
        ])
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(2));
    let status = tendr(&home)
        .args(["status", "portable-held"])
        .output()
        .unwrap();
    assert!(status.status.success());
    let before: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    let after: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(before["run_id"], after["run_id"]);
}

#[cfg(windows)]
#[test]
fn windows_pass_names_are_case_insensitive_at_launch() {
    let home = Home::new();
    let secret = "case-sensitive-regression-417";
    let output = tendr(&home)
        .env("TENDR_TEST_MIXED_CASE", secret)
        .args([
            "start",
            "case-test",
            "--env-mode",
            "clean",
            "--pass-env",
            "tendr_test_mixed_case",
            "--pass-env",
            "TENDR_TEST_MIXED_CASE",
            "--",
            &harness::test_callback_bin(),
            "env-probe",
            "TeNdR_TeSt_MiXeD_CaSe",
            "TENDR_TEST_ABSENT_CASE",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    wait_terminal(&home, "case-test");
    let log = tendr(&home)
        .args(["log", "case-test", "--raw"])
        .output()
        .unwrap();
    let probe: serde_json::Value = serde_json::from_slice(&log.stdout).unwrap();
    assert_eq!(probe["selected_present"], true);
    assert_eq!(probe["selected_len"], secret.len());
    assert_eq!(
        probe["selected_sha256"],
        format!(
            "{:x}",
            <sha2::Sha256 as sha2::Digest>::digest(secret.as_bytes())
        )
    );
    assert_eq!(probe["omitted_present"], false);
    let metadata = std::fs::read_to_string(
        home.path()
            .join(".tendr/sessions/default/case-test/meta.json"),
    )
    .unwrap();
    let metadata: serde_json::Value = serde_json::from_str(&metadata).unwrap();
    let spec = &metadata["launch_spec"];
    assert_eq!(
        spec["pass_env"],
        serde_json::json!(["TENDR_TEST_MIXED_CASE"])
    );
    assert!(!spec.to_string().contains(secret));
}

#[cfg(windows)]
#[test]
fn windows_pass_names_use_ordinal_casing_without_unicode_expansion() {
    let home = Home::new();
    let secret = "nonpersisted-unicode-value-é";
    let output = tendr(&home)
        .env("TENDR_ß", secret)
        .env("TENDR_SS", "different-variable")
        .env("TENDR_é", "case-alias")
        .args([
            "start",
            "ordinal-test",
            "--env-mode",
            "clean",
            "--pass-env",
            "TENDR_ß",
            "--pass-env",
            "TENDR_é",
            "--pass-env",
            "TENDR_É",
            "--",
            &harness::test_callback_bin(),
            "env-probe",
            "TENDR_ß",
            "TENDR_SS",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    wait_terminal(&home, "ordinal-test");
    let log = tendr(&home)
        .args(["log", "ordinal-test", "--raw"])
        .output()
        .unwrap();
    assert!(log.status.success());
    let probe: serde_json::Value = serde_json::from_slice(&log.stdout).unwrap();
    assert_eq!(probe["selected_present"], true);
    assert_eq!(probe["selected_len"], secret.len());
    assert_eq!(
        probe["selected_sha256"],
        format!(
            "{:x}",
            <sha2::Sha256 as sha2::Digest>::digest(secret.as_bytes())
        )
    );
    assert_eq!(probe["omitted_present"], false);
    let metadata = std::fs::read_to_string(
        home.path()
            .join(".tendr/sessions/default/ordinal-test/meta.json"),
    )
    .unwrap();
    let metadata: serde_json::Value = serde_json::from_str(&metadata).unwrap();
    assert_eq!(
        metadata["launch_spec"]["pass_env"],
        serde_json::json!(["TENDR_É", "TENDR_ß"])
    );
    assert!(!metadata.to_string().contains(secret));
}
