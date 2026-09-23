use std::mem;
use std::time::{Duration, Instant};

use crate::rtp_::TwccSeq;

use super::super::AckedPacket;
use super::super::time::{TimeDelta, Timestamp};

const BURST_TIME_INTERVAL: Duration = Duration::from_millis(5);
const SEND_TIME_GROUP_LENGTH: Duration = Duration::from_millis(5);
const MAX_BURST_DURATION: Duration = Duration::from_millis(100);
const ARRIVAL_TIME_OFFSET_THRESHOLD: Duration = Duration::from_secs(3);
const REORDERED_RESET_THRESHOLD: usize = 3;

#[derive(Debug, Default)]
pub struct ArrivalGroup {
    first: Option<(TwccSeq, Instant, Instant)>,
    last_seq_no: Option<TwccSeq>,
    last_local_send_time: Option<Instant>,
    last_remote_recv_time: Option<Instant>,
    size: usize,
    last_local_recv_time: Option<Instant>,
}

impl ArrivalGroup {
    /// Maybe add a packet to the group.
    ///
    /// Returns [`true`] if a new group needs to be created and [`false`] otherwise.
    fn add_packet(&mut self, packet: &AckedPacket) -> bool {
        match self.belongs_to_group(packet) {
            Belongs::NewGroup => return true,
            Belongs::Skipped => return false,
            Belongs::Yes => {}
        }

        if self.first.is_none() {
            self.first = Some((
                packet.seq_no,
                packet.local_send_time,
                packet.remote_recv_time,
            ));
        }

        self.last_remote_recv_time = self
            .last_remote_recv_time
            .max(Some(packet.remote_recv_time));
        self.last_local_send_time = self.last_local_send_time.max(Some(packet.local_send_time));
        self.last_local_recv_time = self.last_local_recv_time.max(Some(packet.local_recv_time));
        self.size += 1;
        self.last_seq_no = self.last_seq_no.max(Some(packet.seq_no));

        false
    }

    fn belongs_to_group(&self, packet: &AckedPacket) -> Belongs {
        let Some((_, first_local_send_time, first_remote_recv_time)) = self.first else {
            // Start of the group
            return Belongs::Yes;
        };

        let Some(first_send_delta) = packet
            .local_send_time
            .checked_duration_since(first_local_send_time)
        else {
            // Out of order
            return Belongs::Skipped;
        };

        let send_time_delta = Timestamp::from(packet.local_send_time) - self.local_send_time();
        if send_time_delta == TimeDelta::ZERO {
            return Belongs::Yes;
        }
        let arrival_time_delta = Timestamp::from(packet.remote_recv_time) - self.remote_recv_time();

        let propagation_delta = arrival_time_delta - send_time_delta;
        // A backward receive timestamp cannot extend a forward-moving burst.
        if arrival_time_delta >= TimeDelta::ZERO
            && propagation_delta < TimeDelta::ZERO
            && arrival_time_delta <= BURST_TIME_INTERVAL
            && packet.remote_recv_time - first_remote_recv_time < MAX_BURST_DURATION
        {
            Belongs::Yes
        } else if first_send_delta > SEND_TIME_GROUP_LENGTH {
            Belongs::NewGroup
        } else {
            Belongs::Yes
        }
    }

    /// Calculate the send time delta between self and a subsequent group.
    fn departure_delta(&self, other: &Self) -> TimeDelta {
        Timestamp::from(other.local_send_time()) - self.local_send_time()
    }

    /// Calculate the remote receive time delta between self and a subsequent group.
    fn arrival_delta(&self, other: &Self) -> TimeDelta {
        Timestamp::from(other.remote_recv_time()) - self.remote_recv_time()
    }

    /// The local send time i.e. departure time, for the group.
    ///
    /// Panics if the group doesn't have at least one packet.
    fn local_send_time(&self) -> Instant {
        self.last_local_send_time
            .expect("local_send_time to only be called on non-empty groups")
    }

