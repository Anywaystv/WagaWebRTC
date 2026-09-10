//! Google Congestion Control (GoogCC) Bandwidth Estimation based on TWCC feedback.
//!
//! This implementation is ported from libWebRTC's GoogCC and goes beyond the simplified
//! IETF draft (<https://datatracker.ietf.org/doc/html/draft-ietf-rmcat-gcc-02>) to include
//! WebRTC's production features:
//!
//! - Delay-based control (trendline estimator with AIMD rate control)
//! - Loss-based control (with inherent loss rate estimation)
//! - Probe controller with state machine and multi-stage probing strategy
//! - ALR (Application Limited Region) detection and periodic probing
//! - Link capacity estimation from ALR probes
//!
//! The probe controller in particular closely matches WebRTC's `ProbeController` behavior
//! and default constants, enabling compatible bandwidth discovery with WebRTC endpoints.

use std::cmp::Ordering;
use std::fmt;
use std::time::{Duration, Instant};

use crate::Reason;
use crate::rtp_::{Bitrate, DataSize, TwccClusterId, TwccSendRecord, TwccSeq};
use crate::util::Soonest;

mod acked_bitrate_estimator;
mod alr_detector;
pub(crate) mod api;
mod delay;
mod link_capacity_estimator;
mod loss_controller;
mod path_delay;
mod macros;
mod probe;
mod smoother;
mod time;

use acked_bitrate_estimator::AckedBitrateEstimator;
use alr_detector::AlrDetector;
use delay::DelayController;
use link_capacity_estimator::LinkCapacityEstimator;
use loss_controller::{LossController, LossControllerState};
use macros::log_loss;
use smoother::EstimateSmoother;

pub(crate) use macros::{log_pacer_media_debt, log_pacer_padding_debt};
pub(crate) use probe::{BandwidthLimitedCause, ProbeEstimator};
pub(crate) use probe::{ProbeClusterState, ProbeControl};

#[cfg(feature = "_internal_test_exports")]
pub use probe::ProbeClusterConfig;
#[cfg(not(feature = "_internal_test_exports"))]
pub(crate) use probe::ProbeClusterConfig;

const INITIAL_BITRATE_WINDOW: Duration = Duration::from_millis(500);
const BITRATE_WINDOW: Duration = Duration::from_millis(150);
const STARTUP_PHASE: Duration = Duration::from_secs(2);
const PROBE_DROP_THROUGHPUT_FRACTION: f64 = 0.85;

pub struct Bwe {
    bwe: SendSideBandwidthEstimator,
    desired_bitrate: Bitrate,
    smoother: EstimateSmoother,
    feedback_pending: bool,
    path_started_at: Option<Instant>,
}

impl Bwe {
    pub fn diagnostic_snapshot(&self, now: Instant) -> String {
        format!("estimate={:?} desired={} overuse={} delay_feedback_age_ms={:?} {} {} timing_paths={}",
            self.last_estimate(), self.desired_bitrate.as_f64(), self.is_overusing(),
            self.bwe.delay_controller.feedback_age_ms(now),
            self.bwe.loss_controller.diagnostic_snapshot(now),
            self.bwe.probe_control.diagnostic_snapshot(now), self.bwe.path_delay.path_count())
    }

    pub fn new(initial: Bitrate) -> Self {
        let send_side_bwe = SendSideBandwidthEstimator::new(initial);
        Bwe {
            bwe: send_side_bwe,
            desired_bitrate: Bitrate::ZERO,
            smoother: EstimateSmoother::new(),
            feedback_pending: false,
            path_started_at: None,
        }
    }

    pub fn handle_timeout(&mut self, now: Instant, do_probe: bool) -> Option<ProbeClusterConfig> {
        let result = self.bwe.handle_timeout(self.desired_bitrate, do_probe, now);
        if self.feedback_pending {
            self.feedback_pending = false;
            if let Some(estimate) = self.bwe.last_estimate() {
                self.smoother.record(now, estimate);
            }
        }
        result
    }

    pub fn start_probe(&mut self, config: ProbeClusterConfig, now: Instant) -> bool {
        self.bwe.start_probe(config, now)
    }

