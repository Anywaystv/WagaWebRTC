//! Bandwidth probing controller - decides when and how to probe network capacity.
//!
//! This module implements WebRTC's `ProbeController` state machine for discovering available
//! bandwidth through intentional bursts of packets at rates higher than current estimates.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::{ProbeClusterConfig, ProbeKind};
use crate::rtp_::{Bitrate, TwccClusterId};
use crate::util::{already_happened, not_happening};

// Port notes:
// This module ports WebRTC's `ProbeController` behavior from:
// `webrtc/modules/congestion_controller/goog_cc/probe_controller.cc`
//
// Key integration difference: WebRTC returns vectors of probe clusters, while str0m
// returns a single `ProbeClusterConfig` per `handle_timeout()` call. Configs are queued
// internally and `poll_timeout()` returns `already_happened()` until the queue is drained.

/// WebRTC: `kMaxWaitingTimeForProbingResult`.
const MAX_WAITING_TIME_FOR_PROBING_RESULT: Duration = Duration::from_secs(1);

/// WebRTC: `kBitrateDropThreshold`, `kBitrateDropTimeout`, `kProbeFractionAfterDrop`,
/// `kProbeUncertainty`, `kAlrEndedTimeout`, `kMinTimeBetweenAlrProbes`.
const BITRATE_DROP_THRESHOLD: f64 = 0.66;
const BITRATE_DROP_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_FRACTION_AFTER_DROP: f64 = 0.85;
const PROBE_UNCERTAINTY: f64 = 0.05;
const ALR_ENDED_TIMEOUT: Duration = Duration::from_secs(3);
const MIN_TIME_BETWEEN_ALR_PROBES: Duration = Duration::from_secs(5);

/// WebRTC: inline `* 2` in probe_controller.cc InitiateProbing().
/// Allows probing up to 2x max_bitrate to account for bursty streams.
const MAX_PROBE_BITRATE_FACTOR: f64 = 2.0;

/// Minimum time between stagnant periodic probes to avoid excessive probing when at capacity.
const MIN_TIME_BETWEEN_STAGNANT_PROBES: Duration = Duration::from_secs(15);
const PATH_PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// Threshold for considering an estimate change significant (5%).
const ESTIMATE_CHANGE_THRESHOLD: f64 = 0.05;

/// Probe rate scale for stagnation probes (2× current estimate).
const STAGNANT_PROBE_SCALE: f64 = 2.0;

/// WebRTC's `BandwidthLimitedCause` (subset used by probing gating).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandwidthLimitedCause {
    LossLimitedBweIncreasing,
    LossLimitedBwe,
    DelayBasedLimited,
    DelayBasedLimitedDelayIncreased,
}

pub struct ProbeControl {
    config: Config,
    next_timeout: Instant,
    enabled: bool,

    desired_bitrate: Option<Bitrate>,
    prev_desired: Option<Bitrate>,

    last_estimate: Option<Bitrate>,
    last_cause: BandwidthLimitedCause,

    prev_estimate: Option<Bitrate>,

    alr_start: Option<Instant>,
    alr_stop: Option<Instant>,

    last_probe: Option<LastProbe>,

    large_drop: Option<LargeDrop>,

    next_cluster_id: TwccClusterId,
    pending: VecDeque<ProbeClusterConfig>,
    path_probe_requested: bool,
    last_path_probe_request: Option<Instant>,
    recovery_until: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct LastProbe {
    when: Instant,
    kind: ProbeKind,
    further: Bitrate,
    was_estimate: Option<Bitrate>,
}

struct LargeDrop {
    when: Instant,
    bitrate_before: Bitrate,
}

impl Default for ProbeControl {
    fn default() -> Self {
        Self {
            config: Config::default(),
            enabled: false,
            next_timeout: not_happening(),
            desired_bitrate: None,
            prev_desired: None,
            last_estimate: None,
            last_cause: BandwidthLimitedCause::DelayBasedLimited,
            prev_estimate: None,
            alr_start: None,
            alr_stop: None,
            next_cluster_id: 0.into(),
            last_probe: None,
            large_drop: None,
            pending: VecDeque::new(),
            path_probe_requested: false,
            last_path_probe_request: None,
            recovery_until: None,
        }
    }
}

impl ProbeControl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enable(&mut self, v: bool) {
        if !self.enabled && v {
            self.enabled = true;
            self.request_immediate();
        } else if self.enabled && !v {
            self.enabled = false;
            self.pending.clear();
            self.path_probe_requested = false;
            self.recovery_until = None;
            self.last_estimate = None;
            self.desired_bitrate = None;
            self.last_probe = None;
            self.prev_estimate = None;
            self.next_timeout = not_happening();
        }
    }

    pub fn set_desired_bitrate(&mut self, v: Bitrate) {
        // Don't accept Bitrate::ZERO as first ever value.
        if self.desired_bitrate.is_none() && v.is_zero() {
            return;
        }
        if self.desired_bitrate == Some(v) {
            return;
        }
        self.desired_bitrate = Some(v);
        self.request_immediate();
    }

    pub fn request_path_probe(&mut self, now: Instant) {
        if self.last_path_probe_request.is_some_and(|last| {
            now.saturating_duration_since(last) < PATH_PROBE_INTERVAL
        }) {
            return;
        }
        self.last_path_probe_request = Some(now);
        self.path_probe_requested = true;
        self.request_immediate();
    }

