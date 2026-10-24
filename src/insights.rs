//! Presentation-neutral resource observations, not forecasts or OS pressure.
//!
//! The tracker only consumes snapshots. It performs no I/O, takes no actions on
//! processes, and never substitutes a configured interval for an observed time.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::model::{HISTORY_LIMIT, ProcessKey, Snapshot};

/// Available-memory thresholds have recovery margins to avoid noisy flapping.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HeadroomLevel {
    #[default]
    Unknown,
    Healthy,
    Warning,
    Critical,
}

impl HeadroomLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Healthy => "healthy",
            Self::Warning => "low headroom",
            Self::Critical => "very low headroom",
        }
    }
}

/// Persistent current status. This is an available-memory estimate, not native
/// OS memory pressure; a level alone says nothing about a process's future.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HeadroomStatus {
    pub level: HeadroomLevel,
    /// None means the backend did not provide a nonzero total. Otherwise 0–100.
    pub available_percent: Option<f64>,
}

/// Emitted once when the level changes, not on every snapshot at that level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeadroomTransition {
    pub from: HeadroomLevel,
    pub to: HeadroomLevel,
}

/// Net change over the retained, actually timestamped observations of a session.
/// A positive value means RSS increased; it does not identify a memory leak.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Growth {
    /// Last minus first RSS, clamped to the representable signed range.
    pub delta_bytes: i64,
    /// Net bytes / observed elapsed seconds, not an instantaneous derivative.
    /// None while priming or when the rate cannot be represented finitely.
    pub bytes_per_second: Option<f64>,
    /// The time span covered by this delta; None with fewer than two points.
    pub elapsed_seconds: Option<f64>,
    pub sample_count: usize,
}

#[derive(Clone, Copy, Debug)]
struct Point {
    at: f64,
    rss: u64,
}

#[derive(Debug, Default)]
struct Trace {
    points: VecDeque<Point>,
}

impl Trace {
    fn record(&mut self, at: f64, rss: u64) {
        // Rendering/replaying the same snapshot must not manufacture samples.
        // A corrected observation at the same time replaces the latest value.
        if let Some(last) = self.points.back_mut()
            && last.at == at
        {
            last.rss = rss;
            return;
        }
        if self.points.len() == HISTORY_LIMIT {
            self.points.pop_front();
        }
        self.points.push_back(Point { at, rss });
    }

    fn growth(&self) -> Option<Growth> {
        let first = self.points.front()?;
        let last = self.points.back()?;
        let delta = i128::from(last.rss) - i128::from(first.rss);
        let elapsed = last.at - first.at;
        let elapsed_seconds =
            (self.points.len() >= 2 && elapsed > 0.0 && elapsed.is_finite()).then_some(elapsed);
        // Calculate from the full-width delta before clamping the public integer
        // value, so extreme inputs do not wrap or corrupt the normalized rate.
        let bytes_per_second = elapsed_seconds.and_then(|seconds| {
            let rate = delta as f64 / seconds;
            rate.is_finite().then_some(rate)
        });
        Some(Growth {
            delta_bytes: clamp_delta(delta),
            bytes_per_second,
            elapsed_seconds,
            sample_count: self.points.len(),
        })
    }
}

/// Bounded in-memory history for live session roots and a stable headroom state.
/// Histories are keyed by PID *and* process start time, so reused PIDs do not
/// inherit a previous process's measurements.
#[derive(Debug, Default)]
pub struct Insights {
    headroom: HeadroomStatus,
    sessions: HashMap<ProcessKey, Trace>,
    last_monotonic: Option<f64>,
}

impl Insights {
    /// Consume a real sample. Returns only level transitions; use `headroom()`
    /// for the persistent banner instead of queuing one alert for every sample.
    pub fn update(&mut self, snapshot: &Snapshot) -> Option<HeadroomTransition> {
        let transition = self.update_headroom(
            snapshot.system.memory_total,
            snapshot.system.memory_available,
        );
        let at = snapshot.monotonic;
        if !at.is_finite() || at < 0.0 {
            // An unknown timestamp is an observation gap, not a zero-duration
            // sample. Do not bridge it with a later apparently valid timestamp.
            self.mark_gap();
            return transition;
        }
        if self.last_monotonic.is_some_and(|previous| at < previous) {
            // The sampler may have restarted or changed its clock origin.
            self.mark_gap();
        }
        let active: HashSet<ProcessKey> = snapshot
            .sessions
            .iter()
            .map(|session| session.key)
            .collect();
        self.sessions.retain(|key, _| active.contains(key));
        for session in &snapshot.sessions {
            self.sessions
                .entry(session.key)
                .or_default()
                .record(at, session.rss_bytes());
        }
        self.last_monotonic = Some(at);
        transition
    }

