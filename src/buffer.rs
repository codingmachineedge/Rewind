//! Rolling in-memory clip buffer.
//!
//! The core idea of Rewind: encoded frames are continuously pushed into a
//! time-bounded ring. It always holds roughly the last N seconds of gameplay,
//! dropping the oldest packets as new ones arrive. Nothing touches the disk
//! until the user asks to save, at which point [`ClipBuffer::snapshot`] hands the
//! muxer a keyframe-aligned copy of the window.

use std::collections::VecDeque;

use crate::media::EncodedPacket;

const NS_PER_SEC: u64 = 1_000_000_000;

/// Time-bounded ring of recently encoded packets.
pub struct ClipBuffer {
    packets: VecDeque<EncodedPacket>,
    window_ns: u64,
    /// Hard safety cap on packet count, independent of the time window, so a
    /// misbehaving timestamp source can't grow the buffer without bound.
    max_packets: usize,
}

impl ClipBuffer {
    /// Create a buffer holding roughly `window_seconds` of footage. `approx_fps`
    /// is only used to size the safety cap and initial allocation.
    pub fn new(window_seconds: u32, approx_fps: u32) -> Self {
        let window_ns = window_seconds.max(1) as u64 * NS_PER_SEC;
        // 2x headroom over the nominal frame count for encoder burstiness.
        let max_packets = (window_seconds.max(1) * approx_fps.max(1)).saturating_mul(2) as usize;
        Self {
            packets: VecDeque::with_capacity(max_packets.min(8192)),
            window_ns,
            max_packets: max_packets.max(64),
        }
    }

    /// Length of the rolling window in whole seconds.
    pub fn window_seconds(&self) -> u32 {
        (self.window_ns / NS_PER_SEC) as u32
    }

    /// Number of packets currently buffered.
    pub fn len(&self) -> usize {
        self.packets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    /// Push an encoded packet, then evict anything older than the window.
    pub fn push(&mut self, packet: EncodedPacket) {
        self.packets.push_back(packet);
        self.evict();
    }

    fn evict(&mut self) {
        let Some(newest) = self.packets.back().map(|p| p.pts_ns) else {
            return;
        };
        while self.packets.len() > 1 {
            let too_old = self
                .packets
                .front()
                .map(|p| newest.saturating_sub(p.pts_ns) > self.window_ns)
                .unwrap_or(false);
            let too_many = self.packets.len() > self.max_packets;
            if too_old || too_many {
                self.packets.pop_front();
            } else {
                break;
            }
        }
    }

    /// Span between the oldest and newest buffered packet, in nanoseconds.
    pub fn buffered_duration_ns(&self) -> u64 {
        match (self.packets.front(), self.packets.back()) {
            (Some(f), Some(b)) => b.pts_ns.saturating_sub(f.pts_ns),
            _ => 0,
        }
    }

    /// Copy the buffered window, trimmed to begin at the earliest keyframe so the
    /// muxed clip is independently decodable from its first frame.
    pub fn snapshot(&self) -> Vec<EncodedPacket> {
        let start = self
            .packets
            .iter()
            .position(|p| p.is_keyframe)
            .unwrap_or(0);
        self.packets.iter().skip(start).cloned().collect()
    }

    pub fn clear(&mut self) {
        self.packets.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(pts_ms: u64, key: bool) -> EncodedPacket {
        EncodedPacket {
            data: vec![0u8; 16],
            pts_ns: pts_ms * 1_000_000,
            dts_ns: None,
            is_keyframe: key,
            track: crate::media::Track::Video,
        }
    }

    #[test]
    fn evicts_packets_older_than_window() {
        let mut b = ClipBuffer::new(1, 30); // 1 second window
        for i in 0..200u64 {
            // A frame every 10 ms => 2 s of footage total, only ~1 s should remain.
            b.push(packet(i * 10, i % 30 == 0));
        }
        assert!(b.buffered_duration_ns() <= 1_000_000_000);
        assert!(!b.is_empty());
    }

    #[test]
    fn snapshot_starts_on_keyframe() {
        let mut b = ClipBuffer::new(10, 30);
        b.push(packet(0, false));
        b.push(packet(10, false));
        b.push(packet(20, true));
        b.push(packet(30, false));
        let snap = b.snapshot();
        assert_eq!(snap.len(), 2);
        assert!(snap[0].is_keyframe);
    }
}
