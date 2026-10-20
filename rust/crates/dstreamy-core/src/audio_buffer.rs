use std::collections::VecDeque;
use std::num::NonZeroUsize;

/// Re-chunks a PCM byte stream into fixed-size frames, keeping partial input for the next push.
#[derive(Debug, Clone)]
pub struct AudioFrameBuffer {
    pending: VecDeque<u8>,
    frame_bytes: NonZeroUsize,
}

impl AudioFrameBuffer {
    pub fn new(frame_bytes: NonZeroUsize) -> Self {
        Self {
            pending: VecDeque::new(),
            frame_bytes,
        }
    }

    pub fn frame_bytes(&self) -> usize {
        self.frame_bytes.get()
    }

    pub fn pending_bytes(&self) -> usize {
        self.pending.len()
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.pending.extend(chunk);
    }

    pub fn clear(&mut self) {
        self.pending.clear();
    }

    pub fn pop_frame(&mut self) -> Option<Vec<u8>> {
        let n = self.frame_bytes.get();
        (self.pending.len() >= n).then(|| self.pending.drain(..n).collect())
    }

    /// Yields every complete frame currently buffered.
    pub fn drain_frames(&mut self) -> impl Iterator<Item = Vec<u8>> + '_ {
        std::iter::from_fn(|| self.pop_frame())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(frame_bytes: usize) -> AudioFrameBuffer {
        AudioFrameBuffer::new(NonZeroUsize::new(frame_bytes).unwrap())
    }

    #[test]
    fn emits_fixed_size_frames_and_preserves_partial_input() {
        let mut b = buffer(4);

        b.push(&[1, 2, 3]);
        assert_eq!(b.drain_frames().count(), 0);
        assert_eq!(b.pending_bytes(), 3);

        b.push(&[4, 5, 6, 7, 8, 9]);
        let frames: Vec<_> = b.drain_frames().collect();
        assert_eq!(frames, vec![vec![1, 2, 3, 4], vec![5, 6, 7, 8]]);
        assert_eq!(b.pending_bytes(), 1);
    }

    #[test]
    fn clears_pending_partial_data() {
        let mut b = buffer(4);
        b.push(&[1, 2, 3]);
        b.clear();
        assert_eq!(b.pending_bytes(), 0);
    }

    #[test]
    fn empty_push_is_a_no_op() {
        let mut b = buffer(2);
        b.push(&[]);
        assert_eq!(b.pending_bytes(), 0);
        assert_eq!(b.pop_frame(), None);
    }

    #[test]
    fn frame_spanning_many_chunks() {
        let mut b = buffer(5);
        for byte in 0..5 {
            b.push(&[byte]);
        }
        assert_eq!(b.pop_frame(), Some(vec![0, 1, 2, 3, 4]));
        assert_eq!(b.pending_bytes(), 0);
    }
}