    /// The remote receive time i.e. arrival time, for the group.
    ///
    /// Panics if the group doesn't have at least one packet.
    fn remote_recv_time(&self) -> Instant {
        self.last_remote_recv_time
            .expect("remote_recv_time to only be called on non-empty groups")
    }
}

/// Whether a given packet is belongs to a group or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Belongs {
    /// The packet is belongs to the group.
    Yes,
    /// The packet is does not belong to the group, a new group should be created.
    NewGroup,
    /// The packet was skipped and a decision wasn't made.
    Skipped,
}

impl Belongs {
    #[cfg(test)]
    fn new_group(&self) -> bool {
        matches!(self, Self::NewGroup)
    }
}

#[derive(Debug, Default)]
pub struct ArrivalGroupAccumulator {
    previous_group: Option<ArrivalGroup>,
    current_group: ArrivalGroup,
    consecutive_reordered_groups: usize,
}

/// A clock discontinuity invalidates the trend built from the old timing history.
#[derive(Debug)]
pub enum ArrivalGroupUpdate {
    Pending,
    Delta(InterGroupDelayDelta),
    Reset,
}

impl ArrivalGroupAccumulator {
    /// Accumulate a packet, reporting new delay evidence or a timing reset.
    pub fn accumulate_packet(&mut self, packet: &AckedPacket) -> ArrivalGroupUpdate {
        if !self.current_group.add_packet(packet) {
            return ArrivalGroupUpdate::Pending;
        }

        let arrival_delta = self.arrival_delta();
        let send_delta = self.send_delta();
        let last_remote_recv_time = self.current_group.remote_recv_time();

        if let (Some(previous), Some(arrival_delta)) = (&self.previous_group, arrival_delta) {
            let feedback_delta = Timestamp::from(self.current_group.last_local_recv_time.unwrap())
                - previous.last_local_recv_time.unwrap();
            // Compare clock changes, not absolute timestamps: the two clocks have different epochs.
            if arrival_delta - feedback_delta >= ARRIVAL_TIME_OFFSET_THRESHOLD {
                return self.reset(packet);
            }
            if arrival_delta < TimeDelta::ZERO {
                self.consecutive_reordered_groups += 1;
                if self.consecutive_reordered_groups >= REORDERED_RESET_THRESHOLD {
                    return self.reset(packet);
                }
                // Discard this reordered group, retaining the last valid comparison point.
                self.current_group = ArrivalGroup::default();
                self.current_group.add_packet(packet);
                return ArrivalGroupUpdate::Pending;
            }
            self.consecutive_reordered_groups = 0;
        }

        self.previous_group = Some(mem::take(&mut self.current_group));
        self.current_group.add_packet(packet);

        match (send_delta, arrival_delta) {
            (Some(send_delta), Some(arrival_delta)) => {
                ArrivalGroupUpdate::Delta(InterGroupDelayDelta {
                    send_delta,
                    arrival_delta,
                    last_remote_recv_time,
                })
            }
            _ => ArrivalGroupUpdate::Pending,
        }
    }

    fn reset(&mut self, packet: &AckedPacket) -> ArrivalGroupUpdate {
        *self = Self::default();
        self.current_group.add_packet(packet);
        ArrivalGroupUpdate::Reset
    }

    fn arrival_delta(&self) -> Option<TimeDelta> {
        self.previous_group
            .as_ref()
            .map(|prev| prev.arrival_delta(&self.current_group))
    }

    fn send_delta(&self) -> Option<TimeDelta> {
        self.previous_group
            .as_ref()
            .map(|prev| prev.departure_delta(&self.current_group))
    }
}

/// The calculate delay delta between two groups of packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterGroupDelayDelta {
    /// The delta between the send times of the two groups i.e. delta between the last packet sent
    /// in each group.
    pub send_delta: TimeDelta,
    /// The delta between the remote arrival times of the two groups.
    pub arrival_delta: TimeDelta,
    /// The reported receive time for the last packet in the first arrival group.
    pub last_remote_recv_time: Instant,
}