    pub fn set_estimated_bitrate(&mut self, v: Bitrate, cause: BandwidthLimitedCause) {
        // Don't accept Bitrate::ZERO as first ever value.
        if self.last_estimate.is_none() && v.is_zero() {
            return;
        }

        // Check if estimate changed significantly (>5%) or cause changed.
        let dominated_by_last = self.last_estimate.is_some_and(|last| {
            let upper = last * (1.0 + ESTIMATE_CHANGE_THRESHOLD);
            let lower = last * (1.0 - ESTIMATE_CHANGE_THRESHOLD);
            v <= upper && v >= lower
        });

        if dominated_by_last && self.last_cause == cause {
            return;
        }

        self.last_estimate = Some(v);
        self.last_cause = cause;
        self.request_immediate();
    }

    pub fn set_alr_start_time(&mut self, t: Instant) {
        if self.alr_start.is_some() {
            return;
        }
        self.alr_start = Some(t);
        self.alr_stop = None;
        self.request_immediate();
    }

    pub fn set_alr_stop_time(&mut self, t: Instant) {
        if self.alr_start.is_none() || self.alr_stop.is_some() {
            return;
        }
        self.alr_start = None;
        self.alr_stop = Some(t);
        self.request_immediate();
    }

    fn request_immediate(&mut self) {
        self.next_timeout = already_happened();
    }

    pub fn poll_timeout(&self) -> Instant {
        self.next_timeout
    }

    pub(crate) fn diagnostic_snapshot(&self, now: Instant) -> String {
        format!(
            "probe_enabled={} cause={:?} alr={} path_pending={} path_request_age_ms={:?} queued={} last_probe={:?} last_probe_age_ms={:?} next_probe_ms={:?}",
            self.enabled, self.last_cause, self.in_alr(), self.path_probe_requested,
            self.last_path_probe_request.map(|time| now.saturating_duration_since(time).as_millis()),
            self.pending.len(), self.last_probe.map(|probe| probe.kind),
            self.last_probe.map(|probe| now.saturating_duration_since(probe.when).as_millis()),
            (self.next_timeout != not_happening()).then(|| self.next_timeout.saturating_duration_since(now).as_millis()),
        )
    }

    pub fn handle_timeout(&mut self, now: Instant) -> Option<ProbeClusterConfig> {
        // Spurious call before timeout is due - ignore.
        if now < self.next_timeout {
            return None;
        }

        // Timeout fired - reset to not_happening until we compute the next one.
        self.next_timeout = not_happening();

        // Probing is disabled until first packet sent and padding queue exists.
        if !self.enabled {
            return None;
        }

        // We need to have both desired AND last_estimate set to
        // start considering probing.
        let desired = self.desired_bitrate?;
        let estimate = self.last_estimate?;

        // A queued probe's rate may predate the congestion signal. Recheck
        // capacity from the current estimate when probing becomes safe again.
        if !self.can_probe(estimate) {
            self.path_probe_requested |= !self.pending.is_empty();
            self.pending.clear();
            return None;
        }

        // Return pending probes first.
        if let Some(config) = self.pending.pop_front() {
            // Schedule another.
            self.request_immediate();
            return Some(config);
        }

        // Try each probe type in order - only one fires per timeout.
        let _ = self.maybe_initial(now, desired, estimate)
            || self.maybe_path_recovery(now, desired, estimate)
            || self.maybe_exponential(now, desired, estimate)
            || self.maybe_increase_alr(now, desired, estimate)
            || self.maybe_large_drop(now, desired, estimate)
            || self.maybe_periodic_alr(now, desired)
            || self.maybe_stagnant(now, desired, estimate);

        // Update prev_estimate for large-drop detection.
        self.prev_estimate = Some(estimate);

        // Update timeout based on current state.
        self.next_timeout = self.compute_next_timeout(now);

        if !self.pending.is_empty() {
            self.request_immediate();
        }

        self.pending.pop_front()
    }

    fn maybe_path_recovery(&mut self, now: Instant, desired: Bitrate, estimate: Bitrate) -> bool {
        if !std::mem::take(&mut self.path_probe_requested) {
            return false;
        }
        // Congestion gates this request before consumption. Use the current estimate
        // when it clears, not the estimate or elapsed time at path validation.
        if desired <= estimate {
            return false;
        }
        self.recovery_until = Some(now + Duration::from_secs(20));
        self.queue_probe(
            estimate * self.last_cause.probe_scale(&self.config),
            ProbeKind::PathRecovery,
            desired,
            now,
        );
        true
    }

    fn maybe_initial(&mut self, now: Instant, desired: Bitrate, estimate: Bitrate) -> bool {
        // Initial probes only fire once at startup.
        if self.last_probe.is_some() {
            return false;
        }

        // Queue 3× and 6× of estimate.
        let p1 = estimate * self.config.first_exponential_probe_scale;
        let p2 = estimate * self.config.second_exponential_probe_scale;

        self.queue_probe(p1, ProbeKind::Initial, desired, now);
        self.queue_probe(p2, ProbeKind::Initial, desired, now);
        true
    }

    fn maybe_exponential(&mut self, now: Instant, desired: Bitrate, estimate: Bitrate) -> bool {
        // Wait for pending probes to be dispatched first.
        if !self.pending.is_empty() {
            return false;
        }

        // Need a previous probe to continue from.
        let Some(last) = self.last_probe else {
            return false;
        };

        // Estimate must exceed 70% of last probe rate to trigger further probing.
        if estimate < last.further {
            return false;
        }

        let is_same = Some(estimate) == last.was_estimate;
        let time_since = self.time_since_last_probe(now);

        // Don't re-probe at the same estimate; wait for new result or timeout.
        if is_same && time_since < MAX_WAITING_TIME_FOR_PROBING_RESULT {
            return false;
        }

        let scale = self.last_cause.probe_scale(&self.config);
        let target = estimate * scale;

        // Already probed at max rate; no point probing again.
        let max = desired * MAX_PROBE_BITRATE_FACTOR;
        if target >= max && last.further >= max * self.config.further_probe_threshold {
            return false;
        }

        self.queue_probe(target, ProbeKind::Exponential, desired, now);

        true
    }