    pub fn end_probe(&mut self, now: Instant, cluster_id: TwccClusterId) {
        self.bwe.end_probe(now, cluster_id);
    }

    pub fn reset(&mut self, init_bitrate: Bitrate) {
        self.bwe.reset(init_bitrate);
        self.smoother = EstimateSmoother::new();
        self.feedback_pending = false;
    }

    pub fn restart_on_path_change(&mut self, now: Instant) -> Option<Bitrate> {
        let initial = self.last_estimate()?.min(self.desired_bitrate);
        if initial.is_zero() {
            return None;
        }
        self.reset(initial);
        self.path_started_at = Some(now);
        Some(initial)
    }

    pub fn update<'t>(
        &mut self,
        records: impl Iterator<Item = &'t crate::rtp_::TwccSendRecord>,
        now: Instant,
    ) {
        let path_started_at = self.path_started_at;
        let mut records = records.filter(move |record| {
            path_started_at.is_none_or(|start| record.local_send_time() >= start)
        }).peekable();
        if path_started_at.is_some() && records.peek().is_none() {
            return;
        }
        self.feedback_pending |= records.peek().is_some();
        self.bwe.update(records, now);
    }

    pub fn poll_estimate(&mut self) -> Option<Bitrate> {
        self.smoother.poll()
    }

    pub fn poll_timeout(&self) -> (Option<Instant>, Reason) {
        self.bwe.poll_timeout()
    }

    pub fn last_estimate(&self) -> Option<Bitrate> {
        self.bwe.last_estimate()
    }

    pub fn on_media_sent(&mut self, payload_size: DataSize, is_padding: bool, now: Instant) {
        if !is_padding {
            // Update ALR detector with media bytes sent
            self.bwe.on_media_sent(payload_size, now);
        }
    }

    pub fn is_overusing(&self) -> bool {
        self.bwe.is_overusing()
    }

    pub fn set_desired_bitrate(&mut self, v: Bitrate) {
        self.desired_bitrate = v;
    }

    pub fn request_path_probe(&mut self, now: Instant) {
        self.bwe.probe_control.request_path_probe(now);
    }
}

struct SendSideBandwidthEstimator {
    delay_controller: DelayController,
    loss_controller: LossController,
    acked_bitrate_estimator: AckedBitrateEstimator,
    probe_control: ProbeControl,
    probe_estimator: ProbeEstimator,
    started_at: Option<Instant>,
    alr_detector: AlrDetector,
    link_capacity_estimator: LinkCapacityEstimator,
    last_updated_estimate: Option<Bitrate>,
    path_delay: path_delay::PathDelay,
}

impl SendSideBandwidthEstimator {
    pub fn new(initial_bitrate: Bitrate) -> Self {
        let mut alr_detector = AlrDetector::new();
        alr_detector.set_estimated_bitrate(initial_bitrate);

        let mut loss_controller = LossController::new();
        loss_controller.set_bandwidth_estimate(initial_bitrate);

        Self {
            delay_controller: DelayController::new(initial_bitrate),
            loss_controller,
            acked_bitrate_estimator: AckedBitrateEstimator::new(
                INITIAL_BITRATE_WINDOW,
                BITRATE_WINDOW,
            ),
            probe_control: ProbeControl::new(),
            probe_estimator: ProbeEstimator::new(),
            started_at: None,
            alr_detector,
            link_capacity_estimator: LinkCapacityEstimator::new(),
            last_updated_estimate: None,
            path_delay: path_delay::PathDelay::default(),
        }
    }

    /// Whether the delay-based detector currently signals overuse.
    ///
    /// This is useful for gating behaviors (like padding/probing) that would otherwise
    /// re-excite the system while we're already congested.
    pub fn is_overusing(&self) -> bool {
        self.delay_controller.is_overusing()
    }

