mod app;
mod attachment;
mod changes;
mod doctor;
mod explorer;
mod insights;
mod model;
mod sampling;
mod ui;

use clap::Parser;
use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    time::Duration,
};

#[derive(Debug, Parser)]
#[command(
    name = "lattice",
    version,
    about = "Your coding sessions, seen as constellations.",
    after_help = "Quickstart:\n  ./lattice --tour     Learn the controls with simulated sessions\n  ./lattice           Monitor your real coding sessions\n  ./lattice --all     Explore all your accessible process families\n  ./lattice --doctor  Check this machine without opening the UI\n  ./lattice --pid 12345  Watch an existing dev server or build family\n\nThe repo-local launcher uses your existing Rust 1.95+ toolchain. It never installs it.\nFor a global command: cargo install --path . --locked, then lattice.\n\nKeys: 1–4 or [ ] switch screens · ←/→ previous/next · j/k select · Enter inspect · i inspector · g graph · m RAM/CPU · r nickname · e changes · / search · * pin · s sort · Space pause · ? help · q quit\n\nRead-only. Session memory is summed RSS, not unique physical memory. Headroom is an\navailable-memory heuristic, not native macOS pressure. No crash prevention or telemetry."
)]
struct Args {
    /// Explore a deterministic simulation instead of live processes.
    #[arg(long)]
    demo: bool,
    /// A three-step interactive guide using labeled demo data, never live processes.
    #[arg(long, conflicts_with_all = ["doctor", "snapshot"])]
    tour: bool,
    /// Check platform, terminal, and sampling without a TTY or saved report.
    #[arg(long, conflicts_with_all = ["demo", "tour", "snapshot", "all"])]
    doctor: bool,
    /// Disable screen transitions; sampling and readings remain identical.
    #[arg(long)]
    no_motion: bool,
    /// Include other process families owned by your OS user.
    #[arg(long)]
    all: bool,
    /// Attach to an existing current-user PID and its descendants; never launches it.
    #[arg(long, value_parser=clap::value_parser!(u32).range(1..), conflicts_with_all=["demo", "tour", "doctor", "all", "snapshot"])]
    pid: Option<u32>,
    /// Sample interval in seconds (0.5–30).
    #[arg(long, default_value="1", value_parser=parse_interval)]
    interval: f64,
    /// Export the demo UI to SVG without opening a terminal (requires --demo).
    #[arg(long, requires = "demo", value_name = "FILE")]
    snapshot: Option<PathBuf>,
    /// Choose the demo snapshot screen: overview, map, history, or changes.
    #[arg(long, requires = "snapshot", value_enum, conflicts_with_all = ["inspect", "changes"])]
    view: Option<app::View>,
    /// Choose the graph for a process-map snapshot.
    #[arg(long, requires = "snapshot", value_enum)]
    graph: Option<app::GraphMode>,
    /// Choose RAM (rss) or CPU for a process-map or history snapshot.
    #[arg(long, requires = "snapshot", value_enum)]
    metric: Option<app::Metric>,
    /// Open the demo snapshot in process-inspector view.
    #[arg(long, requires = "snapshot")]
    inspect: bool,
    /// Open the demo snapshot in the observed-changes view.
    #[arg(long, requires = "snapshot", conflicts_with = "inspect")]
    changes: bool,
    /// Snapshot width in terminal cells (40–240).
    #[arg(long, default_value="144", value_parser=clap::value_parser!(u16).range(40..=240))]
    width: u16,
    /// Snapshot height in terminal cells (16–100).
    #[arg(long, default_value="44", value_parser=clap::value_parser!(u16).range(16..=100))]
    height: u16,
}

fn parse_interval(value: &str) -> Result<f64, String> {
    let value: f64 = value
        .parse()
        .map_err(|_| "expected seconds, e.g. 1 or 0.5")?;
    if value.is_finite() && (0.5..=30.0).contains(&value) {
        Ok(value)
    } else {
        Err("interval must be between 0.5 and 30 seconds".into())
    }
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    if args.doctor {
        return doctor::run();
    }
    if let Some(path) = args.snapshot {
        let mut sampler = sampling::DemoSampler::new();
        let mut app = app::App::new(sampler.sample(args.all));
        for _ in 0..59 {
            app.update(sampler.sample(args.all));
        }
        app.no_motion = args.no_motion;
        if let Some(view) = args.view {
            app.set_view(view);
        }
        if args.inspect {
            app.set_view(app::View::ProcessMap);
            app.focused = true;
            app.focus = app::Focus::Processes;
        }
        if args.changes {
            app.open_changes();
        }
        if let Some(graph) = args.graph {
            app.graph_mode = graph;
            if graph == app::GraphMode::Heatmap {
                app.focus = app::Focus::Processes;
            }
        }
        if let Some(metric) = args.metric {
            app.metric = metric;
        }
        // Exports capture the settled screen, never a time-dependent transition.
        app.transition = None;
        ui::save_svg(&app, args.width, args.height, &path)?;
        println!("Saved deterministic demo to {}", path.display());
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "lattice needs an interactive terminal. Try lattice --demo in Ghostty.",
        ));
    }
    app::run(
        args.demo || args.tour,
        args.all,
        Duration::from_secs_f64(args.interval),
        args.tour,
        args.no_motion,
        args.pid,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interval_bounds_and_non_finite() {
        for bad in ["0", "0.49", "31", "NaN", "inf", "-1", "nope"] {
            assert!(parse_interval(bad).is_err());
        }
        for good in ["0.5", "1", "30"] {
            assert!(parse_interval(good).is_ok());
        }
    }
    #[test]
    fn snapshot_requires_explicit_demo() {
        assert!(Args::try_parse_from(["lattice", "--snapshot", "view.svg"]).is_err());
        assert!(Args::try_parse_from(["lattice", "--demo", "--snapshot", "view.svg"]).is_ok());
    }
}
