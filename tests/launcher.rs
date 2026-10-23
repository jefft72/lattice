//! The launcher is exercised only against fake tools; no Rust install is changed.
#![cfg(unix)]

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);
// A concurrent spawn can inherit another test's briefly writable executable
// descriptor before exec closes it, causing ETXTBSY even after fs::write returns.
// Serialize these tiny fixtures through cleanup, preserving direct execution
// coverage without sleeps, retries, or changes to the production launcher.
static FIXTURE_LOCK: Mutex<()> = Mutex::new(());

struct Fixture {
    _isolation: MutexGuard<'static, ()>,
    root: PathBuf,
    project: PathBuf,
    bin: PathBuf,
    home: PathBuf,
    cargo_home: PathBuf,
    rustup_home: PathBuf,
    capture: PathBuf,
}

impl Fixture {
    fn new(cargo: Option<&str>, rustc: Option<&str>) -> Self {
        let isolation = FIXTURE_LOCK
            .lock()
            .expect("fixture synchronization poisoned");
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "lattice launcher test {} {unique} {}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let project = root.join("checkout with spaces");
        let bin = root.join("fake tools");
        let home = root.join("home");
        let cargo_home = home.join(".cargo");
        let rustup_home = home.join(".rustup");
        for directory in [&project, &bin, &cargo_home, &rustup_home] {
            fs::create_dir_all(directory).unwrap();
        }
        fs::write(project.join("Cargo.toml"), "[package]\nname='fixture'\n").unwrap();
        fs::write(project.join("Cargo.lock"), "# fixture\n").unwrap();
        executable(&project.join("lattice"), include_str!("../lattice"));
        for file in [".profile", ".bashrc", ".zshrc"] {
            fs::write(home.join(file), format!("# preserve {file}\n")).unwrap();
        }
        fs::write(
            cargo_home.join("config.toml"),
            "# preserve Cargo configuration\n",
        )
        .unwrap();
        fs::write(
            rustup_home.join("settings.toml"),
            "# preserve toolchain selection\n",
        )
        .unwrap();
        if let Some(version) = cargo {
            executable(
                &bin.join("cargo"),
                &format!(
                    r#"#!/bin/sh
if [ "${{RUSTUP_AUTO_INSTALL:-}}" != '0' ]; then
    printf '%s\n' forbidden > "$HOME/forbidden-action"
    exit 97
fi
if [ "$1" = '--version' ]; then
    printf '%s\n' '{version}'
    exit 0
fi
{{
    printf '%s\n' "$PWD" "$PATH" "$HOME" "$CARGO_HOME" "$RUSTUP_HOME" "$#"
    printf '%s\n' "$@"
}} > "$TEST_CAPTURE"
exit "${{TEST_CARGO_EXIT:-0}}"
"#
                ),
            );
        }
        if let Some(version) = rustc {
            executable(
                &bin.join("rustc"),
                &format!(
                    "#!/bin/sh\n[ \"$RUSTUP_AUTO_INSTALL\" = '0' ] || exit 97\nprintf '%s\\n' '{version}'\n"
                ),
            );
        }
        // Any attempted automatic installer/update would leave detectable evidence.
        for forbidden in ["rustup", "curl", "wget", "brew", "apt", "sudo"] {
            executable(
                &bin.join(forbidden),
                "#!/bin/sh\nprintf '%s\\n' forbidden > \"$HOME/forbidden-action\"\nexit 98\n",
            );
        }
        Self {
            _isolation: isolation,
            capture: root.join("capture"),
            root,
            project,
            bin,
            home,
            cargo_home,
            rustup_home,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(self.project.join("lattice"));
        command
            .current_dir(&self.root)
            .env_clear()
            .env("PATH", &self.bin)
            .env("HOME", &self.home)
            .env("CARGO_HOME", &self.cargo_home)
            .env("RUSTUP_HOME", &self.rustup_home)
            .env("TEST_CAPTURE", &self.capture)
            .env("RUSTUP_AUTO_INSTALL", "1")
            // A hostile CDPATH must not redirect project discovery or pollute stdout.
            .env("CDPATH", "/does/not/exist");
        command
    }

    fn captured(&self) -> Vec<String> {
        fs::read_to_string(&self.capture)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn assert_clean_failure(&self, output: &Output, needle: &str) {
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(needle), "{stderr}");
        if needle.contains("cargo") || needle.contains("rustc") {
            assert!(stderr.contains("https://rustup.rs"), "{stderr}");
            assert!(stderr.contains("current shell"), "{stderr}");
        }
        assert!(
            stderr.contains("PATH and shell configuration were not changed"),
            "{stderr}"
        );
        assert!(
            !self.capture.exists(),
            "cargo run must not execute with an unusable toolchain"
        );
        assert!(!self.home.join("forbidden-action").exists());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn executable(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn files_below(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, directory: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

#[test]
fn missing_cargo_has_an_actionable_error_and_installs_nothing() {
    let fixture = Fixture::new(None, Some("rustc 1.95.0 (fixture)"));
    let before = files_below(&fixture.root);
    fixture.assert_clean_failure(&fixture.command().output().unwrap(), "cargo was not found");
    assert_eq!(files_below(&fixture.root), before);
}

#[test]
fn missing_rustc_has_an_actionable_error_and_installs_nothing() {
    let fixture = Fixture::new(Some("cargo 1.95.0 (fixture)"), None);
    let before = files_below(&fixture.root);
    fixture.assert_clean_failure(&fixture.command().output().unwrap(), "rustc was not found");
    assert_eq!(files_below(&fixture.root), before);
}

#[test]
fn either_tool_older_than_1_95_is_rejected() {
    for (cargo, rustc, old) in [
        ("cargo 1.94.9 (fixture)", "rustc 1.95.0 (fixture)", "cargo"),
        ("cargo 1.95.0 (fixture)", "rustc 1.94.9 (fixture)", "rustc"),
        ("cargo 0.99.0 (fixture)", "rustc 1.95.0 (fixture)", "cargo"),
    ] {
        let fixture = Fixture::new(Some(cargo), Some(rustc));
        let before = files_below(&fixture.root);
        fixture.assert_clean_failure(
            &fixture.command().output().unwrap(),
            &format!("{old} is too old"),
        );
        assert_eq!(files_below(&fixture.root), before);
    }
}

#[test]
fn malformed_versions_are_rejected_without_executing_their_content() {
    for version in [
        "cargo unknown",
        "cargo 95",
        "cargo 1.x.0",
        "cargo $(touch injected)",
        "surprise 1.95.0",
    ] {
        let fixture = Fixture::new(Some(version), Some("rustc 1.95.0 (fixture)"));
        let before = files_below(&fixture.root);
        fixture.assert_clean_failure(
            &fixture.command().output().unwrap(),
            "Cannot read cargo's version",
        );
        assert_eq!(files_below(&fixture.root), before);
    }
}

#[test]
fn spaces_arguments_and_environment_are_preserved_exactly() {
    let fixture = Fixture::new(
        Some("cargo 1.95.0 (fixture)"),
        Some("rustc 1.95.0 (fixture)"),
    );
    let before = files_below(&fixture.root);
    let arguments = [
        "--tour",
        "--snapshot",
        "file with spaces.svg",
        "",
        "*.svg",
        "$(not-a-command)",
        "--literal=a;b",
    ];
    let output = fixture.command().args(arguments).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    let captured = fixture.captured();
    assert_eq!(Path::new(&captured[0]), fixture.project);
    assert_eq!(OsStr::new(&captured[1]), fixture.bin.as_os_str());
    assert_eq!(Path::new(&captured[2]), fixture.home);
    assert_eq!(Path::new(&captured[3]), fixture.cargo_home);
    assert_eq!(Path::new(&captured[4]), fixture.rustup_home);
    assert_eq!(captured[5], "11");
    let mut expected = vec!["run", "--locked", "--release", "--"];
    expected.extend(arguments);
    assert_eq!(captured[6..], expected);
    let mut after = files_below(&fixture.root);
    assert!(after.remove(Path::new("capture")).is_some());
    assert_eq!(
        after, before,
        "launcher must not rewrite shell or toolchain configuration"
    );
}

#[test]
fn newer_toolchains_and_prerelease_suffixes_work() {
    for release in ["1.95.0", "1.95.0-nightly", "1.100.0", "2.0.0"] {
        let fixture = Fixture::new(
            Some(&format!("cargo {release} (fixture)")),
            Some(&format!("rustc {release} (fixture)")),
        );
        assert!(
            fixture
                .command()
                .arg("--doctor")
                .output()
                .unwrap()
                .status
                .success()
        );
        assert_eq!(fixture.captured().last().unwrap(), "--doctor");
    }
}

#[test]
fn cargo_exit_status_is_preserved() {
    let fixture = Fixture::new(
        Some("cargo 1.95.0 (fixture)"),
        Some("rustc 1.95.0 (fixture)"),
    );
    let output = fixture
        .command()
        .env("TEST_CARGO_EXIT", "23")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(23));
}

#[test]
fn relative_launcher_path_resolves_its_own_checkout() {
    let fixture = Fixture::new(
        Some("cargo 1.95.0 (fixture)"),
        Some("rustc 1.95.0 (fixture)"),
    );
    let mut command = fixture.command();
    // An explicit shell invokes the same relative path a user can type from outside it.
    command = {
        let mut relative = Command::new("/bin/sh");
        relative
            .current_dir(&fixture.root)
            .arg("checkout with spaces/lattice");
        for (key, value) in command.get_envs() {
            if let Some(value) = value {
                relative.env(key, value);
            }
        }
        relative
    };
    assert!(command.arg("--help").output().unwrap().status.success());
    assert_eq!(Path::new(&fixture.captured()[0]), fixture.project);
}

#[test]
fn missing_project_files_fail_before_any_build() {
    let fixture = Fixture::new(
        Some("cargo 1.95.0 (fixture)"),
        Some("rustc 1.95.0 (fixture)"),
    );
    fs::remove_file(fixture.project.join("Cargo.lock")).unwrap();
    fixture.assert_clean_failure(
        &fixture.command().output().unwrap(),
        "Keep lattice beside Cargo.toml and Cargo.lock",
    );
}

#[test]
fn launcher_found_on_path_uses_the_checkout_not_the_callers_directory() {
    let fixture = Fixture::new(
        Some("cargo 1.95.0 (fixture)"),
        Some("rustc 1.95.0 (fixture)"),
    );
    let environment = fixture.command();
    let mut command = Command::new("/bin/sh");
    command.env_clear().current_dir(&fixture.home);
    for (key, value) in environment.get_envs() {
        if let Some(value) = value {
            command.env(key, value);
        }
    }
    let path = std::env::join_paths([&fixture.project, &fixture.bin]).unwrap();
    let output = command
        .env("PATH", &path)
        .args(["-c", "exec lattice --doctor"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let captured = fixture.captured();
    assert_eq!(Path::new(&captured[0]), fixture.project);
    assert_eq!(OsStr::new(&captured[1]), path);
    assert_eq!(captured.last().unwrap(), "--doctor");
}

#[test]
fn broken_toolchain_fails_without_echoing_its_raw_error() {
    for tool in ["cargo", "rustc"] {
        let fixture = Fixture::new(
            Some("cargo 1.95.0 (fixture)"),
            Some("rustc 1.95.0 (fixture)"),
        );
        executable(
            &fixture.bin.join(tool),
            "#!/bin/sh\nprintf '%s\\n' 'private diagnostic /path/token' >&2\nexit 42\n",
        );
        let output = fixture.command().output().unwrap();
        fixture.assert_clean_failure(&output, &format!("Cannot run {tool}"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("/path/token"));
    }
}