    /// Update ALR detector with actual bytes sent.
    ///
    /// Should be called for media packets (not padding/probes).
    /// This is typically called from the session's packet sending logic.
    pub fn on_media_sent(&mut self, bytes: DataSize, now: Instant) {
        self.alr_detector.on_bytes_sent(bytes, now);
        self.delay_controller
            .set_application_limited(self.alr_detector.alr_start_time().is_some());
    }

    /// Record a packet from a TWCC report.
    pub fn update<'t>(&mut self, records: impl Iterator<Item = &'t TwccSendRecord>, now: Instant) {
        let _ = self.started_at.get_or_insert(now);

        let send_records: Vec<_> = records.collect();
        // Probe timing removes fixed path offsets; throughput and each path
        // delay trend retain the original receive timestamps.
        let normalized = self.path_delay.update(&send_records);
        let settled_loss = self.path_delay.loss_records(&send_records, now);
        let loss_records: Vec<_> = settled_loss.as_ref().map(|records| records.iter().collect())
            .unwrap_or_else(|| send_records.clone());
        let timing_records: Vec<_> = normalized.as_ref().map(|records| records.iter().collect())
            .unwrap_or_else(|| send_records.iter().copied().filter(|record| !record.redundant).collect());

        // Feed records to probe estimator for analysis and process any new probe results
        let mut latest_probe_result = None;
        for result in self.probe_estimator.update(timing_records.iter().copied()) {
            let mut bitrate = result.bitrate;
            // A probe limited by only one route cannot cap combined capacity.
            // In ALR, jitter can stretch a short probe below the previous estimate.
            // Require delay overuse or RTT growth before accepting a lower ceiling.
            // Delay and settled loss feedback still enforce actual congestion.
            if result.limited_by_sender
                || result.saturated_paths < self.path_delay.path_count()
                || (self.delay_controller.is_application_limited_with_low_delay()
                    && !self.delay_controller.is_overusing())
            {
                if let Some(current) = self.delay_controller.last_estimate() {
                    bitrate = bitrate.max(current);
                }
            }
            latest_probe_result = Some(bitrate);

            // Update link capacity estimator for every successful ALR probe, not just the latest.
            // The estimator internally takes the max of all probe results, building up knowledge
            // of proven link capacity. This differs from the delay controller, which only receives
            // the latest probe result (matching WebRTC's FetchAndResetLastEstimatedBitrate behavior).
            if result.config.is_alr_probe() {
                self.link_capacity_estimator.update_from_probe(result.bitrate, now);
            }
        }

        let mut acked_packets = vec![];

        for record in send_records.iter() {
            let Ok(acked_packet) = (*record).try_into() else {
                continue;
            };
            acked_packets.push(acked_packet);
        }
        acked_packets.sort_by(AckedPacket::order_by_receive_time);

        for acked_packet in acked_packets.iter() {
            self.acked_bitrate_estimator
                .update(acked_packet.remote_recv_time, acked_packet.size);
        }

        let acked_bitrate = self.acked_bitrate_estimator.current_estimate();

        // Use the latest probe result from this update, if any
        let probe_result = latest_probe_result.map(|probe| {
            match (acked_bitrate, self.delay_controller.last_estimate()) {
                (Some(acked), Some(current)) if acked.is_valid() => {
                    // GoogCC bounds probe backoff by delivered throughput while
                    // leaving headroom to drain a genuinely congested queue.
                    probe.max(current.min(acked * PROBE_DROP_THROUGHPUT_FRACTION))
                }
                _ => probe,
            }
        });

        let is_probe_result = probe_result.is_some();

        let paths = (self.path_delay.path_count() > 0).then(|| send_records.iter()
            .filter_map(|record| record.egress_path.map(|path| (record.seq(), path)))
            .collect());

        // Update delay controller with the latest probe result
        let maybe_estimate =
            self.delay_controller
                .update(&acked_packets, paths.as_ref(), acked_bitrate, probe_result, now);

        let Some(delay_estimate) = maybe_estimate else {
            return;
        };

        let loss = if loss_records.is_empty() {
            0.0
        } else {
            loss_records.iter().filter(|record| record.remote_recv_time().is_none()).count() as f64
                / loss_records.len() as f64
        };
        log_loss!(loss);

        // When probe succeeds, set bandwidth directly
        if is_probe_result {
            self.loss_controller.set_bandwidth_estimate(delay_estimate);
        }

        if let Some(acked_bitrate) = acked_bitrate {
            self.loss_controller.set_acknowledged_bitrate(acked_bitrate);
        }

        // This corresponds to UpdateLossBasedEstimator + UpdateEstimate
        self.loss_controller
            .update_bandwidth_estimate(&loss_records, delay_estimate);

        // Keep clean startup traffic in the loss observation's numerator and
        // time span. Skipping it makes isolated gaps look like sustained loss.
        if in_startup_phase(self.started_at, now) && loss <= 0.001 {
            self.loss_controller.set_bandwidth_estimate(delay_estimate);
        }

        // Loss-based result is capped by delay_based_limit
        let loss_result = self.loss_controller.loss_based_result();
        if let Some(loss_estimate) = loss_result.bandwidth_estimate {
            if loss_estimate > delay_estimate {
                // Loss controller produced higher estimate than delay controller
                // Cap it at delay estimate (delay controller is the upper limit)
                self.loss_controller.set_bandwidth_estimate(delay_estimate);
            }
        }

        // Feed the (possibly combined) estimate into subcomponents wanting it.
        self.propagate_estimate();
    }

    pub fn poll_timeout(&self) -> (Option<Instant>, Reason) {
        let delay_timeout = Some(self.delay_controller.poll_timeout());
        let probe_timeout = Some(self.probe_control.poll_timeout());
        let probe_estimator_timeout = Some(self.probe_estimator.poll_timeout());
        (delay_timeout, Reason::BweDelayControl)
            .soonest((probe_timeout, Reason::BweProbeControl))
            .soonest((probe_estimator_timeout, Reason::BweProbeEstimator))
    }

    /// Handle periodic timeout for BWE components.
    pub fn handle_timeout(
        &mut self,
        desired_bitrate: Bitrate,
        do_probe: bool,
        now: Instant,
    ) -> Option<ProbeClusterConfig> {
        self.delay_controller
            .handle_timeout(self.acked_bitrate_estimator.current_estimate(), now);

        // Update probe control with desired bitrate.
        self.probe_control.set_desired_bitrate(desired_bitrate);

        // Get ALR state and forward to both probe control and loss controller
        let alr_start_time = self.alr_detector.alr_start_time();
        if let Some(t) = alr_start_time {
            self.probe_control.set_alr_start_time(t);
        } else {
            self.probe_control.set_alr_stop_time(now);
        }

        self.loss_controller.set_alr_start_time(alr_start_time);

        // Get link capacity estimate and forward to loss controller.
        let link_capacity = self.link_capacity_estimator.capacity_estimate(now);
        self.loss_controller
            .set_link_capacity_estimate(link_capacity);

        // Clean up expired probe cluster state
        self.probe_estimator.handle_timeout(now);

        // Feed the current estimate into subcontrollers, if it changed.
        self.propagate_estimate();

        // If we can't probe, clear any pending/active probes
        if !do_probe {
            self.probe_estimator.clear_probes();
        }

        self.probe_control.enable(do_probe);

        // Timer-driven probe logic (WebRTC `Process()` equivalent).
        self.probe_control.handle_timeout(now)
    }

    fn propagate_estimate(&mut self) {
        // Do we have a value?
        let Some(estimate) = self.last_estimate() else {
            return;
        };
        // Congestion can clear (or start) without changing the numeric estimate.
        let cause = self.bandwidth_limited_cause();
        self.probe_control.set_estimated_bitrate(estimate, cause);

        // Did it change?
        if self.last_updated_estimate == Some(estimate) {
            return;
        }
        self.alr_detector.set_estimated_bitrate(estimate);

        // Don't update until this changes.
        self.last_updated_estimate = Some(estimate);
    }

    fn bandwidth_limited_cause(&self) -> BandwidthLimitedCause {
        if self.delay_controller.is_overusing() {
            return BandwidthLimitedCause::DelayBasedLimitedDelayIncreased;
        }

        match self.loss_controller.loss_based_result().state {
            LossControllerState::DelayBased => BandwidthLimitedCause::DelayBasedLimited,
            LossControllerState::Increasing => BandwidthLimitedCause::LossLimitedBweIncreasing,
            LossControllerState::Decreasing => BandwidthLimitedCause::LossLimitedBwe,
        }
    }

    /// Get the latest estimate.
    pub fn last_estimate(&self) -> Option<Bitrate> {
        let delay_estimate = self.delay_controller.last_estimate();

        let loss_result = self.loss_controller.loss_based_result();

        // Only apply loss-based limiting when actively in a loss-limiting state
        match loss_result.state {
            LossControllerState::DelayBased => {
                // Loss controller defers to delay-based estimate
                delay_estimate
            }
            LossControllerState::Decreasing | LossControllerState::Increasing => {
                // Loss controller is actively limiting or recovering
                match (delay_estimate, loss_result.bandwidth_estimate) {
                    (Some(de), Some(le)) => Some(de.min(le)),
                    (None, le @ Some(_)) => le,
                    (de @ Some(_), None) => de,
                    (None, None) => None,
                }
            }
        }
    }

    /// Start analyzing a probe cluster.
    ///
    /// This should be called when the pacer starts sending a probe cluster,
    /// to tell the estimator which cluster to watch for in TWCC feedback.
    /// Returns `true` if the probe was started, `false` if rejected.
    pub fn start_probe(&mut self, config: ProbeClusterConfig, now: Instant) -> bool {
        self.probe_estimator.probe_start(config, now)
    }

    /// End a probe cluster and mark it for cleanup.
    ///
    /// This should be called when the pacer finishes sending a probe cluster.
    /// The estimator will continue collecting feedback for a cluster history period
    /// to allow late-arriving TWCC reports to refine the estimate.
    pub fn end_probe(&mut self, now: Instant, cluster_id: TwccClusterId) {
        self.probe_estimator.end_probe(now, cluster_id);
    }

    pub fn reset(&mut self, init_bitrate: Bitrate) {
        *self = Self::new(init_bitrate);
    }
}