    /// Break growth continuity after pausing/resuming or changing data sources.
    /// The next sample is a fresh baseline. Headroom remains visible and stable.
    /// Call this explicitly for a known gap; arbitrary real sampling delays do
    /// not need a reset because rates use the full actual elapsed time.
    pub fn mark_gap(&mut self) {
        self.sessions.clear();
        self.last_monotonic = None;
    }

    pub fn headroom(&self) -> HeadroomStatus {
        self.headroom
    }

    /// None for absent sessions or immediately after `mark_gap()`. The first
    /// observation has zero delta and no rate; history-only UIs may separately
    /// use `rss_delta(&session.history)` without inventing a duration for it.
    pub fn growth(&self, key: ProcessKey) -> Option<Growth> {
        self.sessions.get(&key).and_then(Trace::growth)
    }

    fn update_headroom(&mut self, total: u64, available: u64) -> Option<HeadroomTransition> {
        let previous = self.headroom.level;
        let available = available.min(total);
        let level = if total == 0 {
            HeadroomLevel::Unknown
        } else {
            // Integer cross-products preserve inclusive threshold boundaries,
            // even for tiny totals and u64-scale values; u128 cannot overflow.
            let available_scaled = u128::from(available) * 100;
            let at_most = |percent: u128| available_scaled <= u128::from(total) * percent;
            let at_least = |percent: u128| available_scaled >= u128::from(total) * percent;
            match previous {
                HeadroomLevel::Unknown | HeadroomLevel::Healthy => {
                    if at_most(10) {
                        HeadroomLevel::Critical
                    } else if at_most(20) {
                        HeadroomLevel::Warning
                    } else {
                        HeadroomLevel::Healthy
                    }
                }
                HeadroomLevel::Warning => {
                    if at_most(10) {
                        HeadroomLevel::Critical
                    } else if at_least(23) {
                        HeadroomLevel::Healthy
                    } else {
                        HeadroomLevel::Warning
                    }
                }
                HeadroomLevel::Critical => {
                    if at_least(23) {
                        HeadroomLevel::Healthy
                    } else if at_least(13) {
                        HeadroomLevel::Warning
                    } else {
                        HeadroomLevel::Critical
                    }
                }
            }
        };
        self.headroom = HeadroomStatus {
            level,
            available_percent: (total != 0).then(|| available as f64 / total as f64 * 100.0),
        };
        (previous != level).then_some(HeadroomTransition {
            from: previous,
            to: level,
        })
    }
}

/// Signed oldest-to-newest RSS change with no assumptions about sampling time.
/// Empty histories have no delta; a single observation has zero delta.
pub fn rss_delta(history: &[u64]) -> Option<i64> {
    Some(clamp_delta(
        i128::from(*history.last()?) - i128::from(*history.first()?),
    ))
}

