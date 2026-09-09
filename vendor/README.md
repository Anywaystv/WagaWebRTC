# Local str0m patches

`str0m-0.23.1` is the published crates.io source for str0m 0.23.1, including
its original licenses. The workspace pins this local copy using Cargo's patch
mechanism because upstream does not recognize MPEG4-GENERIC audio.

Local changes add AAC codec identification, AAC-hbr format parameters and
single-access-unit RFC 3640 packetization/depacketization. WagaWebRTC enables
AAC only when explicitly requested, at 48 kHz stereo (AudioSpecificConfig 1190).
Opus remains the default. Access units larger than the RTP payload budget are
rejected rather than sent with incorrect fragmentation. Start AAC tests at
128 kbps. Aggregated and fragmented AAC reception is not implemented.

This does not add AAC decoding to browser WebRTC players or to WagaStrim.

## Low-motion bandwidth probing

When cached video payloads are smaller than 50 bytes, padding probes use blank
padding instead of repeatedly resending those tiny payloads. This prevents the
pacer's packet rate from limiting a probe below its intended bitrate. Ordinary
NACK retransmissions are unchanged.

The core regression test simulates 12 seconds of tiny H.264 frames with 20 ms
round-trip latency. It checks startup probes, later ALR probes and a bandwidth
estimate above the initial rate. BWE and ALR remain str0m's implementation;
WagaWebRTC does not add a second congestion controller.

## Session recovery

Overlapping TWCC reports count each missing packet only once. Repeated missing
statuses no longer feed duplicate losses into BWE; a later received status still
reaches the estimator. Regression tests cover duplicate reports and late receipt.

Missing-only feedback no longer refreshes the delay controller's arrival evidence
or reapplies its old throughput sample. Diagnostics report delay-feedback age
separately from the loss observation's packet age.

Low-rate probes keep requesting paced padding until both their byte budget and
minimum packet count are met. Previously a large video packet could exhaust the
byte budget early and leave the probe waiting for more camera traffic. Tests
cover 40, 250 and 500 kbps probes without further media.

The delay-based increase uses one throughput ceiling, so a normal signal cannot
lower the estimate through a conflicting cap. Additive recovery calculates
packet sizes in bits (1200 bytes = 9600 bits). Both have regression tests.

The BWE probe controller receives congestion-state changes even when the numeric
bandwidth estimate stays unchanged. This lets recovery probes resume after loss
clears, while still suppressing probes when congestion starts at the same rate.
Regression tests cover both transitions at 250 kbps.

Outside ALR, unmet demand triggers a recovery probe after 15 seconds without a
probe, even when low estimates fluctuate. Active congestion still blocks it.
Probe deadlines advance after expiry; unchanged targets do not rearm them.
Tests cover fluctuating estimates, timer progress and congestion gating.
After a path-recovery probe, retries use a five-second interval for 20 seconds,
then return to 15 seconds. Congestion still blocks probes throughout that window.

In bonded mode, a fresh authenticated ICE response on a newly usable path also
requests a probe immediately. Repeated heartbeats on a healthy path do not
trigger it. Requests have a five-second cooldown. One pending request survives
congestion and runs when the congestion gate clears, using the then-current
estimate and demand; disabling probing cancels it. Previously the request expired
after five seconds, delaying recovery until a periodic probe. Probes use the existing
congestion gates and rate limits, and do not force the encoder bitrate upward.
Standard WHIP/WHEP and Pion are unchanged.

Congestion is checked before dispatching queued probes too. Queued high-rate
probes are discarded on congestion and replaced by one pending recovery request,
so they cannot bypass the gate or resume at an obsolete rate.

Encoder estimates are delivered at most every 200 ms for increases or unchanged
rates, and immediately for decreases. Delivery requires new TWCC records;
timer ticks alone do not refresh stale estimates. This replaces the extra
three-second averaging window, without changing the congestion controller or pacer.
Moblin applies its configured recovery step every 200 ms, ordinary reductions
every 200 ms, and severe reductions immediately. Its existing 20 ms loop checks
these deadlines. Increases
require an estimate no older than one second and cannot exceed the estimated
video budget or the user's target.

With fresh feedback permitting 5 Mbps and the default 250 kbps step, the sender
ramp rises from 250 kbps to 5 Mbps within four seconds. This does not include
the time needed to discover available bandwidth.

`StreamTx.discard_queued_media()` clears unsent media without resetting SSRCs or
RTX history. WagaWebRTC uses it during a temporary ICE disconnect so keeping the
session alive does not queue stale video or audio for a later burst.
