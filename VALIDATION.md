# Validation and limits

The supported source workflow uses Rust 1.95 or newer and the checked-in lockfile. No installer, daemon, autostart, credential, or process-control integration is required. The repo-local launcher uses an existing toolchain.

## Reproduce checks

```sh
cargo +stable fmt --check
cargo +stable clippy --locked --all-targets -- -D warnings
cargo +stable test --locked
cargo +1.95.0 test --locked
cargo +stable build --locked --release
python3 tools/pty_smoke.py target/release/lattice
python3 tools/onboarding_smoke.py target/release/lattice
python3 tools/practical_smoke.py target/release/lattice
python3 tools/navigation_smoke.py target/release/lattice
python3 tools/recording_smoke.py target/release/lattice
python3 tools/fault_smoke.py
```

The PTY tools need Python 3 and Unix terminal support, but no third-party Python packages. They create disposable child processes and temporary recording directories. Signal/fault tests target only those test children, never monitored workloads. SIGKILL cannot restore a terminal; that one test restores its own disposable PTY explicitly.

CI runs stable formatting, lint, tests, release builds, and PTY workflows on Ubuntu and macOS. A separate Ubuntu job checks the minimum supported Rust version. Cross-compiling with `cargo check --locked --target aarch64-apple-darwin` checks Apple API compatibility, not native runtime, permissions, suspend, or terminal rendering.

## What is covered

- View switching, graph/metric selection, stable process identity, editing priority, resizing and tiny terminals.
- Low-cost sample-driven dashboard accents, neutral paused/stale/syncing states, reduced motion, and deterministic SVG exports.
- Explicit recording only, named start/stop/restart, pause gaps, live/demo segregation and local output permissions.
- Complete-prefix recovery after torn writes or abrupt termination; no silent overwrite or resume of an existing file.
- Disk-full, file-cap and synchronization failures stop recording while preserving earlier records; OS-enforced short writes are tested without filling a disk.
- Sampler failure and startup cancellation restore terminal state, including when recording was requested.
- CPU baselines re-prime as unknown after gaps. A normal slower monitoring cadence is not mistaken for a gap.
- Bounded parser lines/files/contributors, bounded review identities, formula-safe CSV fields, terminal-safe paths, and PID/start-time separation.
- CSV preserves final gap counts and verified runtime, including a pause that was never resumed. Missing runtime remains unknown.
- A worst-case row-size calculation checks 48-hour capacity at the default persistence interval. This is a bounded-size test, not a 48-hour native endurance run.

## Interpreting evidence

A demo screenshot or GIF is always simulated data. The demo uses the real TUI renderer and interactions, but it is not a benchmark, measurement of the user's machine, or proof of macOS rendering. Real process sampling, suspension, terminal gestures and filesystem behavior remain platform-dependent.

Recording is a sampled local history, not continuous profiling. Sparse arithmetic means are not time-weighted totals, summed RSS is not unique physical memory, and available-memory headroom is not crash prediction or native macOS pressure. See [RECORDING.md](RECORDING.md) for storage, privacy, export and recovery details.
