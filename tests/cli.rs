use std::process::Command;

#[test]
fn version_prints_exact_line() {
    let bin = env!("CARGO_BIN_EXE_ampwire");
    let out = Command::new(bin).arg("--version").output().unwrap();
    assert!(out.status.success(), "exit status: {:?}", out.status);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout, format!("ampwire {}\n", env!("CARGO_PKG_VERSION")));
    assert!(out.stderr.is_empty());
}

#[test]
fn help_contains_usage_and_exits_zero() {
    let bin = env!("CARGO_BIN_EXE_ampwire");
    let out = Command::new(bin).arg("--help").output().unwrap();
    assert!(out.status.success(), "exit status: {:?}", out.status);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("Usage:"), "stdout: {stdout}");
}

#[test]
fn short_flags_work() {
    let bin = env!("CARGO_BIN_EXE_ampwire");
    let v = Command::new(bin).arg("-V").output().unwrap();
    assert!(v.status.success());
    assert_eq!(
        String::from_utf8(v.stdout).unwrap(),
        format!("ampwire {}\n", env!("CARGO_PKG_VERSION"))
    );
    let h = Command::new(bin).arg("-h").output().unwrap();
    assert!(h.status.success());
    assert!(String::from_utf8(h.stdout).unwrap().contains("Usage:"));
}

#[test]
fn help_lists_status_and_control() {
    let bin = env!("CARGO_BIN_EXE_ampwire");
    let out = Command::new(bin).arg("--help").output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("--status"), "stdout: {stdout}");
    assert!(stdout.contains("--ctl"), "stdout: {stdout}");
}

#[test]
fn status_json_without_a_session_is_stopped() {
    let bin = env!("CARGO_BIN_EXE_ampwire");
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("control.sock");
    let out = Command::new(bin)
        .args(["--status", "--json"])
        .env("AMPWIRE_CONTROL_SOCKET", &socket)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "stderr: {:?}", out.stderr);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["running"], false);
    assert_eq!(json["state"], "stopped");
    assert_eq!(json["schema"], 1);
}

#[test]
fn ctl_without_a_session_fails_cleanly() {
    let bin = env!("CARGO_BIN_EXE_ampwire");
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("control.sock");
    let out = Command::new(bin)
        .args(["--ctl", "next"])
        .env("AMPWIRE_CONTROL_SOCKET", &socket)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("not running"), "stderr: {stderr}");
}

#[test]
fn ctl_requires_a_command() {
    let bin = env!("CARGO_BIN_EXE_ampwire");
    let out = Command::new(bin).arg("--ctl").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("needs a command"), "stderr: {stderr}");
}