/// A RTP packet that has been sent and acknowledged by the receiver in a TWCC report.
#[derive(Debug, Copy, Clone)]
pub struct AckedPacket {
    /// The TWCC sequence number
    seq_no: TwccSeq,
    /// The size of the packets in bytes.
    size: DataSize,
    /// When we sent the packet
    local_send_time: Instant,
    /// When the packet was received at the remote, note this Instant is only usable with other
    /// instants of the same type i.e. those that represent a TWCC reported receive time for this
    /// session.
    remote_recv_time: Instant,
    /// The local time when received confirmation that the other side received the seq i.e. when we
    /// received the TWCC report for this packet.
    local_recv_time: Instant,
}

impl AckedPacket {
    fn rtt(&self) -> Duration {
        self.local_recv_time - self.local_send_time
    }

    fn order_by_receive_time(lhs: &Self, rhs: &Self) -> Ordering {
        if lhs.remote_recv_time != rhs.remote_recv_time {
            lhs.remote_recv_time.cmp(&rhs.remote_recv_time)
        } else if lhs.local_send_time != rhs.local_send_time {
            lhs.local_send_time.cmp(&rhs.local_send_time)
        } else {
            lhs.seq_no.cmp(&rhs.seq_no)
        }
    }
}

// NB: Extracted for lifetime reasons
fn in_startup_phase(started_at: Option<Instant>, now: Instant) -> bool {
    started_at
        .map(|s| now.duration_since(s) <= STARTUP_PHASE)
        .unwrap_or(false)
}

