![WagaWebRTC banner with a yellow dog mascot](wagawebrtc.png)

# WagaWebRTC

WagaWebRTC is a Swift framework built on the [str0m WebRTC library](https://github.com/algesten/str0m) for Rust. It publishes H.264 or H.265 video and Opus audio over WHIP, with optional experimental AAC publishing.

## Built on str0m

[str0m](https://github.com/algesten/str0m) provides the WebRTC core: SDP, ICE, DTLS, SRTP, RTP packetization, NACK, and RTCP. Its Sans I/O design leaves network operations to the application. WagaWebRTC adds the Swift/C bridge, per-interface UDP sockets through Network.framework, path selection, and shared-history packet recovery.

Credit to the str0m authors and contributors. We use a pinned copy of str0m 0.23.1 with local AAC and low-motion probing patches; see the [patch notes](vendor/README.md). For the upstream library, see the [str0m repository](https://github.com/algesten/str0m) and [API documentation](https://docs.rs/str0m).

## Usage

The public surface is intentionally small:

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

Use `.bonded` with Wagastrim. Validated paths use receiver delivery receipts to limit outstanding RTP traffic and shift packets toward available paths. Idle interfaces receive an occasional media packet to check delivery. Without receipts, the existing RTT/socket-backlog scheduler remains in use. A shared 4096-packet history lets repairs use another healthy path. RTCP stays on str0m's selected route. Use `.standard` for an ordinary WHIP service that expects media on the nominated ICE pair.

Delivery windows cover unique encrypted RTP packets, not parity or duplicate repairs. Missing receipts expire after one second and reduce that path's allowance; this adds no playout delay. Windows guide route preference: if all validated paths are full, packets still use the best available route instead of being discarded. str0m still controls the total sending rate. Cellular playback and handover behavior require live verification.

Bonded publishing also sends one XOR parity datagram after each group of eight encrypted RTP packets. Wagastrim can reconstruct one missing audio or video packet once that parity arrives without waiting for a round trip. If a group loses more than one packet, it requests the remaining packets from the shared history. The parity overhead is about 12.5% of media traffic.

Set `targetBitrate` to enable transport-wide congestion control. The delegate receives bandwidth estimates through `wagaPublisherBitrateEstimate(_:)`; the publisher must lower its media encoder rate when that estimate falls.

Sender-side BWE, padding probes and ALR detection use str0m's controller. Probes
test for more bandwidth, including during low-motion video; the WHIP receiver
must negotiate RTX and return TWCC feedback. Keep the desired rate at the user's
configured target, and adapt the encoder to the latest estimate. Probes and
bonding recovery add traffic above the media bitrate; a 5 Mbps target cannot
guarantee 5 Mbps on a slower connection. See [str0m's BWE notes](vendor/str0m-0.23.1/docs/BWE.md).

The app owns HTTP signaling and encoding. Create one publisher or receiver per session and call `stop()` before releasing it. Delegate callbacks run on the peer's queue, not the main queue. Timestamps use codec clock ticks: 90 kHz for video, 48 kHz for audio (960 samples per 20 ms Opus packet, 1024 per AAC-LC frame).

An established publisher keeps its session for up to 15 seconds after ICE reports
a disconnect, allowing network recovery without a new WHIP request. Bonded path
sockets retry independently; a socket stuck connecting is replaced after 5 seconds.
Removed interfaces invalidate their host and mapped ICE candidates immediately.
Bonded paths also withdraw silent candidates when their health checks expire and
restore them after an authenticated reply. Each interface uses a validated receiver
address, so cellular can use the public endpoint while Wi-Fi uses a LAN endpoint.
Receive failures discard the affected socket so the path can be recreated.
Unsent media is discarded during recovery instead of accumulating a stale backlog.
Successful ICE recovery cancels teardown. A closed peer or expired recovery window
still notifies the app to reconnect. This cannot hide an outage longer than the
receiver's playout buffer or keep a session the server has already closed.

## Local verification

The repository keeps Rust under `rust/` and exposes only a C ABI to Swift. A pinned str0m source copy under `vendor/` adds opt-in AAC-hbr transport; see `vendor/README.md` for the patch scope and limitations.

```sh
./scripts/verify-local.sh
```

## Local AAC recording test

Run `cargo run --example aac_ingest -- <LAN-IP> <random-token-at-least-24-characters> <new-output.aac>` on the Mac. The receiver prints a token-protected HTTP WHIP URL on port 8099. Use that URL in a separate Moblin test stream on the same Wi-Fi, select AAC at 128 kbps and H.264, and disable bonding. Allow local-network access if iOS asks.

This single-session test saves up to 60 seconds of AAC audio as ADTS (maximum 32 MiB); it does not provide video preview or forwarding. Stop it with Ctrl-C if unused. Do not expose this test receiver to the internet. The production WagaStrim preview still requires Opus.

Allow `aac_ingest` through the Mac firewall if prompted. After 60 seconds the receiver exits, so the phone's reconnect attempt will fail. Restart the command with a new output filename for another recording.

## iOS artifact

Requires full Xcode with Swift 6 and Rust installed through rustup. Select Xcode with `xcode-select`, then run:

```sh
./scripts/build-xcframework.sh
```

This produces `Artifacts/CWagaWebRTC.xcframework` for iPhone and simulator using str0m's Apple CryptoKit backend. `Package.swift` detects the artifact automatically, so the repository can be added to Moblin as one local Swift package. `WagaPublisher` handles outgoing WHIP; `WagaReceiver` handles WHEP and incoming WHIP offers.

Build the artifact before adding the local package to Xcode. Prebuilt binaries are not included. Scripts use your existing Rust installation; `WAGA_TOOLCHAIN_DIR` optionally selects a directory containing `rustup/` and `cargo/`.

## Scope

- Host and STUN server-reflexive ICE candidates over UDP are supported on each active interface.
- H.264 and H.265 access units may use VideoToolbox's AVCC length prefixes or Annex B start codes.
- TURN is not implemented. Opus remains the default audio codec. Experimental AAC publishing uses 48 kHz stereo and requires a receiver that accepts MPEG4-GENERIC/AAC-hbr. Start at 128 kbps. WagaStrim's browser preview still requires Opus.
- Bonding distributes original packets rather than duplicating them. XOR parity and history-based repair provide loss recovery; SRTP sequence numbers provide ordering and Wagastrim's receive side deduplicates retransmissions.
- An iPhone cannot expose two independent cellular data interfaces to one app merely because two eSIMs are installed. The framework bonds every interface iOS actually exposes, normally one cellular path plus Wi-Fi and Ethernet.

Rust transport tests, Swift codec/path/recovery tests and a one-minute iPhone AAC recording have passed locally. This is experimental: long-session stability, battery use and AAC under impaired bonded links have not been verified.

## License

WagaWebRTC's own code is licensed under [MIT](LICENSE). Vendored str0m and other dependencies retain their own licenses and copyright notices; see [vendor/](vendor/).
