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
