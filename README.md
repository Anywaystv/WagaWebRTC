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

Receiver delivery receipts guide routing alongside connection priorities, RTT and socket backlog. Idle paths receive occasional media samples. Missing receipts expire after one second and reduce that path's allowance. These windows count unique encrypted RTP packets, not parity or duplicate repairs, and add no playout delay. If all paths are full, traffic uses the best available route rather than being discarded. Without receipts, routing uses RTT and socket backlog.

A shared 4096-packet history allows repairs over another healthy path. One XOR parity datagram per eight encrypted RTP packets can recover one missing packet without a round trip, at about 12.5% overhead. Larger losses require history-based repairs; WagaStrim deduplicates retransmissions.

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
