//! Copies of small interactive data in QUIC datagrams.
//!
//! A lost key press or echo waits for QUIC loss recovery: the probe timeout
//! is about srtt + 4 rttvar + 25 ms, and it doubles when the probe is lost
//! too. On a path that loses packets, the side that sends a small batch (keys,
//! or output with the echo) also sends copies of it in datagrams, a little
//! later and then less often: when the original is lost, a copy arrives
//! soon after the loss ends. The stream stays the reliable path; the
//! receiver uses a copy only at its exact place in the stream and skips that
//! part when the stream brings it, so nothing is applied twice.
//!
//! Datagram: kind (u8), stream id (u64), position (u64), data. For input the
//! position is the input count after the data, and the data is all the input
//! that the server has not acknowledged. For output it is the byte position
//! in the terminal stream where the frames start, and the data are whole
//! OUTPUT and INPUT_ACK frames, as on the stream.

use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use crate::proto::kind;

pub const INPUT: u8 = b'I';
pub const OUTPUT: u8 = b'O';
/// Larger batches are not interactive: the stream alone carries them.
pub const MAX_DATA: usize = 1000;
/// Copies only while at most this much is unacknowledged.
pub const SPARSE: u64 = 2 * MAX_DATA as u64;
const HEADER: usize = 17;
/// Copies of one batch, at δ, 3δ, 7δ and 15δ after it.
const COPIES: u32 = 4;
/// Copies only when the path lost a packet in this time.
const LOSS_MEMORY: Duration = Duration::from_secs(20);

/// Counts for the log: copies sent, and copies that the receiver used.
pub static SENT: AtomicU64 = AtomicU64::new(0);
pub static USED: AtomicU64 = AtomicU64::new(0);

pub fn encode(k: u8, sid: u64, pos: u64, data: &[u8]) -> bytes::Bytes {
    let mut v = Vec::with_capacity(HEADER + data.len());
    v.push(k);
    v.extend_from_slice(&sid.to_be_bytes());
    v.extend_from_slice(&pos.to_be_bytes());
    v.extend_from_slice(data);
    v.into()
}

pub fn decode(d: &[u8]) -> Option<(u8, u64, u64, &[u8])> {
    if d.len() < HEADER {
        return None;
    }
    Some((d[0], u64::from_be_bytes(d[1..9].try_into().ok()?), u64::from_be_bytes(d[9..17].try_into().ok()?), &d[HEADER..]))
}

/// The frames of an output copy, if it holds only whole OUTPUT and
/// INPUT_ACK frames.
pub fn frames(mut d: &[u8]) -> Option<Vec<(u8, &[u8])>> {
    let mut v = Vec::new();
    while !d.is_empty() {
        if d.len() < 5 {
            return None;
        }
        let len = u32::from_be_bytes(d[1..5].try_into().ok()?) as usize;
        if d.len() < 5 + len || !matches!(d[0], kind::OUTPUT | kind::INPUT_ACK) {
            return None;
        }
        v.push((d[0], &d[5..5 + len]));
        d = &d[5 + len..];
    }
    (!v.is_empty()).then_some(v)
}

/// The end of the output in a batch of frames (for the client's ACK).
pub fn output_end(d: &[u8]) -> Option<u64> {
    let f = frames(d)?;
    f.iter().rev().find(|(k, _)| *k == kind::OUTPUT).and_then(|(_, p)| crate::proto::split_offset(p).ok()).map(|(o, b)| o + b.len() as u64)
}

/// Time of copy `n` (0-based) after the original.
pub fn delay(rtt: Duration, n: u32) -> Duration {
    let unit = (rtt / 8).clamp(Duration::from_millis(8), Duration::from_millis(40));
    unit * ((2u32 << n) - 1)
}

/// Copies are on while the path loses packets (and not on the SSH tunnel:
/// it loses none).
pub struct LossWatch {
    lost: u64,
    at: Option<Instant>,
}

impl LossWatch {
    pub fn new() -> LossWatch {
        LossWatch { lost: 0, at: None }
    }

    pub fn lossy(&mut self, conn: &quinn::Connection) -> bool {
        if crate::ablate::off("copies") {
            return false;
        }
        let lost = conn.stats().path.lost_packets;
        if lost > self.lost {
            self.lost = lost;
            self.at = Some(Instant::now());
        }
        self.at.is_some_and(|t| t.elapsed() < LOSS_MEMORY)
    }
}

/// One batch with copies to send.
pub struct Pending {
    pub data: bytes::Bytes,
    /// Output: the end of its output (no more copies once the client has it).
    pub end: u64,
    sent: Instant,
    n: u32,
    pub at: Instant,
}

/// The batches of one sender that still get copies.
pub struct Schedule {
    pub list: Vec<Pending>,
    pub max: usize,
}

impl Schedule {
    pub fn new(max: usize) -> Schedule {
        Schedule { list: Vec::new(), max }
    }

    pub fn add(&mut self, data: bytes::Bytes, end: u64, rtt: Duration) {
        let now = Instant::now();
        if self.list.len() >= self.max {
            self.list.remove(0);
        }
        self.list.push(Pending { data, end, sent: now, n: 0, at: now + delay(rtt, 0) });
    }

    pub fn next(&self) -> Option<Instant> {
        self.list.iter().map(|p| p.at).min()
    }

    /// The copies that are due now; their next times are set.
    pub fn due(&mut self, rtt: Duration) -> Vec<bytes::Bytes> {
        let now = Instant::now();
        let mut out = Vec::new();
        for p in &mut self.list {
            if p.at <= now {
                SENT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                out.push(p.data.clone());
                p.n += 1;
                p.at = p.sent + delay(rtt, p.n);
            }
        }
        self.list.retain(|p| p.n < COPIES);
        out
    }

    pub fn retain(&mut self, keep: impl Fn(&Pending) -> bool) {
        self.list.retain(keep);
    }
}

pub async fn sleep_until(t: Option<Instant>) {
    match t {
        Some(t) => tokio::time::sleep_until(t.into()).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto;

    #[test]
    fn roundtrip() {
        let d = encode(OUTPUT, 4, 99, b"xyz");
        assert_eq!(decode(&d), Some((OUTPUT, 4, 99, &b"xyz"[..])));
        assert_eq!(decode(&d[..10]), None);
    }

    #[test]
    fn output_frames() {
        let mut b = proto::frame_at(kind::OUTPUT, 10, b"hello");
        b.extend(proto::frame(kind::INPUT_ACK, &[0, 0, 0, 0, 0, 0, 0, 3, 0x81]));
        let f = frames(&b).unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!(output_end(&b), Some(15));
        // A size frame is not copied: its place in the stream matters.
        let mut s = proto::frame(kind::SIZE, &[0, 80, 0, 24, 0]);
        s.extend(proto::frame_at(kind::OUTPUT, 0, b"x"));
        assert!(frames(&s).is_none());
        // A cut frame is refused.
        assert!(frames(&b[..b.len() - 1]).is_none());
    }

    #[test]
    fn delays_grow() {
        let rtt = Duration::from_millis(160);
        let d: Vec<_> = (0..4).map(|n| delay(rtt, n).as_millis()).collect();
        assert_eq!(d, vec![20, 60, 140, 300]);
        assert_eq!(delay(Duration::from_millis(10), 0), Duration::from_millis(8));
    }
}