fn clamp_delta(delta: i128) -> i64 {
    delta.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProcessSnapshot, SessionSnapshot, SystemSnapshot};

    const KEY: ProcessKey = ProcessKey { pid: 7, started: 1 };

    fn snapshot(at: f64, rss: u64) -> Snapshot {
        Snapshot {
            monotonic: at,
            sessions: vec![SessionSnapshot {
                key: KEY,
                processes: vec![ProcessSnapshot {
                    key: KEY,
                    rss_bytes: rss,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            system: SystemSnapshot {
                memory_total: 100,
                memory_available: 50,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn available(insights: &mut Insights, amount: u64) -> Option<HeadroomTransition> {
        let mut sample = snapshot(0.0, 0);
        sample.system.memory_available = amount;
        insights.update(&sample)
    }

    #[test]
    fn headroom_starts_unknown_and_enters_at_inclusive_thresholds() {
        for (amount, expected) in [
            (100, HeadroomLevel::Healthy),
            (21, HeadroomLevel::Healthy),
            (20, HeadroomLevel::Warning),
            (11, HeadroomLevel::Warning),
            (10, HeadroomLevel::Critical),
            (0, HeadroomLevel::Critical),
        ] {
            let mut insights = Insights::default();
            assert_eq!(insights.headroom(), HeadroomStatus::default());
            assert_eq!(
                available(&mut insights, amount),
                Some(HeadroomTransition {
                    from: HeadroomLevel::Unknown,
                    to: expected,
                })
            );
            assert_eq!(insights.headroom().level, expected);
            assert_eq!(insights.headroom().available_percent, Some(amount as f64));
        }
    }

    #[test]
    fn headroom_recovery_has_hysteresis_and_no_repeated_alerts() {
        let mut insights = Insights::default();
        available(&mut insights, 9);
        for amount in [10, 11, 12, 9, 12] {
            assert_eq!(available(&mut insights, amount), None);
            assert_eq!(insights.headroom().level, HeadroomLevel::Critical);
        }
        assert_eq!(
            available(&mut insights, 13),
            Some(HeadroomTransition {
                from: HeadroomLevel::Critical,
                to: HeadroomLevel::Warning,
            })
        );
        for amount in [20, 21, 22, 11, 20, 22] {
            assert_eq!(available(&mut insights, amount), None);
            assert_eq!(insights.headroom().level, HeadroomLevel::Warning);
        }
        assert_eq!(
            available(&mut insights, 23),
            Some(HeadroomTransition {
                from: HeadroomLevel::Warning,
                to: HeadroomLevel::Healthy,
            })
        );
        for amount in [23, 22, 21, 100] {
            assert_eq!(available(&mut insights, amount), None);
            assert_eq!(insights.headroom().level, HeadroomLevel::Healthy);
        }
    }

    #[test]
    fn headroom_can_recover_directly_or_escalate() {
        let mut insights = Insights::default();
        available(&mut insights, 10);
        assert_eq!(
            available(&mut insights, 23).unwrap().to,
            HeadroomLevel::Healthy
        );
        available(&mut insights, 20);
        assert_eq!(
            available(&mut insights, 10).unwrap().to,
            HeadroomLevel::Critical
        );
    }

    #[test]
    fn missing_total_is_unknown_and_resets_hysteresis() {
        let mut insights = Insights::default();
        available(&mut insights, 10);
        let mut sample = snapshot(0.0, 0);
        sample.system.memory_total = 0;
        sample.system.memory_available = u64::MAX;
        assert_eq!(insights.update(&sample).unwrap().to, HeadroomLevel::Unknown);
        assert_eq!(insights.headroom().available_percent, None);
        assert_eq!(insights.update(&sample), None);
        assert_eq!(
            available(&mut insights, 12).unwrap().to,
            HeadroomLevel::Warning
        );
    }

    #[test]
    fn available_is_clamped_and_large_thresholds_do_not_overflow() {
        let mut insights = Insights::default();
        available(&mut insights, u64::MAX);
        assert_eq!(insights.headroom().available_percent, Some(100.0));
        let mut sample = snapshot(1.0, 0);
        sample.system.memory_total = u64::MAX;
        sample.system.memory_available = u64::MAX / 10;
        insights.update(&sample);
        assert_eq!(insights.headroom().level, HeadroomLevel::Critical);
        let mut fresh = Insights::default();
        sample.system.memory_available += 1;
        fresh.update(&sample);
        assert_eq!(fresh.headroom().level, HeadroomLevel::Warning);
    }

    #[test]
    fn tiny_totals_use_ratios_not_truncated_percentages() {
        let mut insights = Insights::default();
        let mut sample = snapshot(0.0, 0);
        sample.system.memory_total = 9;
        sample.system.memory_available = 1;
        insights.update(&sample);
        assert_eq!(insights.headroom().level, HeadroomLevel::Warning);
        sample.system.memory_available = 0;
        insights.update(&sample);
        assert_eq!(insights.headroom().level, HeadroomLevel::Critical);
    }

    #[test]
    fn growth_primes_without_inventing_a_rate_from_snapshot_history() {
        let mut insights = Insights::default();
        let mut sample = snapshot(12.0, 700);
        sample.sessions[0].history = vec![100, 300, 700];
        insights.update(&sample);
        assert_eq!(
            insights.growth(KEY),
            Some(Growth {
                delta_bytes: 0,
                bytes_per_second: None,
                elapsed_seconds: None,
                sample_count: 1,
            })
        );
        assert_eq!(rss_delta(&sample.sessions[0].history), Some(600));
    }

    #[test]
    fn growth_uses_actual_irregular_sample_times_not_wall_clock() {
        let mut insights = Insights::default();
        insights.update(&snapshot(1.0, 100));
        let mut sample = snapshot(4.0, 700);
        sample.timestamp = -9_000_000.0;
        insights.update(&sample);
        insights.update(&snapshot(10.0, 1_000));
        assert_eq!(
            insights.growth(KEY),
            Some(Growth {
                delta_bytes: 900,
                bytes_per_second: Some(100.0),
                elapsed_seconds: Some(9.0),
                sample_count: 3,
            })
        );
    }

    #[test]
    fn growth_tracks_decreases_and_flat_memory() {
        let mut insights = Insights::default();
        insights.update(&snapshot(0.0, 1_000));
        insights.update(&snapshot(2.0, 600));
        assert_eq!(insights.growth(KEY).unwrap().delta_bytes, -400);
        assert_eq!(insights.growth(KEY).unwrap().bytes_per_second, Some(-200.0));
        insights.mark_gap();
        insights.update(&snapshot(3.0, 600));
        insights.update(&snapshot(6.0, 600));
        assert_eq!(insights.growth(KEY).unwrap().bytes_per_second, Some(0.0));
    }

    #[test]
    fn growth_retains_at_most_sixty_observed_points() {
        let mut insights = Insights::default();
        for index in 0..101 {
            insights.update(&snapshot(index as f64 * 2.0, index * 10));
        }
        let growth = insights.growth(KEY).unwrap();
        assert_eq!(growth.sample_count, HISTORY_LIMIT);
        assert_eq!(growth.delta_bytes, 590);
        assert_eq!(growth.elapsed_seconds, Some(118.0));
        assert_eq!(growth.bytes_per_second, Some(5.0));
    }

    #[test]
    fn duplicate_timestamps_replace_without_inventing_samples() {
        let mut insights = Insights::default();
        insights.update(&snapshot(1.0, 100));
        insights.update(&snapshot(1.0, 300));
        assert_eq!(insights.growth(KEY).unwrap().sample_count, 1);
        assert_eq!(insights.growth(KEY).unwrap().bytes_per_second, None);
        insights.update(&snapshot(3.0, 700));
        insights.update(&snapshot(3.0, 900));
        let growth = insights.growth(KEY).unwrap();
        assert_eq!(growth.sample_count, 2);
        assert_eq!(growth.delta_bytes, 600);
        assert_eq!(growth.bytes_per_second, Some(300.0));
    }

    #[test]
    fn clock_restarts_reset_growth_but_not_headroom() {
        let mut insights = Insights::default();
        insights.update(&snapshot(10.0, 100));
        insights.update(&snapshot(12.0, 700));
        let headroom = insights.headroom();
        insights.update(&snapshot(2.0, 1_000));
        assert_eq!(insights.growth(KEY).unwrap().sample_count, 1);
        assert_eq!(insights.growth(KEY).unwrap().bytes_per_second, None);
        assert_eq!(insights.headroom(), headroom);
        insights.update(&snapshot(4.0, 1_100));
        assert_eq!(insights.growth(KEY).unwrap().bytes_per_second, Some(50.0));
    }

    #[test]
    fn invalid_timestamps_break_continuity() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            let mut insights = Insights::default();
            insights.update(&snapshot(1.0, 100));
            insights.update(&snapshot(2.0, 200));
            insights.update(&snapshot(bad, 900));
            assert_eq!(insights.growth(KEY), None);
            insights.update(&snapshot(4.0, 1_000));
            assert_eq!(insights.growth(KEY).unwrap().sample_count, 1);
            assert_eq!(insights.growth(KEY).unwrap().bytes_per_second, None);
        }
    }

    #[test]
    fn explicit_pause_gap_never_bridges_unobserved_time() {
        let mut insights = Insights::default();
        insights.update(&snapshot(1.0, 100));
        insights.update(&snapshot(3.0, 300));
        let headroom = insights.headroom();
        insights.mark_gap();
        assert_eq!(insights.growth(KEY), None);
        assert_eq!(insights.headroom(), headroom);
        insights.update(&snapshot(500.0, 10_000));
        assert_eq!(insights.growth(KEY).unwrap().delta_bytes, 0);
        assert_eq!(insights.growth(KEY).unwrap().bytes_per_second, None);
        insights.update(&snapshot(503.0, 10_300));
        assert_eq!(insights.growth(KEY).unwrap().bytes_per_second, Some(100.0));
    }

    #[test]
    fn dead_sessions_are_evicted_and_pid_reuse_primes_a_new_trace() {
        let mut insights = Insights::default();
        insights.update(&snapshot(0.0, 100));
        insights.update(&snapshot(1.0, 200));
        let replacement = ProcessKey {
            pid: KEY.pid,
            started: 2,
        };
        let mut sample = snapshot(2.0, 5_000);
        sample.sessions[0].key = replacement;
        insights.update(&sample);
        assert_eq!(insights.growth(KEY), None);
        assert_eq!(insights.sessions.len(), 1);
        assert_eq!(insights.growth(replacement).unwrap().sample_count, 1);
        sample.sessions.clear();
        sample.monotonic = 3.0;
        insights.update(&sample);
        assert!(insights.sessions.is_empty());
    }

    #[test]
    fn reappearing_session_starts_from_new_baseline() {
        let mut insights = Insights::default();
        insights.update(&snapshot(0.0, 100));
        let mut empty = snapshot(1.0, 0);
        empty.sessions.clear();
        insights.update(&empty);
        insights.update(&snapshot(2.0, 1_000));
        assert_eq!(insights.growth(KEY).unwrap().delta_bytes, 0);
        assert_eq!(insights.growth(KEY).unwrap().bytes_per_second, None);
    }

    #[test]
    fn signed_history_deltas_saturate_instead_of_wrapping() {
        assert_eq!(rss_delta(&[]), None);
        assert_eq!(rss_delta(&[7]), Some(0));
        assert_eq!(rss_delta(&[1, 99, 5]), Some(4));
        assert_eq!(rss_delta(&[5, 99, 1]), Some(-4));
        assert_eq!(rss_delta(&[0, u64::MAX]), Some(i64::MAX));
        assert_eq!(rss_delta(&[u64::MAX, 0]), Some(i64::MIN));
        assert_eq!(rss_delta(&[0, i64::MAX as u64]), Some(i64::MAX));
        assert_eq!(rss_delta(&[i64::MAX as u64 + 1, 0]), Some(i64::MIN));
    }

    #[test]
    fn timed_delta_saturates_and_rate_uses_full_width_difference() {
        let mut insights = Insights::default();
        insights.update(&snapshot(0.0, 0));
        insights.update(&snapshot(2.0, u64::MAX));
        assert_eq!(insights.growth(KEY).unwrap().delta_bytes, i64::MAX);
        assert_eq!(
            insights.growth(KEY).unwrap().bytes_per_second,
            Some(u64::MAX as f64 / 2.0)
        );
        insights.mark_gap();
        insights.update(&snapshot(3.0, u64::MAX));
        insights.update(&snapshot(5.0, 0));
        assert_eq!(insights.growth(KEY).unwrap().delta_bytes, i64::MIN);
        assert_eq!(
            insights.growth(KEY).unwrap().bytes_per_second,
            Some(-(u64::MAX as f64) / 2.0)
        );
    }

    #[test]
    fn an_unrepresentable_rate_is_unknown_not_infinity() {
        let mut insights = Insights::default();
        insights.update(&snapshot(0.0, 0));
        insights.update(&snapshot(f64::from_bits(1), 1));
        let growth = insights.growth(KEY).unwrap();
        assert_eq!(growth.delta_bytes, 1);
        assert!(growth.elapsed_seconds.is_some());
        assert_eq!(growth.bytes_per_second, None);
    }
}
