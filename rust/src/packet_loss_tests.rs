use super::*;
use str0m::stats::MediaEgressStats;

fn stats(mid: str0m::media::Mid, now: Instant, loss: Option<f32>) -> MediaEgressStats {
    MediaEgressStats {
        mid,
        rid: None,
        bytes: 1000,
        packets: 10,
        firs: 0,
        plis: 0,
        nacks: 0,
        rtt: None,
        loss,
        timestamp: now,
        remote: None,
    }
}

#[test]
fn video_loss_requires_valid_video_feedback_and_expires() {
    let mut peer =
        Publisher::new_with_bwe_and_stats(Some(Codec::Opus), Some(Codec::H264), None, None, true)
            .unwrap();
    peer.create_offer().unwrap();
    let now = Instant::now();
    let video = peer.video_mid.unwrap();
    assert_eq!(peer.video_packet_loss(now), None);
    peer.record_video_packet_loss(stats(peer.audio_mid.unwrap(), now, Some(0.5)));
    assert_eq!(peer.video_packet_loss(now), None);
    for invalid in [None, Some(f32::NAN), Some(-0.1), Some(1.1)] {
        peer.record_video_packet_loss(stats(video, now, invalid));
        assert_eq!(peer.video_packet_loss(now), None);
    }
    peer.record_video_packet_loss(stats(video, now, Some(32.0 / 255.0)));
    assert_eq!(peer.video_packet_loss(now), Some(0.125));
    peer.record_video_packet_loss(stats(video, now + Duration::from_secs(4), None));
    assert_eq!(
        peer.video_packet_loss(now + Duration::from_secs(4)),
        Some(0.125)
    );
    assert_eq!(peer.video_packet_loss(now + Duration::from_secs(5)), None);
    peer.record_video_packet_loss(stats(video, now + Duration::from_secs(6), Some(0.0)));
    assert_eq!(
        peer.video_packet_loss(now + Duration::from_secs(6)),
        Some(0.0)
    );
    peer.ice_state_changed(
        IceConnectionState::Disconnected,
        now + Duration::from_secs(6),
    );
    assert_eq!(peer.video_packet_loss(now + Duration::from_secs(6)), None);
    assert_eq!(
        Publisher::new(Some(Codec::Opus), Some(Codec::H264))
            .unwrap()
            .video_packet_loss(now),
        None
    );
}

#[test]
fn encrypted_receiver_reports_show_loss_then_clean_then_unavailable() {
    let mut sender =
        Publisher::new_with_bwe_and_stats(None, Some(Codec::H264), None, None, true).unwrap();
    let mut receiver = Publisher::new_receiver().unwrap();
    sender
        .add_local_candidate("127.0.0.1:40100".parse().unwrap())
        .unwrap();
    receiver
        .add_local_candidate("127.0.0.1:40101".parse().unwrap())
        .unwrap();
    let answer = receiver
        .accept_offer(&sender.create_offer().unwrap())
        .unwrap();
    sender.accept_answer(&answer).unwrap();
    let start = Instant::now();
    let mut dropped = 0;
    let mut before = Vec::new();
    let mut during = Vec::new();
    let mut after = Vec::new();
    for step in 0..1200_u64 {
        let now = start + Duration::from_millis(step * 20);
        for peer in [&mut sender, &mut receiver] {
            peer.rtc.handle_input(Input::Timeout(now)).unwrap();
            peer.drain_at(now).unwrap();
        }
        let mut primary_pt = None;
        if sender.rtc.is_connected() {
            let writer = sender.rtc.writer(sender.video_mid.unwrap()).unwrap();
            let pt = writer
                .payload_params()
                .find(|p| p.spec().codec == str0m::format::Codec::H264)
                .unwrap()
                .pt();
            primary_pt = Some(*pt);
            writer
                .write(
                    pt,
                    now,
                    MediaTime::new(step * 1800, Frequency::NINETY_KHZ),
                    Arc::<[u8]>::from([0, 0, 0, 1, 0x65, 0x01]),
                )
                .unwrap();
            sender.drain_at(now).unwrap();
        }
        while let Some(packet) = sender.poll_transmit() {
            let data = &packet.contents;
            let rtp = data.len() >= 12 && data[0] >> 6 == 2 && !(192..=223).contains(&data[1]);
            if (250..650).contains(&step) && rtp {
                let sequence = u16::from_be_bytes([data[2], data[3]]);
                if Some(data[1] & 0x7f) != primary_pt || sequence % 5 == 0 {
                    dropped += 1;
                    continue;
                }
            }
            let input =
                Receive::new(Protocol::Udp, packet.source, packet.destination, data).unwrap();
            receiver
                .rtc
                .handle_input(Input::Receive(now, input))
                .unwrap();
            receiver.drain_at(now).unwrap();
        }
        while let Some(packet) = receiver.poll_transmit() {
            let input = Receive::new(
                Protocol::Udp,
                packet.source,
                packet.destination,
                &packet.contents,
            )
            .unwrap();
            sender.rtc.handle_input(Input::Receive(now, input)).unwrap();
            sender.drain_at(now).unwrap();
        }
        while receiver.poll_media().is_some() {}
        if let Some(loss) = sender.video_packet_loss(now) {
            if (150..250).contains(&step) {
                before.push(loss);
            }
            if (400..650).contains(&step) {
                during.push(loss);
            }
            if step >= 1000 {
                after.push(loss);
            }
        }
    }
    assert!(dropped > 0);
    assert!(
        !before.is_empty() && before.iter().all(|x| *x == 0.0),
        "clean: {before:?}"
    );
    assert!(during.iter().any(|x| *x > 0.0), "loss: {during:?}");
    assert!(
        !after.is_empty() && after.iter().all(|x| *x == 0.0),
        "restored: {after:?}"
    );
    assert_eq!(
        sender.video_packet_loss(start + Duration::from_secs(30)),
        None
    );
}
