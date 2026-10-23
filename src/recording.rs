//! Explicit, local-only recording. Append-only records are bounded and independently
//! parseable so an interrupted final write never invalidates the complete prefix.
//! This is a sampled history, not an energy meter, profiler, daemon or crash guard.

use crate::model::{ProcessKey, Snapshot, sanitize_name};
use std::{
    collections::HashMap,
    env,
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const DEFAULT_RECORD_INTERVAL: Duration = Duration::from_secs(5);
pub const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_CONTRIBUTORS: usize = 32;
const MAX_LINE_BYTES: usize = 16 * 1024;
const MAX_REVIEW_IDENTITIES: usize = 1024;
const MAX_LIST_ENTRIES: usize = 200;
const MAX_EXPORT_BYTES: u64 = 1024 * 1024 * 1024;
const SYNC_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct RecordingOptions {
    pub name: String,
    /// Base directory; live and simulated recordings always use separate children.
    pub directory: Option<PathBuf>,
    pub interval: Duration,
    /// Expected live sampling cadence, separate from file persistence cadence.
    pub sampling_interval: Duration,
}

#[derive(Clone, Debug)]
pub struct RecordingStatus {
    pub name: String,
    pub path: PathBuf,
    pub samples: u64,
    /// App runtime, not sampled coverage; includes pauses but may exclude OS sleep.
    pub elapsed_secs: f64,
    pub bytes_written: u64,
    pub demo: bool,
    pub stopped: bool,
}

pub struct Recorder {
    file: File,
    status: RecordingStatus,
    interval: Duration,
    sampling_interval: Duration,
    started: Instant,
    last_sync: Instant,
    initial_monotonic: f64,
    last_observed: f64,
    last_wall: f64,
    last_written: Option<f64>,
    pending_gap: bool,
    last_all: bool,
    byte_limit: u64,
}

pub fn format_duration(seconds: f64) -> String {
    let seconds = if seconds.is_finite() {
        seconds.max(0.0) as u64
    } else {
        0
    };
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

pub fn default_directory() -> io::Result<PathBuf> {
    let home = env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        home.map(|p| p.join("Library/Application Support/lattice/recordings"))
    } else {
        env::var_os("XDG_STATE_HOME")
            .filter(|p| Path::new(p).is_absolute())
            .map(PathBuf::from)
            .or_else(|| home.map(|p| p.join(".local/state")))
            .map(|p| p.join("lattice/recordings"))
    }
    .ok_or_else(|| io::Error::other("No home/state directory; choose --record-dir explicitly"))
}

/// Recording names are user-provided labels only, never filesystem paths.
pub fn validate_name(name: &str) -> io::Result<String> {
    let name = name.trim();
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Recording name needs 1–64 ASCII letters, digits, spaces, dots, hyphens or underscores",
        ));
    }
    Ok(name.to_owned())
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn private_dir(path: &Path) -> io::Result<()> {
    let existed = path.try_exists()?;
    if !existed {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(path)?;
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Err(io::Error::other(
            "Recording destination must be a directory, not a symlink",
        ));
    }
    Ok(())
}

fn new_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn finite(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}
fn cpu_text(value: Option<f32>) -> String {
    value
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| format!("{v:.3}"))
        .unwrap_or_else(|| "-".into())
}