#[cfg(test)]
mod test {
    use std::time::{Duration, Instant};

    use crate::rtp_::DataSize;

    use super::{
        AckedPacket, ArrivalGroup, ArrivalGroupAccumulator, ArrivalGroupUpdate, Belongs, TimeDelta,
    };

    #[test]
    fn test_arrival_group_all_packets_belong_to_empty_group() {
        let now = Instant::now();
        let group = ArrivalGroup::default();

        assert_eq!(
            group.belongs_to_group(&AckedPacket {
                seq_no: 1.into(),
                size: DataSize::ZERO,
                local_send_time: now,
                remote_recv_time: now + duration_us(10),
                local_recv_time: now + duration_us(12),
            }),
            Belongs::Yes,
            "Any packet should belong to an empty arrival group"
        );
    }

    #[test]
    fn test_arrival_group_all_packets_sent_within_burst_interval_belong() {
        let now = Instant::now();
        #[allow(clippy::vec_init_then_push)]
        let packets = {
            let mut packets = vec![];

            packets.push(AckedPacket {
                seq_no: 0.into(),
                size: DataSize::ZERO,
                local_send_time: now,
                remote_recv_time: now + duration_us(150),
                local_recv_time: now + duration_us(200),
            });

            packets.push(AckedPacket {
                seq_no: 1.into(),
                size: DataSize::ZERO,
                local_send_time: now + duration_us(50),
                remote_recv_time: now + duration_us(225),
                local_recv_time: now + duration_us(275),
            });

            packets.push(AckedPacket {
                seq_no: 2.into(),
                size: DataSize::ZERO,
                local_send_time: now + duration_us(1005),
                remote_recv_time: now + duration_us(1140),
                local_recv_time: now + duration_us(1190),
            });

            packets.push(AckedPacket {
                seq_no: 3.into(),
                size: DataSize::ZERO,
                local_send_time: now + duration_us(4995),
                remote_recv_time: now + duration_us(5001),
                local_recv_time: now + duration_us(5051),
            });

            // Should not belong
            packets.push(AckedPacket {
                seq_no: 4.into(),
                size: DataSize::ZERO,
                local_send_time: now + duration_us(5700),
                remote_recv_time: now + duration_us(6000),
                local_recv_time: now + duration_us(5750),
            });

            packets
        };

        let mut group = ArrivalGroup::default();

        for p in packets {
            let need_new_group = group.belongs_to_group(&p).new_group();
            if !need_new_group {
                group.add_packet(&p);
            }
        }

        assert_eq!(group.size, 4, "Expected group to contain 4 packets");
    }

    #[test]
    fn test_arrival_group_out_order_arrival_ignored() {
        let now = Instant::now();
        #[allow(clippy::vec_init_then_push)]
        let packets = {
            let mut packets = vec![];

            packets.push(AckedPacket {
                seq_no: 0.into(),
                size: DataSize::ZERO,
                local_send_time: now,
                remote_recv_time: now + duration_us(150),
                local_recv_time: now + duration_us(200),
            });

            packets.push(AckedPacket {
                seq_no: 1.into(),
                size: DataSize::ZERO,
                local_send_time: now + duration_us(50),
                remote_recv_time: now + duration_us(225),
                local_recv_time: now + duration_us(275),
            });

            packets.push(AckedPacket {
                seq_no: 2.into(),
                size: DataSize::ZERO,
                local_send_time: now + duration_us(1005),
                remote_recv_time: now + duration_us(1140),
                local_recv_time: now + duration_us(1190),
            });

            packets.push(AckedPacket {
                seq_no: 3.into(),
                size: DataSize::ZERO,
                local_send_time: now + duration_us(4995),
                remote_recv_time: now + duration_us(5001),
                local_recv_time: now + duration_us(5051),
            });

            // Should be skipped
            packets.push(AckedPacket {
                seq_no: 4.into(),
                size: DataSize::ZERO,
                local_send_time: now - duration_us(100),
                remote_recv_time: now + duration_us(5000),
                local_recv_time: now + duration_us(5050),
            });

            // Should not belong
            packets.push(AckedPacket {
                seq_no: 5.into(),
                size: DataSize::ZERO,
                local_send_time: now + duration_us(5700),
                remote_recv_time: now + duration_us(6000),
                local_recv_time: now + duration_us(6050),
            });

            packets
        };

        let mut group = ArrivalGroup::default();

        for p in packets {
            let need_new_group = group.belongs_to_group(&p).new_group();
            if !need_new_group {
                group.add_packet(&p);
            }
        }

        assert_eq!(group.size, 4, "Expected group to contain 4 packets");
    }

