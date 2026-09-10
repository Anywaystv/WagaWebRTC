use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use super::super::macros::{log_bitrate_estimate, log_delay_variation};
use super::super::{AckedPacket, BandwidthUsage};
use super::arrival_group::ArrivalGroupAccumulator;
use super::rate_control::RateControl;
use super::trendline::TrendlineEstimator;
use crate::rtp_::{Bitrate, TwccSeq};
use crate::util::{MovingAverage, already_happened};

const MAX_RTT_HISTORY_WINDOW: usize = 32;
const UPDATE_INTERVAL: Duration = Duration::from_millis(25);
/// The maximum time we keep updating our estimate without receiving a TWCC report.
const MAX_TWCC_GAP: Duration = Duration::from_millis(500);
/// RFC 6298: Exponentially Weighted Moving Average smoothing factor for RTT (alpha = 1/8)
const RTT_SMOOTHING_FACTOR: f64 = 0.125;

/// Delay controller for googcc inspired BWE.
///
/// This controller attempts to estimate the available send bandwidth by looking at the variations
/// in packet arrival times for groups of packets sent together. Broadly, if the delay variation is
/// increasing this indicates overuse.
struct PathDetector {
    groups: ArrivalGroupAccumulator,
    trend: TrendlineEstimator,
    updated: Instant,
}

pub struct DelayController {
    paths: HashMap<u64, PathDetector>,
    arrival_group_accumulator: ArrivalGroupAccumulator,
    trendline_estimator: TrendlineEstimator,
    rate_control: RateControl,
    /// Last estimate produced, unlike [`next_estimate`] this will always have a value after the
    /// first estimate.
    last_estimate: Option<Bitrate>,
    /// Smoothed RTT using EWMA (RFC 6298, alpha = 1/8).
    smoothed_rtt: MovingAverage,
    /// History of the max RTT derived for each TWCC report (kept for fallback).
    max_rtt_history: VecDeque<Duration>,

    /// The next time we should poll.
    next_timeout: Instant,
    /// The last time we ingested a TWCC report.
    last_twcc_report: Instant,
}

impl DelayController {
    pub fn new(initial_bitrate: Bitrate) -> Self {
        Self {
            paths: HashMap::new(),
            arrival_group_accumulator: ArrivalGroupAccumulator::default(),
            trendline_estimator: TrendlineEstimator::new(20),
            rate_control: RateControl::new(initial_bitrate, Bitrate::kbps(40), Bitrate::gbps(10)),
            last_estimate: Some(initial_bitrate),
            smoothed_rtt: MovingAverage::new(RTT_SMOOTHING_FACTOR),
            max_rtt_history: VecDeque::default(),
            next_timeout: already_happened(),
            last_twcc_report: already_happened(),
        }
    }

    /// Record a packet from a TWCC report.
    pub fn update(
        &mut self,
        acked: &[AckedPacket],
        path_for_sequence: Option<&HashMap<TwccSeq, u64>>,
        acked_bitrate: Option<Bitrate>,
        probe_bitrate: Option<Bitrate>,
        now: Instant,
    ) -> Option<Bitrate> {
        // Missing-only or duplicate feedback provides no new arrival-time evidence.
        if acked.is_empty() && probe_bitrate.is_none() {
            return self.last_estimate;
        }
        let mut max_rtt = None;

        for acked_packet in acked {
            max_rtt = max_rtt.max(Some(acked_packet.rtt()));
            let (groups, trend) = if let Some(path_for_sequence) = path_for_sequence {
                let Some(path) = path_for_sequence.get(&acked_packet.seq_no) else { continue; };
                if self.paths.len() >= 256 && !self.paths.contains_key(path) { continue; }
                let detector = self.paths.entry(*path).or_insert_with(|| PathDetector {
                    groups: ArrivalGroupAccumulator::default(),
                    trend: TrendlineEstimator::new(20),
                    updated: now,
                });
                detector.updated = now;
                (&mut detector.groups, &mut detector.trend)
            } else {
                (&mut self.arrival_group_accumulator, &mut self.trendline_estimator)
            };
            if let Some(variation) = groups.accumulate_packet(acked_packet) {
                log_delay_variation!(variation.arrival_delta);
                // Each detector uses receiver time, independent of feedback batching.
                trend.add_delay_observation(variation, variation.last_remote_recv_time);
            }
        }

        if let Some(rtt) = max_rtt {
            self.update_rtt(rtt);
        }

        let new_hypothesis = self.hypothesis(now);

        self.update_estimate(
            new_hypothesis,
            acked_bitrate,
            probe_bitrate,
            self.get_smoothed_rtt(),
            now,
        );
        self.last_twcc_report = now;

        self.last_estimate
    }

    pub fn poll_timeout(&self) -> Instant {
        self.next_timeout
    }

    pub fn handle_timeout(&mut self, acked_bitrate: Option<Bitrate>, now: Instant) {
        if !self.trendline_hypothesis_valid(now) {
            // We haven't received a TWCC report in a while. The trendline hypothesis can
            // no longer be considered valid. We need another TWCC report before we can update
            // estimates.
            let next_timeout_in = self
                .get_smoothed_rtt()
                .unwrap_or(MAX_TWCC_GAP)
                .min(UPDATE_INTERVAL);

            // Set this even if we didn't update, otherwise we get stuck in a poll -> handle loop
            // that starves the run loop.
            self.next_timeout = now + next_timeout_in;
            return;
        }

        self.update_estimate(
            self.hypothesis(now),
            acked_bitrate,
            None,
            self.get_smoothed_rtt(),
            now,
        );
    }