    fn maybe_increase_alr(&mut self, now: Instant, desired: Bitrate, estimate: Bitrate) -> bool {
        // Don't interfere with initial probing phase.
        if self.is_during_initial(now) {
            return false;
        }

        // Allocation probes only fire in ALR (application-limited region).
        if !self.in_alr() {
            return false;
        }

        let prev = self.prev_desired;
        self.prev_desired = Some(desired);

        // Need a previous desired value to compare against.
        let Some(prev) = prev else {
            return false;
        };

        // Only probe if desired increased
        if desired <= prev {
            return false;
        }

        // No point probing if we already have enough bandwidth.
        if desired <= estimate {
            return false;
        }

        // Allocation probes at 1× and 2× of desired, capped by 2× estimate
        let current_bwe_limit = estimate * self.config.allocation_probe_limit_by_current_scale;

        let p1 = (desired * self.config.first_allocation_probe_scale).min(current_bwe_limit);
        self.queue_probe(p1, ProbeKind::IncreaseAlr, desired, now);

        let p2 = desired * self.config.second_allocation_probe_scale;
        if p2 <= current_bwe_limit && p2 > p1 {
            self.queue_probe(p2, ProbeKind::IncreaseAlr, desired, now);
        }

        true
    }

    fn maybe_periodic_alr(&mut self, now: Instant, desired: Bitrate) -> bool {
        // Don't interfere with initial probing phase.
        if self.is_during_initial(now) {
            return false;
        }

        // Periodic probes only fire in ALR (application-limited region).
        if !self.in_alr() {
            return false;
        }

        // Respect minimum interval between ALR probes.
        if self.time_since_last_probe(now) < MIN_TIME_BETWEEN_ALR_PROBES {
            return false;
        }

        // Periodic ALR probe at 2× desired (capped by queue_probe to 2× desired anyway).
        // Using desired rather than estimate allows discovering higher capacity when
        // the app wants more bandwidth than currently estimated.
        let target = desired * self.config.further_exponential_probe_scale;
        self.queue_probe(target, ProbeKind::PeriodicAlr, desired, now);
        true
    }

    /// Probe unmet demand faster briefly after path recovery, then every 15 seconds.
    /// Anchor this to the last probe, not estimate changes: oscillating low estimates
    /// must not postpone recovery indefinitely. Active congestion is gated by can_probe.
    fn maybe_stagnant(&mut self, now: Instant, desired: Bitrate, estimate: Bitrate) -> bool {
        // Don't interfere with initial probing phase.
        if self.is_during_initial(now) {
            return false;
        }

        // Don't probe in ALR (periodic ALR handles that).
        if self.in_alr() {
            return false;
        }

        if self.time_since_last_probe(now) < self.stagnant_interval(now) {
            return false;
        }

        // Only if there's unmet demand.
        if desired <= estimate {
            return false;
        }

        // Probe at 2× estimate (conservative, won't overwhelm if at capacity).
        let probe_rate = estimate * STAGNANT_PROBE_SCALE;
        self.queue_probe(probe_rate, ProbeKind::Stagnant, desired, now);

        true
    }

    fn maybe_large_drop(&mut self, now: Instant, desired: Bitrate, estimate: Bitrate) -> bool {
        // Don't interfere with initial probing phase.
        if self.is_during_initial(now) {
            return false;
        }

        // Detect large drops: estimate fell below 66% of previous.
        if self.large_drop.is_none() {
            if let Some(prev) = self.prev_estimate {
                if estimate < prev * BITRATE_DROP_THRESHOLD {
                    self.large_drop = Some(LargeDrop {
                        when: now,
                        bitrate_before: prev,
                    });
                }
            }
        }

        // No large drop detected.
        let Some(drop) = &self.large_drop else {
            return false;
        };

        // Drop expires after 5 seconds.
        if now.saturating_duration_since(drop.when) > BITRATE_DROP_TIMEOUT {
            self.large_drop = None;
            return false;
        }

        // Large-drop probing requires ALR context (in ALR or recently exited).
        if !self.in_alr() && !self.alr_ended_recently(now) {
            return false;
        }

        // Respect minimum interval between ALR probes.
        if self.time_since_last_probe(now) < MIN_TIME_BETWEEN_ALR_PROBES {
            return false;
        }

        // Probe at 85% of pre-drop bitrate.
        let target = drop.bitrate_before * PROBE_FRACTION_AFTER_DROP;
        self.queue_probe(target, ProbeKind::LargeDrop, desired, now);

        self.large_drop = None;
        true
    }

    fn queue_probe(&mut self, bitrate: Bitrate, kind: ProbeKind, desired: Bitrate, now: Instant) {
        // Cap at 2× desired bitrate.
        let max = desired * MAX_PROBE_BITRATE_FACTOR;
        let bitrate = bitrate.min(max);

        // No probe at too small values.
        if bitrate < Bitrate::kbps(5) {
            return;
        }

        let cluster_id = self.next_cluster_id.inc();

        let config = ProbeClusterConfig::new(cluster_id, bitrate, kind)
            .with_min_packet_count(self.config.min_probe_packets_sent)
            .with_duration(self.config.min_probe_duration)
            .with_min_probe_delta(self.config.min_probe_delta);

        // Threshold for further exponential probing (probe_bitrate * 0.7).
        let probe_further = bitrate * self.config.further_probe_threshold;

        self.pending.push_back(config);
        self.last_probe = Some(LastProbe {
            when: now,
            kind,
            further: probe_further,
            was_estimate: self.last_estimate,
        });
    }

