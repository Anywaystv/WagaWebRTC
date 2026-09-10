use super::*;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct Driver {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    binary: std::path::PathBuf,
}

impl Driver {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let binary = std::env::temp_dir().join(format!("waga-bonding-test-{}", std::process::id()));
        let status = Command::new("xcrun")
            .args([
                "swiftc",
                "-O",
                "-parse-as-library",
                "-package-name",
                "WagaWebRTC",
            ])
            .args([
                "Sources/WagaWebRTC/Delivery.swift",
                "Sources/WagaWebRTC/Scheduler.swift",
                "Sources/WagaWebRTC/Recovery.swift",
                "Sources/WagaWebRTC/BitrateRamp.swift",
                "Tests/Support/BondingDriver.swift",
            ])
            .arg("-o")
            .arg(&binary)
            .current_dir(root)
            .status()
            .unwrap();
        assert!(status.success());
        let mut child = Command::new(&binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            input,
            output,
            binary,
        }
    }

    fn request(&mut self, command: String) -> String {
        writeln!(self.input, "{command}").unwrap();
        self.input.flush().unwrap();
        let mut result = String::new();
        assert!(
            self.output.read_line(&mut result).unwrap() > 0,
            "Swift driver exited"
        );
        result.trim().to_owned()
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.binary);
    }
}

fn hex(data: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    String::from_utf8(
        data.iter()
            .flat_map(|byte| [DIGITS[(byte >> 4) as usize], DIGITS[(byte & 15) as usize]])
            .collect(),
    )
    .unwrap()
}

fn unhex(text: &str) -> Vec<u8> {
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let digit = |byte: u8| {
                if byte <= b'9' {
                    byte - b'0'
                } else {
                    byte - b'a' + 10
                }
            };
            digit(pair[0]) * 16 + digit(pair[1])
        })
        .collect()
}

fn is_rtp(data: &[u8]) -> bool {
    data.len() >= 12 && data[0] & 0xc0 == 0x80 && !(192..=223).contains(&data[1])
}