impl TryFrom<&TwccSendRecord> for AckedPacket {
    type Error = ();

    fn try_from(value: &TwccSendRecord) -> Result<Self, Self::Error> {
        let Some(remote_recv_time) = value.remote_recv_time() else {
            return Err(());
        };
        let Some(local_recv_time) = value.local_recv_time() else {
            return Err(());
        };

        Ok(Self {
            seq_no: value.seq(),
            size: value.size().into(),
            local_send_time: value.local_send_time(),
            remote_recv_time,
            local_recv_time,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BandwidthUsage {
    Overuse,
    Normal,
    Underuse,
}

impl fmt::Display for BandwidthUsage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BandwidthUsage::Overuse => write!(f, "overuse"),
            BandwidthUsage::Normal => write!(f, "normal"),
            BandwidthUsage::Underuse => write!(f, "underuse"),
        }
    }
}

#[cfg(test)]
mod probe_result_tests {
    use super::*;
    use super::probe::ProbeKind;
    use crate::rtp_::TwccPacketId;

    #[test]
    fn bonded_probe_preserves_aggregate_capacity_across_different_path_delays() {
        for congested in 0..3 {
            let start = Instant::now();
            let mut bwe = SendSideBandwidthEstimator::new(Bitrate::mbps(5));
            let config = ProbeClusterConfig::new(1.into(), Bitrate::mbps(10), ProbeKind::Initial);
            assert!(bwe.start_probe(config, start));
            let records: Vec<_> = (0..40u64).map(|index| {
                let path = index % 2;
                let sent = start + Duration::from_millis(index);
                let saturated = congested == 2 || (congested == 1 && path == 0);
                let arrival = if saturated { index / 2 * 10 + path } else { index };
                let mut record = TwccSendRecord::test_new(
                    TwccPacketId::with_cluster(index, config.cluster()), sent, 1200,
                    start + Duration::from_millis(300),
                    Some(start + Duration::from_millis(arrival + 10 + path * 70)),
                );
                record.egress_path = Some(path);
                record
            }).collect();
            bwe.update(records.iter(), start + Duration::from_millis(300));
            let estimate = bwe.last_estimate().unwrap();
            if congested == 2 {
                assert!(estimate < Bitrate::mbps(2), "queued probe must reduce: {estimate:?}");
            } else if congested == 1 {
                assert!(estimate >= Bitrate::mbps(5), "one saturated route cannot cap both: {estimate:?}");
            } else {
                assert!(estimate > Bitrate::mbps(9), "combined path capacity was lost: {estimate:?}");
            }
        }
    }