impl Recorder {
    pub fn start(options: RecordingOptions, snapshot: &Snapshot) -> io::Result<Self> {
        let name = validate_name(&options.name)?;
        if options.interval < Duration::from_secs(1) || options.interval > Duration::from_secs(3600)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Recording interval must be 1–3600 seconds",
            ));
        }
        if !finite(snapshot.monotonic) || !finite(snapshot.timestamp) {
            return Err(io::Error::other(
                "Cannot record an invalid sample timestamp",
            ));
        }
        let base = options
            .directory
            .map(Ok)
            .unwrap_or_else(default_directory)?;
        private_dir(&base)?;
        let directory = base.join(if snapshot.demo { "demo" } else { "live" });
        private_dir(&directory)?;
        // User labels never become path components. create_new rejects existing files
        // and symlinks; process ID plus a counter handles concurrent starts.
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut selected = None;
        for counter in 0..100 {
            let path = directory.join(format!("{stamp}-{}-{counter}.lattice", std::process::id()));
            match new_private_file(&path) {
                Ok(file) => {
                    selected = Some((path, file));
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        let (path, file) =
            selected.ok_or_else(|| io::Error::other("Could not create a unique recording file"))?;
        let started = Instant::now();
        let mut recorder = Self {
            file,
            status: RecordingStatus {
                name,
                path,
                samples: 0,
                elapsed_secs: 0.0,
                bytes_written: 0,
                demo: snapshot.demo,
                stopped: false,
            },
            interval: options.interval,
            sampling_interval: options.sampling_interval,
            started,
            last_sync: started,
            initial_monotonic: snapshot.monotonic,
            last_observed: snapshot.monotonic,
            last_wall: snapshot.timestamp,
            last_written: None,
            pending_gap: false,
            last_all: snapshot.all_processes,
            byte_limit: MAX_FILE_BYTES,
        };
        let header = format!(
            "LATTICE_RECORDING\t1\t{}\t{}\t{:.3}\t{:.3}\n",
            recorder.status.name,
            if snapshot.demo { "SIMULATED" } else { "LIVE" },
            unix_now(),
            options.interval.as_secs_f64()
        );
        recorder.append(&header)?;
        recorder.file.sync_data()?;
        recorder.observe(snapshot)?;
        Ok(recorder)
    }

    pub fn info(&self) -> RecordingStatus {
        let mut status = self.status.clone();
        if !status.stopped {
            status.elapsed_secs = self.started.elapsed().as_secs_f64();
        }
        status
    }

    fn append(&mut self, line: &str) -> io::Result<()> {
        if line.len() > MAX_LINE_BYTES
            || self.status.bytes_written.saturating_add(line.len() as u64) > self.byte_limit
        {
            self.status.stopped = true;
            self.status.elapsed_secs = self.started.elapsed().as_secs_f64();
            return Err(io::Error::other(
                "Recording stopped at its 256 MiB storage limit; complete samples remain reviewable",
            ));
        }
        let result = self
            .file
            .write_all(line.as_bytes())
            .and_then(|()| self.file.flush());
        if let Err(error) = result {
            self.status.stopped = true;
            self.status.elapsed_secs = self.started.elapsed().as_secs_f64();
            return Err(error);
        }
        self.status.bytes_written += line.len() as u64;
        if self.last_sync.elapsed() >= SYNC_INTERVAL {
            if let Err(error) = self.file.sync_data() {
                self.status.stopped = true;
                self.status.elapsed_secs = self.started.elapsed().as_secs_f64();
                return Err(error);
            }
            self.last_sync = Instant::now();
        }
        Ok(())
    }

    pub fn observe(&mut self, snapshot: &Snapshot) -> io::Result<()> {
        if self.status.stopped {
            return Ok(());
        }
        if snapshot.demo != self.status.demo {
            self.status.stopped = true;
            self.status.elapsed_secs = self.started.elapsed().as_secs_f64();
            return Err(io::Error::other(
                "Cannot mix simulated and live data in one recording",
            ));
        }
        if !finite(snapshot.monotonic)
            || !finite(snapshot.timestamp)
            || snapshot.monotonic < self.last_observed
        {
            self.mark_gap("invalid-clock")?;
            return Ok(());
        }
        let delta = snapshot.monotonic - self.last_observed;
        let wall_delta = snapshot.timestamp - self.last_wall;
        // Monotonic clocks may exclude suspend on some OSes. A disagreement is
        // labeled as a gap/clock change, never attributed to sleep as a certainty.
        let threshold = (self.sampling_interval.as_secs_f64() * 3.0).max(3.0);
        if delta > threshold || wall_delta > delta + 2.0 || wall_delta < -2.0 {
            self.mark_gap("sample-gap-or-clock-change")?;
        }
        if snapshot.all_processes != self.last_all {
            self.mark_gap("scope-change")?;
        }
        self.last_all = snapshot.all_processes;
        self.last_observed = snapshot.monotonic;
        self.last_wall = snapshot.timestamp;
        let elapsed = (snapshot.monotonic - self.initial_monotonic).max(0.0);
        if self
            .last_written
            .is_some_and(|last| elapsed - last < self.interval.as_secs_f64())
            && !self.pending_gap
        {
            return Ok(());
        }
        // Keep at most 32 contributors, even if the input contains thousands of
        // families. Compute RSS only once per family; no cloned process lists.
        let mut contributors = Vec::with_capacity(MAX_CONTRIBUTORS + 1);
        let mut scope_rss = 0_u64;
        for session in &snapshot.sessions {
            let rss = session.rss_bytes();
            scope_rss = scope_rss.saturating_add(rss);
            let rank = contributors.partition_point(
                |(other, other_rss): &(&crate::model::SessionSnapshot, u64)| {
                    *other_rss > rss || (*other_rss == rss && other.key < session.key)
                },
            );
            if rank < MAX_CONTRIBUTORS {
                contributors.insert(rank, (session, rss));
                contributors.truncate(MAX_CONTRIBUTORS);
            }
        }
        let omitted = snapshot.sessions.len().saturating_sub(contributors.len());
        let mut line = format!(
            "S\t{}\t{:.3}\t{elapsed:.3}\t{}\t{}\t{}\t{}\t{}\t{scope_rss}\t{}\t{omitted}\t{}\t{}\t",
            self.status.samples + 1,
            snapshot.timestamp,
            u8::from(self.pending_gap),
            cpu_text(if self.pending_gap {
                None
            } else {
                snapshot.system.cpu_percent
            }),
            snapshot.system.memory_total,
            snapshot.system.memory_available,
            snapshot.system.swap_used,
            snapshot.sessions.len(),
            u8::from(snapshot.truncated),
            u8::from(snapshot.all_processes)
        );
        for (index, (session, rss)) in contributors.iter().enumerate() {
            if index > 0 {
                line.push(';');
            }
            // Do not persist user nicknames, full names, executable paths, process
            // arguments or environment. Every persisted family label is sanitized.
            let _ = write!(
                line,
                "{},{},{},{},{}",
                session.key.pid,
                session.key.started,
                sanitize_name(&session.label),
                rss,
                cpu_text(if self.pending_gap {
                    None
                } else {
                    session.cpu_percent()
                })
            );
        }
        line.push('\n');
        self.append(&line)?;
        self.status.samples += 1;
        self.last_written = Some(elapsed);
        self.pending_gap = false;
        Ok(())
    }

    pub fn mark_gap(&mut self, reason: &str) -> io::Result<()> {
        if self.status.stopped || self.pending_gap {
            return Ok(());
        }
        let reason = sanitize_name(reason);
        let elapsed = (self.last_observed - self.initial_monotonic).max(0.0);
        self.append(&format!("G\t{elapsed:.3}\t{reason}\n"))?;
        self.pending_gap = true;
        Ok(())
    }

    pub fn finish(&mut self, reason: &str) -> io::Result<()> {
        if self.status.stopped {
            return Ok(());
        }
        let elapsed = self.started.elapsed().as_secs_f64();
        let result = self
            .append(&format!("F\t{elapsed:.3}\t{}\n", sanitize_name(reason)))
            .and_then(|()| self.file.sync_data());
        self.status.stopped = true;
        self.status.elapsed_secs = elapsed;
        result
    }
}

#[derive(Clone, Debug)]
struct Header {
    name: String,
    demo: bool,
    created: f64,
    interval: f64,
}
#[derive(Clone, Debug)]
struct Contributor {
    key: ProcessKey,
    name: String,
    rss: u64,
    cpu: Option<f64>,
}
#[derive(Clone, Debug)]
struct Sample {
    sequence: u64,
    wall: f64,
    elapsed: f64,
    gap: bool,
    cpu: Option<f64>,
    total: u64,
    available: u64,
    swap: u64,
    scope_rss: u64,
    families: usize,
    omitted: usize,
    partial: bool,
    all: bool,
    contributors: Vec<Contributor>,
}
#[derive(Clone, Debug)]
enum Record {
    Sample(Sample),
    Gap,
    Finish(f64),
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "Invalid or unsupported recording data",
    )
}
fn number<T: std::str::FromStr>(value: &str) -> io::Result<T> {
    value.parse().map_err(|_| invalid())
}
fn seconds(value: &str) -> io::Result<f64> {
    let value = number(value)?;
    if finite(value) {
        Ok(value)
    } else {
        Err(invalid())
    }
}
fn optional_cpu(value: &str) -> io::Result<Option<f64>> {
    if value == "-" {
        Ok(None)
    } else {
        seconds(value).map(Some)
    }
}
fn boolean(value: &str) -> io::Result<bool> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(invalid()),
    }
}
fn safe_field(value: &str) -> io::Result<String> {
    if !value.is_empty() && sanitize_name(value) == value {
        Ok(value.into())
    } else {
        Err(invalid())
    }
}

