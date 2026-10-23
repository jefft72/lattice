//! Non-TTY recording commands remain separate from opt-in process monitoring.
use std::{
    fs,
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lattice"))
}
fn scratch(label: &str) -> PathBuf {
    let id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "lattice-recording-{label}-{}-{id}",
        std::process::id()
    ));
    fs::create_dir(&path).unwrap();
    path
}

#[test]
fn recording_help_explains_explicit_start_and_review() {
    let output = binary().arg("--help").output().unwrap();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "--record",
        "--record-dir",
        "--record-every",
        "recordings",
        "CalHacks",
        "local files",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }
    let output = binary().args(["recordings", "--help"]).output().unwrap();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    for expected in ["list", "show", "export"] {
        assert!(text.contains(expected), "{text}");
    }
}

#[test]
fn incompatible_recording_modes_and_bad_intervals_are_rejected() {
    for args in [
        vec!["--record", "CalHacks", "--doctor"],
        vec!["--record", "CalHacks", "--tour"],
        vec!["--record", "CalHacks", "--demo", "--snapshot", "never.svg"],
        vec!["--record-every", "0"],
        vec!["--record-every", "61"],
        vec!["--record-every", "NaN"],
        vec!["--record", "CalHacks", "recordings", "list"],
        vec!["--demo", "recordings", "list"],
    ] {
        let output = binary().args(&args).output().unwrap();
        assert!(!output.status.success(), "unexpected success for {args:?}");
    }
}

#[test]
fn non_terminal_recording_does_not_create_storage() {
    let root = scratch("non-tty");
    let storage = root.join("not-created");
    let output = binary()
        .args(["--demo", "--record", "CalHacks", "--record-dir"])
        .arg(&storage)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("interactive terminal"));
    assert!(
        !storage.exists(),
        "failed TTY check must not create a recording"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn listing_is_read_only_and_review_errors_do_not_require_a_terminal() {
    let root = scratch("read-only");
    let missing = root.join("not-created");
    let output = binary()
        .args(["recordings", "list", "--record-dir"])
        .arg(&missing)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!missing.exists(), "listing must not create storage");
    let output = binary()
        .args(["recordings", "show"])
        .arg(root.join("missing.lattice"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("interactive terminal"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn malformed_recording_never_overwrites_export_destination() {
    let root = scratch("no-overwrite");
    let source = root.join("invalid.lattice");
    let destination = root.join("keep.csv");
    fs::write(&source, b"not a recording\n").unwrap();
    fs::write(&destination, b"existing report\n").unwrap();
    let output = binary()
        .args(["recordings", "export"])
        .arg(&source)
        .arg("--output")
        .arg(&destination)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(fs::read(&destination).unwrap(), b"existing report\n");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn csv_keeps_its_schema_and_neutralizes_formula_names() {
    let root = scratch("csv-formula");
    let source = root.join("fixture.lattice");
    let destination = root.join("report.csv");
    fs::write(&source, concat!(
        "LATTICE_RECORDING\t1\t-CalHacks\tSIMULATED\t1700000000.000\t5.000\n",
        "S\t1\t1700000000.000\t0.000\t0\t20.000\t1000\t500\t0\t100\t1\t0\t0\t0\t42,1699999999,+agent,100,50.000\n",
        "F\t1.000\tapp_exit\n"
    )).unwrap();
    let output = binary()
        .args(["recordings", "export"])
        .arg(&source)
        .arg("--output")
        .arg(&destination)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let csv = fs::read_to_string(&destination).unwrap();
    let lines: Vec<_> = csv.lines().collect();
    assert_eq!(lines.len(), 4, "{csv}");
    for line in &lines {
        assert_eq!(line.split(',').count(), 23, "{line}");
    }
    assert!(csv.contains("\"'-CalHacks\""), "{csv}");
    assert!(csv.contains("\"'+agent\""), "{csv}");
    assert!(csv.contains("status-stopped"), "{csv}");
    fs::remove_dir_all(root).unwrap();
}
