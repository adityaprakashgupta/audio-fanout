//! Wire protocol for network streaming. Everything is UDP, little-endian.
//!
//! Common 12-byte header: magic "AF" | version | type | session u32 | seq u32
//!
//!   HELLO    broadcaster -> receiver  rate u32, channels u16, frames/packet u16, name
//!   ACK      receiver -> broadcaster  status u8 (0 ok, 1 busy, 2 unsupported), name
//!   AUDIO    broadcaster -> receiver  frames u16, then frames*channels f32 samples
//!   STATE    broadcaster -> receiver  delay_ms u16, volume_pct u8, muted u8, send time u64 (us)
//!            (sent immediately on any change and 4x/second as heartbeat; doubles as a ping)
//!   PONG     receiver -> broadcaster  echoed send time, buffer fill/target, packet counters
//!   BYE      either way               end the session
//!   ANNOUNCE receiver -> LAN broadcast  stream port, busy flag, name (discovery)
//!
//! Audio is raw f32 PCM: no codec means no codec latency. A UDP packet is kept under
//! ~1.2 KB so it is never IP-fragmented.

pub const MAGIC: [u8; 2] = *b"AF";
pub const VERSION: u8 = 1;
pub const DEFAULT_PORT: u16 = 47800;
pub const DISCOVERY_PORT: u16 = 47801;
pub const HEADER_LEN: usize = 12;
pub const MAX_NAME: usize = 64;
/// Payload budget per audio packet.
pub const MAX_AUDIO_BYTES: usize = 1200;

const T_HELLO: u8 = 1;
const T_ACK: u8 = 2;
const T_AUDIO: u8 = 3;
const T_STATE: u8 = 4;
const T_PONG: u8 = 5;
const T_BYE: u8 = 6;
const T_ANNOUNCE: u8 = 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    Busy,
    Unsupported,
}