fn parse_header(line: &str) -> io::Result<Header> {
    let fields: Vec<_> = line.trim_end_matches('\n').split('\t').collect();
    if fields.len() != 6 || fields[0] != "LATTICE_RECORDING" || fields[1] != "1" {
        return Err(invalid());
    }
    let name = validate_name(fields[2]).map_err(|_| invalid())?;
    let demo = match fields[3] {
        "SIMULATED" => true,
        "LIVE" => false,
        _ => return Err(invalid()),
    };
    let interval = seconds(fields[5])?;
    if !(1.0..=3600.0).contains(&interval) {
        return Err(invalid());
    }
    Ok(Header {
        name,
        demo,
        created: seconds(fields[4])?,
        interval,
    })
}

fn parse_record(line: &str) -> io::Result<Record> {
    let fields: Vec<_> = line.trim_end_matches('\n').split('\t').collect();
    match fields.first().copied() {
        Some("G") if fields.len() == 3 => {
            seconds(fields[1])?;
            safe_field(fields[2])?;
            Ok(Record::Gap)
        }
        Some("F") if fields.len() == 3 => {
            safe_field(fields[2])?;
            Ok(Record::Finish(seconds(fields[1])?))
        }
        Some("S") if fields.len() == 15 => {
            let mut contributors = Vec::new();
            if !fields[14].is_empty() {
                for item in fields[14].split(';') {
                    if contributors.len() == MAX_CONTRIBUTORS {
                        return Err(invalid());
                    }
                    let part: Vec<_> = item.split(',').collect();
                    if part.len() != 5 {
                        return Err(invalid());
                    }
                    let key = ProcessKey {
                        pid: number(part[0])?,
                        started: number(part[1])?,
                    };
                    if contributors.iter().any(|c: &Contributor| c.key == key) {
                        return Err(invalid());
                    }
                    contributors.push(Contributor {
                        key,
                        name: safe_field(part[2])?,
                        rss: number(part[3])?,
                        cpu: optional_cpu(part[4])?,
                    });
                }
            }
            let families: usize = number(fields[10])?;
            let omitted: usize = number(fields[11])?;
            if contributors.len().checked_add(omitted) != Some(families) {
                return Err(invalid());
            }
            Ok(Record::Sample(Sample {
                sequence: number(fields[1])?,
                wall: seconds(fields[2])?,
                elapsed: seconds(fields[3])?,
                gap: boolean(fields[4])?,
                cpu: optional_cpu(fields[5])?,
                total: number(fields[6])?,
                available: number(fields[7])?,
                swap: number(fields[8])?,
                scope_rss: number(fields[9])?,
                families,
                omitted,
                partial: boolean(fields[12])?,
                all: boolean(fields[13])?,
                contributors,
            }))
        }
        _ => Err(invalid()),
    }
}

/// Fixed-size reads avoid allocating an attacker-controlled line or reading an
/// arbitrarily large file. A malformed/torn suffix is not treated as samples.
fn read_line(reader: &mut BufReader<File>, bytes: &mut Vec<u8>) -> io::Result<bool> {
    bytes.clear();
    let mut limited = std::io::Read::take(reader, (MAX_LINE_BYTES + 1) as u64);
    limited.read_until(b'\n', bytes)?;
    if bytes.len() > MAX_LINE_BYTES || (!bytes.is_empty() && !bytes.ends_with(b"\n")) {
        return Err(invalid());
    }
    Ok(!bytes.is_empty())
}

fn open_reader(path: &Path) -> io::Result<(BufReader<File>, Header)> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return Err(io::Error::other(
            "Recording must be a regular file no larger than 256 MiB",
        ));
    }
    let mut reader = BufReader::with_capacity(MAX_LINE_BYTES, File::open(path)?);
    let mut line = Vec::new();
    if !read_line(&mut reader, &mut line)? {
        return Err(invalid());
    }
    let header = parse_header(std::str::from_utf8(&line).map_err(|_| invalid())?)?;
    Ok((reader, header))
}

#[derive(Default)]
struct Scan {
    samples: u64,
    recovered_tail: bool,
    finished: Option<f64>,
    gaps: u64,
}
fn scan_records(
    mut reader: BufReader<File>,
    mut on_sample: impl FnMut(&Sample) -> io::Result<()>,
) -> io::Result<Scan> {
    let mut scan = Scan::default();
    let mut line = Vec::with_capacity(MAX_LINE_BYTES);
    let mut last_elapsed = 0.0;
    let mut pending_gap = false;
    let mut read_bytes = 0_u64;
    loop {
        match read_line(&mut reader, &mut line) {
            Ok(false) => break,
            Ok(true) => {}
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                scan.recovered_tail = true;
                break;
            }
            Err(error) => return Err(error),
        }
        read_bytes += line.len() as u64;
        if read_bytes > MAX_FILE_BYTES || scan.finished.is_some() {
            scan.recovered_tail = true;
            break;
        }
        let record = std::str::from_utf8(&line)
            .map_err(|_| invalid())
            .and_then(parse_record);
        match record {
            Ok(Record::Sample(mut sample))
                if sample.sequence == scan.samples + 1 && sample.elapsed >= last_elapsed =>
            {
                if sample.gap || pending_gap {
                    // Gap boundaries are authoritative even for old/edited files
                    // that contain a CPU delta spanning unobserved time.
                    sample.gap = true;
                    sample.cpu = None;
                    for contributor in &mut sample.contributors {
                        contributor.cpu = None;
                    }
                }
                pending_gap = false;
                last_elapsed = sample.elapsed;
                scan.samples += 1;
                on_sample(&sample)?;
            }
            Ok(Record::Gap) => {
                scan.gaps += 1;
                pending_gap = true;
            }
            Ok(Record::Finish(duration)) => scan.finished = Some(duration),
            _ => {
                scan.recovered_tail = true;
                break;
            }
        }
    }
    Ok(scan)
}

#[derive(Default)]
struct Stats {
    count: u64,
    cpu_mean: f64,
    cpu_count: u64,
    peak_cpu: f64,
    peak_rss: u64,
    name: String,
}
impl Stats {
    fn add(&mut self, rss: u64, cpu: Option<f64>) {
        self.count += 1;
        self.peak_rss = self.peak_rss.max(rss);
        if let Some(cpu) = cpu {
            self.cpu_count += 1;
            // Online mean cannot overflow when parsing very large finite values.
            self.cpu_mean += (cpu - self.cpu_mean) / self.cpu_count as f64;
            self.peak_cpu = self.peak_cpu.max(cpu);
        }
    }
    fn mean_cpu(&self) -> String {
        if self.cpu_count == 0 {
            "unknown".into()
        } else {
            format!("{:.1}%", self.cpu_mean)
        }
    }
    fn peak_cpu(&self) -> String {
        if self.cpu_count == 0 {
            "unknown".into()
        } else {
            format!("{:.1}%", self.peak_cpu)
        }
    }
}
fn gib(bytes: u64) -> String {
    format!("{:.2} GiB", bytes as f64 / (1_u64 << 30) as f64)
}