    /// Get the latest estimate.
    pub fn last_estimate(&self) -> Option<Bitrate> {
        self.last_estimate
    }

    pub(crate) fn feedback_age_ms(&self, now: Instant) -> Option<u128> {
        (self.last_twcc_report != already_happened())
            .then(|| now.saturating_duration_since(self.last_twcc_report).as_millis())
    }

    /// Whether the delay-based detector currently signals overuse.
    ///
    /// This is useful for gating behaviors (like probing) that would otherwise
    /// re-excite the system while we're already congested.
    pub fn is_overusing(&self) -> bool {
        self.hypothesis(self.last_twcc_report) == BandwidthUsage::Overuse
    }

    fn hypothesis(&self, now: Instant) -> BandwidthUsage {
        if self.paths.is_empty() { return self.trendline_estimator.hypothesis(); }
        let mut active = self.paths.values().filter(|path| now.saturating_duration_since(path.updated) <= MAX_TWCC_GAP).peekable();
        if active.peek().is_none() { return BandwidthUsage::Normal; }
        // Queueing on one route should first shift traffic to a route with room.
        // Shared rate backs off when all recently used routes are congested.
        if active.all(|path| path.trend.hypothesis() == BandwidthUsage::Overuse) {
            BandwidthUsage::Overuse
        } else { BandwidthUsage::Normal }
    }

    /// Update smoothed RTT using EWMA (RFC 6298, alpha = 1/8).
    fn update_rtt(&mut self, rtt: Duration) {
        // Keep history as fallback in case smoothed RTT is not yet available
        while self.max_rtt_history.len() >= MAX_RTT_HISTORY_WINDOW {
            self.max_rtt_history.pop_front();
        }
        self.max_rtt_history.push_back(rtt);

        // Update smoothed RTT using EWMA: smoothed = (7/8) * smoothed + (1/8) * sample
        self.smoothed_rtt.update(rtt.as_secs_f64());
    }

    /// Get the current smoothed RTT, with fallback to mean of history if not yet available.
    fn get_smoothed_rtt(&self) -> Option<Duration> {
        // Try smoothed RTT first (EWMA)
        if let Some(avg_secs) = self.smoothed_rtt.get() {
            return Some(Duration::from_secs_f64(avg_secs));
        }

        // Fallback to mean of history during initialization
        if self.max_rtt_history.is_empty() {
            return None;
        }

        let sum = self
            .max_rtt_history
            .iter()
            .fold(Duration::ZERO, |acc, rtt| acc + *rtt);
        Some(sum / self.max_rtt_history.len() as u32)
    }

    fn update_estimate(
        &mut self,
        hypothesis: BandwidthUsage,
        observed_bitrate: Option<Bitrate>,
        probe_bitrate: Option<Bitrate>,
        mean_max_rtt: Option<Duration>,
        now: Instant,
    ) {
        // WebRTC's logic from delay_based_bwe.cc MaybeUpdateEstimate():
        // - If we have a probe result, apply it directly and skip delay-based updates
        // - Otherwise, apply normal delay-based rate control
        //
        // This prevents probe results from being immediately overridden by delay-based
        // decreases caused by the probe itself (probes cause temporary queuing delay).

        if let Some(probe_rate) = probe_bitrate {
            // Apply probe result directly, bypassing delay-based updates
            self.rate_control.set_probe_result(probe_rate, now);
            let estimated_rate = self.rate_control.estimated_bitrate();
            log_bitrate_estimate!(estimated_rate.as_f64());
            self.last_estimate = Some(estimated_rate);
        } else if let Some(observed_bitrate) = observed_bitrate {
            // No probe result, apply normal delay-based rate control
            self.rate_control
                .update(hypothesis.into(), observed_bitrate, mean_max_rtt, now);
            let estimated_rate = self.rate_control.estimated_bitrate();

            log_bitrate_estimate!(estimated_rate.as_f64());
            self.last_estimate = Some(estimated_rate);
        }

        // Set this even if we didn't update, otherwise we get stuck in a poll -> handle loop
        // that starves the run loop.
        self.next_timeout = now + UPDATE_INTERVAL;
    }

    /// Whether the current trendline hypothesis is valid i.e. not too old.
    fn trendline_hypothesis_valid(&self, now: Instant) -> bool {
        now.duration_since(self.last_twcc_report)
            <= self
                .get_smoothed_rtt()
                .map(|rtt| rtt * 2)
                .unwrap_or(MAX_TWCC_GAP)
                .min(UPDATE_INTERVAL * 2)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn empty_feedback_does_not_refresh_delay_evidence() {
        let now = Instant::now();
        let mut controller = DelayController::new(Bitrate::mbps(5));
        controller.last_twcc_report = now;
        let later = now + Duration::from_secs(16);
        assert_eq!(controller.update(&[], None, Some(Bitrate::kbps(250)), None, later),
                   Some(Bitrate::mbps(5)));
        assert_eq!(controller.last_twcc_report, now);
        assert!(!controller.trendline_hypothesis_valid(later));
        controller.handle_timeout(Some(Bitrate::kbps(250)), later);
        assert!(controller.poll_timeout() > later);
        assert_eq!(controller.last_estimate(), Some(Bitrate::mbps(5)));
    }
}
