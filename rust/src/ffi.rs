use std::ffi::{CStr, CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

use crate::{Codec, Media, PeerEvent, Publisher, Transmit};

#[repr(C)]
pub struct WagaTransmit {
    pub source: *const c_char,
    pub destination: *const c_char,
    pub data: *const u8,
    pub length: usize,
}

#[repr(C)]
pub struct WagaMedia {
    pub codec: i32,
    pub media_time: u64,
    pub clock_rate: u32,
    pub ntp_micros: u64,
    pub sender_media_time: u64,
    pub data: *const u8,
    pub length: usize,
}

pub struct WagaPeer {
    publisher: Publisher,
    last_error: CString,
    transmit: Option<OwnedTransmit>,
    media: Option<Media>,
}

struct OwnedTransmit {
    source: CString,
    destination: CString,
    data: Vec<u8>,
    transport_sequence: Option<u64>,
}

impl From<Transmit> for OwnedTransmit {
    fn from(value: Transmit) -> Self {
        Self {
            source: CString::new(value.source.to_string()).expect("socket address has no NUL"),
            destination: CString::new(value.destination.to_string())
                .expect("socket address has no NUL"),
            data: value.contents,
            transport_sequence: value.transport_sequence,
        }
    }
}

fn peer_mut<'a>(peer: *mut WagaPeer) -> Result<&'a mut WagaPeer, String> {
    unsafe { peer.as_mut() }.ok_or_else(|| "peer is null".to_string())
}

fn text(value: *const c_char) -> Result<&'static str, String> {
    if value.is_null() {
        return Err("string is null".into());
    }
    unsafe { CStr::from_ptr(value) }
        .to_str()
        .map_err(|error| error.to_string())
}

fn bytes<'a>(data: *const u8, length: usize) -> Result<&'a [u8], String> {
    if length == 0 {
        return Ok(&[]);
    }
    if data.is_null() && length != 0 {
        return Err("data is null".into());
    }
    Ok(unsafe { std::slice::from_raw_parts(data, length) })
}