#[test]
fn bonded_encoder_feedback_and_scheduler_use_simulated_time() {
    let mut driver = Driver::new();
    let mut results = Vec::new();
    for (label, capacity, utilization, seconds) in [
        ("healthy startup", [10_000_000u64, 2_000_000], 100u64, 12u64),
        ("startup receipt loss", [10_000_000, 2_000_000], 100, 18),
        ("periodic ALR probes", [10_000_000, 2_000_000], 35, 18),
        ("combined capacity", [3_500_000, 3_500_000], 100, 30),
        ("constrained links", [1_000_000, 500_000], 100, 12),
    ] {
        driver.request("reset".into());
        let mut sender = Publisher::new_with_bwe(
            Some(Codec::Opus),
            Some(Codec::H264),
            Some(6_070_588),
            Some(6_070_588),
        )
        .unwrap();
        let mut receiver = Publisher::new_receiver().unwrap();
        sender
            .add_local_candidate("127.0.0.1:40000".parse().unwrap())
            .unwrap();
        receiver
            .add_local_candidate("127.0.0.1:40001".parse().unwrap())
            .unwrap();
        let offer = sender.create_offer().unwrap();
        sender
            .accept_answer(&receiver.accept_offer(&offer).unwrap())
            .unwrap();
        let start = Instant::now();
        let mut pending: Vec<(u64, usize, Transmit)> = Vec::new();
        let mut feedback: Vec<(u64, Transmit)> = Vec::new();
        let mut receipts: Vec<(u64, usize, String)> = Vec::new();
        let mut available = [0u64; 2];
        let mut path_probes: Vec<(u64, usize, u64)> = Vec::new();
        let rtt = [30_000_000u64, 80_000_000];
        let mut video = 5_000_000u64;
        let mut minimum = video;
        let mut settled_minimum = video;
        let mut frames = 0;
        let mut estimates = 0;
        let mut periodic = false;
        let mut dropped = [0u64; 2];
        let mut sent_bytes = [0u64; 2];
        let mut omit_wifi_receipt = label == "startup receipt loss";
        for millis in 0..seconds * 1000 {
            let tick = millis * 1_000_000 + 1;
            let now = start + Duration::from_nanos(tick);
            for peer in [&mut sender, &mut receiver] {
                peer.rtc.handle_input(Input::Timeout(now)).unwrap();
                peer.drain_at(now).unwrap();
            }
            // Production sends authenticated path RTT probes every 500 ms.
            if millis % 500 == 0 {
                for path in 0..2 {
                    let elapsed = available[path].saturating_sub(tick) + rtt[path];
                    path_probes.push((tick + elapsed, path, elapsed));
                }
            }
            path_probes.sort_by_key(|probe| probe.0);
            let count = path_probes.partition_point(|probe| probe.0 <= tick);
            for (_, path, elapsed) in path_probes.drain(..count) {
                driver.request(format!(
                    "rtt {} {}",
                    if path == 0 { "wifi" } else { "cell" },
                    elapsed as f64 / 1_000_000.0
                ));
            }
            if sender.rtc.is_connected() && millis % 33 == 0 {
                let writer = sender.rtc.writer(sender.video_mid.unwrap()).unwrap();
                let pt = writer
                    .payload_params()
                    .find(|p| p.spec().codec == str0m::format::Codec::H264)
                    .unwrap()
                    .pt();
                let mut frame = vec![0x55; (video * utilization / 100 / 8 / 30) as usize + 5];
                frame[..5].copy_from_slice(&[0, 0, 0, 1, 0x65]);
                writer
                    .write(
                        pt,
                        now,
                        MediaTime::new(millis * 90, Frequency::NINETY_KHZ),
                        Arc::<[u8]>::from(frame),
                    )
                    .unwrap();
                sender.drain_at(now).unwrap();
            }
            while let Some(packet) = sender.poll_transmit() {
                if !is_rtp(&packet.contents) {
                    pending.push((tick + rtt[0] / 2, 0, packet));
                    continue;
                }
                let response = driver.request(format!("send {tick} {}", hex(&packet.contents)));
                let fields: Vec<_> = response.split_whitespace().collect();
                let routes: Vec<_> = fields[0]
                    .split(',')
                    .map(|path| usize::from(path == "cell"))
                    .collect();
                if let Some(sequence) = packet.transport_sequence {
                    sender
                        .rtc
                        .set_egress_path(sequence, (routes.len() == 1).then_some(routes[0] as u64));
                }
                let mut sends: Vec<_> = routes
                    .into_iter()
                    .map(|path| (path, packet.contents.clone()))
                    .collect();
                if fields.len() == 3 {
                    sends.push((usize::from(fields[1] == "cell"), unhex(fields[2])));
                }
                for (path, contents) in sends {
                    let departure = tick.max(available[path])
                        + contents.len() as u64 * 8_000_000_000 / capacity[path];
                    sent_bytes[path] += contents.len() as u64;
                    if departure - tick > 100_000_000 {
                        dropped[path] += 1;
                        continue;
                    }
                    available[path] = departure;
                    pending.push((
                        departure + rtt[path] / 2,
                        path,
                        Transmit {
                            source: packet.source,
                            destination: packet.destination,
                            contents,
                            transport_sequence: None,
                        },
                    ));
                }
            }
            pending.sort_by_key(|item| item.0);
            let count = pending.partition_point(|item| item.0 <= tick);
            for (_, path, packet) in pending.drain(..count) {
                if is_rtp(&packet.contents) {
                    receipts.push((tick + rtt[path] / 2, path, hex(&packet.contents)));
                }
                if !packet.contents.starts_with(b"WGR1") {
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
            }
            receipts.sort_by_key(|item| item.0);
            let count = receipts.partition_point(|item| item.0 <= tick);
            for (_, path, packet) in receipts.drain(..count) {
                if path == 0 && omit_wifi_receipt {
                    omit_wifi_receipt = false;
                    continue;
                }
                driver.request(format!(
                    "ack {tick} {} {packet}",
                    if path == 0 { "wifi" } else { "cell" }
                ));
            }
            while let Some(packet) = receiver.poll_transmit() {
                feedback.push((tick + rtt[0] / 2, packet));
            }
            let count = feedback.partition_point(|item| item.0 <= tick);
            for (_, packet) in feedback.drain(..count) {
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
            while receiver.poll_media().is_some() {
                frames += 1;
            }
            let estimate = sender.poll_bitrate_estimate();
            if estimate.is_some() || millis % 25 == 0 {
                if estimate.is_some() {
                    estimates += 1;
                }
                video = driver
                    .request(format!(
                        "rate {tick} {} {video}",
                        estimate
                            .map(|value| value.to_string())
                            .unwrap_or("-".into())
                    ))
                    .parse()
                    .unwrap();
                minimum = minimum.min(video);
                if millis >= seconds * 1000 / 2 {
                    settled_minimum = settled_minimum.min(video);
                }
            }
            if millis % 1000 == 0 {
                if label == "combined capacity" && millis % 5000 == 0 {
                    eprintln!(
                        "t={millis} video={video} drops={dropped:?} bytes={sent_bytes:?} {}",
                        sender.rtc.bwe().diagnostic_snapshot(now)
                    );
                }
                periodic |= sender
                    .rtc
                    .bwe()
                    .diagnostic_snapshot(now)
                    .contains("last_probe=Some(PeriodicAlr)");
            }
        }
        eprintln!("{label}: minimum={minimum} final={video} frames={frames} estimates={estimates}");
        if label == "combined capacity" {
            assert!(
                settled_minimum > 4_900_000,
                "{label}: settled minimum={settled_minimum}"
            );
        }
        results.push((
            label,
            minimum,
            video,
            frames,
            estimates,
            periodic,
            utilization,
        ));
    }
    for (label, minimum, video, frames, estimates, periodic, utilization) in results {
        assert!(
            frames > 20 && estimates > 5,
            "{label}: feedback or media missing"
        );
        if label == "constrained links" {
            assert!(video < 1_500_000);
        } else {
            assert!(video > 4_900_000, "{label}: final={video}");
            if label != "combined capacity" {
                assert!(minimum > 4_000_000, "{label}: minimum={minimum}");
            }
            if utilization == 35 {
                assert!(periodic, "ALR probes were not exercised");
            }
        }
    }
}
