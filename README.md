![WagaWebRTC banner with a yellow dog mascot](wagawebrtc.png)

# WagaWebRTC (Experimental)

WagaWebRTC is a Swift framework for iOS 15 and later, built on the [str0m WebRTC library](https://github.com/algesten/str0m) for Rust. It publishes H.264 or H.265 video and Opus audio over WHIP, with optional experimental AAC publishing. It also receives media through WHEP or incoming WHIP offers.

Experimental: long-session stability, battery use and AAC under impaired bonded links have not been fully verified. Codec support depends on the receiving service and player.

## Built on str0m

[str0m](https://github.com/algesten/str0m) provides the WebRTC core: SDP, ICE, DTLS, SRTP, RTP packetization, NACK, and RTCP. Its Sans I/O design leaves network operations to the application. WagaWebRTC adds the Swift/C bridge, per-interface UDP sockets through Network.framework, path selection, and shared-history packet recovery.

Credit to the str0m authors and contributors. We pin str0m 0.23.1 with local AAC, bandwidth-estimation and recovery patches; see the [patch notes](vendor/README.md) and [upstream API documentation](https://docs.rs/str0m).

## Installation

Build on a Mac with full Xcode, Swift 6, Python 3, and Rust installed through rustup. From the repository directory, run:

```sh
./scripts/build-xcframework.sh
```

This creates `Artifacts/CWagaWebRTC.xcframework` for iPhone and simulator using str0m's Apple CryptoKit backend. Then add this checkout to your app as a local Swift package. `Package.swift` detects the artifact automatically; prebuilt binaries are not included.

Scripts use your existing Rust installation. `WAGA_TOOLCHAIN_DIR` optionally selects a directory containing `rustup/` and `cargo/`.

Each framework includes the project license and dependency notices. Keep them when redistributing the XCFramework. The build remaps local source paths and checks the artifact for remaining build paths.

## Usage

This example assumes your app supplies the delegate, encoded access units and
timestamps. Send the offer over HTTP, accept the server's SDP answer, and wait for
`wagaPublisherConnected()` before sending media. These are separate asynchronous steps.

```swift
import WagaWebRTC

let publisher = try WagaPublisher(
    audio: .opus,
    video: .h264,
    mode: .standard,
    iceServers: ["stun:stun.l.google.com:19302"],
    targetBitrate: 3_500_000,
    delegate: delegate
)
publisher.createOffer { result in /* POST SDP to the WHIP endpoint */ }
publisher.acceptAnswer(answerSdp)
publisher.send(codec: .h264, mediaTime: timestamp90k, data: accessUnit)
```

Use `.standard` for ordinary WHIP services. Use `.bonded` with a compatible receiver such as WagaStrim; it distributes media across validated interfaces. RTCP stays on str0m's selected route.

The app owns HTTP signaling and encoding. Create one publisher or receiver per session and call `stop()` before releasing it. Handle `wagaPublisherNeedsKeyframe()` by requesting a keyframe from your encoder. Delegate callbacks run on the peer's queue; dispatch UI work to the main queue.

Timestamps use codec clock ticks: 90 kHz for video, 48 kHz for audio (960 samples per 20 ms Opus packet, 1024 per AAC-LC frame).

### Adaptive bitrate

Set `targetBitrate` when creating the publisher to enable bandwidth estimation. Both this target and `wagaPublisherBitrateEstimate(_:)` use bits per second and describe transport bandwidth. Reserve room for audio, packet overhead, and recovery traffic when converting the estimate into a video encoder bitrate.

Keep the transport target tied to the user's requested rate, including that headroom. Call `setTargetBitrate(_:)` when the requested rate changes. Feed estimates into the encoder, rather than repeatedly lowering the transport target to match them. The framework does not encode media or change the encoder itself.

The optional [`WagaBitrateRamp`](Sources/WagaWebRTC/BitrateRamp.swift) helper controls the timing of encoder updates. Pass the video budget to `observe(ceiling:now:)`, then call `next(current:target:maximumIncrease:now:)` from your update loop. It limits increases to the supplied step every 200 ms, allows severe reductions immediately, and requires estimates no older than one second. Times are monotonic nanoseconds.

str0m handles congestion control and probes, including when the encoder sends little data. Probing requires negotiated RTX and TWCC feedback. The target is not a guarantee of available bandwidth or a hard cap on all network traffic. See the [bandwidth patch notes](vendor/README.md#low-motion-bandwidth-probing) for details.

### Bonding and recovery

Bonded WHIP uses SRTLA-inspired packet assignment across validated interfaces. Delivery receipts, queued traffic, round-trip time, and connection priorities determine how much each path carries. Faster delivery is favored, while backlog can shift traffic to another path. str0m still controls the shared sending rate; this is a WebRTC transport, not an SRT connection.

Pass `connectionPriorities: priorities` when creating a bonded `WagaPublisher` to adjust relative preference. These are the defaults:

```swift
let priorities = WagaConnectionPriorities(wifi: 1, cellular: 0.9, wiredEthernet: 1)
```

Higher values favor an interface, but do not reserve a fixed traffic percentage. Only the ratios matter: Wi-Fi/cellular priorities of 10/9 behave like 1/0.9. Idle paths still receive occasional duplicate samples, so bonding can use cellular data even when Wi-Fi is healthy. Setting a priority to zero does not disable that interface.

Handoffs request capacity probes without resetting the shared bandwidth estimator. Timing measurements account for different path delays and reordered arrivals. A shared 4096-packet history and one XOR parity datagram per eight encrypted RTP packets support recovery over healthy paths. Repairs are paced and add network traffic. See the [scheduler](Sources/WagaWebRTC/Scheduler.swift), [recovery code](Sources/WagaWebRTC/Recovery.swift), and [patch notes](vendor/README.md) for the algorithms and limits.

An established publisher keeps its session for up to 15 seconds after an ICE disconnect. Bonded sockets retry independently, replacing sockets stuck connecting after 5 seconds. Removed or silent paths are withdrawn; authenticated replies can restore them. Each interface uses its own validated receiver address, allowing LAN access over Wi-Fi and public access over cellular.

Unsent media is discarded during recovery to avoid a stale backlog. Successful ICE recovery cancels teardown; otherwise the app is notified to reconnect. This cannot hide outages longer than the receiver's playout buffer or preserve a session the server has closed.

### Diagnostics

Set `diagnostics: true` and implement `wagaPublisherDiagnostic(_:)` to receive brief interface and transport events. Periodic bandwidth and packet-counter dumps are disabled. The native estimator snapshot remains available on demand for debugging and regression tests.

## Local verification

Run Rust formatting, core and bandwidth tests, and lint, then build the XCFramework and Swift package for iPhone and simulator:

```sh
./scripts/verify-local.sh
```

The script builds the Swift framework but does not execute its Swift tests. After building the artifact, list available test destinations:

```sh
xcodebuild -scheme WagaWebRTC -showdestinations
```

Choose an installed iOS simulator and replace `SIMULATOR_UDID` with its identifier:

```sh
xcodebuild -scheme WagaWebRTC \
  -destination 'platform=iOS Simulator,id=SIMULATOR_UDID' \
  -derivedDataPath .build/tests test
```

CI runs the verification script for code, dependency, build, and workflow changes. Changes limited to README files, changelogs, documentation folders, or the README banner skip it.

## Local AAC recording test

Run `cargo run --example aac_ingest -- LAN_IP RANDOM_TOKEN new-output.aac` on the Mac, replacing `LAN_IP` with your local address and `RANDOM_TOKEN` with a new random secret of at least 24 characters. The receiver prints a token-protected HTTP WHIP URL on port 8099. Use that URL in a separate Moblin test stream on the same Wi-Fi, select AAC at 128 kbps and H.264, and disable bonding. Allow local-network access if iOS asks.

This single-session test saves up to 60 seconds of AAC audio as ADTS (maximum 32 MiB); it does not provide video preview or forwarding. Stop it with Ctrl-C if unused. Do not expose this test receiver to the internet. WagaStrim's built-in preview requires Opus.

Allow `aac_ingest` through the Mac firewall if prompted. After 60 seconds the receiver exits, so the phone's reconnect attempt will fail. Restart the command with a new output filename for another recording.

## Scope and known limitations

- Host and STUN server-reflexive ICE candidates over UDP are supported on each active interface.
- H.264 and H.265 access units may use VideoToolbox's AVCC length prefixes or Annex B start codes.
- TURN is not implemented. Opus remains the default audio codec. Experimental AAC publishing uses 48 kHz stereo and requires a receiver that accepts MPEG4-GENERIC/AAC-hbr. Start at 128 kbps. WagaStrim's browser preview still requires Opus.
- An iPhone cannot expose two independent cellular data interfaces to one app merely because two eSIMs are installed. The framework bonds every interface iOS actually exposes, normally one cellular path plus Wi-Fi and Ethernet.

Phone testing showed smooth playback during repeated Wi-Fi/cellular handoffs. One stationary black-scene test produced a temporary adaptive video-target drop before recovery; a repeat held the 5 Mbps target. The cause of that isolated drop remains unresolved.

## License

WagaWebRTC's own code is licensed under [MIT](LICENSE). Vendored str0m and other dependencies retain their own licenses and copyright notices; see [vendor/](vendor/).