    #[test]
    fn ciphertext_repairs_must_not_measure_the_original_probe_route() {
        for marked_redundant in [false, true] {
            let start = Instant::now();
            let mut bwe = SendSideBandwidthEstimator::new(Bitrate::mbps(5));
            let config = ProbeClusterConfig::new(1.into(), Bitrate::mbps(10), ProbeKind::Initial);
            assert!(bwe.start_probe(config, start));
            let records: Vec<_> = (0..40u64).map(|index| {
                let sent = start + Duration::from_millis(index);
                let repaired = index < 10;
                let mut record = TwccSendRecord::test_new(
                    TwccPacketId::with_cluster(index, config.cluster()), sent, 1200,
                    start + Duration::from_millis(500),
                    Some(sent + Duration::from_millis(if repaired { 350 } else { 20 + index % 2 * 60 })),
                );
                record.egress_path = Some(index % 2);
                if repaired && marked_redundant {
                    record.egress_path = None;
                    record.redundant = true;
                }
                record
            }).collect();
            bwe.update(records.iter(), start + Duration::from_millis(500));
            let estimate = bwe.last_estimate().unwrap();
            if marked_redundant {
                assert!(estimate >= Bitrate::mbps(5), "repair arrival was treated as probe congestion: {estimate:?}");
            } else {
                assert!(estimate < Bitrate::mbps(3), "fixture must reproduce the old false backoff: {estimate:?}");
            }
        }
    }

