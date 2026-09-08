//! Local AAC test receiver: bind-ip token output.aac. Records 60 seconds after the WHIP offer.
use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::time::{Duration, Instant};
use waga_webrtc_core::{Codec, Publisher};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 || args[2].len() < 24 {
        return Err(
            "usage: aac_ingest <LAN IP> <random token, at least 24 characters> <output.aac>".into(),
        );
    }
    let udp = UdpSocket::bind(format!("{}:0", args[1]))?;
    let http = TcpListener::bind(format!("{}:8099", args[1]))?;
    let path = format!("/whip/{}", args[2]);
    println!("AAC test URL: http://{}:8099{path}", args[1]);
    println!("Select AAC, H.264 and disable bonding for this local receiver.");
    let mut peer = Publisher::new(Some(Codec::Aac), Some(Codec::H264))?;
    peer.add_local_candidate(udp.local_addr()?)?;
    loop {
        let (mut client, _) = http.accept()?;
        client.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut request = Vec::new();
        let mut byte = [0];
        while !request.ends_with(b"\r\n\r\n") && request.len() < 16_384 {
            client.read_exact(&mut byte)?;
            request.push(byte[0]);
        }
        let headers = String::from_utf8(request)?;
        if !headers.starts_with(&format!("POST {path} HTTP/1.1\r\n")) {
            client.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )?;
            continue;
        }
        let length = headers
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .filter(|size| *size > 0 && *size <= 65_536)
            .ok_or("invalid SDP length")?;
        let mut offer = vec![0; length];
        client.read_exact(&mut offer)?;
        let answer = peer.accept_offer(std::str::from_utf8(&offer)?)?;
        write!(
            client,
            "HTTP/1.1 201 Created\r\nContent-Type: application/sdp\r\nLocation: {path}/session\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
            answer.len()
        )?;
        break;
    }
    drop(http);
    let mut output = File::options()
        .write(true)
        .create_new(true)
        .open(&args[3])?;
    udp.set_read_timeout(Some(Duration::from_millis(10)))?;
    let started = Instant::now();
    let mut buffer = [0; 65_536];
    let mut frames = 0;
    let mut bytes = 0;
    while started.elapsed() < Duration::from_secs(60) && bytes < 32 * 1024 * 1024 {
        match udp.recv_from(&mut buffer) {
            Ok((length, source)) => {
                if !buffer[..length].starts_with(b"WGR1") {
                    peer.receive(source, udp.local_addr()?, &buffer[..length])?;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error.into()),
        }
        peer.handle_timeout()?;
        while let Some(packet) = peer.poll_transmit() {
            udp.send_to(&packet.contents, packet.destination)?;
        }
        while peer.poll_event().is_some() {}
        while let Some(frame) = peer.poll_media() {
            if frame.codec != Codec::Aac {
                continue;
            }
            let length = frame.contents.len() + 7;
            if length > 8191 {
                return Err("AAC frame exceeds ADTS size limit".into());
            }
            output.write_all(&[
                0xff,
                0xf1,
                0x4c,
                0x80 | (length >> 11) as u8,
                (length >> 3) as u8,
                ((length & 7) << 5) as u8 | 0x1f,
                0xfc,
            ])?;
            output.write_all(&frame.contents)?;
            frames += 1;
            bytes += length;
            if frames == 1 {
                println!("Receiving AAC audio; recording for up to 60 seconds.");
            }
        }
    }
    output.flush()?;
    println!("Saved {frames} AAC frames to {}", args[3]);
    Ok(())
}
