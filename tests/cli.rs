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

#[test]
fn doctor_is_read_only_non_tty_and_demo_options_are_explicit() {
    let output = binary().arg("--doctor").output().unwrap();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "read-only diagnostics",
        "TTY: stdin=no, stdout=no",
        "Live sample:",
        "Detected agent sessions:",
    ] {
        assert!(text.contains(expected), "{text}");
    }
    assert!(!text.contains('\x1b'));
    for args in [
        ["--doctor", "--demo"],
        ["--doctor", "--tour"],
        ["--doctor", "--all"],
    ] {
        assert!(!binary().args(args).output().unwrap().status.success());
    }
    assert!(!binary().arg("--tour").output().unwrap().status.success());
}

#[test]
fn quickstart_exposes_local_launch_tour_live_and_diagnostics() {
    let output = binary().arg("--help").output().unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "./lattice --tour",
        "./lattice --all",
        "./lattice --doctor",
        "--no-motion",
        "cargo install --path . --locked",
    ] {
        assert!(text.contains(expected), "{text}");
    }
}

#[test]
fn pid_attachment_is_positive_explicit_live_and_not_a_launcher() {
    for args in [
        vec!["--pid", "0"],
        vec!["--pid", "-1"],
        vec!["--pid", "4294967296"],
        vec!["--pid", "123", "--demo"],
        vec!["--pid", "123", "--tour"],
        vec!["--pid", "123", "--doctor"],
        vec!["--pid", "123", "--all"],
        vec!["--pid", "123", "--", "echo"],
        vec!["--changes"],
    ] {
        let output = binary().args(&args).output().unwrap();
        assert!(!output.status.success(), "{args:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("error:"));
    }
    let help = binary().arg("--help").output().unwrap();
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("./lattice --pid 12345"));
    assert!(help.contains("never launches"));
    assert!(help.contains("r nickname"));
    assert!(help.contains("e changes"));
}

#[test]
fn changes_demo_export_contains_observed_events_without_live_access() {
    let dir = std::env::temp_dir().join(format!("lattice-changes-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("changes.svg");
    let output = binary()
        .args(["--demo", "--changes", "--snapshot"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success());
    let svg = fs::read_to_string(&path).unwrap();
    let rendered: String = svg
        .split("</text>")
        .filter_map(|part| part.rsplit_once('>').map(|(_, text)| text))
        .collect();
    for text in [
        "WHAT CHANGED",
        "Observed, not causes",
        "vite",
        "not seen",
        "no disk log",
    ] {
        assert!(rendered.contains(&text.replace(' ', "")), "missing {text}");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn snapshot_screen_and_graph_selectors_are_explicit_and_validated() {
    for args in [
        vec!["--view", "map"],
        vec!["--graph", "heatmap"],
        vec!["--metric", "cpu"],
        vec!["--demo", "--snapshot", "unused.svg", "--view", "unknown"],
        vec!["--demo", "--snapshot", "unused.svg", "--graph", "tree"],
        vec!["--demo", "--snapshot", "unused.svg", "--metric", "disk"],
        vec![
            "--demo",
            "--snapshot",
            "unused.svg",
            "--view",
            "map",
            "--inspect",
        ],
        vec![
            "--demo",
            "--snapshot",
            "unused.svg",
            "--view",
            "changes",
            "--changes",
        ],
    ] {
        let output = binary().args(&args).output().unwrap();
        assert!(!output.status.success(), "unexpected success for {args:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("error:"));
    }
    let output = binary().arg("--help").output().unwrap();
    let help = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "--view",
        "--graph",
        "--metric",
        "heatmap",
        "Disable screen transitions",
    ] {
        assert!(help.contains(expected), "missing {expected} in {help}");
    }
    let version = binary().arg("--version").output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        "lattice 0.3.0"
    );
}

#[test]
fn all_snapshot_screens_are_settled_repeatable_and_distinct() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("lattice-views-{}-{unique}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let mut outputs = Vec::new();
    for (index, options) in [
        vec!["--view", "overview"],
        vec!["--view", "map"],
        vec!["--view", "history"],
        vec!["--view", "changes"],
        vec!["--inspect"],
        vec!["--view", "map", "--graph", "heatmap", "--metric", "cpu"],
        vec!["--view", "history", "--metric", "cpu"],
    ]
    .into_iter()
    .enumerate()
    {
        let path = dir.join(format!("screen-{index}.svg"));
        let mut run_outputs = Vec::new();
        for _ in 0..2 {
            let output = binary()
                .args(["--demo", "--snapshot"])
                .arg(&path)
                .args(&options)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{options:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            run_outputs.push(fs::read_to_string(&path).unwrap());
        }
        assert_eq!(
            run_outputs[0], run_outputs[1],
            "snapshot not settled: {options:?}"
        );
        outputs.push(run_outputs.pop().unwrap());
    }
    for left in 0..outputs.len() {
        for right in left + 1..outputs.len() {
            assert_ne!(
                outputs[left], outputs[right],
                "screens {left} and {right} should differ"
            );
        }
    }
    let alias = dir.join("changes-alias.svg");
    let result = binary()
        .args(["--demo", "--changes", "--snapshot"])
        .arg(&alias)
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(fs::read_to_string(alias).unwrap(), outputs[3]);
    fs::remove_dir_all(dir).unwrap();
}
