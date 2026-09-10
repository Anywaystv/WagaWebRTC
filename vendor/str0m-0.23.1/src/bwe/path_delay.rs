use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use super::time::{TimeDelta, Timestamp};
use crate::rtp_::TwccSendRecord;

#[derive(Default)]
pub(super) struct PathDelay {
    minimum: HashMap<u64, TimeDelta>,
    pending_loss: BTreeMap<u64, (Instant, TwccSendRecord)>,
    emitted: Option<u64>,
}

impl PathDelay {
    pub fn path_count(&self) -> usize {
        self.minimum.len()
    }

    pub fn loss_records(&mut self, records: &[&TwccSendRecord], now: Instant) -> Option<Vec<TwccSendRecord>> {
        if self.minimum.is_empty() { return None; }
        for record in records {
            let sequence = *record.seq();
            if self.emitted.is_some_and(|emitted| sequence <= emitted) { continue; }
            self.pending_loss.entry(sequence).and_modify(|(_, pending)| {
                if record.remote_recv_time().is_some() { *pending = (*record).clone(); }
            }).or_insert_with(|| (now, (*record).clone()));
        }
        // Allow two feedback intervals for a slower path's arrival report.
        let spread = *self.minimum.values().max().unwrap() - *self.minimum.values().min().unwrap();
        let spread = match spread { TimeDelta::Positive(value) => value, _ => Duration::ZERO };
        let grace = (Duration::from_millis(200) + spread).min(Duration::from_secs(1));
        let mut ready = Vec::new();
        while self.pending_loss.first_key_value().is_some_and(|(_, (first_seen, _))| {
            now.saturating_duration_since(*first_seen) >= grace || self.pending_loss.len() > 4096
        }) {
            let (sequence, (_, record)) = self.pending_loss.pop_first().unwrap();
            self.emitted = Some(sequence);
            ready.push(record);
        }
        Some(ready)
    }

    pub fn update(&mut self, records: &[&TwccSendRecord]) -> Option<Vec<TwccSendRecord>> {
        if self.minimum.is_empty() && !records.iter().any(|record| record.egress_path.is_some()) {
            return None;
        }
        for record in records {
            let (Some(path), Some(received)) = (record.egress_path, record.remote_recv_time()) else {
                continue;
            };
            let transit = Timestamp::from(received) - record.local_send_time();
            if let Some(minimum) = self.minimum.get_mut(&path) {
                *minimum = (*minimum).min(transit);
            } else if self.minimum.len() < 256 {
                self.minimum.insert(path, transit);
            }
        }
        Some(records.iter().filter_map(|record| {
            let path = record.egress_path?;
            let received = record.remote_recv_time()?;
            let transit = Timestamp::from(received) - record.local_send_time();
            let TimeDelta::Positive(queue) = transit - *self.minimum.get(&path)? else {
                return None;
            };
            let normalized: Instant = record.local_send_time().checked_add(queue)?;
            Some(record.with_receive_time(normalized))
        }).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp_::TwccPacketId;
    use std::time::Duration;

    #[test]
    fn late_packets_settle_before_loss_and_missing_packets_still_count() {
        let start = Instant::now();
        let mut delay = PathDelay::default();
        let mut missing = TwccSendRecord::test_new(TwccPacketId::new(0u64), start, 1200,
            start + Duration::from_millis(100), None);
        missing.egress_path = Some(0);
        let mut clean = TwccSendRecord::test_new(TwccPacketId::new(1u64), start + Duration::from_millis(1), 1200,
            start + Duration::from_millis(100), Some(start + Duration::from_millis(11)));
        clean.egress_path = Some(1);
        for arrives in [true, false] {
            let records = [&missing, &clean];
            delay.update(&records);
            assert!(delay.loss_records(&records, start + Duration::from_millis(100)).unwrap().is_empty());
            if arrives {
                let late = missing.with_receive_time(start + Duration::from_millis(60));
                delay.update(&[&late]);
                assert!(delay.loss_records(&[&late], start + Duration::from_millis(150)).unwrap().is_empty());
            }
            let settled = delay.loss_records(&[], start + Duration::from_millis(500)).unwrap();
            assert_eq!(settled.len(), 2);
            assert_eq!(settled.iter().filter(|r| r.remote_recv_time().is_none()).count(), usize::from(!arrives));
            assert!(delay.loss_records(&records, start + Duration::from_secs(1)).unwrap().is_empty());
            delay = PathDelay::default();
        }
    }

    #[test]
    fn fixed_path_delay_is_removed_but_queue_growth_is_preserved() {
        let start = Instant::now();
        let mut delay = PathDelay::default();
        let records: Vec<_> = (0..20u64).map(|index| {
            let sent = start + Duration::from_millis(index * 10);
            let path = index % 2;
            let mut record = TwccSendRecord::test_new(
                TwccPacketId::new(index), sent, 1200, start + Duration::from_secs(1),
                Some(sent + Duration::from_millis(20 + path * 80 + index / 2 * 3)),
            );
            record.egress_path = Some(path);
            record
        }).collect();
        let normalized = delay.update(&records.iter().collect::<Vec<_>>()).unwrap();
        for (index, record) in normalized.iter().enumerate() {
            assert_eq!(record.remote_recv_time().unwrap() - record.local_send_time(),
                Duration::from_millis(index as u64 / 2 * 3));
            assert_eq!(record.rtt(), records[index].rtt());
        }
        assert_eq!(delay.path_count(), 2);
    }

    #[test]
    fn standard_transport_and_ambiguous_duplicates_do_not_create_path_baselines() {
        let start = Instant::now();
        let mut delay = PathDelay::default();
        let mut record = TwccSendRecord::test_new(TwccPacketId::new(0u64), start, 1200,
            start + Duration::from_millis(40), Some(start + Duration::from_millis(20)));
        assert!(delay.update(&[&record]).is_none());
        record.egress_path = Some(1);
        assert_eq!(delay.update(&[&record]).unwrap().len(), 1);
        record.egress_path = None;
        record.redundant = true;
        assert!(delay.update(&[&record]).unwrap().is_empty());
        assert_eq!(delay.path_count(), 1);
    }
}