    #[test]
    fn test_arrival_group_arrival_membership() {
        let now = Instant::now();
        #[allow(clippy::vec_init_then_push)]
        let packets = {
            let mut packets = vec![];

            packets.push(AckedPacket {
                seq_no: 0.into(),
                size: DataSize::ZERO,
                local_send_time: now,
                remote_recv_time: now + duration_us(150),
                local_recv_time: now + duration_us(200),
            });

            packets.push(AckedPacket {
                seq_no: 1.into(),
                size: DataSize::ZERO,
                local_send_time: now + duration_us(50),
                remote_recv_time: now + duration_us(225),
                local_recv_time: now + duration_us(275),
            });

            packets.push(AckedPacket {
                seq_no: 2.into(),
                size: DataSize::ZERO,
                local_send_time: now + duration_us(5152),
                // Just less than 5ms inter arrival delta
                remote_recv_time: now + duration_us(5224),
                local_recv_time: now + duration_us(5274),
            });

            // Should not belong
            packets.push(AckedPacket {
                seq_no: 3.into(),
                size: DataSize::ZERO,
                local_send_time: now + duration_us(5700),
                remote_recv_time: now + duration_us(6000),
                local_recv_time: now + duration_us(6050),
            });

            packets
        };

        let mut group = ArrivalGroup::default();

        for p in packets {
            let need_new_group = group.belongs_to_group(&p).new_group();
            if !need_new_group {
                group.add_packet(&p);
            }
        }

        assert_eq!(group.size, 3, "Expected group to contain 4 packets");
    }

    #[test]
    fn group_reorder() {
        let data = vec![
            ((Duration::from_millis(0), Duration::from_millis(0)), None),
            ((Duration::from_millis(60), Duration::from_millis(5)), None),
            ((Duration::from_millis(40), Duration::from_millis(10)), None),
            (
                (Duration::from_millis(70), Duration::from_millis(20)),
                Some((TimeDelta::from_millis(-20), TimeDelta::from_millis(5))),
            ),
        ];

        let now = Instant::now();
        let mut aga = ArrivalGroupAccumulator::default();

        for ((local_send_time, remote_recv_time), deltas) in data {
            let group_delta = aga.accumulate_packet(&AckedPacket {
                seq_no: Default::default(),
                size: Default::default(),
                local_send_time: now + local_send_time,
                remote_recv_time: now + remote_recv_time,
                local_recv_time: Instant::now(), // does not matter
            });

            let group_delta = match group_delta {
                ArrivalGroupUpdate::Delta(d) => Some((d.send_delta, d.arrival_delta)),
                ArrivalGroupUpdate::Pending => None,
                ArrivalGroupUpdate::Reset => panic!("ordinary reordering must not reset timing"),
            };
            assert_eq!(group_delta, deltas);
        }
    }

    #[test]
    fn timestamp_jump_does_not_stall_group_completion() {
        let now = Instant::now();
        let mut groups = ArrivalGroupAccumulator::default();
        let mut recovered = 0;
        for i in 0..200u64 {
            let send = now + Duration::from_millis(i * 10);
            // One feedback timestamp jumps ahead, then returns to its original clock.
            let remote = send + Duration::from_secs(if i == 100 { 46 } else { 10 });
            let observation = groups.accumulate_packet(&AckedPacket {
                seq_no: i.into(),
                size: DataSize::bytes(1200),
                local_send_time: send,
                remote_recv_time: remote,
                local_recv_time: send + Duration::from_millis(50),
            });
            if i > 110 && matches!(observation, ArrivalGroupUpdate::Delta(_)) {
                recovered += 1;
            }
        }
        assert!(
            recovered > 80,
            "normal packets must produce fresh delay observations: {recovered}"
        );
    }

