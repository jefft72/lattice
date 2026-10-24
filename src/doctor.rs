//! Read-only diagnostics suitable for terminals, pipes, and redirected output.
//! Only fixed labels and aggregate numeric sample data can reach the report.

use std::io::{self, IsTerminal, Write};

use crate::{model::Snapshot, sampling::LiveSampler};

/// Take one live sample without entering terminal mode or starting subprocesses.
/// No detected agents is a normal, successful diagnostic result.
pub fn run() -> io::Result<()> {
    let stdin_tty = io::stdin().is_terminal();
    let stdout_tty = io::stdout().is_terminal();
    let snapshot = LiveSampler::new().sample(false);
    write_report(
        &mut io::stdout().lock(),
        &snapshot,
        stdin_tty,
        stdout_tty,
        sysinfo::IS_SUPPORTED_SYSTEM,
    )
}

fn write_report(
    output: &mut impl Write,
    snapshot: &Snapshot,
    stdin_tty: bool,
    stdout_tty: bool,
    supported: bool,
) -> io::Result<()> {
    writeln!(
        output,
        "lattice {} | read-only diagnostics",
        env!("CARGO_PKG_VERSION")
    )?;
    writeln!(
        output,
        "Platform: {} / {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    )?;
    writeln!(
        output,
        "TTY: stdin={}, stdout={}",
        yes_no(stdin_tty),
        yes_no(stdout_tty)
    )?;
    let sample_status = if !supported {
        "unavailable (unsupported operating-system backend)"
    } else if snapshot.process_count == 0 {
        "unavailable (no readable current-user processes)"
    } else {
        "available"
    };
    writeln!(output, "Live sample: {sample_status}")?;
    writeln!(
        output,
        "Current-user processes sampled: {}",
        snapshot.process_count
    )?;
    let agents = snapshot
        .sessions
        .iter()
        .filter(|session| session.agent.is_some())
        .count();
    writeln!(output, "Detected agent sessions: {agents}")?;
    if agents == 0 {
        writeln!(
            output,
            "No agents detected is normal; start a supported coding agent to see a live session."
        )?;
    }
    let system = &snapshot.system;
    if supported && system.memory_total > 0 && system.memory_available <= system.memory_total {
        const GIB: f64 = 1_073_741_824.0;
        writeln!(
            output,
            "Available RAM: {:.2} GiB / {:.2} GiB total",
            system.memory_available as f64 / GIB,
            system.memory_total as f64 / GIB,
        )?;
    } else {
        writeln!(
            output,
            "Available RAM: unknown (not reported reliably by the OS backend)"
        )?;
    }
    if snapshot.inaccessible_count > 0 {
        writeln!(
            output,
            "Sampling limitation: ownership unavailable for {} visible processes; excluded.",
            snapshot.inaccessible_count
        )?;
    }
    if snapshot.truncated {
        writeln!(
            output,
            "Sampling limitation: the current-user process safety limit was reached."
        )?;
    }
    writeln!(
        output,
        "Scope: recognized agents owned by your OS user; inaccessible processes may be absent."
    )?;
    writeln!(
        output,
        "Memory is an available-memory estimate, not native macOS pressure; CPU needs later samples."
    )?;
    writeln!(
        output,
        "Privacy: no process names, arguments, environment values, or paths are printed."
    )?;
    writeln!(
        output,
        "macOS limitation: the OS backend may transiently read argument/environment blocks; lattice does not retain or display them."
    )?;
    Ok(())
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProcessSnapshot, SessionSnapshot, SystemSnapshot};

    fn report(snapshot: &Snapshot, supported: bool) -> String {
        let mut bytes = Vec::new();
        write_report(&mut bytes, snapshot, false, false, supported).unwrap();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn zero_agents_is_success_and_reports_known_memory() {
        let snapshot = Snapshot {
            process_count: 10,
            system: SystemSnapshot {
                memory_total: 16 * 1_073_741_824,
                memory_available: 6 * 1_073_741_824,
                ..SystemSnapshot::default()
            },
            ..Snapshot::default()
        };
        let text = report(&snapshot, true);
        assert!(text.contains("TTY: stdin=no, stdout=no"));
        assert!(text.contains("Live sample: available"));
        assert!(text.contains("Detected agent sessions: 0"));
        assert!(text.contains("No agents detected is normal"));
        assert!(text.contains("Available RAM: 6.00 GiB / 16.00 GiB total"));
        assert!(!text.contains('\x1b'));
    }

    #[test]
    fn unavailable_data_is_distinct_from_zero_agents() {
        let text = report(&Snapshot::default(), true);
        assert!(text.contains("Live sample: unavailable (no readable current-user processes)"));
        assert!(text.contains("Available RAM: unknown"));
        let unsupported = report(&Snapshot::default(), false);
        assert!(unsupported.contains("unsupported operating-system backend"));
    }

    #[test]
    fn output_never_includes_process_names_labels_or_agent_strings() {
        let secret = "private-/path/--token=SECRET\x1b[31m";
        let snapshot = Snapshot {
            process_count: 1,
            inaccessible_count: 4,
            truncated: true,
            sessions: vec![SessionSnapshot {
                label: secret.into(),
                agent: Some(secret.into()),
                processes: vec![ProcessSnapshot {
                    name: secret.into(),
                    ..ProcessSnapshot::default()
                }],
                ..SessionSnapshot::default()
            }],
            ..Snapshot::default()
        };
        let text = report(&snapshot, true);
        assert!(text.contains("Detected agent sessions: 1"));
        assert!(text.contains("ownership unavailable for 4 visible processes"));
        assert!(text.contains("safety limit was reached"));
        for forbidden in ["private-", "/path/", "SECRET", "\x1b"] {
            assert!(!text.contains(forbidden));
        }
    }

    #[test]
    fn impossible_memory_values_are_not_presented_as_known() {
        let snapshot = Snapshot {
            system: SystemSnapshot {
                memory_total: 1,
                memory_available: 2,
                ..SystemSnapshot::default()
            },
            ..Snapshot::default()
        };
        assert!(report(&snapshot, true).contains("Available RAM: unknown"));
    }

    #[test]
    fn output_errors_are_propagated() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let error =
            write_report(&mut Broken, &Snapshot::default(), false, false, true).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }
}
