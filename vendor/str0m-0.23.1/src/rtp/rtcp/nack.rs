use super::extend_u16;
use super::{FeedbackMessageType, RtcpHeader, RtcpPacket, SeqNo};
use super::{RtcpType, Ssrc, TransportType};

/// A NACK entry indiciating packets missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nack {
    /// Sender of this feedback. Mostly irrelevant, but part of RTCP packets.
    pub sender_ssrc: Ssrc,
    /// The SSRC this nack reports missing packets for.
    pub ssrc: Ssrc,
    /// The missing nack. This can be multiple segments.
    pub reports: Vec<NackEntry>,
}

/// A range of sequence numbers missing.
#[allow(missing_docs)]
#[derive(Debug, PartialEq, Eq, Default, Clone, Copy)]
pub struct NackEntry {
    pub pid: u16,
    pub blp: u16,
}

impl RtcpPacket for Nack {
    fn header(&self) -> RtcpHeader {
        RtcpHeader {
            rtcp_type: RtcpType::TransportLayerFeedback,
            feedback_message_type: FeedbackMessageType::TransportFeedback(TransportType::Nack),
            words_less_one: (self.length_words() - 1) as u16,
        }
    }

    fn length_words(&self) -> usize {
        // header
        // sender SSRC
        // media SSRC
        // 1 word per NackPair
        1 + 2 + self.reports.len()
    }

    fn write_to(&self, buf: &mut [u8]) -> usize {
        self.header().write_to(&mut buf[..4]);
        buf[4..8].copy_from_slice(&self.sender_ssrc.to_be_bytes());
        buf[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
        let mut buf = &mut buf[12..];
        for r in &self.reports {
            buf[0..2].copy_from_slice(&r.pid.to_be_bytes());
            buf[2..4].copy_from_slice(&r.blp.to_be_bytes());
            buf = &mut buf[4..];
        }
        self.length_words() * 4
    }
}

impl<'a> TryFrom<&'a [u8]> for Nack {
    type Error = &'static str;

    fn try_from(buf: &'a [u8]) -> Result<Self, Self::Error> {
        if buf.len() < 12 {
            return Err("Nack less than 12 bytes");
        }

        let sender_ssrc = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]).into();
        let ssrc = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]).into();

        if (buf.len() - 8) % 4 != 0 {
            return Err("Nack has incomplete feedback entry");
        }

        // Generic NACK has no 31-entry reception-report limit (RFC 4585).
        let reports = buf[8..]
            .chunks_exact(4)
            .map(|entry| NackEntry {
                pid: u16::from_be_bytes([entry[0], entry[1]]),
                blp: u16::from_be_bytes([entry[2], entry[3]]),
            })
            .collect();

        Ok(Nack {
            sender_ssrc,
            ssrc,
            reports,
        })
    }
}

impl NackEntry {
    /// Iterator over sequence numbers missing.
    ///
    /// The given sequence number is used to interpret ROC.
    pub fn into_iter(self, seq_no: SeqNo) -> impl Iterator<Item = SeqNo> {
        NackEntryIterator(self, 0, seq_no)
    }
}

pub struct NackEntryIterator(NackEntry, u16, SeqNo);

impl Iterator for NackEntryIterator {
    type Item = SeqNo;

    fn next(&mut self) -> Option<Self::Item> {
        let seq_16 = if self.1 == 0 {
            self.1 += 1;
            self.0.pid
        } else {
            loop {
                if self.1 >= 17 {
                    return None;
                }
                let i = self.1 - 1;
                self.1 += 1;
                if 1 << i & self.0.blp > 0 {
                    break self.0.pid.wrapping_add(self.1 - 1);
                }
            }
        };
        let l = extend_u16(Some(*self.2), seq_16);
        Some(l.into())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn nack_preserves_all_feedback_entries() {
        use super::super::{Rtcp, RtcpFb};
        use std::collections::VecDeque;

        for count in [1, 31, 32, 71, 300] {
            let mut body = vec![0; 8 + count * 4];
            body[..4].copy_from_slice(&123u32.to_be_bytes());
            body[4..8].copy_from_slice(&456u32.to_be_bytes());
            for (index, entry) in body[8..].chunks_exact_mut(4).enumerate() {
                entry[..2].copy_from_slice(&((index * 17) as u16).to_be_bytes());
                entry[2..].copy_from_slice(&0x8001u16.to_be_bytes());
            }
            let nack = Nack::try_from(body.as_slice()).unwrap();
            assert_eq!(nack.reports.len(), count);
            let mut wire = vec![0; 4 + body.len()];
            assert_eq!(nack.write_to(&mut wire), wire.len());
            assert_eq!(&wire[4..], body.as_slice());
            let mut packets = VecDeque::new();
            Rtcp::read_packet(&wire, &mut packets);
            let feedback: Vec<_> = RtcpFb::from_rtcp(packets).collect();
            assert_eq!(feedback.len(), 1);
            let RtcpFb::Nack(ssrc, entries) = &feedback[0] else {
                panic!("expected NACK")
            };
            assert_eq!(*ssrc, 456.into());
            assert_eq!(entries.len(), count);
            for (index, entry) in entries.iter().enumerate() {
                assert_eq!(entry.pid, (index * 17) as u16);
                assert_eq!(entry.blp, 0x8001);
            }
        }
    }

    #[test]
    fn nack_rejects_incomplete_feedback_entries() {
        for len in [0, 8, 11, 13, 14, 15] {
            assert!(Nack::try_from(vec![0; len].as_slice()).is_err());
        }
    }

    #[test]
    fn nack_entry_iter() {
        // 196_618
        let seq_no: SeqNo = (65_536_u64 * 3 + 10).into();

        // 196_508
        let pid = (65_536_u32 - 100) as u16;

        println!("{seq_no:?} {pid:?}");

        // 196_509, 196_512, 196_524
        let blp = 0b1000_0000_0000_1001;

        let entry = NackEntry { pid, blp };

        let nacks: Vec<_> = entry.into_iter(seq_no).collect();

        assert_eq!(
            nacks,
            vec![196508.into(), 196509.into(), 196512.into(), 196524.into()]
        );
    }
}
