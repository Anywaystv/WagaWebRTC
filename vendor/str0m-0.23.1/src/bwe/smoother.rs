use std::time::{Duration, Instant};

use crate::rtp_::Bitrate;

const EMIT_INTERVAL: Duration = Duration::from_millis(200);

/// Rate-limits encoder notifications without averaging the controller a second time.
pub struct EstimateSmoother {
    pending: Option<(Instant, Bitrate)>,
    emitted: Option<(Instant, Bitrate)>,
}

impl EstimateSmoother {
    pub fn new() -> Self {
        Self {
            pending: None,
            emitted: None,
        }
    }

    pub fn record(&mut self, now: Instant, estimate: Bitrate) {
        self.pending = Some((now, estimate));
    }

    pub fn poll(&mut self) -> Option<Bitrate> {
        let (now, rate) = self.pending?;
        if let Some((last, previous)) = self.emitted {
            if rate >= previous && now.saturating_duration_since(last) < EMIT_INTERVAL {
                return None;
            }
        }
        self.pending = None;
        self.emitted = Some((now, rate));
        Some(rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_feedback_refreshes_but_polling_does_not() {
        let now = Instant::now();
        let rate = Bitrate::from(500_000.0);
        let mut smoother = EstimateSmoother::new();
        smoother.record(now, rate);
        assert_eq!(smoother.poll(), Some(rate));
        assert_eq!(smoother.poll(), None);
        smoother.record(now + Duration::from_millis(20), rate);
        assert_eq!(smoother.poll(), None);
        smoother.record(now + EMIT_INTERVAL, rate);
        assert_eq!(smoother.poll(), Some(rate));
        assert_eq!(smoother.poll(), None);
    }

    #[test]
    fn decreases_are_immediate_and_increases_are_not_averaged() {
        let now = Instant::now();
        let low = Bitrate::from(250_000.0);
        let high = Bitrate::from(5_000_000.0);
        let mut smoother = EstimateSmoother::new();
        smoother.record(now, high);
        assert_eq!(smoother.poll(), Some(high));
        smoother.record(now + Duration::from_millis(20), low);
        assert_eq!(smoother.poll(), Some(low));
        smoother.record(now + Duration::from_millis(40), high);
        assert_eq!(smoother.poll(), None);
        smoother.record(now + Duration::from_millis(220), high);
        assert_eq!(smoother.poll(), Some(high));
    }

    #[test]
    fn timeouts_without_feedback_do_not_refresh_encoder_estimates() {
        let now = Instant::now();
        let mut bwe = super::super::Bwe::new(Bitrate::from(500_000.0));
        for tick in 0..100 {
            let time = now + Duration::from_millis(tick * 20);
            bwe.update(std::iter::empty(), time);
            bwe.handle_timeout(time, false);
            assert_eq!(bwe.poll_estimate(), None);
        }
    }
}