impl Status {
    fn to_u8(self) -> u8 {
        match self {
            Status::Ok => 0,
            Status::Busy => 1,
            Status::Unsupported => 2,
        }
    }
    fn from_u8(v: u8) -> Option<Status> {
        match v {
            0 => Some(Status::Ok),
            1 => Some(Status::Busy),
            2 => Some(Status::Unsupported),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Packet<'a> {
    Hello { session: u32, rate: u32, channels: u16, frames: u16, name: &'a str },
    Ack { session: u32, status: Status, name: &'a str },
    Audio { session: u32, seq: u32, frames: u16, data: &'a [u8] },
    State { session: u32, seq: u32, delay_ms: u16, volume_pct: u8, muted: bool, t_us: u64 },
    Pong {
        session: u32,
        t_us: u64,
        fill_ms: u16,
        target_ms: u16,
        received: u32,
        lost: u32,
        underruns: u32,
        playing: bool,
    },
    Bye { session: u32 },
    Announce { port: u16, busy: bool, name: &'a str },
}

/// Audio frames per packet for a channel count (keeps packets under the budget).
pub fn packet_frames(channels: usize) -> usize {
    (MAX_AUDIO_BYTES / (4 * channels.max(1))).clamp(16, 240)
}

// ---- encoding ---------------------------------------------------------------------

fn header(buf: &mut Vec<u8>, ty: u8, session: u32, seq: u32) {
    buf.clear();
    buf.extend_from_slice(&MAGIC);
    buf.push(VERSION);
    buf.push(ty);
    buf.extend_from_slice(&session.to_le_bytes());
    buf.extend_from_slice(&seq.to_le_bytes());
}

fn put_name(buf: &mut Vec<u8>, name: &str) {
    let mut n = name.len().min(MAX_NAME);
    while !name.is_char_boundary(n) {
        n -= 1;
    }
    buf.push(n as u8);
    buf.extend_from_slice(&name.as_bytes()[..n]);
}

pub fn encode_hello(buf: &mut Vec<u8>, session: u32, rate: u32, channels: u16, frames: u16, name: &str) {
    header(buf, T_HELLO, session, 0);
    buf.extend_from_slice(&rate.to_le_bytes());
    buf.extend_from_slice(&channels.to_le_bytes());
    buf.extend_from_slice(&frames.to_le_bytes());
    put_name(buf, name);
}

pub fn encode_ack(buf: &mut Vec<u8>, session: u32, status: Status, name: &str) {
    header(buf, T_ACK, session, 0);
    buf.push(status.to_u8());
    put_name(buf, name);
}

/// Writes the header only; follow with [`push_samples`].
pub fn encode_audio_start(buf: &mut Vec<u8>, session: u32, seq: u32, frames: u16) {
    header(buf, T_AUDIO, session, seq);
    buf.extend_from_slice(&frames.to_le_bytes());
}

pub fn push_samples(buf: &mut Vec<u8>, samples: &[f32]) {
    for s in samples {
        buf.extend_from_slice(&s.to_le_bytes());
    }
}

pub fn encode_state(buf: &mut Vec<u8>, session: u32, seq: u32, delay_ms: u16, volume_pct: u8, muted: bool, t_us: u64) {
    header(buf, T_STATE, session, seq);
    buf.extend_from_slice(&delay_ms.to_le_bytes());
    buf.push(volume_pct);
    buf.push(muted as u8);
    buf.extend_from_slice(&t_us.to_le_bytes());
}

#[allow(clippy::too_many_arguments)]
pub fn encode_pong(
    buf: &mut Vec<u8>,
    session: u32,
    t_us: u64,
    fill_ms: u16,
    target_ms: u16,
    received: u32,
    lost: u32,
    underruns: u32,
    playing: bool,
) {
    header(buf, T_PONG, session, 0);
    buf.extend_from_slice(&t_us.to_le_bytes());
    buf.extend_from_slice(&fill_ms.to_le_bytes());
    buf.extend_from_slice(&target_ms.to_le_bytes());
    buf.extend_from_slice(&received.to_le_bytes());
    buf.extend_from_slice(&lost.to_le_bytes());
    buf.extend_from_slice(&underruns.to_le_bytes());
    buf.push(playing as u8);
}

pub fn encode_bye(buf: &mut Vec<u8>, session: u32) {
    header(buf, T_BYE, session, 0);
}

pub fn encode_announce(buf: &mut Vec<u8>, port: u16, busy: bool, name: &str) {
    header(buf, T_ANNOUNCE, 0, 0);
    buf.extend_from_slice(&port.to_le_bytes());
    buf.push(busy as u8);
    put_name(buf, name);
}

// ---- decoding ---------------------------------------------------------------------

fn u16_at(p: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes(p.get(i..i + 2)?.try_into().ok()?))
}
fn u32_at(p: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(p.get(i..i + 4)?.try_into().ok()?))
}
fn u64_at(p: &[u8], i: usize) -> Option<u64> {
    Some(u64::from_le_bytes(p.get(i..i + 8)?.try_into().ok()?))
}
fn name_at(p: &[u8], i: usize) -> Option<&str> {
    let len = *p.get(i)? as usize;
    if len > MAX_NAME {
        return None;
    }
    std::str::from_utf8(p.get(i + 1..i + 1 + len)?).ok()
}

/// Never panics, whatever the bytes: anything on the LAN can send us a datagram.
pub fn decode(b: &[u8]) -> Option<Packet<'_>> {
    if b.len() < HEADER_LEN || b[0..2] != MAGIC || b[2] != VERSION {
        return None;
    }
    let ty = b[3];
    let session = u32_at(b, 4)?;
    let seq = u32_at(b, 8)?;
    let p = &b[HEADER_LEN..];
    Some(match ty {
        T_HELLO => Packet::Hello {
            session,
            rate: u32_at(p, 0)?,
            channels: u16_at(p, 4)?,
            frames: u16_at(p, 6)?,
            name: name_at(p, 8)?,
        },
        T_ACK => Packet::Ack { session, status: Status::from_u8(*p.first()?)?, name: name_at(p, 1)? },
        T_AUDIO => {
            let data = p.get(2..)?;
            if data.len() % 4 != 0 {
                return None;
            }
            Packet::Audio { session, seq, frames: u16_at(p, 0)?, data }
        }
        T_STATE => Packet::State {
            session,
            seq,
            delay_ms: u16_at(p, 0)?,
            volume_pct: *p.get(2)?,
            muted: *p.get(3)? != 0,
            t_us: u64_at(p, 4)?,
        },
        T_PONG => Packet::Pong {
            session,
            t_us: u64_at(p, 0)?,
            fill_ms: u16_at(p, 8)?,
            target_ms: u16_at(p, 10)?,
            received: u32_at(p, 12)?,
            lost: u32_at(p, 16)?,
            underruns: u32_at(p, 20)?,
            playing: *p.get(24)? != 0,
        },
        T_BYE => Packet::Bye { session },
        T_ANNOUNCE => Packet::Announce { port: u16_at(p, 0)?, busy: *p.get(2)? != 0, name: name_at(p, 3)? },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_every_packet_type() {
        let mut b = Vec::new();

        encode_hello(&mut b, 7, 48_000, 2, 150, "Laptop A");
        assert_eq!(decode(&b), Some(Packet::Hello { session: 7, rate: 48_000, channels: 2, frames: 150, name: "Laptop A" }));

        encode_ack(&mut b, 7, Status::Busy, "Laptop B");
        assert_eq!(decode(&b), Some(Packet::Ack { session: 7, status: Status::Busy, name: "Laptop B" }));

        let samples = [0.5f32, -0.25, 1.0, -1.0];
        encode_audio_start(&mut b, 7, 41, 2);
        push_samples(&mut b, &samples);
        match decode(&b) {
            Some(Packet::Audio { session: 7, seq: 41, frames: 2, data }) => {
                let back: Vec<f32> = data.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
                assert_eq!(back, samples);
            }
            other => panic!("{other:?}"),
        }

        encode_state(&mut b, 7, 9, 180, 55, true, 123_456_789);
        assert_eq!(
            decode(&b),
            Some(Packet::State { session: 7, seq: 9, delay_ms: 180, volume_pct: 55, muted: true, t_us: 123_456_789 })
        );

        encode_pong(&mut b, 7, 99, 38, 50, 1000, 3, 1, true);
        assert_eq!(
            decode(&b),
            Some(Packet::Pong { session: 7, t_us: 99, fill_ms: 38, target_ms: 50, received: 1000, lost: 3, underruns: 1, playing: true })
        );

        encode_bye(&mut b, 7);
        assert_eq!(decode(&b), Some(Packet::Bye { session: 7 }));

        encode_announce(&mut b, 47_800, true, "Kitchen \u{1F3A7}");
        assert_eq!(decode(&b), Some(Packet::Announce { port: 47_800, busy: true, name: "Kitchen \u{1F3A7}" }));
    }

    #[test]
    fn rejects_foreign_and_malformed_packets() {
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(b"not our protocol at all"), None);
        let mut b = Vec::new();
        encode_bye(&mut b, 1);
        let mut wrong_version = b.clone();
        wrong_version[2] = VERSION + 1;
        assert_eq!(decode(&wrong_version), None);
        let mut unknown_type = b.clone();
        unknown_type[3] = 200;
        assert_eq!(decode(&unknown_type), None);
        // audio whose payload is not a whole number of f32s
        encode_audio_start(&mut b, 1, 1, 1);
        b.extend_from_slice(&[1, 2, 3]);
        assert_eq!(decode(&b), None);
    }

    /// Anything on the LAN can send us bytes: decoding must never panic, on truncation
    /// at every length or on pseudo-random garbage.
    #[test]
    fn decoder_never_panics() {
        let mut all = Vec::new();
        let mut b = Vec::new();
        encode_hello(&mut b, 1, 48_000, 2, 150, "x");
        all.push(b.clone());
        encode_ack(&mut b, 1, Status::Ok, "y");
        all.push(b.clone());
        encode_state(&mut b, 1, 1, 1, 1, false, 1);
        all.push(b.clone());
        encode_pong(&mut b, 1, 1, 1, 1, 1, 1, 1, true);
        all.push(b.clone());
        encode_announce(&mut b, 1, false, "z");
        all.push(b.clone());
        for pkt in &all {
            for cut in 0..pkt.len() {
                let _ = decode(&pkt[..cut]);
            }
        }
        let mut x = 0x1234_5678u32;
        for _ in 0..20_000 {
            let len = (x % 80) as usize;
            let mut junk: Vec<u8> = (0..len)
                .map(|_| {
                    x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (x >> 24) as u8
                })
                .collect();
            if junk.len() >= 4 {
                junk[0..3].copy_from_slice(&[b'A', b'F', VERSION]); // get past the magic check
            }
            let _ = decode(&junk);
        }
    }

    #[test]
    fn long_names_are_truncated_on_a_char_boundary() {
        let long = "\u{00e9}".repeat(100); // 2 bytes per char, 200 bytes
        let mut b = Vec::new();
        encode_announce(&mut b, 1, false, &long);
        match decode(&b) {
            Some(Packet::Announce { name, .. }) => {
                assert!(name.len() <= MAX_NAME && name.chars().all(|c| c == '\u{00e9}'));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn packets_fit_the_budget_for_any_channel_count() {
        for ch in 1..=8usize {
            let bytes = packet_frames(ch) * ch * 4;
            assert!(bytes <= MAX_AUDIO_BYTES, "{ch} ch -> {bytes} bytes");
            assert!(HEADER_LEN + 2 + bytes < 1400);
        }
        assert_eq!(packet_frames(2), 150);
    }
}
