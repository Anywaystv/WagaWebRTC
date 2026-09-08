use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use str0m::bwe::{Bitrate, BweKind};
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::media::{Direction, Frequency, MediaKind, MediaTime};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};

mod ffi;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Codec {
    H264,
    H265,
    Opus,
    Aac,
}

#[derive(Debug, Eq, PartialEq)]
pub enum PeerEvent {
    Connected,
    Disconnected,
    Closed,
    KeyframeRequest,
}

#[derive(Debug)]
pub struct Transmit {
    pub source: SocketAddr,
    pub destination: SocketAddr,
    pub contents: Vec<u8>,
}

#[derive(Debug)]
pub struct Media {
    pub codec: Codec,
    pub media_time: u64,
    pub clock_rate: u32,
    pub ntp_micros: Option<u64>,
    pub sender_media_time: u64,
    pub contents: Vec<u8>,
}

pub struct Publisher {
    rtc: Rtc,
    pending: Option<SdpPendingOffer>,
    audio_mid: Option<str0m::media::Mid>,
    video_mid: Option<str0m::media::Mid>,
    output: VecDeque<Transmit>,
    events: VecDeque<PeerEvent>,
    media: VecDeque<Media>,
    bitrate_estimates: VecDeque<u64>,
    next_timeout: Option<Instant>,
    audio_enabled: bool,
    audio_codec: Option<Codec>,
    video_enabled: bool,
}

impl Publisher {
    pub fn new(audio: Option<Codec>, video: Option<Codec>) -> Result<Self, String> {
        Self::new_with_bwe(audio, video, None, None)
    }

    pub fn new_with_bwe(
        audio: Option<Codec>,
        video: Option<Codec>,
        initial_bitrate: Option<u64>,
        desired_bitrate: Option<u64>,
    ) -> Result<Self, String> {
        if audio.is_none() && video.is_none() {
            return Err("at least one media track is required".into());
        }
        if audio.is_some_and(|codec| !matches!(codec, Codec::Opus | Codec::Aac)) {
            return Err("audio must use Opus or AAC".into());
        }
        if video.is_some_and(|codec| !matches!(codec, Codec::H264 | Codec::H265)) {
            return Err("video must use H264 or H265".into());
        }
        Self::build(
            audio,
            video == Some(Codec::H264),
            video == Some(Codec::H265),
            initial_bitrate,
            desired_bitrate,
        )
    }

    pub fn new_receiver() -> Result<Self, String> {
        Self::build(Some(Codec::Opus), true, true, None, None)
    }

    fn build(
        audio: Option<Codec>,
        h264: bool,
        h265: bool,
        initial_bitrate: Option<u64>,
        desired_bitrate: Option<u64>,
    ) -> Result<Self, String> {
        let mut config = RtcConfig::new()
            .clear_codecs()
            .enable_opus(audio == Some(Codec::Opus))
            .enable_h264(h264)
            .enable_h265(h265);
        if audio == Some(Codec::Aac) {
            config.codec_config().add_config(
                112.into(), None, str0m::format::Codec::Aac,
                Frequency::FORTY_EIGHT_KHZ, Some(2),
                str0m::format::FormatParams::parse_line(
                    "config=1190;streamtype=5;profile-level-id=1;mode=AAC-hbr;sizeLength=13;indexLength=3;indexDeltaLength=3"
                ),
            );
        }
        if let Some(initial_bitrate) = initial_bitrate {
            config = config.enable_bwe(Some(Bitrate::bps(initial_bitrate)));
        }
        let mut rtc = config.build(Instant::now());
        if let Some(desired_bitrate) = desired_bitrate {
            rtc.bwe().set_desired_bitrate(Bitrate::bps(desired_bitrate));
        }

        Ok(Self {
            rtc,
            pending: None,
            audio_mid: None,
            video_mid: None,
            output: VecDeque::new(),
            events: VecDeque::new(),
            media: VecDeque::new(),
            bitrate_estimates: VecDeque::new(),
            next_timeout: None,
            audio_enabled: audio.is_some(),
            audio_codec: audio,
            video_enabled: h264 || h265,
        })
    }

    pub fn add_local_candidate(&mut self, address: SocketAddr) -> Result<(), String> {
        let candidate = Candidate::host(address, "udp").map_err(|error| error.to_string())?;
        self.rtc.add_local_candidate(candidate);
        self.drain()
    }