    fn compute_next_timeout(&self, now: Instant) -> Instant {
        let Some(last) = self.last_probe else {
            return not_happening();
        };
        let result_deadline = last.when + MAX_WAITING_TIME_FOR_PROBING_RESULT;
        if matches!(last.kind, ProbeKind::Initial | ProbeKind::Exponential) && result_deadline > now
        {
            return result_deadline;
        }

        let interval = if self.in_alr() {
            MIN_TIME_BETWEEN_ALR_PROBES
        } else if self.desired_bitrate > self.last_estimate {
            self.stagnant_interval(now)
        } else {
            return not_happening();
        };
        let deadline = last.when + interval;
        if deadline > now {
            deadline
        } else {
            // A suppressed probe must never leave an already-expired timer armed.
            now + interval
        }
    }

    fn can_probe(&self, estimate: Bitrate) -> bool {
        // Infinite estimate indicates no valid measurement yet.
        if estimate == Bitrate::INFINITY {
            return false;
        }

        // Only probe when delay-limited or loss-limited-but-increasing.
        // Don't probe during active congestion (loss-limited, delay-increased).
        matches!(
            self.last_cause,
            BandwidthLimitedCause::LossLimitedBweIncreasing
                | BandwidthLimitedCause::DelayBasedLimited
        )
    }

    fn stagnant_interval(&self, now: Instant) -> Duration {
        if self.recovery_until.is_some_and(|until| now < until) {
            PATH_PROBE_INTERVAL
        } else {
            MIN_TIME_BETWEEN_STAGNANT_PROBES
        }
    }

    fn in_alr(&self) -> bool {
        self.alr_start.is_some() && self.alr_stop.is_none()
    }

    fn alr_ended_recently(&self, now: Instant) -> bool {
        self.alr_stop
            .map(|stop| now.saturating_duration_since(stop) < ALR_ENDED_TIMEOUT)
            .unwrap_or(false)
    }

    fn is_during_initial(&self, now: Instant) -> bool {
        let is_initial = matches!(
            self.last_probe.map(|p| p.kind),
            Some(ProbeKind::Initial) | Some(ProbeKind::Exponential)
        );
        is_initial && self.time_since_last_probe(now) <= MAX_WAITING_TIME_FOR_PROBING_RESULT
    }

    fn last_when(&self) -> Option<Instant> {
        self.last_probe.map(|p| p.when)
    }

    fn time_since_last_probe(&self, now: Instant) -> Duration {
        self.last_when()
            .map(|t| now.saturating_duration_since(t))
            .unwrap_or(Duration::MAX)
    }
}

/// Configuration using WebRTC default constants (no field-trial plumbing).
#[derive(Debug, Clone, Copy)]
struct Config {
    // Initial/exponential probing
    first_exponential_probe_scale: f64,   // p1 = 3.0
    second_exponential_probe_scale: f64,  // p2 = 6.0
    further_exponential_probe_scale: f64, // step_size = 2.0
    further_probe_threshold: f64,         // 0.7

    // Allocation probing
    first_allocation_probe_scale: f64,            // 1.0
    second_allocation_probe_scale: f64,           // 2.0
    allocation_probe_limit_by_current_scale: f64, // 2.0

    // Probe cluster config defaults
    min_probe_packets_sent: usize, // 5
    min_probe_duration: Duration,  // 15ms
    min_probe_delta: Duration,     // 2ms

    // Gating / limits
    loss_limited_probe_scale: f64, // 1.5
}

impl Default for Config {
    fn default() -> Self {
        Self {
            first_exponential_probe_scale: 3.0,
            second_exponential_probe_scale: 6.0,
            further_exponential_probe_scale: 2.0,
            further_probe_threshold: 0.7,

            first_allocation_probe_scale: 1.0,
            second_allocation_probe_scale: 2.0,
            allocation_probe_limit_by_current_scale: 2.0,

            min_probe_packets_sent: 5,
            min_probe_duration: Duration::from_millis(15),
            min_probe_delta: Duration::from_millis(2),

            loss_limited_probe_scale: 1.5,
        }
    }
}

