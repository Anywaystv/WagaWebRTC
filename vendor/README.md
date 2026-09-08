# str0m AAC test support

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