    pub fn add_server_reflexive_candidate(
        &mut self,
        address: SocketAddr,
        base: SocketAddr,
    ) -> Result<(), String> {
        let candidate =
            Candidate::server_reflexive(address, base, "udp").map_err(|error| error.to_string())?;
        self.rtc.add_local_candidate(candidate);
        self.drain()
    }

    pub fn create_offer(&mut self) -> Result<String, String> {
        if self.pending.is_some() {
            return Err("an offer is already pending".into());
        }
        let stream_id = Some("waga".to_string());
        let mut change = self.rtc.sdp_api();
        if self.audio_enabled {
            self.audio_mid = Some(change.add_media(
                MediaKind::Audio,
                Direction::SendOnly,
                stream_id.clone(),
                Some("audio".to_string()),
                None,
            ));
        }
        if self.video_enabled {
            self.video_mid = Some(change.add_media(
                MediaKind::Video,
                Direction::SendOnly,
                stream_id,
                Some("video".to_string()),
                None,
            ));
        }
        let (offer, pending) = change
            .apply()
            .ok_or_else(|| "offer contained no changes".to_string())?;
        self.pending = Some(pending);
        self.drain()?;

        Ok(offer.to_sdp_string())
    }

    pub fn accept_answer(&mut self, sdp: &str) -> Result<(), String> {
        let answer = SdpAnswer::from_sdp_string(sdp).map_err(|error| {
            if self.audio_codec == Some(Codec::Aac) {
                format!("Invalid AAC answer: {error}. Select Opus if this receiver does not support AAC.")
            } else {
                error.to_string()
            }
        })?;
        let pending = self
            .pending
            .take()
            .ok_or_else(|| "no offer is pending".to_string())?;
        self.rtc
            .sdp_api()
            .accept_answer(pending, answer)
            .map_err(|error| {
                if self.audio_codec == Some(Codec::Aac) {
                    format!("AAC negotiation failed: {error}. Select Opus if this receiver does not support AAC.")
                } else {
                    error.to_string()
                }
            })?;
        self.drain()?;
        if self.audio_codec == Some(Codec::Aac) {
            let negotiated = self
                .audio_mid
                .and_then(|mid| self.rtc.writer(mid))
                .is_some_and(|writer| {
                    writer
                        .payload_params()
                        .any(|p| p.spec().codec == str0m::format::Codec::Aac)
                });
            if !negotiated {
                return Err(
                    "The WHIP receiver does not accept AAC. Select Opus for this receiver.".into(),
                );
            }
        }
        Ok(())
    }

