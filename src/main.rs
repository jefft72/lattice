mod app;
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
    after_help = "Keys: j/k or arrows select · Enter inspect · / search · * pin · s sort · Space pause · ? help · q quit\n\nRead-only. Session memory is summed RSS, not unique physical memory. Headroom is an\navailable-memory heuristic, not native macOS pressure. No crash prevention or telemetry."
)]
struct Args {
    /// Explore a deterministic simulation instead of live processes.
    #[arg(long)]
    demo: bool,
    /// Include other process families owned by your OS user.
    #[arg(long)]
    all: bool,
    /// Sample interval in seconds (0.5–30).
    #[arg(long, default_value="1", value_parser=parse_interval)]
    interval: f64,
    /// Export the demo UI to SVG without opening a terminal (requires --demo).
    #[arg(long, requires = "demo", value_name = "FILE")]
    snapshot: Option<PathBuf>,
    /// Open the demo snapshot in process-inspector view.
    #[arg(long, requires = "snapshot")]
    inspect: bool,
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
    if let Some(path) = args.snapshot {
        let mut sampler = sampling::DemoSampler::new();
        let mut app = app::App::new(sampler.sample(args.all));
        for _ in 0..59 {
            app.update(sampler.sample(args.all));
        }
        if args.inspect {
            app.focused = true;
            app.focus = app::Focus::Processes;
        }
        ui::save_svg(&app, args.width, args.height, &path)?;
        println!("Saved deterministic demo to {}", path.display());
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "lattice needs an interactive terminal. Try lattice --demo in Ghostty.",
        ));
    }
    app::run(args.demo, args.all, Duration::from_secs_f64(args.interval))
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
