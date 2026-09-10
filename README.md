![WagaWebRTC banner with a yellow dog mascot](wagawebrtc.png)

# WagaWebRTC (Experimental)

WagaWebRTC is a Swift framework built on the [str0m WebRTC library](https://github.com/algesten/str0m) for Rust. It publishes H.264 or H.265 video and Opus audio over WHIP, with optional experimental AAC publishing.

Experimental: long-session stability, battery use and AAC under impaired bonded links have not been fully verified. Codec support depends on the receiving service and player.

## Built on str0m

[str0m](https://github.com/algesten/str0m) provides the WebRTC core: SDP, ICE, DTLS, SRTP, RTP packetization, NACK, and RTCP. Its Sans I/O design leaves network operations to the application. WagaWebRTC adds the Swift/C bridge, per-interface UDP sockets through Network.framework, path selection, and shared-history packet recovery.

Credit to the str0m authors and contributors. We pin str0m 0.23.1 with local AAC, bandwidth-estimation and recovery patches; see the [patch notes](vendor/README.md) and [upstream API documentation](https://docs.rs/str0m).

## Usage

This example assumes your app supplies the delegate, encoded access units and
timestamps. Send the offer over HTTP, then accept the server's SDP answer before
sending media.

```swift
let publisher = try WagaPublisher(
    audio: .opus,
    video: .h264,
    mode: .bonded,
    iceServers: ["stun:stun.l.google.com:19302"],
    targetBitrate: 3_500_000,
    delegate: delegate
)
publisher.createOffer { result in /* POST SDP to the WHIP endpoint */ }
publisher.acceptAnswer(answerSdp)
publisher.send(codec: .h264, mediaTime: timestamp90k, data: accessUnit)
```

Use `.standard` for ordinary WHIP services. Use `.bonded` with a compatible receiver such as WagaStrim; it distributes media across validated interfaces. RTCP stays on str0m's selected route.

The app owns HTTP signaling and encoding. Create one publisher or receiver per session and call `stop()` before releasing it. Delegate callbacks run on the peer's queue, not the main queue. Timestamps use codec clock ticks: 90 kHz for video, 48 kHz for audio (960 samples per 20 ms Opus packet, 1024 per AAC-LC frame).

### Adaptive bitrate

Set `targetBitrate` to enable transport-wide congestion control. Keep it at the user's configured target and adapt the encoder using `wagaPublisherBitrateEstimate(_:)`; the framework does not encode media or change the encoder itself.

str0m controls the total sending rate, bandwidth probing and application-limited region (ALR) detection. Probing requires negotiated RTX and TWCC feedback from the receiver. Probes and recovery add traffic above the media bitrate; the target is not a guarantee of available bandwidth. See [str0m's BWE notes](vendor/str0m-0.23.1/docs/BWE.md).

### Bonding and recovery

Bonded WHIP uses SRTLA-inspired packet assignment: each path is ranked by unacknowledged traffic plus the next packet, divided by its delivery window. Windows use bytes because RTP packet sizes vary. Successful receipts grow a busy path faster than idle samples; missing receipts expire after one second and reduce only that path's window. Priorities are normalized to the lowest active priority before fading toward equal weight as a path's window shrinks. Thus 10/9 and 1/0.9 behave alike, including after a missing receipt. Paths with delivery tracking use a relative RTT cost as a floor, bounded below one window of load. The scheduler takes the larger of this floor and the outstanding-traffic score, so RTT is not charged again once traffic is in flight. This favors faster delivery while allowing backlog to shift traffic to another path. RTT breaks equal scores. Socket-pending media is not counted a second time.

Windows guide packet assignment without blocking already-paced traffic or pinning a handoff to one path. Idle paths still receive occasional duplicate media samples. Receipts count unique encrypted RTP packets, excluding parity and duplicate repairs, and add no playout delay. New paths start with a provisional 32 KB window and RTT weighting; receipts then adjust the windows. Revalidation preserves that history and packets still in flight. Traffic redistribution requests a capacity probe without resetting the shared estimator. str0m still controls the shared transport rate and bandwidth probes; this scheduling change does not replace WebRTC congestion control with SRT's.

The sender records each packet's actual route alongside its TWCC sequence.
Delay trends are measured separately on each route, so queueing on one route can
shift traffic to another before reducing the shared rate. Probe measurements remove
each route's minimum transit delay while retaining queue growth. Aggregate throughput
uses the original feedback. Loss accounting waits two feedback intervals plus the
measured path delay difference (at most one second) for reordered arrivals.
Packets duplicated across routes have an ambiguous winning path and contribute
to throughput and loss, but not path timing. This metadata is local to the sender
and does not change the RTP wire format or require an ingest upgrade.

Debug snapshots include each active interface's cumulative primary RTP bytes, RTT,
outstanding bytes, socket-pending bytes, window and receipt support, plus repair request and byte counters. Primary RTP counts exclude the
extra idle-path copies and parity; they include padding and retransmitted RTP.

A shared 4096-packet history allows repairs over another healthy path. One XOR parity datagram per eight encrypted RTP packets can recover one missing packet without a round trip, at about 12.5% overhead. History repairs wait at least the measured path RTT (100 ms to one second), use at most one packet every 20 ms, and receive 1/16 of primary traffic as byte credit, capped at 1500 bytes. Retransmitted ciphertext marks its original TWCC route ambiguous so delayed repairs cannot masquerade as probe congestion. Native WebRTC NACK/RTX remains available; WagaStrim deduplicates retransmissions.

An established publisher keeps its session for up to 15 seconds after an ICE disconnect. Bonded sockets retry independently, replacing sockets stuck connecting after 5 seconds. Removed or silent paths are withdrawn; authenticated replies can restore them. Each interface uses its own validated receiver address, allowing LAN access over Wi-Fi and public access over cellular.

Unsent media is discarded during recovery to avoid a stale backlog. Successful ICE recovery cancels teardown; otherwise the app is notified to reconnect. This cannot hide outages longer than the receiver's playout buffer or preserve a session the server has closed.

## Local verification

Run Rust formatting, tests and lint, then build the XCFramework and Swift package for iPhone and simulator:

```sh
./scripts/verify-local.sh
```

## Local AAC recording test

Run `cargo run --example aac_ingest -- LAN_IP RANDOM_TOKEN new-output.aac` on the Mac, replacing `LAN_IP` with your local address and `RANDOM_TOKEN` with a new random secret of at least 24 characters. The receiver prints a token-protected HTTP WHIP URL on port 8099. Use that URL in a separate Moblin test stream on the same Wi-Fi, select AAC at 128 kbps and H.264, and disable bonding. Allow local-network access if iOS asks.

This single-session test saves up to 60 seconds of AAC audio as ADTS (maximum 32 MiB); it does not provide video preview or forwarding. Stop it with Ctrl-C if unused. Do not expose this test receiver to the internet. WagaStrim's built-in preview requires Opus.

Allow `aac_ingest` through the Mac firewall if prompted. After 60 seconds the receiver exits, so the phone's reconnect attempt will fail. Restart the command with a new output filename for another recording.

## iOS artifact

Requires full Xcode with Swift 6, Python 3, and Rust installed through rustup. Select Xcode with `xcode-select`, then run:

```sh
./scripts/build-xcframework.sh
```

This produces `Artifacts/CWagaWebRTC.xcframework` for iPhone and simulator using str0m's Apple CryptoKit backend. `Package.swift` detects the artifact automatically, so the repository can be added to Moblin as one local Swift package. `WagaPublisher` handles outgoing WHIP; `WagaReceiver` handles WHEP and incoming WHIP offers.

Build the artifact before adding the local package to Xcode. Prebuilt binaries are not included. Scripts use your existing Rust installation; `WAGA_TOOLCHAIN_DIR` optionally selects a directory containing `rustup/` and `cargo/`.

Each framework includes the project license and generated dependency notices.
Keep these files when redistributing the XCFramework. The build remaps local
source paths and rejects artifacts that still contain the checked build paths.

## Scope

- Host and STUN server-reflexive ICE candidates over UDP are supported on each active interface.
- H.264 and H.265 access units may use VideoToolbox's AVCC length prefixes or Annex B start codes.
- TURN is not implemented. Opus remains the default audio codec. Experimental AAC publishing uses 48 kHz stereo and requires a receiver that accepts MPEG4-GENERIC/AAC-hbr. Start at 128 kbps. WagaStrim's browser preview still requires Opus.
- An iPhone cannot expose two independent cellular data interfaces to one app merely because two eSIMs are installed. The framework bonds every interface iOS actually exposes, normally one cellular path plus Wi-Fi and Ethernet.

## License

WagaWebRTC's own code is licensed under [MIT](LICENSE). Vendored str0m and other dependencies retain their own licenses and copyright notices; see [vendor/](vendor/).