    pub fn create_receive_offer(&mut self) -> Result<String, String> {
        if self.pending.is_some() {
            return Err("an offer is already pending".into());
        }
        let mut change = self.rtc.sdp_api();
        self.video_mid =
            Some(change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None));
        self.audio_mid =
            Some(change.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None));
        let (offer, pending) = change
            .apply()
            .ok_or_else(|| "offer contained no changes".to_string())?;
        self.pending = Some(pending);
        self.drain()?;
        Ok(offer.to_sdp_string())
    }

    pub fn accept_offer(&mut self, sdp: &str) -> Result<String, String> {
        let offer = SdpOffer::from_sdp_string(sdp).map_err(|error| error.to_string())?;
        let answer = self
            .rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|error| error.to_string())?;
        self.drain()?;
        Ok(answer.to_sdp_string())
    }

    pub fn receive(
        &mut self,
        source: SocketAddr,
        destination: SocketAddr,
        contents: &[u8],
    ) -> Result<(), String> {
        let receive = Receive::new(Protocol::Udp, source, destination, contents)
            .map_err(|error| error.to_string())?;
        self.rtc
            .handle_input(Input::Receive(Instant::now(), receive))
            .map_err(|error| error.to_string())?;
        self.drain()
    }

    pub fn handle_timeout(&mut self) -> Result<(), String> {
        self.rtc
            .handle_input(Input::Timeout(Instant::now()))
            .map_err(|error| error.to_string())?;
        self.drain()
    }

    pub fn timeout_after(&self) -> Duration {
        self.next_timeout
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::MAX)
    }

    pub fn send(&mut self, codec: Codec, media_time: u64, data: &[u8]) -> Result<(), String> {
        let mid = match codec {
            Codec::Opus | Codec::Aac => self.audio_mid,
            Codec::H264 | Codec::H265 => self.video_mid,
        }
        .ok_or_else(|| "media track was not offered".to_string())?;
        let writer = self
            .rtc
            .writer(mid)
            .ok_or_else(|| "media track is not negotiated".to_string())?;
        let payload_type = {
            let payload = writer
                .payload_params()
                .find(|payload| match codec {
                    Codec::H264 => payload.spec().codec == str0m::format::Codec::H264,
                    Codec::H265 => payload.spec().codec == str0m::format::Codec::H265,
                    Codec::Opus => payload.spec().codec == str0m::format::Codec::Opus,
                    Codec::Aac => payload.spec().codec == str0m::format::Codec::Aac,
                })
                .ok_or_else(|| "codec was not negotiated".to_string())?;
            payload.pt()
        };
        let frequency = match codec {
            Codec::H264 | Codec::H265 => Frequency::NINETY_KHZ,
            Codec::Opus | Codec::Aac => Frequency::FORTY_EIGHT_KHZ,
        };
        let data: Arc<[u8]> = match codec {
            Codec::H264 | Codec::H265 => avcc_to_annex_b(data)
                .unwrap_or_else(|| data.to_vec())
                .into(),
            Codec::Opus | Codec::Aac => Arc::from(data),
        };
        writer
            .write(
                payload_type,
                Instant::now(),
                MediaTime::new(media_time, frequency),
                data,
            )
            .map_err(|error| error.to_string())?;
        self.drain()
    }

    pub fn set_desired_bitrate(&mut self, bitrate: u64) {
        self.rtc.bwe().set_desired_bitrate(Bitrate::bps(bitrate));
    }

    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.output.pop_front()
    }

    pub fn poll_event(&mut self) -> Option<PeerEvent> {
        self.events.pop_front()
    }

    pub fn poll_media(&mut self) -> Option<Media> {
        self.media.pop_front()
    }

    pub fn poll_bitrate_estimate(&mut self) -> Option<u64> {
        self.bitrate_estimates.pop_front()
    }

    fn drain(&mut self) -> Result<(), String> {
        loop {
            match self.rtc.poll_output().map_err(|error| error.to_string())? {
                Output::Timeout(deadline) => {
                    self.next_timeout = Some(deadline);
                    return Ok(());
                }
                Output::Transmit(transmit) => self.output.push_back(Transmit {
                    source: transmit.source,
                    destination: transmit.destination,
                    contents: transmit.contents.into(),
                }),
                Output::Event(event) => match event {
                    Event::Connected => self.events.push_back(PeerEvent::Connected),
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                        self.events.push_back(PeerEvent::Disconnected)
                    }
                    Event::KeyframeRequest(_) => self.events.push_back(PeerEvent::KeyframeRequest),
                    Event::EgressBitrateEstimate(estimate) => {
                        let bitrate = match estimate {
                            BweKind::Twcc(bitrate) | BweKind::Remb(_, bitrate) => bitrate.as_u64(),
                            _ => continue,
                        };
                        self.bitrate_estimates.push_back(bitrate);
                    }
                    Event::Closed => self.events.push_back(PeerEvent::Closed),
                    Event::MediaData(data) => {
                        let codec = match data.params.spec().codec {
                            str0m::format::Codec::H264 => Some(Codec::H264),
                            str0m::format::Codec::H265 => Some(Codec::H265),
                            str0m::format::Codec::Opus => Some(Codec::Opus),
                            str0m::format::Codec::Aac => Some(Codec::Aac),
                            _ => None,
                        };
                        if let Some(codec) = codec {
                            let (ntp_micros, sender_media_time) = data
                                .last_sender_info
                                .and_then(|info| {
                                    info.ntp_time
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .ok()
                                        .map(|duration| {
                                            (
                                                duration.as_micros().min(u64::MAX as u128) as u64,
                                                info.rtp_time.numer(),
                                            )
                                        })
                                })
                                .map_or((None, 0), |(ntp, rtp)| (Some(ntp), rtp));
                            self.media.push_back(Media {
                                codec,
                                media_time: data.time.numer(),
                                clock_rate: data.time.denom(),
                                ntp_micros,
                                sender_media_time,
                                contents: data.data.to_vec(),
                            });
                        }
                    }
                    _ => {}
                },
            }
        }
    }
}