pub fn review(path: &Path) -> io::Result<String> {
    let (reader, header) = open_reader(path)?;
    let mut system = Stats::default();
    let mut system_memory_known = false;
    let mut families: HashMap<ProcessKey, Stats> = HashMap::new();
    let mut omitted_identity_observations = 0_u64;
    let mut omitted_contributors = 0_u64;
    let mut partial_samples = 0_u64;
    let mut last_elapsed = 0.0;
    let mut first_wall = None;
    let mut last_wall = 0.0;
    let mut scope_peak = 0_u64;
    let mut swap_peak = 0_u64;
    let mut gap_samples = 0_u64;
    let scan = scan_records(reader, |sample| {
        system_memory_known |= sample.total > 0;
        system.add(sample.total.saturating_sub(sample.available), sample.cpu);
        first_wall.get_or_insert(sample.wall);
        last_wall = sample.wall;
        last_elapsed = sample.elapsed;
        scope_peak = scope_peak.max(sample.scope_rss);
        swap_peak = swap_peak.max(sample.swap);
        omitted_contributors = omitted_contributors.saturating_add(sample.omitted as u64);
        partial_samples += u64::from(sample.partial);
        gap_samples += u64::from(sample.gap);
        for contributor in &sample.contributors {
            if !families.contains_key(&contributor.key) && families.len() == MAX_REVIEW_IDENTITIES {
                omitted_identity_observations += 1;
                continue;
            }
            let stats = families.entry(contributor.key).or_default();
            stats.name.clone_from(&contributor.name);
            stats.add(contributor.rss, contributor.cpu);
        }
        Ok(())
    })?;
    let state = if scan.recovered_tail {
        "RECOVERED PREFIX (invalid or incomplete suffix ignored)"
    } else if scan.finished.is_some() {
        "STOPPED (clean footer)"
    } else {
        "UNFINALIZED (may still be active, or was interrupted)"
    };
    let mode = if header.demo {
        "SIMULATED DATA, not your machine"
    } else {
        "LIVE local samples"
    };
    let mut out = format!(
        "{}\n{mode}\n{state}\nCreated: {:.3} Unix seconds\nRecorded samples: {} at a target {:.1}s interval\nSampled monotonic span: {} (not continuous coverage)\n",
        header.name,
        header.created,
        scan.samples,
        header.interval,
        format_duration(last_elapsed)
    );
    if let Some(duration) = scan.finished {
        let _ = writeln!(
            out,
            "App recording runtime: {} (includes pause; OS sleep may be excluded)",
            format_duration(duration)
        );
    }
    if let Some(first) = first_wall {
        let _ = writeln!(
            out,
            "Sample wall-clock span: {:.1}s (clock changes may affect this)",
            last_wall - first
        );
    }
    let _ = writeln!(
        out,
        "Gap markers: {} · samples after gaps: {gap_samples}",
        scan.gaps
    );
    let _ = writeln!(
        out,
        "System CPU: sampled mean {} · sampled peak {} (normalized across cores)",
        system.mean_cpu(),
        system.peak_cpu()
    );
    let _ = writeln!(
        out,
        "System used RAM peak: {} · swap used peak: {}",
        if system_memory_known {
            gib(system.peak_rss)
        } else {
            "unknown".into()
        },
        gib(swap_peak)
    );
    let _ = writeln!(
        out,
        "Scope summed RSS peak: {} (shared pages may be counted twice)",
        gib(scope_peak)
    );
    let _ = writeln!(
        out,
        "Partial scan samples: {partial_samples} · omitted family observations: {omitted_contributors}"
    );
    let _ = writeln!(out, "\nFamily contributors (top 20 by sampled peak RSS):");
    let mut ranked: Vec<_> = families.into_iter().collect();
    ranked
        .sort_unstable_by(|(ak, a), (bk, b)| b.peak_rss.cmp(&a.peak_rss).then_with(|| ak.cmp(bk)));
    for (key, stats) in ranked.iter().take(20) {
        let _ = writeln!(
            out,
            "{} · PID {}/start {} · peak RSS {} · CPU mean {} / peak {} · {} observed samples",
            stats.name,
            key.pid,
            key.started,
            gib(stats.peak_rss),
            stats.mean_cpu(),
            stats.peak_cpu(),
            stats.count
        );
    }
    if ranked.len() > 20 {
        let _ = writeln!(
            out,
            "{} more retained family identities; CSV contains all recorded contributors.",
            ranked.len() - 20
        );
    }
    if omitted_identity_observations > 0 {
        let _ = writeln!(
            out,
            "Review identity cap (1024) omitted {omitted_identity_observations} family observations; CSV preserves the recorded data."
        );
    }
    out.push_str("\nMeans are arithmetic means of recorded samples, not time-weighted totals.\nFamily CPU is core-percent (100% = one logical core); absent/omitted families are unknown, not zero.\nNo CPU-hours, energy use, crash causes, or activity during gaps are inferred.\nPeaks between saved samples may be missed. Keep lattice running; closing it stops recording.\n");
    Ok(out)
}

pub fn list(directory: Option<&Path>) -> io::Result<String> {
    let base = directory
        .map(Path::to_path_buf)
        .map(Ok)
        .unwrap_or_else(default_directory)?;
    let mut out = String::new();
    let mut count = 0;
    let mut inspected = 0;
    for mode in ["live", "demo"] {
        let entries = match fs::read_dir(base.join(mode)) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            inspected += 1;
            if inspected > MAX_LIST_ENTRIES {
                out.push_str(
                    "Listing limited to 200 directory entries. Review a known file directly.\n",
                );
                return Ok(out);
            }
            let path = entry?.path();
            if path
                .extension()
                .is_none_or(|extension| extension != "lattice")
            {
                continue;
            }
            if let Ok((_, header)) = open_reader(&path) {
                count += 1;
                // Paths are local output only, not persisted data. Escape controls
                // so a malicious filename cannot inject terminal commands.
                let printable: String = path
                    .to_string_lossy()
                    .chars()
                    .flat_map(char::escape_default)
                    .collect();
                let _ = writeln!(
                    out,
                    "{} · {} · created {:.3}\n  {}",
                    header.name,
                    if header.demo { "SIMULATED" } else { "LIVE" },
                    header.created,
                    printable
                );
            }
        }
    }
    if count == 0 {
        out.push_str("No recordings yet. Start explicitly with lattice --record CalHacks.\n");
    }
    Ok(out)
}