fn run(peer: *mut WagaPeer, operation: impl FnOnce(&mut WagaPeer) -> Result<(), String>) -> bool {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let peer = peer_mut(peer)?;
        operation(peer)
    }));
    match result {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            if let Ok(peer) = peer_mut(peer) {
                peer.last_error = CString::new(error).unwrap_or_default();
            }
            false
        }
        Err(_) => {
            if let Ok(peer) = peer_mut(peer) {
                peer.last_error = CString::new("str0m panicked").unwrap();
            }
            false
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_create(audio_codec: i32, video_codec: i32) -> *mut WagaPeer {
    create_peer(|| {
        let audio = decode_codec(audio_codec).ok()?;
        let video = decode_codec(video_codec).ok()?;
        Publisher::new(audio, video).ok()
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_create_with_bwe(
    audio_codec: i32,
    video_codec: i32,
    initial_bitrate: u64,
    desired_bitrate: u64,
) -> *mut WagaPeer {
    create_peer(|| {
        let audio = decode_codec(audio_codec).ok()?;
        let video = decode_codec(video_codec).ok()?;
        Publisher::new_with_bwe(audio, video, Some(initial_bitrate), Some(desired_bitrate)).ok()
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_receiver_create() -> *mut WagaPeer {
    create_peer(|| Publisher::new_receiver().ok())
}

fn create_peer(
    create: impl FnOnce() -> Option<Publisher> + std::panic::UnwindSafe,
) -> *mut WagaPeer {
    catch_unwind(|| {
        create().map(|publisher| {
            Box::into_raw(Box::new(WagaPeer {
                publisher,
                last_error: CString::default(),
                transmit: None,
                media: None,
            }))
        })
    })
    .ok()
    .flatten()
    .unwrap_or(ptr::null_mut())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn waga_peer_destroy(peer: *mut WagaPeer) {
    if !peer.is_null() {
        drop(unsafe { Box::from_raw(peer) });
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_last_error(peer: *mut WagaPeer) -> *const c_char {
    peer_mut(peer)
        .map(|peer| peer.last_error.as_ptr())
        .unwrap_or(ptr::null())
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_add_local_candidate(
    peer: *mut WagaPeer,
    address: *const c_char,
) -> bool {
    run(peer, |peer| {
        let address = text(address)?
            .parse()
            .map_err(|error| format!("invalid candidate: {error}"))?;
        peer.publisher.add_local_candidate(address)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_remove_local_candidate(
    peer: *mut WagaPeer,
    address: *const c_char,
) -> bool {
    run(peer, |peer| {
        let address = text(address)?
            .parse()
            .map_err(|error| format!("invalid candidate: {error}"))?;
        peer.publisher.remove_local_candidate(address)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_add_server_reflexive_candidate(
    peer: *mut WagaPeer,
    address: *const c_char,
    base: *const c_char,
) -> bool {
    run(peer, |peer| {
        let address = text(address)?
            .parse()
            .map_err(|error| format!("invalid server-reflexive candidate: {error}"))?;
        let base = text(base)?
            .parse()
            .map_err(|error| format!("invalid base candidate: {error}"))?;
        peer.publisher.add_server_reflexive_candidate(address, base)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_create_offer(peer: *mut WagaPeer) -> *mut c_char {
    make_string(peer, |peer| peer.publisher.create_offer())
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_create_receive_offer(peer: *mut WagaPeer) -> *mut c_char {
    make_string(peer, |peer| peer.publisher.create_receive_offer())
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_accept_offer(peer: *mut WagaPeer, sdp: *const c_char) -> *mut c_char {
    make_string(peer, |peer| peer.publisher.accept_offer(text(sdp)?))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn waga_string_destroy(value: *mut c_char) {
    if !value.is_null() {
        drop(unsafe { CString::from_raw(value) });
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_accept_answer(peer: *mut WagaPeer, sdp: *const c_char) -> bool {
    run(peer, |peer| peer.publisher.accept_answer(text(sdp)?))
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_receive(
    peer: *mut WagaPeer,
    source: *const c_char,
    destination: *const c_char,
    data: *const u8,
    length: usize,
) -> bool {
    run(peer, |peer| {
        let source = text(source)?
            .parse()
            .map_err(|error| format!("invalid source: {error}"))?;
        let destination = text(destination)?
            .parse()
            .map_err(|error| format!("invalid destination: {error}"))?;
        peer.publisher
            .receive(source, destination, bytes(data, length)?)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_handle_timeout(peer: *mut WagaPeer) -> bool {
    run(peer, |peer| peer.publisher.handle_timeout())
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_timeout_millis(peer: *mut WagaPeer) -> u64 {
    peer_mut(peer)
        .map(|peer| {
            peer.publisher
                .timeout_after()
                .as_millis()
                .min(u64::MAX as u128) as u64
        })
        .unwrap_or(u64::MAX)
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_send(
    peer: *mut WagaPeer,
    codec: i32,
    media_time: u64,
    data: *const u8,
    length: usize,
) -> bool {
    run(peer, |peer| {
        let codec = decode_codec(codec)?.ok_or_else(|| "codec is required".to_string())?;
        peer.publisher.send(codec, media_time, bytes(data, length)?)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_set_desired_bitrate(peer: *mut WagaPeer, bitrate: u64) -> bool {
    run(peer, |peer| {
        peer.publisher.set_desired_bitrate(bitrate);

        Ok(())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_request_path_probe(peer: *mut WagaPeer) -> bool {
    run(peer, |peer| peer.publisher.request_path_probe())
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_restart_on_path_change(peer: *mut WagaPeer) -> bool {
    run(peer, |peer| peer.publisher.restart_on_path_change())
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_poll_transmit(peer: *mut WagaPeer, output: *mut WagaTransmit) -> bool {
    let Ok(peer) = peer_mut(peer) else {
        return false;
    };
    let Some(transmit) = peer.publisher.poll_transmit() else {
        return false;
    };
    peer.transmit = Some(transmit.into());
    let transmit = peer.transmit.as_ref().unwrap();
    if let Some(output) = unsafe { output.as_mut() } {
        *output = WagaTransmit {
            source: transmit.source.as_ptr(),
            destination: transmit.destination.as_ptr(),
            data: transmit.data.as_ptr(),
            length: transmit.data.len(),
        };
        true
    } else {
        false
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_last_transmit_sequence(peer: *mut WagaPeer) -> u64 {
    peer_mut(peer)
        .ok()
        .and_then(|peer| peer.transmit.as_ref())
        .and_then(|transmit| transmit.transport_sequence)
        .unwrap_or(u64::MAX)
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_set_egress_path(peer: *mut WagaPeer, sequence: u64, path: u64) {
    if let Ok(peer) = peer_mut(peer) {
        peer.publisher
            .rtc
            .set_egress_path(sequence, (path != u64::MAX).then_some(path));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_poll_event(peer: *mut WagaPeer) -> i32 {
    match peer_mut(peer)
        .ok()
        .and_then(|peer| peer.publisher.poll_event())
    {
        Some(PeerEvent::Connected) => 1,
        Some(PeerEvent::Disconnected) => 2,
        Some(PeerEvent::Closed) => 3,
        Some(PeerEvent::KeyframeRequest) => 4,
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_poll_media(peer: *mut WagaPeer, output: *mut WagaMedia) -> bool {
    let Ok(peer) = peer_mut(peer) else {
        return false;
    };
    let Some(media) = peer.publisher.poll_media() else {
        return false;
    };
    peer.media = Some(media);
    let media = peer.media.as_ref().unwrap();
    if let Some(output) = unsafe { output.as_mut() } {
        *output = WagaMedia {
            codec: encode_codec(media.codec),
            media_time: media.media_time,
            clock_rate: media.clock_rate,
            ntp_micros: media.ntp_micros.unwrap_or(u64::MAX),
            sender_media_time: media.sender_media_time,
            data: media.contents.as_ptr(),
            length: media.contents.len(),
        };
        true
    } else {
        false
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_poll_bitrate_estimate(peer: *mut WagaPeer, output: *mut u64) -> bool {
    let Ok(peer) = peer_mut(peer) else {
        return false;
    };
    let Some(estimate) = peer.publisher.poll_bitrate_estimate() else {
        return false;
    };
    let Some(output) = (unsafe { output.as_mut() }) else {
        return false;
    };
    *output = estimate;

    true
}

#[unsafe(no_mangle)]
pub extern "C" fn waga_peer_bwe_diagnostic_snapshot(peer: *mut WagaPeer) -> *mut c_char {
    make_string(peer, |peer| Ok(peer.publisher.bwe_diagnostic_snapshot()))
}

#[cfg(test)]
#[test]
fn diagnostic_snapshot_handles_null_and_owned_string() {
    assert!(waga_peer_bwe_diagnostic_snapshot(ptr::null_mut()).is_null());
    let peer = waga_peer_create(3, 1);
    assert!(!peer.is_null());
    let snapshot = waga_peer_bwe_diagnostic_snapshot(peer);
    assert!(!snapshot.is_null());
    unsafe {
        assert_eq!(CStr::from_ptr(snapshot).to_str().unwrap(), "bwe=disabled");
        waga_string_destroy(snapshot);
        waga_peer_destroy(peer);
    }
}

fn make_string(
    peer: *mut WagaPeer,
    operation: impl FnOnce(&mut WagaPeer) -> Result<String, String>,
) -> *mut c_char {
    let mut value = ptr::null_mut();
    let ok = run(peer, |peer| {
        value = CString::new(operation(peer)?)
            .map_err(|error| error.to_string())?
            .into_raw();
        Ok(())
    });
    if ok { value } else { ptr::null_mut() }
}

fn encode_codec(codec: Codec) -> i32 {
    match codec {
        Codec::H264 => 1,
        Codec::H265 => 2,
        Codec::Opus => 3,
        Codec::Aac => 4,
    }
}

fn decode_codec(value: i32) -> Result<Option<Codec>, String> {
    match value {
        0 => Ok(None),
        1 => Ok(Some(Codec::H264)),
        2 => Ok(Some(Codec::H265)),
        3 => Ok(Some(Codec::Opus)),
        4 => Ok(Some(Codec::Aac)),
        _ => Err(format!("unknown codec {value}")),
    }
}