fn avcc_to_annex_b(data: &[u8]) -> Option<Vec<u8>> {
    let mut input = data;
    let mut output = Vec::with_capacity(data.len());
    while !input.is_empty() {
        let length = u32::from_be_bytes(input.get(..4)?.try_into().ok()?) as usize;
        if length == 0 || input.len() < 4 + length {
            return None;
        }
        output.extend_from_slice(&[0, 0, 0, 1]);
        output.extend_from_slice(&input[4..4 + length]);
        input = &input[4 + length..];
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transfer(from: &mut Publisher, to: &mut Publisher) {
        while let Some(packet) = from.poll_transmit() {
            to.receive(packet.source, packet.destination, &packet.contents)
                .unwrap();
        }
    }

    #[test]
    fn offer_contains_send_only_audio_and_video() {
        let mut publisher = Publisher::new(Some(Codec::Opus), Some(Codec::H264)).unwrap();
        publisher
            .add_local_candidate("127.0.0.1:40000".parse().unwrap())
            .unwrap();
        let offer = publisher.create_offer().unwrap();

        assert!(offer.contains("m=audio"));
        assert!(offer.contains("m=video"));
        assert!(offer.contains("a=sendonly"));
        assert!(offer.contains("H264"));
        assert!(offer.contains("opus"));
        assert!(!offer.contains("VP8"));
        assert!(!offer.contains("H265"));
        assert!(offer.contains("127.0.0.1 40000 typ host"));
    }

    #[test]
    fn offer_enables_transport_wide_feedback_with_bwe() {
        let mut publisher = Publisher::new_with_bwe(
            Some(Codec::Opus),
            Some(Codec::H264),
            Some(1_000_000),
            Some(3_000_000),
        )
        .unwrap();
        publisher
            .add_local_candidate("127.0.0.1:40000".parse().unwrap())
            .unwrap();

        let offer = publisher.create_offer().unwrap();

        assert!(offer.contains("transport-cc"));
        assert!(offer.contains("transport-wide-cc"));
    }

    #[test]
    fn offer_requires_media() {
        let error = Publisher::new(None, None).err().unwrap();

        assert_eq!(error, "at least one media track is required");
    }

    #[test]
    fn aac_is_opt_in_and_has_matching_audio_configuration() {
        let mut aac = Publisher::new(Some(Codec::Aac), Some(Codec::H264)).unwrap();
        let offer = aac.create_offer().unwrap();
        assert!(offer.contains("MPEG4-GENERIC/48000/2"));
        assert!(offer.contains("config=1190"));
        assert!(offer.contains("mode=AAC-hbr"));
        assert!(!offer.contains("opus/48000"));
        let mut opus = Publisher::new(Some(Codec::Opus), Some(Codec::H264)).unwrap();
        assert!(!opus.create_offer().unwrap().contains("MPEG4-GENERIC"));
    }

    #[test]
    fn aac_receiver_rejection_has_actionable_error() {
        let mut publisher = Publisher::new(Some(Codec::Aac), Some(Codec::H264)).unwrap();
        let mut receiver = Publisher::new_receiver().unwrap();
        let offer = publisher.create_offer().unwrap();
        let answer = receiver.accept_offer(&offer).unwrap();
        let error = publisher.accept_answer(&answer).unwrap_err();
        assert!(error.contains("Select Opus"), "{error}");
    }

    #[test]
    fn aac_access_units_and_timestamps_survive_encrypted_transport() {
        let mut publisher = Publisher::new(Some(Codec::Aac), None).unwrap();
        let mut receiver = Publisher::new(Some(Codec::Aac), None).unwrap();
        publisher
            .add_local_candidate("127.0.0.1:41000".parse().unwrap())
            .unwrap();
        receiver
            .add_local_candidate("127.0.0.1:41001".parse().unwrap())
            .unwrap();
        let offer = publisher.create_offer().unwrap();
        let answer = receiver.accept_offer(&offer).unwrap();
        assert!(answer.contains("MPEG4-GENERIC/48000/2"));
        publisher.accept_answer(&answer).unwrap();
        let mut connected = false;
        for _ in 0..200 {
            publisher.handle_timeout().unwrap();
            receiver.handle_timeout().unwrap();
            transfer(&mut publisher, &mut receiver);
            transfer(&mut receiver, &mut publisher);
            connected |= matches!(publisher.poll_event(), Some(PeerEvent::Connected));
            if connected {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(connected);
        for index in 0..3 {
            let frame = vec![0x20 + index as u8; 350];
            let timestamp = 48_000 + index * 1024;
            publisher.send(Codec::Aac, timestamp, &frame).unwrap();
            let mut received = false;
            for _ in 0..100 {
                transfer(&mut publisher, &mut receiver);
                transfer(&mut receiver, &mut publisher);
                if let Some(media) = receiver.poll_media() {
                    assert_eq!(media.codec, Codec::Aac);
                    assert_eq!(media.media_time, timestamp);
                    assert_eq!(media.contents, frame);
                    received = true;
                    break;
                }
                publisher.handle_timeout().unwrap();
                receiver.handle_timeout().unwrap();
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(received, "AAC access unit was not delivered");
        }
    }

    #[test]
    fn receiver_negotiates_publisher_offer() {
        let mut publisher = Publisher::new(Some(Codec::Opus), Some(Codec::H264)).unwrap();
        publisher
            .add_local_candidate("127.0.0.1:40000".parse().unwrap())
            .unwrap();
        let offer = publisher.create_offer().unwrap();
        let mut receiver = Publisher::new_receiver().unwrap();
        receiver
            .add_local_candidate("127.0.0.1:40001".parse().unwrap())
            .unwrap();
        let answer = receiver.accept_offer(&offer).unwrap();

        assert!(answer.contains("a=recvonly"));
        publisher.accept_answer(&answer).unwrap();
    }

    #[test]
    fn offer_contains_server_reflexive_candidate() {
        let mut publisher = Publisher::new(Some(Codec::Opus), Some(Codec::H264)).unwrap();
        publisher
            .add_local_candidate("10.0.0.2:40000".parse().unwrap())
            .unwrap();
        publisher
            .add_server_reflexive_candidate(
                "203.0.113.2:50000".parse().unwrap(),
                "10.0.0.2:40000".parse().unwrap(),
            )
            .unwrap();
        let offer = publisher.create_offer().unwrap();

        assert!(offer.contains("203.0.113.2 50000 typ srflx"));
    }

    #[test]
    fn publisher_delivers_a_complete_video_frame_to_receiver() {
        let mut publisher = Publisher::new(Some(Codec::Opus), Some(Codec::H264)).unwrap();
        let mut receiver = Publisher::new_receiver().unwrap();
        publisher
            .add_local_candidate("127.0.0.1:40000".parse().unwrap())
            .unwrap();
        receiver
            .add_local_candidate("127.0.0.1:40001".parse().unwrap())
            .unwrap();
        let offer = publisher.create_offer().unwrap();
        let answer = receiver.accept_offer(&offer).unwrap();
        publisher.accept_answer(&answer).unwrap();

        let mut connected = false;
        for _ in 0..200 {
            publisher.handle_timeout().unwrap();
            receiver.handle_timeout().unwrap();
            transfer(&mut publisher, &mut receiver);
            transfer(&mut receiver, &mut publisher);
            connected |= matches!(publisher.poll_event(), Some(PeerEvent::Connected));
            if connected {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(connected, "ICE and DTLS did not connect");

        let frame = [0, 0, 0, 2, 0x65, 0x01];
        publisher.send(Codec::H264, 90_000, &frame).unwrap();
        for _ in 0..100 {
            transfer(&mut publisher, &mut receiver);
            transfer(&mut receiver, &mut publisher);
            if let Some(media) = receiver.poll_media() {
                assert_eq!(media.codec, Codec::H264);
                assert_eq!(media.media_time, 90_000);
                assert_eq!(media.contents, [0, 0, 0, 1, 0x65, 0x01]);
                return;
            }
            publisher.handle_timeout().unwrap();
            receiver.handle_timeout().unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("video frame was not delivered");
    }

    #[test]
    fn converts_videotoolbox_avcc_to_annex_b() {
        let avcc = [0, 0, 0, 2, 0x67, 0x01, 0, 0, 0, 3, 0x65, 0x02, 0x03];

        assert_eq!(
            avcc_to_annex_b(&avcc).unwrap(),
            [0, 0, 0, 1, 0x67, 0x01, 0, 0, 0, 1, 0x65, 0x02, 0x03]
        );
        assert!(avcc_to_annex_b(&[0, 0, 0, 8, 1]).is_none());
    }
}