fn csv_text(value: &str) -> String {
    let mut value = value.to_owned();
    if value.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        value.insert(0, '\'');
    }
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// A CSV can be larger than its compact input. Keep that expansion bounded too.
struct LimitedExport {
    file: File,
    written: u64,
    limit: u64,
}
impl Write for LimitedExport {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.written.saturating_add(bytes.len() as u64) > self.limit {
            return Err(io::Error::other(
                "CSV export exceeded its 1 GiB limit; the partial export was retained",
            ));
        }
        let count = self.file.write(bytes)?;
        self.written += count as u64;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// One row per saved system sample plus rows for its saved family contributors.
/// create_new never overwrites a file or follows an existing output symlink.
pub fn export_csv(path: &Path, output: &Path) -> io::Result<()> {
    let (reader, header) = open_reader(path)?;
    let mut file = BufWriter::with_capacity(
        64 * 1024,
        LimitedExport {
            file: new_private_file(output)?,
            written: 0,
            limit: MAX_EXPORT_BYTES,
        },
    );
    let result = (|| -> io::Result<()> {
        writeln!(
            file,
            "recording,mode,row_kind,sample,wall_unix_seconds,elapsed_monotonic_seconds,gap_before,system_cpu_normalized_percent,system_memory_total_bytes,system_memory_available_bytes,swap_used_bytes,scope_summed_rss_bytes,scope_families,omitted_families,partial_scan,all_families,pid,process_start_unix_seconds,family_name,family_summed_rss_bytes,family_cpu_core_percent,recording_gap_markers,recording_runtime_seconds"
        )?;
        let name = csv_text(&header.name);
        let mode = if header.demo { "SIMULATED" } else { "LIVE" };
        let scan = scan_records(reader, |sample| {
            let cpu = sample.cpu.map(|v| format!("{v:.3}")).unwrap_or_default();
            let common = format!(
                "{},{:.3},{:.3},{},{cpu},{},{},{},{},{},{},{},{}",
                sample.sequence,
                sample.wall,
                sample.elapsed,
                u8::from(sample.gap),
                sample.total,
                sample.available,
                sample.swap,
                sample.scope_rss,
                sample.families,
                sample.omitted,
                u8::from(sample.partial),
                u8::from(sample.all)
            );
            writeln!(file, "{name},{mode},system,{common},,,,,,,")?;
            for contributor in &sample.contributors {
                let cpu = contributor
                    .cpu
                    .map(|v| format!("{v:.3}"))
                    .unwrap_or_default();
                writeln!(
                    file,
                    "{name},{mode},family,{common},{},{},{},{},{cpu},,",
                    contributor.key.pid,
                    contributor.key.started,
                    csv_text(&contributor.name),
                    contributor.rss
                )?;
            }
            Ok(())
        })?;
        // A status row prevents a recovered/unfinalized export from looking like
        // a complete recording when opened without its originating .lattice file.
        let status = if scan.recovered_tail {
            "recovered-prefix"
        } else if scan.finished.is_some() {
            "stopped"
        } else {
            "unfinalized"
        };
        // Metadata belongs in named columns on the status row, including a
        // trailing pause with no resumed sample. No footer means unknown runtime.
        let runtime = scan
            .finished
            .map(|seconds| format!("{seconds:.3}"))
            .unwrap_or_default();
        writeln!(
            file,
            "{name},{mode},status-{status},,,,,,,,,,,,,,,,,,,{},{}",
            scan.gaps, runtime
        )?;
        file.flush()?;
        file.get_ref().file.sync_data()
    })();
    // Keep a failed export for diagnosis; never delete a user path automatically.
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProcessSnapshot, SessionSnapshot, SystemSnapshot};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = env::temp_dir().join(format!(
                "lattice-recording-test-{}-{}-{}",
                std::process::id(),
                unix_now().to_bits(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            private_dir(&path).unwrap();
            Self(path)
        }
        fn options(&self, name: &str) -> RecordingOptions {
            RecordingOptions {
                name: name.into(),
                directory: Some(self.0.clone()),
                interval: DEFAULT_RECORD_INTERVAL,
                sampling_interval: Duration::from_secs(5),
            }
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn snapshot(at: f64) -> Snapshot {
        Snapshot {
            timestamp: 1_800_000_000.0 + at,
            monotonic: 100.0 + at,
            system: SystemSnapshot {
                cpu_percent: Some(25.0),
                memory_total: 16 << 30,
                memory_available: 10 << 30,
                swap_used: 1 << 30,
                ..Default::default()
            },
            sessions: vec![family(42, 1000, "codex", 1 << 30, Some(120.0))],
            ..Default::default()
        }
    }
    fn family(pid: u32, started: u64, label: &str, rss: u64, cpu: Option<f32>) -> SessionSnapshot {
        let key = ProcessKey { pid, started };
        SessionSnapshot {
            key,
            label: label.into(),
            processes: vec![ProcessSnapshot {
                key,
                rss_bytes: rss,
                cpu_percent: cpu,
                ..Default::default()
            }],
            ..Default::default()
        }
    }
    fn append_raw(path: &Path, data: &[u8]) {
        OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(data)
            .unwrap();
    }

    #[test]
    fn records_at_bounded_interval_then_stops_cleanly() {
        let temp = Temp::new();
        let mut recorder = Recorder::start(temp.options("CalHacks 2026"), &snapshot(0.0)).unwrap();
        assert_eq!(recorder.info().samples, 1);
        for at in 1..=12 {
            recorder.observe(&snapshot(at as f64)).unwrap();
        }
        assert_eq!(recorder.info().samples, 3);
        let path = recorder.info().path;
        assert_eq!(path.parent().unwrap().file_name().unwrap(), "live");
        assert!(review(&path).unwrap().contains("UNFINALIZED"));
        recorder.finish("user_stop").unwrap();
        assert!(recorder.info().stopped);
        recorder.observe(&snapshot(30.0)).unwrap();
        assert_eq!(recorder.info().samples, 3);
        let result = review(&path).unwrap();
        assert!(result.contains("STOPPED (clean footer)"));
        assert!(result.contains("Recorded samples: 3"));
        assert!(result.contains("sampled mean 25.0%"));
        assert!(result.contains("CPU mean 120.0% / peak 120.0%"));
        assert!(result.contains("not time-weighted totals"));
    }

    #[test]
    fn pause_and_sleep_are_gaps_not_interpolated_samples() {
        let temp = Temp::new();
        let mut recorder = Recorder::start(temp.options("CalHacks"), &snapshot(0.0)).unwrap();
        recorder.mark_gap("pause").unwrap();
        recorder.mark_gap("resume").unwrap();
        recorder.observe(&snapshot(100.0)).unwrap();
        let mut after_sleep = snapshot(105.0);
        after_sleep.timestamp += 3600.0;
        recorder.observe(&after_sleep).unwrap();
        let result = review(&recorder.info().path).unwrap();
        assert!(result.contains("Recorded samples: 3"));
        assert!(result.contains("Gap markers: 2"));
        assert!(result.contains("No CPU-hours"));
    }

    #[test]
    fn interrupted_or_torn_tail_keeps_complete_prefix() {
        let temp = Temp::new();
        let recorder = Recorder::start(temp.options("CalHacks"), &snapshot(0.0)).unwrap();
        let path = recorder.info().path;
        drop(recorder);
        assert!(review(&path).unwrap().contains("UNFINALIZED"));
        append_raw(&path, b"S\t2\t1800000005\t5.0\t0");
        let result = review(&path).unwrap();
        assert!(result.contains("RECOVERED PREFIX"));
        assert!(result.contains("Recorded samples: 1"));
        export_csv(&path, &temp.0.join("recovered.csv")).unwrap();
        assert!(
            fs::read_to_string(temp.0.join("recovered.csv"))
                .unwrap()
                .contains("status-recovered-prefix")
        );
    }

    #[test]
    fn stops_at_first_malformed_record_not_later_valid_looking_rows() {
        let temp = Temp::new();
        let recorder = Recorder::start(temp.options("CalHacks"), &snapshot(0.0)).unwrap();
        let path = recorder.info().path;
        drop(recorder);
        append_raw(&path, b"bad\t\x1b[31msecret\nF\t1\tapp_exit\n");
        let result = review(&path).unwrap();
        assert!(result.contains("RECOVERED PREFIX"));
        assert!(!result.contains("secret"));
        assert!(!result.contains('\x1b'));
    }

    #[test]
    fn line_size_and_file_size_are_bounded_on_read() {
        let temp = Temp::new();
        let recorder = Recorder::start(temp.options("CalHacks"), &snapshot(0.0)).unwrap();
        let path = recorder.info().path;
        drop(recorder);
        append_raw(&path, &vec![b'X'; MAX_LINE_BYTES + 100]);
        assert!(review(&path).unwrap().contains("RECOVERED PREFIX"));
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        assert!(review(&path).is_err());
    }

    #[test]
    fn storage_limit_stops_all_future_writes_and_preserves_prefix() {
        let temp = Temp::new();
        let mut recorder = Recorder::start(temp.options("CalHacks"), &snapshot(0.0)).unwrap();
        let path = recorder.info().path;
        let before = fs::metadata(&path).unwrap().len();
        recorder.byte_limit = before;
        assert!(recorder.observe(&snapshot(5.0)).is_err());
        assert!(recorder.info().stopped);
        recorder.observe(&snapshot(10.0)).unwrap();
        recorder.mark_gap("pause").unwrap();
        recorder.finish("user_stop").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), before);
        assert!(review(&path).unwrap().contains("Recorded samples: 1"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn disk_full_stops_recorder_without_losing_prior_samples() {
        let temp = Temp::new();
        let mut recorder = Recorder::start(temp.options("CalHacks"), &snapshot(0.0)).unwrap();
        let path = recorder.info().path;
        recorder.file = OpenOptions::new().write(true).open("/dev/full").unwrap();
        assert!(recorder.observe(&snapshot(5.0)).is_err());
        assert!(recorder.info().stopped);
        recorder.observe(&snapshot(10.0)).unwrap();
        assert!(review(&path).unwrap().contains("Recorded samples: 1"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_finish_is_stopped_and_does_not_retry() {
        let temp = Temp::new();
        let mut recorder = Recorder::start(temp.options("CalHacks"), &snapshot(0.0)).unwrap();
        recorder.file = OpenOptions::new().write(true).open("/dev/full").unwrap();
        assert!(recorder.finish("app_exit").is_err());
        assert!(recorder.info().stopped);
        assert!(recorder.finish("app_exit").is_ok());
    }

    #[test]
    fn sanitized_basenames_and_formula_safe_csv_no_overwrites() {
        let temp = Temp::new();
        let mut sample = snapshot(0.0);
        sample.sessions[0].label = "/private/never-save-this/+cmd\x1b\n".into();
        let mut recorder = Recorder::start(temp.options("-formula"), &sample).unwrap();
        recorder.finish("done").unwrap();
        let source = fs::read_to_string(&recorder.info().path).unwrap();
        assert!(!source.contains("/private/"));
        assert!(!source.contains("never-save-this"));
        assert!(!source.contains('\x1b'));
        let out = temp.0.join("usage.csv");
        export_csv(&recorder.info().path, &out).unwrap();
        let csv = fs::read_to_string(&out).unwrap();
        assert!(csv.contains("\"'-formula\""));
        assert!(csv.contains("\"'+cmd\""));
        let before = fs::read(&out).unwrap();
        assert_eq!(
            export_csv(&recorder.info().path, &out).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&out).unwrap(), before);
        for line in csv.lines() {
            assert_eq!(line.split(',').count(), 23, "{line}");
        }
    }

    #[test]
    fn demo_files_segregated_from_live_and_labeled_in_every_export() {
        let temp = Temp::new();
        let mut sample = snapshot(0.0);
        sample.demo = true;
        let recorder = Recorder::start(temp.options("CalHacks"), &sample).unwrap();
        let path = recorder.info().path;
        assert_eq!(path.parent().unwrap().file_name().unwrap(), "demo");
        assert!(
            review(&path)
                .unwrap()
                .contains("SIMULATED DATA, not your machine")
        );
        let csv = temp.0.join("demo.csv");
        export_csv(&path, &csv).unwrap();
        for row in fs::read_to_string(csv).unwrap().lines().skip(1) {
            assert!(row.contains(",SIMULATED,"));
        }
        assert!(list(Some(&temp.0)).unwrap().contains("SIMULATED"));
    }

    #[test]
    fn contributors_top32_with_explicit_omission_and_pid_start_identity() {
        let temp = Temp::new();
        let mut sample = snapshot(0.0);
        sample.sessions = (0..100)
            .map(|i| family(i + 1, 1000, "codex", (i + 1) as u64 * 100, Some(i as f32)))
            .collect();
        let mut recorder = Recorder::start(temp.options("CalHacks"), &sample).unwrap();
        let path = recorder.info().path;
        let (reader, _) = open_reader(&path).unwrap();
        scan_records(reader, |saved| {
            assert_eq!(saved.contributors.len(), 32);
            assert_eq!(saved.omitted, 68);
            assert_eq!(saved.contributors[0].key.pid, 100);
            assert_eq!(saved.contributors[31].key.pid, 69);
            assert_eq!(saved.scope_rss, 505_000);
            Ok(())
        })
        .unwrap();
        sample.monotonic += 5.0;
        sample.timestamp += 5.0;
        sample.sessions[99].key.started += 1;
        recorder.observe(&sample).unwrap();
        let result = review(&path).unwrap();
        assert!(result.contains("PID 100/start 1000"));
        assert!(result.contains("PID 100/start 1001"));
        assert!(result.contains("omitted family observations: 136"));
    }

    #[test]
    fn absent_unknown_cpu_is_not_counted_as_zero() {
        let temp = Temp::new();
        let mut initial = snapshot(0.0);
        initial.system.cpu_percent = None;
        initial.sessions[0].processes[0].cpu_percent = None;
        let mut recorder = Recorder::start(temp.options("CalHacks"), &initial).unwrap();
        let mut later = snapshot(5.0);
        later.system.cpu_percent = Some(80.0);
        later.sessions[0].processes[0].cpu_percent = Some(240.0);
        recorder.observe(&later).unwrap();
        let result = review(&recorder.info().path).unwrap();
        assert!(result.contains("sampled mean 80.0%"));
        assert!(result.contains("CPU mean 240.0% / peak 240.0%"));
    }

    #[test]
    fn malformed_header_duplicate_keys_nan_and_sequence_jump_rejected() {
        assert!(parse_header("LATTICE_RECORDING\t2\tName\tLIVE\t1\t5\n").is_err());
        assert!(parse_header("LATTICE_RECORDING\t1\tName\tLIVE\tNaN\t5\n").is_err());
        assert!(parse_record("S\t1\t1\t0\t0\tNaN\t0\t0\t0\t0\t0\t0\t0\t0\t\n").is_err());
        assert!(
            parse_record("S\t1\t1\t0\t0\t0\t0\t0\t0\t0\t2\t0\t0\t0\t1,1,codex,0,-;1,1,codex,0,-\n")
                .is_err()
        );
        let temp = Temp::new();
        let recorder = Recorder::start(temp.options("CalHacks"), &snapshot(0.0)).unwrap();
        let path = recorder.info().path;
        drop(recorder);
        append_raw(
            &path,
            b"S\t99\t1800000005\t5\t0\t0\t0\t0\t0\t0\t0\t0\t0\t0\t\n",
        );
        assert!(review(&path).unwrap().contains("RECOVERED PREFIX"));
    }

    #[test]
    fn long_running_review_has_bounded_identity_memory() {
        let temp = Temp::new();
        let mut recorder = Recorder::start(temp.options("CalHacks"), &snapshot(0.0)).unwrap();
        for i in 1..1030 {
            let mut sample = snapshot(i as f64 * 5.0);
            sample.sessions[0].key.started = 1000 + i;
            recorder.observe(&sample).unwrap();
        }
        let result = review(&recorder.info().path).unwrap();
        assert!(result.contains("Review identity cap (1024) omitted 6 family observations"));
    }

    #[test]
    fn normal_worst_case_default_rows_fit_48_hours_below_cap() {
        let temp = Temp::new();
        let mut sample = snapshot(0.0);
        sample.system.memory_total = u64::MAX;
        sample.system.memory_available = u64::MAX;
        sample.system.swap_used = u64::MAX;
        sample.system.cpu_percent = Some(f32::MAX);
        sample.sessions = (0..32)
            .map(|i| {
                family(
                    u32::MAX - i,
                    u64::MAX,
                    &"a".repeat(40),
                    u64::MAX,
                    Some(f32::MAX),
                )
            })
            .collect();
        let recorder = Recorder::start(temp.options(&"n".repeat(64)), &sample).unwrap();
        let source = fs::read_to_string(recorder.info().path).unwrap();
        let sample_size = source
            .lines()
            .find(|line| line.starts_with("S\t"))
            .unwrap()
            .len() as u64
            + 32;
        assert!(sample_size * (48 * 3600 / 5 + 1) < MAX_FILE_BYTES);
    }

    #[cfg(unix)]
    #[test]
    fn private_permissions_and_existing_symlink_refusal() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = Temp::new();
        let recorder = Recorder::start(temp.options("CalHacks"), &snapshot(0.0)).unwrap();
        let path = recorder.info().path;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let link = temp.0.join("linked.csv");
        symlink(&path, &link).unwrap();
        assert!(export_csv(&path, &link).is_err());
        assert!(review(&link).is_err());
        let target = temp.0.join("other");
        fs::create_dir(&target).unwrap();
        let base_link = temp.0.join("record-link");
        symlink(&target, &base_link).unwrap();
        let mut options = temp.options("CalHacks");
        options.directory = Some(base_link);
        assert!(Recorder::start(options, &snapshot(0.0)).is_err());
    }

    #[test]
    fn invalid_names_and_intervals_fail_before_creating_files() {
        let temp = Temp::new();
        for name in ["", "\n", "../../private", "=SUM(A1)", "hello\x1b[31m"] {
            assert!(Recorder::start(temp.options(name), &snapshot(0.0)).is_err());
        }
        let mut options = temp.options("CalHacks");
        options.interval = Duration::ZERO;
        assert!(Recorder::start(options, &snapshot(0.0)).is_err());
        assert_eq!(fs::read_dir(&temp.0).unwrap().count(), 0);
    }
    #[test]
    fn unknown_ram_is_not_reported_as_a_measured_zero() {
        let temp = Temp::new();
        let mut sample = snapshot(0.0);
        sample.system.memory_total = 0;
        sample.system.memory_available = 0;
        let recorder = Recorder::start(temp.options("CalHacks"), &sample).unwrap();
        assert!(
            review(&recorder.info().path)
                .unwrap()
                .contains("System used RAM peak: unknown")
        );
    }

    #[test]
    fn numeric_mean_and_export_size_cannot_overflow() {
        let mut stats = Stats::default();
        stats.add(0, Some(f64::MAX));
        stats.add(0, Some(f64::MAX));
        assert!(stats.cpu_mean.is_finite());
        let temp = Temp::new();
        let path = temp.0.join("limited.csv");
        let mut writer = LimitedExport {
            file: new_private_file(&path).unwrap(),
            written: 0,
            limit: 4,
        };
        writer.write_all(b"1234").unwrap();
        assert!(writer.write_all(b"5").is_err());
        assert_eq!(fs::read(path).unwrap(), b"1234");
    }
    #[test]
    fn cpu_after_gap_is_unknown_not_included_in_means_peaks_or_export() {
        let temp = Temp::new();
        let mut initial = snapshot(0.0);
        initial.system.cpu_percent = Some(10.0);
        initial.sessions[0].processes[0].cpu_percent = Some(20.0);
        let mut recorder = Recorder::start(temp.options("CalHacks"), &initial).unwrap();
        recorder.mark_gap("pause").unwrap();
        let mut after = snapshot(100.0);
        after.system.cpu_percent = Some(99.0);
        after.sessions[0].processes[0].cpu_percent = Some(990.0);
        recorder.observe(&after).unwrap();
        let path = recorder.info().path;
        let result = review(&path).unwrap();
        assert!(result.contains("sampled mean 10.0% · sampled peak 10.0%"));
        assert!(result.contains("CPU mean 20.0% / peak 20.0%"));
        let (reader, _) = open_reader(&path).unwrap();
        scan_records(reader, |sample| {
            if sample.sequence == 2 {
                assert!(sample.gap);
                assert_eq!(sample.cpu, None);
                assert!(sample.contributors.iter().all(|c| c.cpu.is_none()));
            }
            Ok(())
        })
        .unwrap();
        let csv = temp.0.join("gap.csv");
        export_csv(&path, &csv).unwrap();
        for line in fs::read_to_string(csv).unwrap().lines().skip(1) {
            let fields: Vec<_> = line.split(',').collect();
            if fields[3] == "2" {
                assert_eq!(fields[7], "");
                assert_eq!(fields[20], "");
            }
        }
    }
    #[test]
    fn export_preserves_trailing_pause_and_known_runtime_in_named_metadata() {
        let temp = Temp::new();
        let mut recorder = Recorder::start(temp.options("CalHacks"), &snapshot(0.0)).unwrap();
        recorder.mark_gap("pause").unwrap();
        let path = recorder.info().path;
        drop(recorder);
        // Simulate the clean footer of an hour-long run ending while paused.
        append_raw(&path, b"F\t3600.000\tuser_stop\n");
        let output = temp.0.join("paused.csv");
        export_csv(&path, &output).unwrap();
        let csv = fs::read_to_string(output).unwrap();
        let columns: Vec<_> = csv.lines().next().unwrap().split(',').collect();
        assert_eq!(columns.len(), 23);
        assert_eq!(columns[21], "recording_gap_markers");
        assert_eq!(columns[22], "recording_runtime_seconds");
        let status: Vec<_> = csv.lines().last().unwrap().split(',').collect();
        assert_eq!(status.len(), 23);
        assert_eq!(status[2], "status-stopped");
        assert_eq!(status[21], "1");
        assert_eq!(status[22], "3600.000");
        for row in csv.lines().skip(1).filter(|row| !row.contains(",status-")) {
            let fields: Vec<_> = row.split(',').collect();
            assert_eq!(fields[21], "");
            assert_eq!(fields[22], "");
        }
    }

    #[test]
    fn unfinished_export_metadata_never_invents_a_runtime() {
        let temp = Temp::new();
        let mut recorder = Recorder::start(temp.options("CalHacks"), &snapshot(0.0)).unwrap();
        recorder.mark_gap("pause").unwrap();
        let path = recorder.info().path;
        drop(recorder);
        for (suffix, recovered) in [("open", false), ("recovered", true)] {
            if recovered {
                append_raw(&path, b"F\t3600");
            }
            let output = temp.0.join(format!("{suffix}.csv"));
            export_csv(&path, &output).unwrap();
            let csv = fs::read_to_string(output).unwrap();
            let status: Vec<_> = csv.lines().last().unwrap().split(',').collect();
            assert_eq!(status.len(), 23);
            assert_eq!(
                status[2],
                if recovered {
                    "status-recovered-prefix"
                } else {
                    "status-unfinalized"
                }
            );
            assert_eq!(status[21], "1");
            assert_eq!(status[22], "");
        }
    }
    #[test]
    fn slow_expected_sampling_is_not_a_gap_even_with_faster_persistence_target() {
        let temp = Temp::new();
        let mut options = temp.options("CalHacks");
        options.interval = Duration::from_secs(5);
        options.sampling_interval = Duration::from_secs(30);
        let mut recorder = Recorder::start(options, &snapshot(0.0)).unwrap();
        recorder.observe(&snapshot(30.0)).unwrap();
        recorder.observe(&snapshot(60.0)).unwrap();
        let path = recorder.info().path;
        let result = review(&path).unwrap();
        assert!(result.contains("Recorded samples: 3"));
        assert!(result.contains("Gap markers: 0"));
        let (reader, _) = open_reader(&path).unwrap();
        scan_records(reader, |sample| {
            assert!(!sample.gap);
            assert_eq!(sample.cpu, Some(25.0));
            assert!(sample.contributors.iter().all(|c| c.cpu == Some(120.0)));
            Ok(())
        })
        .unwrap();
        // A wall/monotonic clock disagreement is still a gap at this cadence.
        let mut drift = snapshot(90.0);
        drift.timestamp += 60.0;
        recorder.observe(&drift).unwrap();
        let (reader, _) = open_reader(&path).unwrap();
        scan_records(reader, |sample| {
            if sample.sequence == 4 {
                assert!(sample.gap);
                assert_eq!(sample.cpu, None);
            }
            Ok(())
        })
        .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sync_failure_stops_all_further_recording() {
        let temp = Temp::new();
        let mut recorder = Recorder::start(temp.options("Sync failure"), &snapshot(0.0)).unwrap();
        let original = recorder.info().path;
        // /dev/null accepts write/flush, but sync_data is invalid on Linux. This
        // covers the separate durability failure after a successful write.
        recorder.file = OpenOptions::new().write(true).open("/dev/null").unwrap();
        recorder.last_sync = Instant::now() - Duration::from_secs(31);
        let error = recorder.observe(&snapshot(5.0)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(recorder.info().stopped);
        assert_eq!(recorder.info().samples, 1);
        recorder.observe(&snapshot(10.0)).unwrap();
        assert!(review(&original).unwrap().contains("Recorded samples: 1"));
    }
}