    #[test]
    fn clean_startup_feedback_is_not_missing_from_loss_observations() {
        let start = Instant::now();
        let mut bwe = SendSideBandwidthEstimator::new(Bitrate::mbps(5));
        for batch in 0..30u64 {
            let records: Vec<_> = (0..50u64).map(|index| {
                let seq = batch * 50 + index;
                let sent = start + Duration::from_millis(seq * 2);
                let lost = index < 10 && (batch == 0 || batch == 20);
                TwccSendRecord::test_new(
                    TwccPacketId::new(seq), sent, 1200,
                    start + Duration::from_millis((batch + 1) * 100 + 20),
                    (!lost).then_some(sent + Duration::from_millis(10)),
                )
            }).collect();
            let now = start + Duration::from_millis((batch + 1) * 100 + 20);
            bwe.update(records.iter(), now);
            assert!(bwe.last_estimate().unwrap() >= Bitrate::mbps(4),
                "startup batch {batch}: {:?} {}", bwe.last_estimate(),
                bwe.loss_controller.diagnostic_snapshot(now));
        }
    }

    #[test]
    fn sender_limited_startup_probe_does_not_collapse_estimate() {
        let now = Instant::now();
        let initial = Bitrate::bps(6_070_588);
        let mut bwe = SendSideBandwidthEstimator::new(initial);
        let config = ProbeClusterConfig::new(1.into(), Bitrate::mbps(12), ProbeKind::Initial);
        assert!(bwe.start_probe(config, now));
        // A stalled sender spreads its probe over 190 ms. The receiver keeps up;
        // this measures sender output, not a 960 kbps network capacity limit.
        let records: Vec<_> = (0..20u64).map(|i| {
            let sent = now + Duration::from_millis(i * 10);
            TwccSendRecord::test_new(
                TwccPacketId::with_cluster(i, config.cluster()), sent, 1200,
                now + Duration::from_millis(220), Some(sent + Duration::from_millis(10)),
            )
        }).collect();
        bwe.update(records.iter(), now + Duration::from_millis(220));
        assert_eq!(bwe.last_estimate(), Some(initial));
    }

    #[test]
    fn saturated_probe_can_reduce_startup_estimate() {
        let now = Instant::now();
        let mut bwe = SendSideBandwidthEstimator::new(Bitrate::bps(6_070_588));
        let config = ProbeClusterConfig::new(1.into(), Bitrate::mbps(12), ProbeKind::Initial);
        assert!(bwe.start_probe(config, now));
        let records: Vec<_> = (0..20u64).map(|i| {
            TwccSendRecord::test_new(
                TwccPacketId::with_cluster(i, config.cluster()),
                now + Duration::from_micros(i * 800), 1200,
                now + Duration::from_millis(220),
                Some(now + Duration::from_millis(10 + i * 10)),
            )
        }).collect();
        bwe.update(records.iter(), now + Duration::from_millis(220));
        let estimate = bwe.last_estimate().unwrap();
        assert!(estimate >= Bitrate::kbps(900) && estimate <= Bitrate::mbps(1));
    }

    #[test]
    fn low_probe_does_not_erase_acknowledged_throughput() {
        let now = Instant::now();
        for initial in [Bitrate::mbps(3), Bitrate::mbps(6)] {
            let mut bwe = SendSideBandwidthEstimator::new(initial);
            // Establish 4.8 Mbps delivery before a delayed probe result arrives.
            for i in 0..=325u64 {
                bwe.acked_bitrate_estimator.update(
                    now + Duration::from_millis(i * 2), DataSize::bytes(1200),
                );
            }
            assert_eq!(bwe.acked_bitrate_estimator.current_estimate(), Some(Bitrate::bps(4_800_000)));
            let start = now + Duration::from_millis(652);
            let config = ProbeClusterConfig::new(1.into(), Bitrate::mbps(12), ProbeKind::Initial);
            assert!(bwe.start_probe(config, start));
            let records: Vec<_> = (0..20u64).map(|i| {
                TwccSendRecord::test_new(
                    TwccPacketId::with_cluster(i, config.cluster()),
                    start + Duration::from_micros(i * 800), 1200,
                    start + Duration::from_millis(120),
                    Some(start + Duration::from_millis(i * 5)),
                )
            }).collect();
            bwe.update(records.iter(), start + Duration::from_millis(120));
            assert_eq!(bwe.last_estimate(), Some(initial.min(Bitrate::bps(4_080_000))));
        }
    }
}

#[cfg(test)]
mod handoff_tests {
    use super::*;
    use crate::rtp_::TwccPacketId;