    #[test]
    fn persistent_clock_changes_reset_once_and_resume_observations() {
        for offset_ms in [36_000i64, -36_000] {
            let now = Instant::now();
            let mut groups = ArrivalGroupAccumulator::default();
            let mut resets = 0;
            let mut observations = 0;
            for i in 0..200u64 {
                let send = now + Duration::from_millis(i * 10);
                let remote_ms = 60_000 + i as i64 * 10 + if i >= 100 { offset_ms } else { 0 };
                match groups.accumulate_packet(&AckedPacket {
                    seq_no: i.into(),
                    size: DataSize::bytes(1200),
                    local_send_time: send,
                    remote_recv_time: now + Duration::from_millis(remote_ms as u64),
                    local_recv_time: send + Duration::from_millis(50),
                }) {
                    ArrivalGroupUpdate::Reset => resets += 1,
                    ArrivalGroupUpdate::Delta(delta) if i > 110 => {
                        assert_eq!(delta.send_delta, TimeDelta::from_millis(10));
                        assert_eq!(delta.arrival_delta, TimeDelta::from_millis(10));
                        observations += 1;
                    }
                    _ => {}
                }
            }
            assert_eq!(resets, 1, "offset {offset_ms}");
            assert_eq!(observations, 89, "offset {offset_ms}");
        }
    }

    #[test]
    fn long_feedback_gap_is_not_a_clock_change() {
        let now = Instant::now();
        let mut groups = ArrivalGroupAccumulator::default();
        let mut observations = 0;
        for i in 0..100u64 {
            let send = now + Duration::from_millis(i * 10 + if i >= 50 { 36_000 } else { 0 });
            match groups.accumulate_packet(&AckedPacket {
                seq_no: i.into(),
                size: DataSize::bytes(1200),
                local_send_time: send,
                remote_recv_time: send + Duration::from_secs(10),
                local_recv_time: send + Duration::from_millis(50),
            }) {
                ArrivalGroupUpdate::Reset => panic!("all clocks advanced together"),
                ArrivalGroupUpdate::Delta(delta) => {
                    assert_eq!(delta.send_delta, delta.arrival_delta);
                    observations += 1;
                }
                ArrivalGroupUpdate::Pending => {}
            }
        }
        assert_eq!(observations, 98);
    }

    #[test]
    fn reordered_receive_group_is_skipped_without_resetting() {
        let now = Instant::now();
        let mut groups = ArrivalGroupAccumulator::default();
        let mut deltas = Vec::new();
        for (i, remote_ms) in [0, 10, 5, 30, 40, 50].into_iter().enumerate() {
            let send = now + Duration::from_millis(i as u64 * 10);
            match groups.accumulate_packet(&AckedPacket {
                seq_no: (i as u64).into(),
                size: DataSize::bytes(1200),
                local_send_time: send,
                remote_recv_time: now + Duration::from_millis(remote_ms),
                local_recv_time: send + Duration::from_millis(50),
            }) {
                ArrivalGroupUpdate::Reset => panic!("isolated reordering must not reset timing"),
                ArrivalGroupUpdate::Delta(delta) => {
                    deltas.push((delta.send_delta, delta.arrival_delta))
                }
                ArrivalGroupUpdate::Pending => {}
            }
        }
        assert_eq!(
            deltas,
            [10, 20, 10].map(|ms| (TimeDelta::from_millis(ms), TimeDelta::from_millis(ms)))
        );
    }

    fn duration_us(us: u64) -> Duration {
        Duration::from_micros(us)
    }
}
