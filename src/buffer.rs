//! Rolling in-memory clip buffer.
//!
//! The core idea of ClipForge: frames are continuously encoded into a fixed-size
//! ring buffer. When the user presses the save hotkey, the buffer's contents
//! (the last N seconds) are flushed to a container file on disk. Nothing touches
//! the disk until the user asks for it.

use std::path::PathBuf;

/// A placeholder for an encoded frame / GOP chunk.
///
/// In the real implementation this holds compressed video data (e.g. an H.264
/// NAL unit or a fragment) plus a presentation timestamp.
#[derive(Debug, Default, Clone)]
pub struct EncodedFrame {
    pub timestamp_ns: u64,
    pub data: Vec<u8>,
}

/// Fixed-capacity ring buffer of recently captured frames.
pub struct ClipBuffer {
    frames: Vec<EncodedFrame>,
    capacity: usize,
    write_head: usize,
    len: usize,
}

impl ClipBuffer {
    /// Create a buffer sized to hold `seconds * fps` frames.
    pub fn new(seconds: u32, fps: u32) -> Self {
        let capacity = (seconds.max(1) * fps.max(1)) as usize;
        Self {
            frames: Vec::with_capacity(capacity),
            capacity,
            write_head: 0,
            len: 0,
        }
    }

    /// Maximum number of frames the buffer holds before overwriting the oldest.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of frames currently buffered.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Push an encoded frame, overwriting the oldest once full.
    pub fn push(&mut self, frame: EncodedFrame) {
        if self.frames.len() < self.capacity {
            self.frames.push(frame);
        } else {
            self.frames[self.write_head] = frame;
        }
        self.write_head = (self.write_head + 1) % self.capacity;
        self.len = (self.len + 1).min(self.capacity);
    }

    /// Convenience used by the scaffold's stub entry point.
    pub fn push_frame_placeholder(&mut self) {
        self.push(EncodedFrame::default());
    }

    /// Flush the buffered frames to a clip file and return the output path.
    ///
    /// TODO: mux the buffered frames into an `.mp4`/`.mkv` container. For now this
    /// just reports where the clip *would* be written.
    pub fn flush_to_clip(&self, output_dir: &PathBuf) -> Result<String, String> {
        // A real implementation would order frames from `write_head`, mux them,
        // and write atomically. Timestamp-based naming is deferred to config/clock.
        let path = output_dir.join("clip_latest.mp4");
        Ok(path.display().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_overwrites_oldest_when_full() {
        let mut b = ClipBuffer::new(1, 3); // capacity 3
        assert_eq!(b.capacity(), 3);
        for _ in 0..5 {
            b.push(EncodedFrame::default());
        }
        // Never exceeds capacity.
        assert_eq!(b.len(), 3);
    }
}
