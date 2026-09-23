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

A probe whose receive rate keeps up with its send rate proves only a lower bound
on network capacity. Such a probe can raise the estimate, but cannot lower it
when the sender emits the probe too slowly. Saturated probes can still reduce
the estimate. Probe reductions also use GoogCC's throughput safeguard: they stay
above the smaller of the current delay estimate and 85% of acknowledged throughput.
This leaves room to drain congestion without accepting a probe far below recent
delivery. Delay and loss control continue to react independently. Regression tests
cover sender-limited startup probes, saturated probes and throughput backoff.

During ALR, when smoothed RTT is within 100 ms of its observed minimum, a lower probe
result also requires delay overuse before reducing the estimate. Jitter can stretch
a short probe even when the network has spare capacity. Under those conditions,
delay backoff limits each reduction to 15% of the current estimate using the existing
RTT-based interval, instead of treating low media throughput as a capacity ceiling.
Sustained overuse still reduces the rate. Greater RTT growth, missing RTT evidence,
or leaving ALR restores throughput-based backoff. Loss control remains active.
The cellular ramp-up test covers three loss seeds without changing its 1.5 Mbps
requirement or time limits. Unit tests cover transient delay, RTT spacing, queue
growth, missing RTT, sustained overuse and exit from ALR. The local verification
script also runs the vendored bandwidth-estimation suite so CI covers this failure.

The core exposes the most recent transmit's TWCC sequence as local metadata,
cleared at each output poll. Bonding records the actual sending path against that
sequence. Each path has its own delay trend; shared delay backoff requires congestion
on all recently used paths. Probe estimates subtract each path's minimum transit
delay before combining timing samples; increasing queue delay remains visible.
A probe can lower the combined estimate only when its timing samples show every
known route was saturated; partially loaded routes provide a lower bound.
Acknowledged throughput retains the original feedback. Loss observations wait two
feedback intervals plus the measured path delay difference (capped at one second)
for a later report to replace a provisional missing status. Redundant sends
have no identifiable winning path and are excluded only from timing samples.
The Swift sender also marks an original TWCC sequence ambiguous before sending
a ciphertext repair. A regression reproduces false probe backoff without this
mark and preserves the estimate with it.
Standard transport does not tag paths and retains its existing timing behavior.
Tests cover aggregate probe capacity, congestion on both paths, different fixed
path delays, ambiguous duplicates and reordered loss reports. The macOS Rust test
drives the production Swift scheduler, delivery tracker, parity encoder and bitrate
ramp against two real str0m peers using simulated time. The C transmit structure remains unchanged.
The combined-capacity regression now runs for 30 seconds and requires video to
stay above 4.9 Mbps throughout the second half. Two 3.5 Mbps links meet that
requirement after using RTT as a scheduling floor instead of adding it to
outstanding traffic, which already reflects RTT. The previous double charge
overloaded the faster path while leaving capacity unused on the slower path.
The 10+2 Mbps startup and periodic-ALR scenarios
hold 5 Mbps; the 1+0.5 Mbps scenario backs off under congestion. The harness uses
Moblin's 10/9 priorities and also drops one Wi-Fi receipt at startup. Before
normalizing priorities, that single receipt loss drove video to 0.50 Mbps despite
10 Mbps of Wi-Fi capacity. With normalized priorities it holds 5 Mbps.

A publisher starting at its full allocation validates that rate with one initial
probe instead of sending 3× and 6× bursts. Below-target startup and later recovery
retain exponential probing.

Clean startup feedback still enters the loss controller's observations before
the startup estimate override. Previously those reports were skipped while their
elapsed time remained in the observation, amplifying isolated missing packets.
A regression with two short loss bursts separated by clean 4.8 Mbps traffic
reproduced a 667 kbps estimate and now stays above 4 Mbps.

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

During ALR, an unchanged estimate below the delay-based limit can resume recovery
after HOLD expires and a full loss-free observation window arrives. This avoids
waiting for a previous probe's capacity cap to expire before probing again.
The estimate and capacity cap stay intact; probes must demonstrate any increase.
Tests cover same-link capacity recovery, active loss, HOLD and an unchanged slow link.

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

## Loss-estimator arithmetic

The loss-controller bandwidth bias uses kilobits per second and divides its
weight by the sum of smoothing and absolute loss difference, matching libWebRTC.
The inherited expressions used bits per second and misplaced the denominator's
parentheses, exaggerating the bandwidth penalty under loss. A deterministic
60-second case with 224 kbps delivered collapsed to 8.7 bps before correction;
it now remains above 100 kbps and resumes increasing when loss clears.
This correction has not yet been verified against the Mac streaming stall.

The testing branch passes 713 vendored unit tests, 21 core tests and seven
bonded simulation scenarios. One bandwidth integration test still fails:
`changing::bwe_changing_bandwidth` reports 421.491 kbps where its congestion
checkpoint requires at least 500 kbps. That assertion is unchanged. Treat this
as a candidate for Mac testing, not a validated streaming fix. Rebuild the
XCFramework after switching branches; an existing binary will not include it.