    #[test]
    fn handoff_reprobes_from_low_rate_without_old_feedback_or_hold() {
        let start = Instant::now();
        let mut bwe = Bwe::new(Bitrate::kbps(250));
        bwe.set_desired_bitrate(Bitrate::mbps(6));
        bwe.handle_timeout(start, true);
        bwe.handle_timeout(start, true);
        bwe.bwe.probe_control.set_estimated_bitrate(
            Bitrate::kbps(250), BandwidthLimitedCause::LossLimitedBwe,
        );
        bwe.request_path_probe(start + Duration::from_secs(2));
        assert!(bwe.bwe.probe_control.handle_timeout(start + Duration::from_secs(2)).is_none());

        let handoff = start + Duration::from_secs(3);
        assert_eq!(bwe.restart_on_path_change(handoff), Some(Bitrate::kbps(250)));
        assert_eq!(bwe.last_estimate(), Some(Bitrate::kbps(250)));
        assert_eq!(bwe.poll_estimate(), None);
        let old = TwccSendRecord::test_new(
            TwccPacketId::with_cluster(1u64, 1u64), start, 1200, handoff, None,
        );
        bwe.update([&old].into_iter(), handoff);
        assert_eq!(bwe.bwe.delay_controller.feedback_age_ms(handoff), None);
        assert_eq!(bwe.bwe.started_at, None);

        let mut now = handoff;
        let mut sequence = 100u64;
        for _ in 0..8 {
            let config = bwe.handle_timeout(now, true).expect("fresh handoff must probe promptly");
            assert!(bwe.start_probe(config, now));
            let spacing = Duration::from_secs_f64(9600.0 / config.target_bitrate().as_f64());
            let records: Vec<_> = (0..32).map(|index| {
                let sent = now + spacing * index;
                sequence += 1;
                TwccSendRecord::test_new(
                    TwccPacketId::with_cluster(sequence, config.cluster()), sent,
                    1200, sent + Duration::from_millis(20), Some(sent + Duration::from_millis(10)),
                )
            }).collect();
            now += spacing * 32 + Duration::from_millis(20);
            bwe.update(records.iter().chain([&old]), now);
            bwe.end_probe(now, config.cluster());
            if bwe.last_estimate().unwrap() >= Bitrate::mbps(6) { break; }
        }
        assert!(bwe.last_estimate().unwrap() >= Bitrate::mbps(6));
        assert!(now.duration_since(handoff) < Duration::from_secs(3));
    }

    #[test]
    fn handoff_without_feedback_does_not_raise_estimate() {
        let now = Instant::now();
        let mut bwe = Bwe::new(Bitrate::kbps(250));
        bwe.set_desired_bitrate(Bitrate::mbps(6));
        bwe.restart_on_path_change(now);
        for tick in 0..200 {
            bwe.handle_timeout(now + Duration::from_millis(tick * 25), true);
            assert_eq!(bwe.last_estimate(), Some(Bitrate::kbps(250)));
            assert_eq!(bwe.poll_estimate(), None);
        }
    }

    #[test]
    fn handoff_still_reacts_to_loss_on_the_new_path() {
        let now = Instant::now();
        let mut bwe = Bwe::new(Bitrate::mbps(2));
        bwe.set_desired_bitrate(Bitrate::mbps(6));
        bwe.restart_on_path_change(now);
        for batch in 0..20u64 {
            let records: Vec<_> = (0..50u64).map(|index| {
                let seq = batch * 50 + index;
                let sent = now + Duration::from_millis(seq * 10);
                TwccSendRecord::test_new(
                    TwccPacketId::with_cluster(seq, 999u64), sent, 1200,
                    sent + Duration::from_millis(20),
                    (index % 5 == 0).then_some(sent + Duration::from_millis(10)),
                )
            }).collect();
            let time = now + Duration::from_millis((batch + 1) * 500 + 20);
            bwe.update(records.iter(), time);
            bwe.handle_timeout(time, true);
        }
        assert!(bwe.last_estimate().unwrap() < Bitrate::mbps(2));
    }
}
