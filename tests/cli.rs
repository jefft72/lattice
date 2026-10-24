use std::{
    fs,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lattice"))
}

#[test]
fn help_and_version_work_without_a_terminal() {
    let output = binary().arg("--help").output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("--demo") && help.contains("No crash prevention"));
    assert!(binary().arg("--version").output().unwrap().status.success());
}

#[test]
fn non_interactive_run_fails_helpfully() {
    let output = binary().arg("--demo").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("interactive terminal"));
}

#[test]
fn demo_export_is_repeatable_and_does_not_require_a_terminal() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("lattice-test-{}-{unique}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    let first = dir.join("first.svg");
    let second = dir.join("second.svg");
    for path in [&first, &second] {
        let output = binary()
            .args(["--demo", "--snapshot"])
            .arg(path)
            .args(["--width", "80", "--height", "24"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let one = fs::read_to_string(first).unwrap();
    let two = fs::read_to_string(second).unwrap();
    assert_eq!(one, two);
    assert!(one.starts_with("<svg") && one.ends_with("</g></svg>"));
    fs::remove_dir_all(dir).unwrap();
}
