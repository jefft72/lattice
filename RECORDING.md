# Local hackathon recordings

Start a named recording in an interactive terminal:

```sh
./lattice --record CalHacks
```

Keep lattice open while you work. It monitors existing processes, never starts or controls them. Recording is off by default and only adds local metric-file writes when explicitly started. There is no daemon, autostart, cloud service, or telemetry. It cannot record while the app is closed or the computer is asleep. Restarting lattice creates a separate recording, not an implicit continuation of an old file.

- `Shift-R`: stop and save an active recording, or open the recording-name prompt.
- `Enter`: start after entering a name. `Esc`: cancel without writing a recording.
- `Space`: pause sampling and recording. Resume leaves an explicit gap, not invented values.
- `q` / `Ctrl-C`: quit and finalize. A normal termination signal also attempts to finalize.
- `--record-dir DIRECTORY`: choose a storage root. Live and demo files are separated.
- `--record-every 5`: persistence interval, in seconds (1–60). This does not change the monitor's sampling interval. The default is five seconds. Saving cannot be more frequent than monitoring: `--interval 30 --record-every 5` correctly saves each 30-second observation without inventing intermediate samples.

The footer shows recording state, elapsed duration, and sample count. A gap/stale indicator means the latest displayed values are not a fresh observation. Elapsed session duration is not observed CPU time. On exit, lattice prints the file location and review command. Never depend on recording to prevent a crash.

Default storage on macOS is `~/Library/Application Support/lattice/recordings/`. On Linux it is `$XDG_STATE_HOME/lattice/recordings/` (when the variable is an absolute path), otherwise `~/.local/state/lattice/recordings/`. Each root contains separate `live` and `demo` folders. Files use a unique `.lattice` name; the human session name is inside the file, not in the filename. New directories/files use private 0700/0600 permissions on Unix, without changing existing permissions.

## Find, review, export

```sh
./lattice recordings list
./lattice recordings show /path/to/recording
./lattice recordings export /path/to/recording --output calhacks.csv
# For a custom root:
./lattice recordings list --record-dir ./my-recordings
```

Review and export work without an interactive terminal and do not sample processes. Export creates a new CSV file and refuses to overwrite an existing file. CSV contains time-series and family rows suitable for post-session analysis. Its final status row preserves gap-marker count and clean-footer app runtime, including a pause that was never resumed. Missing-footer runtime stays unknown. Export is capped at 1 GiB; an error leaves a partial file for inspection rather than deleting or overwriting anything. Spreadsheet text fields are sanitized against formula injection and terminal control sequences.

The report summarizes recorded-sample peaks and arithmetic means, not continuous averages, total work, energy, or inferred CPU-seconds. Short spikes between stored observations can be missed. System CPU is normalized across logical cores; family CPU uses core-percent (100% means one fully used logical core). Family memory is summed RSS and can double-count shared pages. Available-memory headroom is a heuristic, not native macOS memory pressure or a crash forecast.

## Privacy, limits, and recovery

Records contain timestamps, machine-wide CPU/RAM/swap metrics, the current monitoring scope, and bounded family contributors identified by PID/start time and sanitized process basename. They do not contain command arguments, working directories, executable paths, environment variables, user nicknames, or secrets. A basename and your recording name can still reveal what tools or project label you chose; inspect exports before sharing. The OS sampling backend may read argument buffers transiently on macOS, but lattice does not persist them.

At most 32 family contributors are retained per stored sample, with an explicit omitted count. Family identities include process start time so a reused PID is not silently treated as the same family. OS start times have one-second precision, which remains a limitation. Search and display sorting do not filter recorded machine/scope totals.

Data is streamed instead of retaining a whole hackathon in memory. Each file is capped at 256 MiB, which supports roughly 48 hours at the default five-second interval even with 32 contributors. Recording stops visibly at the cap; it does not silently discard old data. Review retains at most 1,024 family identities, and reports omitted identities. Files and review parsing are bounded. Existing files are never silently appended to or overwritten. Complete records before a torn final write can be reviewed; a file without a final stop marker is reported as unfinished, not proof that recording is still running. A crash or forced kill may lose the latest unsynced writes, and power-loss guarantees depend on the filesystem. Disk errors stop recording with a visible message while monitoring can continue.

Pauses, delayed observations, and clock/suspend discontinuities are reported as gaps. The first sample after a gap has unknown CPU while counters re-prime, not a false zero or an average over the unseen interval. A clock change cannot establish whether the machine slept, so gaps are not labeled as confirmed sleep. No data is fabricated across a gap. A recording may be incomplete if the app or computer stopped; review the status and gaps before drawing conclusions.

For an explicitly labeled simulated trial:

```sh
./lattice --demo --record Trial --record-dir ./trial-recordings
```

Demo files are segregated from live recordings and marked simulated in the UI, review, and export. They are not measurements of your computer.
