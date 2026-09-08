use super::{CodecExtra, Depacketizer, PacketError, Packetizer};

// RFC 3640 AAC-hbr: one access unit, 13-bit size and 3-bit index.
#[derive(Debug)]
pub struct AacPacketizer;

impl Packetizer for AacPacketizer {
    fn packetize(&mut self, mtu: usize, payload: &[u8]) -> Result<Vec<Vec<u8>>, PacketError> {
        if payload.is_empty() {
            return Ok(vec![]);
        }
        if payload.len() > 8191 || payload.len() + 4 > mtu {
            return Err(PacketError::ErrPayloadTooLarge);
        }
        let mut packet = Vec::with_capacity(payload.len() + 4);
        packet.extend_from_slice(&[0, 16]);
        packet.extend_from_slice(&((payload.len() as u16) << 3).to_be_bytes());
        packet.extend_from_slice(payload);
        Ok(vec![packet])
    }

    fn is_marker(&mut self, _: &[u8], _: Option<&[u8]>, last: bool) -> bool {
        last
    }
}

#[derive(Debug)]
pub struct AacDepacketizer;

impl Depacketizer for AacDepacketizer {
    fn out_size_hint(&self, size: usize) -> Option<usize> {
        Some(size)
    }

    fn depacketize(
        &mut self,
        packet: &[u8],
        out: &mut Vec<u8>,
        _: &mut CodecExtra,
    ) -> Result<(), PacketError> {
        if packet.len() < 4 || packet[..2] != [0, 16] || packet[3] & 7 != 0 {
            return Err(PacketError::ErrShortPacket);
        }
        let size = (u16::from_be_bytes([packet[2], packet[3]]) >> 3) as usize;
        if size == 0 || size != packet.len() - 4 {
            return Err(PacketError::ErrShortPacket);
        }
        out.extend_from_slice(&packet[4..]);
        Ok(())
    }

    fn is_partition_head(&self, _: &[u8]) -> bool {
        true
    }
    fn is_partition_tail(&self, marker: bool, _: &[u8]) -> bool {
        marker
    }
}