impl BandwidthLimitedCause {
    /// Probe scale factor for exponential probing.
    ///
    /// When loss-limited but increasing, use a more conservative 1.575× (1.5 * 1.05).
    /// Otherwise use the standard 2× scale.
    fn probe_scale(&self, config: &Config) -> f64 {
        match self {
            BandwidthLimitedCause::LossLimitedBweIncreasing => {
                config.loss_limited_probe_scale * (1.0 + PROBE_UNCERTAINTY)
            }
            _ => config.further_exponential_probe_scale,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn path_recovery_retries_are_faster_but_bounded() {
        let now = Instant::now();
        let mut pc = ProbeControl::new();
        pc.enable(true);
        pc.set_desired_bitrate(Bitrate::mbps(5));
        pc.set_estimated_bitrate(Bitrate::kbps(250), BandwidthLimitedCause::DelayBasedLimited);
        pc.handle_timeout(now);
        pc.handle_timeout(now);
        let restored = now + Duration::from_secs(2);
        pc.request_path_probe(restored);
        assert!(pc.handle_timeout(restored).is_some());
        pc.handle_timeout(restored);
        assert_eq!(pc.poll_timeout(), restored + Duration::from_secs(5));
        for second in [5, 10, 15] {
            assert!(pc.handle_timeout(restored + Duration::from_secs(second)).is_some());
            pc.handle_timeout(restored + Duration::from_secs(second));
        }
        assert!(pc.handle_timeout(restored + Duration::from_secs(20)).is_none());
        assert_eq!(pc.poll_timeout(), restored + Duration::from_secs(30));
        pc.set_estimated_bitrate(Bitrate::kbps(250), BandwidthLimitedCause::LossLimitedBwe);
        assert!(pc.handle_timeout(restored + Duration::from_secs(30)).is_none());
        assert_eq!(pc.poll_timeout(), not_happening());
    }

    #[test]
    fn congestion_discards_queued_probe_rates_and_rechecks_capacity_afterwards() {
        for cause in [
            BandwidthLimitedCause::LossLimitedBwe,
            BandwidthLimitedCause::DelayBasedLimitedDelayIncreased,
        ] {
            let now = Instant::now();
            let mut pc = ProbeControl::new();
            pc.enable(true);
            pc.set_desired_bitrate(Bitrate::mbps(5));
            pc.set_estimated_bitrate(Bitrate::mbps(1), BandwidthLimitedCause::DelayBasedLimited);
            assert!(pc.handle_timeout(now).is_some());
            assert!(!pc.pending.is_empty());
            pc.set_estimated_bitrate(Bitrate::kbps(250), cause);
            assert!(pc.handle_timeout(now + Duration::from_millis(1)).is_none());
            assert!(pc.pending.is_empty());
            assert_eq!(pc.poll_timeout(), not_happening());
            pc.set_estimated_bitrate(Bitrate::kbps(250), BandwidthLimitedCause::DelayBasedLimited);
            let probe = pc.handle_timeout(now + Duration::from_secs(6)).unwrap();
            assert_eq!(probe.target_bitrate(), Bitrate::kbps(500));
            assert!(pc.handle_timeout(now + Duration::from_secs(6)).is_none());
        }
    }

    #[test]
    fn validated_path_probes_without_periodic_wait_and_is_rate_limited() {
        let now = Instant::now();
        let mut pc = ProbeControl::new();
        pc.enable(true);
        pc.set_desired_bitrate(Bitrate::mbps(5));
        pc.set_estimated_bitrate(Bitrate::kbps(250), BandwidthLimitedCause::DelayBasedLimited);
        assert!(pc.handle_timeout(now).is_some());
        assert!(pc.handle_timeout(now).is_some());
        let restored = now + Duration::from_secs(2);
        pc.request_path_probe(restored);
        let probe = pc
            .handle_timeout(restored)
            .expect("path recovery should not wait 15 seconds");
        assert_eq!(probe.target_bitrate(), Bitrate::kbps(500));
        assert_eq!(pc.last_probe.unwrap().kind, ProbeKind::PathRecovery);
        assert_eq!(pc.last_estimate, Some(Bitrate::kbps(250)));
        for second in 3..7 {
            let time = now + Duration::from_secs(second);
            pc.request_path_probe(time);
            assert!(pc.handle_timeout(time).is_none());
        }
        let time = restored + PATH_PROBE_INTERVAL;
        pc.request_path_probe(time);
        assert!(pc.handle_timeout(time).is_some());
    }

    #[test]
    fn path_probe_waits_for_congestion_to_clear_without_expiring() {
        for cause in [
            BandwidthLimitedCause::LossLimitedBwe,
            BandwidthLimitedCause::DelayBasedLimitedDelayIncreased,
        ] {
            for clear_after in [1, 6, 60] {
                let now = Instant::now();
                let mut pc = ProbeControl::new();
                pc.enable(true);
                pc.set_desired_bitrate(Bitrate::mbps(5));
                pc.set_estimated_bitrate(Bitrate::kbps(250), BandwidthLimitedCause::DelayBasedLimited);
                pc.handle_timeout(now);
                pc.handle_timeout(now);
                let restored = now + Duration::from_secs(2);
                pc.set_estimated_bitrate(Bitrate::kbps(250), cause);
                pc.request_path_probe(restored);
                assert!(pc.handle_timeout(restored).is_none());
                assert!(pc.poll_timeout() > restored);
                pc.set_estimated_bitrate(Bitrate::kbps(250), BandwidthLimitedCause::DelayBasedLimited);
                let result = pc.handle_timeout(restored + Duration::from_secs(clear_after));
                let probe = result.expect("path recovery must survive congestion lasting over five seconds");
                assert_eq!(probe.target_bitrate(), Bitrate::kbps(500));
                assert_eq!(pc.last_probe.unwrap().kind, ProbeKind::PathRecovery);
                assert!(pc.handle_timeout(restored + Duration::from_secs(clear_after)).is_none());
            }
        }
    }

    #[test]
    fn pending_path_probe_is_cancelled_when_probing_stops() {
        let now = Instant::now();
        let mut pc = ProbeControl::new();
        pc.enable(true);
        pc.request_path_probe(now);
        assert!(pc.path_probe_requested);
        pc.enable(false);
        assert!(!pc.path_probe_requested);
        assert!(pc.handle_timeout(now + Duration::from_secs(60)).is_none());
    }

    #[test]
    fn deferred_path_probe_uses_current_demand_and_estimate() {
        let now = Instant::now();
        for desired in [Bitrate::kbps(100), Bitrate::mbps(5)] {
            let mut pc = ProbeControl::new();
            pc.enable(true);
            pc.set_desired_bitrate(Bitrate::mbps(5));
            pc.set_estimated_bitrate(Bitrate::kbps(250), BandwidthLimitedCause::DelayBasedLimited);
            pc.handle_timeout(now);
            pc.handle_timeout(now);
            pc.set_estimated_bitrate(Bitrate::kbps(250), BandwidthLimitedCause::LossLimitedBwe);
            pc.request_path_probe(now + Duration::from_secs(2));
            assert!(pc.handle_timeout(now + Duration::from_secs(2)).is_none());
            pc.set_desired_bitrate(desired);
            pc.set_estimated_bitrate(Bitrate::kbps(100), BandwidthLimitedCause::DelayBasedLimited);
            let result = pc.handle_timeout(now + Duration::from_secs(10));
            assert!(!pc.path_probe_requested);
            if desired == Bitrate::kbps(100) {
                assert!(result.is_none());
            } else {
                assert_eq!(result.unwrap().target_bitrate(), Bitrate::kbps(200));
            }
        }
    }

    #[test]
    fn fluctuating_low_estimates_do_not_starve_recovery_probes() {
        let mut pc = ProbeControl::new();
        let now = Instant::now();
        pc.enable(true);
        pc.set_desired_bitrate(Bitrate::mbps(5));
        pc.set_estimated_bitrate(Bitrate::kbps(250), BandwidthLimitedCause::DelayBasedLimited);
        assert!(pc.handle_timeout(now).is_some());
        assert!(pc.handle_timeout(now).is_some());
        assert!(pc.handle_timeout(now).is_none());

        let mut probes = Vec::new();
        for second in 1..=45 {
            let estimate = Bitrate::kbps(if second % 2 == 0 { 250 } else { 230 });
            pc.set_estimated_bitrate(estimate, BandwidthLimitedCause::DelayBasedLimited);
            if let Some(probe) = pc.handle_timeout(now + Duration::from_secs(second)) {
                assert_eq!(probe.target_bitrate(), estimate * STAGNANT_PROBE_SCALE);
                probes.push(second);
            }
        }
        assert_eq!(probes, vec![15, 30, 45]);
    }

    #[test]
    fn exhausted_probe_deadlines_do_not_busy_loop() {
        let mut pc = ProbeControl::new();
        let now = Instant::now();
        pc.enable(true);
        pc.set_desired_bitrate(Bitrate::mbps(5));
        pc.set_estimated_bitrate(Bitrate::kbps(250), BandwidthLimitedCause::DelayBasedLimited);
        assert!(pc.handle_timeout(now).is_some());
        assert!(pc.handle_timeout(now).is_some());
        assert!(pc.handle_timeout(now).is_none());

        for _ in 0..6 {
            let due = pc.poll_timeout();
            assert!(due < now + Duration::from_secs(120));
            let _ = pc.handle_timeout(due);
            // Only queued probes may request an immediate follow-up.
            if pc.poll_timeout() == already_happened() {
                assert!(pc.handle_timeout(due).is_none());
            }
            assert!(pc.poll_timeout() > due, "deadline must advance");
        }
    }

    #[test]
    fn recovery_waits_for_congestion_to_clear_and_stops_at_target() {
        for cause in [
            BandwidthLimitedCause::LossLimitedBwe,
            BandwidthLimitedCause::DelayBasedLimitedDelayIncreased,
        ] {
            let mut pc = ProbeControl::new();
            let now = Instant::now();
            pc.enable(true);
            pc.set_desired_bitrate(Bitrate::kbps(500));
            pc.set_estimated_bitrate(Bitrate::kbps(250), BandwidthLimitedCause::DelayBasedLimited);
            assert!(pc.handle_timeout(now).is_some());
            assert!(pc.handle_timeout(now).is_some());
            assert!(pc.handle_timeout(now).is_none());

            pc.set_estimated_bitrate(Bitrate::kbps(250), cause);
            assert!(pc.handle_timeout(now + Duration::from_secs(30)).is_none());
            assert_eq!(pc.poll_timeout(), not_happening());
            pc.set_estimated_bitrate(Bitrate::kbps(250), BandwidthLimitedCause::DelayBasedLimited);
            assert!(pc.handle_timeout(now + Duration::from_secs(30)).is_some());
            assert!(pc.handle_timeout(now + Duration::from_secs(30)).is_none());
            assert_eq!(pc.poll_timeout(), now + Duration::from_secs(45));

            pc.set_estimated_bitrate(Bitrate::kbps(500), BandwidthLimitedCause::DelayBasedLimited);
            // A successful probe can trigger the existing exponential follow-up.
            let _ = pc.handle_timeout(now + Duration::from_secs(31));
            let _ = pc.handle_timeout(now + Duration::from_secs(31));
            assert!(pc.handle_timeout(now + Duration::from_secs(33)).is_none());
            assert_eq!(pc.poll_timeout(), not_happening());
            assert!(pc.handle_timeout(now + Duration::from_secs(60)).is_none());
        }
    }

    #[test]
    fn unchanged_target_does_not_reset_probe_deadline() {
        let mut pc = ProbeControl::new();
        let now = Instant::now();
        pc.enable(true);
        pc.set_desired_bitrate(Bitrate::mbps(5));
        pc.set_estimated_bitrate(Bitrate::kbps(250), BandwidthLimitedCause::DelayBasedLimited);
        assert!(pc.handle_timeout(now).is_some());
        assert!(pc.handle_timeout(now).is_some());
        assert!(pc.handle_timeout(now).is_none());
        let due = pc.poll_timeout();
        pc.set_desired_bitrate(Bitrate::mbps(5));
        assert_eq!(pc.poll_timeout(), due);
    }

    #[test]
    fn initial_exponential_probes_are_queued_and_emitted_one_per_tick() {
        let mut pc = ProbeControl::new();
        pc.enable(true);
        let now = Instant::now();

        pc.set_desired_bitrate(Bitrate::mbps(50));
        pc.set_estimated_bitrate(Bitrate::kbps(300), BandwidthLimitedCause::DelayBasedLimited);

        // First handle_timeout triggers initial probing and returns first probe.
        let p1 = pc.handle_timeout(now).unwrap();

        // poll_timeout returns already_happened while there are pending probes.
        assert_eq!(pc.poll_timeout(), already_happened());

        // Second handle_timeout returns the second queued probe.
        let p2 = pc.handle_timeout(now).unwrap();

        assert_eq!(p1.target_bitrate(), Bitrate::kbps(900));
        assert_eq!(p2.target_bitrate(), Bitrate::kbps(1800));
        assert_eq!(p1.min_packet_count(), 5);
        assert_eq!(p1.min_probe_delta(), Duration::from_millis(2));
        assert!(!p1.is_alr_probe());

        // Queue drained - no more probes.
        assert!(pc.handle_timeout(now).is_none());
    }

    #[test]
    fn further_probe_is_triggered_when_probe_result_is_high_enough() {
        let mut pc = ProbeControl::new();
        pc.enable(true);
        let now = Instant::now();

        pc.enable(true);
        pc.set_desired_bitrate(Bitrate::mbps(50));
        pc.set_estimated_bitrate(Bitrate::mbps(1), BandwidthLimitedCause::DelayBasedLimited);

        // Drain initial two probes.
        let _ = pc.handle_timeout(now).unwrap();
        let _ = pc.handle_timeout(now).unwrap();

        // WebRTC rule: if measured bitrate > min_bitrate_to_probe_further, probe at 2x measured.
        // min_bitrate_to_probe_further is 0.7 * last_probe_rate (6x start) = 4.2 Mbps.
        pc.set_estimated_bitrate(Bitrate::mbps(5), BandwidthLimitedCause::DelayBasedLimited);

        let p = pc.handle_timeout(now + Duration::from_millis(10)).unwrap();
        assert_eq!(p.target_bitrate(), Bitrate::mbps(10));
    }

    #[test]
    fn allocation_probe_is_triggered_in_alr_when_allocation_increases() {
        let mut pc = ProbeControl::new();
        pc.enable(true);
        let now = Instant::now();

        pc.set_desired_bitrate(Bitrate::mbps(1));
        pc.set_estimated_bitrate(Bitrate::mbps(1), BandwidthLimitedCause::DelayBasedLimited);

        // Drain initial probes.
        let _ = pc.handle_timeout(now).unwrap();
        let _ = pc.handle_timeout(now).unwrap();

        // Time out waiting for probing result -> probing complete.
        assert!(pc.handle_timeout(now + Duration::from_secs(2)).is_none());

        // Enter ALR
        pc.set_alr_start_time(now + Duration::from_secs(2));

        // No probe yet - desired hasn't increased
        assert!(pc.handle_timeout(now + Duration::from_secs(2)).is_none());

        // Increase desired bitrate while in ALR (desired > prev AND desired > estimate)
        pc.set_desired_bitrate(Bitrate::mbps(4));

        // Should trigger allocation probe: p1 = 4 Mbps * 1.0 = 4 Mbps, capped by 2× estimate = 2 Mbps
        let p = pc.handle_timeout(now + Duration::from_secs(2)).unwrap();
        assert_eq!(p.target_bitrate(), Bitrate::mbps(2));
    }

    #[test]
    fn handles_bitrate_infinity_without_panic() {
        let mut pc = ProbeControl::new();
        pc.enable(true);
        let now = Instant::now();

        pc.set_desired_bitrate(Bitrate::mbps(50));

        // Should not panic with Infinity
        pc.set_estimated_bitrate(Bitrate::INFINITY, BandwidthLimitedCause::DelayBasedLimited);

        // Verify behavior is reasonable (no probing with infinite estimate)
        assert!(pc.handle_timeout(now).is_none());
    }

    #[test]
    fn handles_clock_skew_gracefully() {
        let mut pc = ProbeControl::new();
        pc.enable(true);
        let now = Instant::now();

        pc.set_desired_bitrate(Bitrate::mbps(50));
        pc.set_estimated_bitrate(Bitrate::kbps(300), BandwidthLimitedCause::DelayBasedLimited);

        // Drain initial probes
        let _ = pc.handle_timeout(now);
        let _ = pc.handle_timeout(now);

        // Simulate time going backwards (clock skew)
        let earlier = now - Duration::from_secs(5);

        // Should handle gracefully with saturating_duration_since
        let _ = pc.handle_timeout(earlier);

        // Should still be able to continue normally
        let _ = pc.handle_timeout(now + Duration::from_secs(1));
    }

    #[test]
    fn handles_max_bitrate_zero() {
        let mut pc = ProbeControl::new();
        pc.enable(true);
        let now = Instant::now();

        // Set max_bitrate to zero - this is rejected as first value to avoid
        // creating probes with zero cap.
        pc.set_desired_bitrate(Bitrate::ZERO);
        pc.set_estimated_bitrate(Bitrate::kbps(300), BandwidthLimitedCause::DelayBasedLimited);

        // No probes should be created since desired was rejected.
        let p1 = pc.handle_timeout(now);
        assert!(p1.is_none(), "Should not create probes with zero desired");
    }

    #[test]
    fn allocation_probe_fires_when_desired_increases_in_alr() {
        let mut pc = ProbeControl::new();
        pc.enable(true);
        let now = Instant::now();

        pc.set_desired_bitrate(Bitrate::kbps(500));
        pc.set_estimated_bitrate(Bitrate::kbps(500), BandwidthLimitedCause::DelayBasedLimited);

        // Drain initial probes
        let _ = pc.handle_timeout(now);
        let _ = pc.handle_timeout(now);

        // Timeout to reach probing complete
        assert!(pc.handle_timeout(now + Duration::from_secs(2)).is_none());

        // Enter ALR
        pc.set_alr_start_time(now + Duration::from_secs(3));

        // No probe on ALR entry alone
        assert!(pc.handle_timeout(now + Duration::from_secs(3)).is_none());

        // Increase desired while in ALR (desired > prev AND desired > estimate)
        pc.set_desired_bitrate(Bitrate::mbps(4));

        // Should trigger allocation probe
        let probe = pc.handle_timeout(now + Duration::from_secs(3));
        assert!(
            probe.is_some(),
            "Allocation probe should trigger when desired increases in ALR"
        );
    }

    #[test]
    fn large_drop_probing_after_alr_ended() {
        let mut pc = ProbeControl::new();
        pc.enable(true);
        let now = Instant::now();

        pc.set_desired_bitrate(Bitrate::mbps(5));
        pc.set_estimated_bitrate(Bitrate::mbps(5), BandwidthLimitedCause::DelayBasedLimited);

        // Drain initial probes
        let _ = pc.handle_timeout(now);
        let _ = pc.handle_timeout(now);

        // Timeout to probing complete
        assert!(pc.handle_timeout(now + Duration::from_secs(2)).is_none());

        // Enter and exit ALR (large-drop works when ALR ended recently)
        pc.set_alr_start_time(now + Duration::from_secs(2));
        pc.set_alr_stop_time(now + Duration::from_secs(3));

        // Simulate large drop (to 60% of original = 3 Mbps, below 66% threshold)
        pc.set_estimated_bitrate(Bitrate::mbps(3), BandwidthLimitedCause::DelayBasedLimited);

        // Check at now+5s (within 3s of ALR ending, so alr_ended_recently is true)
        let later = now + Duration::from_secs(5);

        // Should trigger large-drop recovery probe at 85% of pre-drop rate (4.25 Mbps)
        let p = pc.handle_timeout(later);
        assert!(p.is_some(), "Large-drop recovery should schedule probe");
        if let Some(probe) = p {
            // 85% of 5 Mbps = 4.25 Mbps
            assert!(probe.target_bitrate() >= Bitrate::mbps(4));
            assert!(probe.target_bitrate() <= Bitrate::mbps(5));
        }
    }

    #[test]
    fn allocation_probe_requires_desired_increase_in_alr() {
        let mut pc = ProbeControl::new();
        pc.enable(true);
        let now = Instant::now();

        pc.set_desired_bitrate(Bitrate::mbps(5));
        pc.set_estimated_bitrate(Bitrate::mbps(1), BandwidthLimitedCause::DelayBasedLimited);

        // Drain initial probes
        let _ = pc.handle_timeout(now);
        let _ = pc.handle_timeout(now);

        // Timeout to probing complete
        assert!(pc.handle_timeout(now + Duration::from_secs(2)).is_none());

        // Enter ALR with estimate < max_bitrate
        pc.set_alr_start_time(now + Duration::from_secs(2));

        // No allocation probe on ALR entry - need desired to increase
        let probe = pc.handle_timeout(now + Duration::from_secs(2));
        assert!(
            probe.is_none(),
            "Should NOT trigger allocation probe on ALR entry alone"
        );

        // Increase desired while in ALR
        pc.set_desired_bitrate(Bitrate::mbps(10));

        // Now should trigger allocation probe
        let probe = pc.handle_timeout(now + Duration::from_secs(2));
        assert!(
            probe.is_some(),
            "Should trigger allocation probe when desired increases in ALR"
        );
    }

    #[test]
    fn periodic_alr_probing() {
        let mut pc = ProbeControl::new();
        pc.enable(true);
        let now = Instant::now();

        pc.set_desired_bitrate(Bitrate::mbps(5));
        pc.set_estimated_bitrate(Bitrate::mbps(1), BandwidthLimitedCause::DelayBasedLimited);

        // Drain initial probes
        let _ = pc.handle_timeout(now);
        let _ = pc.handle_timeout(now);

        // Timeout to probing complete
        assert!(pc.handle_timeout(now + Duration::from_secs(2)).is_none());

        // Enter ALR
        pc.set_alr_start_time(now + Duration::from_secs(2));

        // No immediate probe on ALR entry
        assert!(pc.handle_timeout(now + Duration::from_secs(2)).is_none());

        // Wait 5 seconds for periodic probe (2s to complete initial + 5s = 7s)
        let probe = pc.handle_timeout(now + Duration::from_secs(7));
        assert!(
            probe.is_some(),
            "Should trigger periodic ALR probe after 5 seconds in ALR"
        );
        assert!(probe.unwrap().is_alr_probe());
    }

    #[test]
    fn periodic_alr_probing_continues_even_when_estimate_reaches_max() {
        let mut pc = ProbeControl::new();
        pc.enable(true);
        let now = Instant::now();

        pc.set_desired_bitrate(Bitrate::mbps(2));
        pc.set_estimated_bitrate(Bitrate::mbps(1), BandwidthLimitedCause::DelayBasedLimited);

        // Drain initial probes
        let _ = pc.handle_timeout(now);
        let _ = pc.handle_timeout(now);

        // Timeout to probing complete
        assert!(pc.handle_timeout(now + Duration::from_secs(2)).is_none());

        // Enter ALR
        pc.set_alr_start_time(now + Duration::from_secs(2));

        // No immediate probe on ALR entry
        assert!(pc.handle_timeout(now + Duration::from_secs(2)).is_none());

        // Now increase estimate to match max_bitrate
        pc.set_estimated_bitrate(Bitrate::mbps(2), BandwidthLimitedCause::DelayBasedLimited);

        // Wait 5 seconds - should still trigger periodic probe in ALR
        // even though estimate >= max_bitrate, to maintain confidence in the estimate
        let probe = pc.handle_timeout(now + Duration::from_secs(7));
        assert!(
            probe.is_some(),
            "Should continue periodic probing in ALR even when estimate >= max_bitrate"
        );
    }
}
