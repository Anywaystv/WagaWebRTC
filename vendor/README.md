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

The BWE probe controller receives congestion-state changes even when the numeric
bandwidth estimate stays unchanged. This lets recovery probes resume after loss
clears, while still suppressing probes when congestion starts at the same rate.
Regression tests cover both transitions at 250 kbps.

Encoder estimates are delivered at most every 200 ms for increases or unchanged
rates, and immediately for decreases. Delivery requires new TWCC records;
timer ticks alone do not refresh stale estimates. This replaces the extra
three-second averaging window, without changing the congestion controller or pacer.
Moblin applies increases every 400 ms (100 kbps plus 1/30 of the current rate,
capped by its configured step), ordinary reductions every 200 ms, and severe
reductions immediately. Its existing 20 ms loop checks these deadlines. Increases
require an estimate no older than one second and cannot exceed the estimated
video budget or the user's target.

`StreamTx.discard_queued_media()` clears unsent media without resetting SSRCs or
RTX history. WagaWebRTC uses it during a temporary ICE disconnect so keeping the
session alive does not queue stale video or audio for a later burst.
