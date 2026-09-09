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
    local_candidates: Vec<(SocketAddr, Candidate)>,
    pending: Option<SdpPendingOffer>,
    audio_mid: Option<str0m::media::Mid>,
    video_mid: Option<str0m::media::Mid>,
    output: VecDeque<Transmit>,
    events: VecDeque<PeerEvent>,
    media: VecDeque<Media>,
    bitrate_estimates: VecDeque<u64>,
    next_timeout: Option<Instant>,
    disconnect_grace: Duration,
    disconnect_deadline: Option<Instant>,
    disconnect_reported: bool,
    ever_connected: bool,
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
        let mut publisher = Self::build(
            audio,
            video == Some(Codec::H264),
            video == Some(Codec::H265),
            initial_bitrate,
            desired_bitrate,
        )?;
        publisher.disconnect_grace = Duration::from_secs(15);
        Ok(publisher)
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
        if let Some(desired_bitrate) = desired_bitrate.or(initial_bitrate) {
            rtc.bwe().set_desired_bitrate(Bitrate::bps(desired_bitrate));
        }

        Ok(Self {
            rtc,
            local_candidates: Vec::new(),
            pending: None,
            audio_mid: None,
            video_mid: None,
            output: VecDeque::new(),
            events: VecDeque::new(),
            media: VecDeque::new(),
            bitrate_estimates: VecDeque::new(),
            next_timeout: None,
            disconnect_grace: Duration::ZERO,
            disconnect_deadline: None,
            disconnect_reported: false,
            ever_connected: false,
            audio_codec: audio,
            video_enabled: h264 || h265,
        })
    }

    pub fn add_local_candidate(&mut self, address: SocketAddr) -> Result<(), String> {
        let candidate = Candidate::host(address, "udp").map_err(|error| error.to_string())?;
        if let Some(candidate) = self.rtc.add_local_candidate(candidate) {
            self.local_candidates.push((address, candidate.clone()));
        }
        self.drain()
    }

    pub fn add_server_reflexive_candidate(
        &mut self,
        address: SocketAddr,
        base: SocketAddr,
    ) -> Result<(), String> {
        let candidate =
            Candidate::server_reflexive(address, base, "udp").map_err(|error| error.to_string())?;
        if let Some(candidate) = self.rtc.add_local_candidate(candidate) {
            self.local_candidates.push((base, candidate.clone()));
        }
        self.drain()
    }

    pub fn remove_local_candidate(&mut self, base: SocketAddr) -> Result<(), String> {
        self.local_candidates.retain(|(address, candidate)| {
            if *address == base {
                self.rtc.direct_api().invalidate_candidate(candidate);
                false
            } else {
                true
            }
        });
        self.output.retain(|packet| packet.source != base);
        self.drain()
    }

    pub fn create_offer(&mut self) -> Result<String, String> {
        if self.pending.is_some() {
            return Err("an offer is already pending".into());
        }
        let stream_id = Some("waga".to_string());
        let mut change = self.rtc.sdp_api();
        if self.audio_codec.is_some() {
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
        // Untrusted UDP input can be unrelated or malformed; a parse failure
        // must not terminate an otherwise healthy ICE/DTLS session.
        let Ok(receive) = Receive::new(Protocol::Udp, source, destination, contents) else {
            return Ok(());
        };
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
        if self.disconnect_deadline.is_some() || self.disconnect_reported {
            return self.drain();
        }
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

    pub fn request_path_probe(&mut self) -> Result<(), String> {
        let now = Instant::now();
        self.rtc.bwe().request_path_probe(now);
        self.rtc
            .handle_input(Input::Timeout(now))
            .map_err(|error| error.to_string())?;
        self.drain_at(now)
    }

    pub fn restart_on_path_change(&mut self) -> Result<(), String> {
        let now = Instant::now();
        self.bitrate_estimates.clear();
        self.rtc.bwe().restart_on_path_change(now);
        self.rtc
            .handle_input(Input::Timeout(now))
            .map_err(|error| error.to_string())?;
        self.drain_at(now)
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

    pub fn bwe_diagnostic_snapshot(&mut self) -> String {
        self.rtc.bwe().diagnostic_snapshot(Instant::now())
    }

    fn drain(&mut self) -> Result<(), String> {
        self.drain_at(Instant::now())
    }

    fn drain_at(&mut self, now: Instant) -> Result<(), String> {
        loop {
            match self.rtc.poll_output().map_err(|error| error.to_string())? {
                Output::Timeout(deadline) => {
                    if self.disconnect_deadline.is_some_and(|expiry| now >= expiry) {
                        self.disconnect_deadline = None;
                        self.disconnect_reported = true;
                        self.events.push_back(PeerEvent::Disconnected);
                    }
                    self.next_timeout = Some(
                        self.disconnect_deadline
                            .map_or(deadline, |expiry| deadline.min(expiry)),
                    );
                    return Ok(());
                }
                Output::Transmit(transmit) => self.output.push_back(Transmit {
                    source: transmit.source,
                    destination: transmit.destination,
                    contents: transmit.contents.into(),
                }),
                Output::Event(event) => match event {
                    Event::Connected => {
                        self.ever_connected = true;
                        self.events.push_back(PeerEvent::Connected);
                    }
                    Event::IceConnectionStateChange(state) => {
                        self.ice_state_changed(state, now);
                    }
                    Event::KeyframeRequest(_) => self.events.push_back(PeerEvent::KeyframeRequest),
                    Event::EgressBitrateEstimate(estimate) => {
                        let bitrate = match estimate {
                            BweKind::Twcc(bitrate) | BweKind::Remb(_, bitrate) => bitrate.as_u64(),
                            _ => continue,
                        };
                        // Only the current estimate should drive the encoder after
                        // an application stall; replaying old rates delays recovery.
                        self.bitrate_estimates.clear();
                        self.bitrate_estimates.push_back(bitrate);
                    }
                    Event::Closed => {
                        self.disconnect_deadline = None;
                        self.events.push_back(PeerEvent::Closed);
                    }
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

    fn ice_state_changed(&mut self, state: IceConnectionState, now: Instant) {
        match state {
            IceConnectionState::Connected | IceConnectionState::Completed => {
                self.disconnect_deadline = None;
            }
            IceConnectionState::Disconnected if !self.disconnect_reported => {
                if self.ever_connected && !self.disconnect_grace.is_zero() {
                    // Keep ICE, DTLS and the bonded paths alive while a phone
                    // changes networks. Repeated events must not extend this window.
                    self.disconnect_deadline
                        .get_or_insert(now + self.disconnect_grace);
                    for mid in [self.audio_mid, self.video_mid].into_iter().flatten() {
                        if let Some(stream) = self.rtc.direct_api().stream_tx_by_mid(mid, None) {
                            stream.discard_queued_media();
                        }
                    }
                } else {
                    self.disconnect_reported = true;
                    self.events.push_back(PeerEvent::Disconnected);
                }
            }
            _ => {}
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
    fn removing_interface_invalidates_host_and_reflexive_candidates() {
        let mut peer = Publisher::new(Some(Codec::Opus), None).unwrap();
        let wifi = "192.0.2.1:40000".parse().unwrap();
        let cellular = "192.0.2.2:40000".parse().unwrap();
        for _ in 0..3 {
            peer.add_local_candidate(wifi).unwrap();
            peer.add_server_reflexive_candidate("203.0.113.1:50000".parse().unwrap(), wifi)
                .unwrap();
            peer.add_local_candidate(cellular).unwrap();
            let removed: Vec<_> = peer
                .local_candidates
                .iter()
                .filter(|(base, _)| *base == wifi)
                .map(|(_, candidate)| candidate.clone())
                .collect();
            assert_eq!(removed.len(), 2);
            peer.remove_local_candidate(wifi).unwrap();
            assert!(
                peer.local_candidates
                    .iter()
                    .all(|(base, _)| *base == cellular)
            );
            for candidate in removed {
                assert!(!peer.rtc.direct_api().invalidate_candidate(&candidate));
            }
        }
    }

    #[test]
    fn repeated_recovery_never_fires_an_old_disconnect_deadline() {
        let mut peer = Publisher::new(Some(Codec::Opus), None).unwrap();
        peer.ever_connected = true;
        let start = Instant::now();
        for cycle in 0..3 {
            let now = start + Duration::from_secs(cycle * 30);
            peer.ice_state_changed(IceConnectionState::Disconnected, now);
            peer.ice_state_changed(IceConnectionState::Completed, now + Duration::from_secs(10));
            peer.drain_at(now + Duration::from_secs(20)).unwrap();
            assert_eq!(peer.disconnect_deadline, None);
            assert!(!peer.disconnect_reported);
            assert_eq!(peer.poll_event(), None);
        }
    }

    #[test]
    fn temporary_disconnect_cancels_teardown_on_recovery() {
        let mut peer = Publisher::new(Some(Codec::Opus), Some(Codec::H264)).unwrap();
        peer.ever_connected = true;
        let now = Instant::now();
        peer.ice_state_changed(IceConnectionState::Disconnected, now);
        let deadline = peer.disconnect_deadline;
        assert_eq!(deadline, Some(now + Duration::from_secs(15)));
        peer.ice_state_changed(
            IceConnectionState::Disconnected,
            now + Duration::from_secs(8),
        );
        assert_eq!(peer.disconnect_deadline, deadline);
        assert_eq!(peer.poll_event(), None);
        peer.ice_state_changed(IceConnectionState::Completed, now + Duration::from_secs(10));
        peer.drain_at(now + Duration::from_secs(20)).unwrap();
        assert_eq!(peer.disconnect_deadline, None);
        assert!(!peer.disconnect_reported);
        assert_eq!(peer.poll_event(), None);
    }

    #[test]
    fn sustained_disconnect_reports_once_at_deadline() {
        let mut peer = Publisher::new(Some(Codec::Opus), None).unwrap();
        peer.ever_connected = true;
        let now = Instant::now();
        peer.ice_state_changed(IceConnectionState::Disconnected, now);
        peer.drain_at(now + Duration::from_secs(14)).unwrap();
        assert_eq!(peer.poll_event(), None);
        peer.drain_at(now + Duration::from_secs(15)).unwrap();
        assert_eq!(peer.poll_event(), Some(PeerEvent::Disconnected));
        peer.ice_state_changed(
            IceConnectionState::Disconnected,
            now + Duration::from_secs(16),
        );
        peer.drain_at(now + Duration::from_secs(40)).unwrap();
        assert_eq!(peer.poll_event(), None);
    }

    #[test]
    fn initial_failure_and_receiver_disconnect_are_not_delayed() {
        for mut peer in [
            Publisher::new(Some(Codec::Opus), None).unwrap(),
            Publisher::new_receiver().unwrap(),
        ] {
            peer.ice_state_changed(IceConnectionState::Disconnected, Instant::now());
            assert_eq!(peer.poll_event(), Some(PeerEvent::Disconnected));
            assert_eq!(peer.disconnect_deadline, None);
        }
    }

    #[test]
    fn repeated_interface_replacement_resumes_media_without_new_offer() {
        check_repeated_interface_recovery(false);
    }

    #[test]
    fn repeated_health_withdrawal_restores_same_socket_without_new_offer() {
        check_repeated_interface_recovery(true);
    }

    fn check_repeated_interface_recovery(reuse_socket: bool) {
        let mut publisher = Publisher::new(Some(Codec::Opus), Some(Codec::H264)).unwrap();
        let mut receiver = Publisher::new_receiver().unwrap();
        let mut active: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        publisher.add_local_candidate(active).unwrap();
        receiver
            .add_local_candidate("127.0.0.1:40001".parse().unwrap())
            .unwrap();
        let answer = receiver
            .accept_offer(&publisher.create_offer().unwrap())
            .unwrap();
        publisher.accept_answer(&answer).unwrap();
        let start = Instant::now();
        let mut received = [false; 4];
        let mut available = true;
        for step in 0..1000_u64 {
            let millis = step * 20;
            let now = start + Duration::from_millis(millis);
            if [2000, 6000, 10_000].contains(&millis) {
                publisher.remove_local_candidate(active).unwrap();
                available = !reuse_socket;
                if !reuse_socket {
                    active.set_port(active.port() + 2);
                    publisher.add_local_candidate(active).unwrap();
                }
            }
            if reuse_socket && [4000, 8000, 12_000].contains(&millis) {
                available = true;
                publisher.add_local_candidate(active).unwrap();
            }
            for peer in [&mut publisher, &mut receiver] {
                peer.rtc.handle_input(Input::Timeout(now)).unwrap();
                peer.drain_at(now).unwrap();
            }
            if publisher.rtc.is_connected() {
                let writer = publisher.rtc.writer(publisher.video_mid.unwrap()).unwrap();
                let pt = writer
                    .payload_params()
                    .find(|p| p.spec().codec == str0m::format::Codec::H264)
                    .unwrap()
                    .pt();
                writer
                    .write(
                        pt,
                        now,
                        MediaTime::new(millis * 90, Frequency::NINETY_KHZ),
                        Arc::<[u8]>::from([0, 0, 0, 1, 0x65, 0x01]),
                    )
                    .unwrap();
                publisher.drain_at(now).unwrap();
            }
            while let Some(packet) = publisher.poll_transmit() {
                if !available || packet.source != active {
                    continue;
                }
                let input = Receive::new(
                    Protocol::Udp,
                    packet.source,
                    packet.destination,
                    &packet.contents,
                )
                .unwrap();
                receiver
                    .rtc
                    .handle_input(Input::Receive(now, input))
                    .unwrap();
                receiver.drain_at(now).unwrap();
            }
            while let Some(packet) = receiver.poll_transmit() {
                if !available || packet.destination != active {
                    continue;
                }
                let input = Receive::new(
                    Protocol::Udp,
                    packet.source,
                    packet.destination,
                    &packet.contents,
                )
                .unwrap();
                publisher
                    .rtc
                    .handle_input(Input::Receive(now, input))
                    .unwrap();
                publisher.drain_at(now).unwrap();
            }
            while receiver.poll_media().is_some() {
                let phase = match millis {
                    0..2000 => Some(0),
                    4000..6000 => Some(1),
                    8000..10_000 => Some(2),
                    12_000.. => Some(3),
                    _ => None,
                };
                if let Some(phase) = phase {
                    received[phase] = true;
                }
            }
            while let Some(event) = publisher.poll_event() {
                assert_ne!(event, PeerEvent::Disconnected);
                assert_ne!(event, PeerEvent::Closed);
            }
        }
        assert_eq!(
            received, [true; 4],
            "media must resume after every interface replacement"
        );
    }

    #[test]
    fn ten_second_network_outage_resumes_same_media_session() {
        let mut publisher = Publisher::new(Some(Codec::Opus), Some(Codec::H264)).unwrap();
        let mut receiver = Publisher::new_receiver().unwrap();
        publisher
            .add_local_candidate("127.0.0.1:40000".parse().unwrap())
            .unwrap();
        receiver
            .add_local_candidate("127.0.0.1:40001".parse().unwrap())
            .unwrap();
        let answer = receiver
            .accept_offer(&publisher.create_offer().unwrap())
            .unwrap();
        publisher.accept_answer(&answer).unwrap();
        let start = Instant::now();
        let mut resumed = false;
        let mut before_outage = false;
        for step in 0..800_u64 {
            let millis = step * 20;
            let now = start + Duration::from_millis(millis);
            for peer in [&mut publisher, &mut receiver] {
                peer.rtc.handle_input(Input::Timeout(now)).unwrap();
                peer.drain_at(now).unwrap();
            }
            if publisher.rtc.is_connected() {
                let writer = publisher.rtc.writer(publisher.video_mid.unwrap()).unwrap();
                let pt = writer
                    .payload_params()
                    .find(|p| p.spec().codec == str0m::format::Codec::H264)
                    .unwrap()
                    .pt();
                writer
                    .write(
                        pt,
                        now,
                        MediaTime::new(millis * 90, Frequency::NINETY_KHZ),
                        Arc::<[u8]>::from([0, 0, 0, 1, 0x65, 0x01]),
                    )
                    .unwrap();
                publisher.drain_at(now).unwrap();
            }
            for (from, to) in [(&mut publisher, &mut receiver)] {
                while let Some(packet) = from.poll_transmit() {
                    if (2000..12_000).contains(&millis) {
                        continue;
                    }
                    let receive = Receive::new(
                        Protocol::Udp,
                        packet.source,
                        packet.destination,
                        &packet.contents,
                    )
                    .unwrap();
                    to.rtc.handle_input(Input::Receive(now, receive)).unwrap();
                    to.drain_at(now).unwrap();
                }
            }
            while let Some(packet) = receiver.poll_transmit() {
                if (2000..12_000).contains(&millis) {
                    continue;
                }
                let receive = Receive::new(
                    Protocol::Udp,
                    packet.source,
                    packet.destination,
                    &packet.contents,
                )
                .unwrap();
                publisher
                    .rtc
                    .handle_input(Input::Receive(now, receive))
                    .unwrap();
                publisher.drain_at(now).unwrap();
            }
            while receiver.poll_media().is_some() {
                before_outage |= millis < 2000;
                resumed |= millis >= 12_000;
            }
            while let Some(event) = publisher.poll_event() {
                assert_ne!(event, PeerEvent::Disconnected);
                assert_ne!(event, PeerEvent::Closed);
            }
        }
        assert!(
            before_outage && resumed,
            "media must arrive before and after the outage"
        );
        assert!(publisher.rtc.is_connected());
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
        assert!(offer.contains("rtx/90000"));
    }

    #[test]
    fn bwe_probes_and_reprobes_an_application_limited_sender() {
        let mut publisher = Publisher::new_with_bwe(
            Some(Codec::Opus),
            Some(Codec::H264),
            Some(500_000),
            Some(5_000_000),
        )
        .unwrap();
        let mut receiver = Publisher::new_receiver().unwrap();
        publisher
            .add_local_candidate("127.0.0.1:40000".parse().unwrap())
            .unwrap();
        receiver
            .add_local_candidate("127.0.0.1:40001".parse().unwrap())
            .unwrap();
        let offer = publisher.create_offer().unwrap();
        let rtx_pts: Vec<u8> = offer
            .lines()
            .filter(|line| line.contains(" rtx/90000"))
            .map(|line| {
                line.strip_prefix("a=rtpmap:")
                    .unwrap()
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap()
            })
            .collect();
        assert!(!rtx_pts.is_empty());
        let answer = receiver.accept_offer(&offer).unwrap();
        publisher.accept_answer(&answer).unwrap();
        let start = Instant::now();
        let mut outbound = VecDeque::new();
        let mut feedback = VecDeque::new();
        let mut early_probes = 0;
        let mut late_probes = 0;
        let mut estimates = Vec::new();
        for millis in 0..12_000_u64 {
            let now = start + Duration::from_millis(millis);
            for peer in [&mut publisher, &mut receiver] {
                peer.rtc.handle_input(Input::Timeout(now)).unwrap();
                peer.drain().unwrap();
            }
            if publisher.rtc.is_connected() && millis % 20 == 0 {
                let writer = publisher.rtc.writer(publisher.video_mid.unwrap()).unwrap();
                let pt = writer
                    .payload_params()
                    .find(|p| p.spec().codec == str0m::format::Codec::H264)
                    .unwrap()
                    .pt();
                writer
                    .write(
                        pt,
                        now,
                        MediaTime::new(millis * 90, Frequency::NINETY_KHZ),
                        Arc::<[u8]>::from([0, 0, 0, 1, 0x65, 0x01]),
                    )
                    .unwrap();
                publisher.drain().unwrap();
            }
            while let Some(packet) = publisher.poll_transmit() {
                if packet.contents.len() >= 12
                    && packet.contents[0] & 0xc0 == 0x80
                    && rtx_pts.contains(&(packet.contents[1] & 0x7f))
                {
                    if millis < 3000 {
                        early_probes += 1;
                    }
                    if millis > 6000 {
                        late_probes += 1;
                    }
                }
                outbound.push_back((now + Duration::from_millis(10), packet));
            }
            while let Some(packet) = receiver.poll_transmit() {
                feedback.push_back((now + Duration::from_millis(10), packet));
            }
            for (queue, peer) in [
                (&mut outbound, &mut receiver),
                (&mut feedback, &mut publisher),
            ] {
                while queue.front().is_some_and(|(due, _)| *due <= now) {
                    let (_, packet) = queue.pop_front().unwrap();
                    let receive = Receive::new(
                        Protocol::Udp,
                        packet.source,
                        packet.destination,
                        &packet.contents,
                    )
                    .unwrap();
                    peer.rtc.handle_input(Input::Receive(now, receive)).unwrap();
                    peer.drain().unwrap();
                }
            }
            while receiver.poll_media().is_some() {}
            if let Some(estimate) = publisher.poll_bitrate_estimate() {
                estimates.push(estimate);
            }
        }
        assert!(publisher.rtc.is_connected());
        assert!(early_probes > 0, "startup probes were not sent");
        assert!(late_probes > 0, "ALR did not trigger periodic probing");
        assert!(
            estimates.iter().any(|rate| *rate > 500_000),
            "TWCC did not raise the starting estimate: {estimates:?}"
        );
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
        check_video_delivery(false);
    }

    #[test]
    fn bandwidth_handoff_preserves_the_media_session() {
        check_video_delivery(true);
    }

    fn check_video_delivery(handoff: bool) {
        let mut publisher = if handoff {
            Publisher::new_with_bwe(
                Some(Codec::Opus),
                Some(Codec::H264),
                Some(250_000),
                Some(6_000_000),
            )
        } else {
            Publisher::new(Some(Codec::Opus), Some(Codec::H264))
        }
        .unwrap();
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

        for packet in [&[][..], &[0x57, 0x47, 0x52, 0x31], &[0; 20]] {
            publisher
                .receive(
                    "127.0.0.1:40001".parse().unwrap(),
                    "127.0.0.1:40000".parse().unwrap(),
                    packet,
                )
                .unwrap();
            receiver
                .receive(
                    "127.0.0.1:40000".parse().unwrap(),
                    "127.0.0.1:40001".parse().unwrap(),
                    packet,
                )
                .unwrap();
        }
        assert!(publisher.rtc.is_connected());
        assert!(receiver.rtc.is_connected());

        if handoff {
            publisher.restart_on_path_change().unwrap();
            assert!(publisher.rtc.is_connected());
            assert!(receiver.rtc.is_connected());
        }
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
