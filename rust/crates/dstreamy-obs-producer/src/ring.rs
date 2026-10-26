//! Bounded packet queue from `encoded_packet` to the IPC writer, with `dstreamy_media::external`'s drop policy.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use tokio::sync::Notify;

/// About 2 s at 60 fps plus 20 ms audio.
pub const CAPACITY: usize = 256;
/// Four maximum-size producer frames.
pub const MAX_BYTES: usize = 32 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Track {
    Video,
    Audio,
}

/// One `encoder_packet`, copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub track: Track,
    pub keyframe: bool,
    pub pts: i64,
    pub dts: i64,
    pub timebase: (i32, i32),
    pub data: Vec<u8>,
    pub arrived: Instant,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RingStats {
    pub queued: usize,
    pub queued_bytes: usize,
    pub dropped_video: u64,
    pub dropped_audio: u64,
}

#[derive(Debug, Default)]
struct Inner {
    queue: VecDeque<Packet>,
    bytes: usize,
    awaiting_keyframe: bool,
    dropped_video: u64,
    dropped_audio: u64,
    closed: bool,
}

impl Inner {
    fn full(&self, capacity: usize, max_bytes: usize, incoming: usize) -> bool {
        self.queue.len() >= capacity || self.bytes + incoming > max_bytes
    }

    fn drop_queued_video(&mut self) {
        let before = self.queue.len();
        let mut freed = 0;
        self.queue.retain(|p| {
            let keep = p.track == Track::Audio;
            if !keep {
                freed += p.data.len();
            }
            keep
        });
        self.bytes -= freed;
        self.dropped_video += (before - self.queue.len()) as u64;
        self.awaiting_keyframe = true;
    }

    fn drop_oldest_audio(&mut self) -> bool {
        let Some(i) = self.queue.iter().position(|p| p.track == Track::Audio) else {
            return false;
        };
        let p = self.queue.remove(i).expect("position is in range");
        self.bytes -= p.data.len();
        self.dropped_audio += 1;
        true
    }

    fn admit(&mut self, packet: Packet, capacity: usize, max_bytes: usize) {
        let len = packet.data.len();
        match packet.track {
            Track::Video => {
                if self.awaiting_keyframe && !packet.keyframe {
                    self.dropped_video += 1;
                    return;
                }
                if self.full(capacity, max_bytes, len) {
                    self.drop_queued_video();
                    while self.full(capacity, max_bytes, len) && self.drop_oldest_audio() {}
                    if !packet.keyframe || self.full(capacity, max_bytes, len) {
                        self.dropped_video += 1;
                        return;
                    }
                }
                if packet.keyframe {
                    self.awaiting_keyframe = false;
                }
            }
            Track::Audio => {
                while self.full(capacity, max_bytes, len) {
                    if self.drop_oldest_audio() {
                        continue;
                    }
                    if self.queue.is_empty() {
                        self.dropped_audio += 1;
                        return;
                    }
                    self.drop_queued_video();
                }
            }
        }
        self.bytes += len;
        self.queue.push_back(packet);
    }
}

#[derive(Debug)]
pub struct Ring {
    capacity: usize,
    max_bytes: usize,
    inner: Mutex<Inner>,
    ready: Notify,
}

impl Default for Ring {
    fn default() -> Self {
        Self::new(CAPACITY, MAX_BYTES)
    }
}

impl Ring {
    pub fn new(capacity: usize, max_bytes: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            max_bytes,
            inner: Mutex::new(Inner::default()),
            ready: Notify::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Never blocks on the consumer; wakes it only when the queue was empty.
    pub fn push(&self, packet: Packet) {
        let wake = {
            let mut inner = self.lock();
            if inner.closed {
                return;
            }
            let was_empty = inner.queue.is_empty();
            inner.admit(packet, self.capacity, self.max_bytes);
            was_empty && !inner.queue.is_empty()
        };
        if wake {
            self.ready.notify_one();
        }
    }

    pub fn pop(&self) -> Option<Packet> {
        let mut inner = self.lock();
        let packet = inner.queue.pop_front()?;
        inner.bytes -= packet.data.len();
        Some(packet)
    }

    /// The next packet; `None` once the ring is closed and drained.
    pub async fn next(&self) -> Option<Packet> {
        loop {
            {
                let mut inner = self.lock();
                if let Some(packet) = inner.queue.pop_front() {
                    inner.bytes -= packet.data.len();
                    return Some(packet);
                }
                if inner.closed {
                    return None;
                }
            }
            self.ready.notified().await;
        }
    }

    /// Refuses further packets; what is queued can still be taken.
    pub fn close(&self) {
        self.lock().closed = true;
        self.ready.notify_one();
    }

    /// Empties the ring and accepts packets again, with fresh counters.
    pub fn reopen(&self) {
        *self.lock() = Inner::default();
    }

    pub fn stats(&self) -> RingStats {
        let inner = self.lock();
        RingStats {
            queued: inner.queue.len(),
            queued_bytes: inner.bytes,
            dropped_video: inner.dropped_video,
            dropped_audio: inner.dropped_audio,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    fn video(pts: i64, keyframe: bool) -> Packet {
        Packet {
            track: Track::Video,
            keyframe,
            pts,
            dts: pts,
            timebase: (1, 30),
            data: vec![0; 10],
            arrived: Instant::now(),
        }
    }

    fn audio(pts: i64) -> Packet {
        Packet {
            track: Track::Audio,
            keyframe: true,
            pts,
            dts: pts,
            timebase: (1, 48_000),
            data: vec![0; 3],
            arrived: Instant::now(),
        }
    }

    fn drain(ring: &Ring) -> Vec<(Track, i64)> {
        std::iter::from_fn(|| ring.pop())
            .map(|p| (p.track, p.pts))
            .collect()
    }

    #[test]
    fn packets_come_out_in_order() {
        let ring = Ring::new(8, 1 << 20);
        ring.push(video(0, true));
        ring.push(audio(0));
        ring.push(video(1, false));
        assert_eq!(
            drain(&ring),
            [(Track::Video, 0), (Track::Audio, 0), (Track::Video, 1)]
        );
        assert_eq!(ring.stats(), RingStats::default());
    }

    #[test]
    fn full_drops_the_oldest_audio_for_audio() {
        let ring = Ring::new(3, 1 << 20);
        ring.push(audio(0));
        ring.push(video(0, true));
        ring.push(audio(1));
        ring.push(audio(2));
        assert_eq!(
            drain(&ring),
            [(Track::Video, 0), (Track::Audio, 1), (Track::Audio, 2)]
        );
        assert_eq!(ring.stats().dropped_audio, 1);
        assert_eq!(ring.stats().dropped_video, 0);
    }

    #[test]
    fn full_drops_video_until_the_next_keyframe() {
        let ring = Ring::new(3, 1 << 20);
        ring.push(video(0, true));
        ring.push(video(1, false));
        ring.push(audio(0));
        ring.push(video(2, false));
        assert_eq!(ring.stats().dropped_video, 3, "0 and 1 queued, then 2");
        ring.push(video(3, false));
        assert_eq!(drain(&ring), [(Track::Audio, 0)]);
        ring.push(video(4, true));
        ring.push(video(5, false));
        assert_eq!(drain(&ring), [(Track::Video, 4), (Track::Video, 5)]);
        let stats = ring.stats();
        assert_eq!(stats.dropped_video, 4, "3 waited for a keyframe");
        assert_eq!(stats.dropped_audio, 0);
    }

    #[test]
    fn a_keyframe_into_a_full_ring_of_audio_displaces_the_oldest_audio() {
        let ring = Ring::new(2, 1 << 20);
        ring.push(audio(0));
        ring.push(audio(1));
        ring.push(video(0, true));
        assert_eq!(drain(&ring), [(Track::Audio, 1), (Track::Video, 0)]);
        assert_eq!(ring.stats().dropped_audio, 1);
        assert_eq!(ring.stats().dropped_video, 0);
    }

    #[test]
    fn audio_into_a_ring_full_of_video_drops_the_video() {
        let ring = Ring::new(2, 1 << 20);
        ring.push(video(0, true));
        ring.push(video(1, false));
        ring.push(audio(0));
        ring.push(video(2, false));
        ring.push(video(3, true));
        assert_eq!(drain(&ring), [(Track::Audio, 0), (Track::Video, 3)]);
        assert_eq!(ring.stats().dropped_video, 3);
    }

    #[test]
    fn the_byte_limit_counts_as_full() {
        let ring = Ring::new(100, 25);
        ring.push(video(0, true));
        ring.push(video(1, false));
        ring.push(audio(0));
        ring.push(audio(1));
        assert_eq!(ring.stats().queued_bytes, 23);
        assert_eq!(
            drain(&ring),
            [(Track::Video, 0), (Track::Video, 1), (Track::Audio, 1)]
        );
        assert_eq!(ring.stats().dropped_audio, 1);
        let mut big = video(3, true);
        big.data = vec![0; 26];
        ring.push(big);
        assert!(
            drain(&ring).is_empty(),
            "a packet over the byte limit never fits"
        );
        assert_eq!(ring.stats().queued_bytes, 0);
    }

    #[test]
    fn memory_stays_bounded_and_counters_are_exact() {
        let ring = Ring::new(16, 1 << 20);
        let mut pushed = (0u64, 0u64);
        for i in 0..10_000 {
            ring.push(video(i, i % 60 == 0));
            ring.push(audio(i));
            pushed.0 += 1;
            pushed.1 += 1;
            assert!(ring.stats().queued <= 16);
        }
        let stats = ring.stats();
        let left = drain(&ring);
        let left_video = left.iter().filter(|(t, _)| *t == Track::Video).count() as u64;
        let left_audio = left.len() as u64 - left_video;
        assert_eq!(stats.dropped_video + left_video, pushed.0);
        assert_eq!(stats.dropped_audio + left_audio, pushed.1);
    }

    #[test]
    fn closed_refuses_new_packets_but_drains_old_ones() {
        let ring = Ring::new(8, 1 << 20);
        ring.push(audio(0));
        ring.close();
        ring.push(audio(1));
        assert_eq!(drain(&ring), [(Track::Audio, 0)]);
        ring.reopen();
        ring.push(audio(2));
        assert_eq!(drain(&ring), [(Track::Audio, 2)]);
    }

    #[tokio::test]
    async fn next_waits_for_a_packet_and_ends_after_close() {
        let ring = Arc::new(Ring::new(8, 1 << 20));
        let consumer = tokio::spawn({
            let ring = ring.clone();
            async move {
                let mut got = Vec::new();
                while let Some(p) = ring.next().await {
                    got.push(p.pts);
                }
                got
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        ring.push(audio(7));
        tokio::time::sleep(Duration::from_millis(20)).await;
        ring.push(audio(8));
        ring.close();
        assert_eq!(consumer.await.unwrap(), [7, 8]);
    }

    /// One thread pushes as `encoded_packet` does while another drains: no packet is lost or reordered beyond the policy.
    #[test]
    fn two_threads_keep_order_and_account_for_every_packet() {
        const N: i64 = 200_000;
        let ring = Arc::new(Ring::new(32, 1 << 20));
        let producer = std::thread::spawn({
            let ring = ring.clone();
            move || {
                for i in 0..N {
                    ring.push(video(i, i % 30 == 0));
                    ring.push(audio(i));
                }
                ring.close();
            }
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let got = runtime.block_on(async {
            let mut got = Vec::new();
            while let Some(p) = ring.next().await {
                got.push((p.track, p.pts, p.keyframe));
                if got.len() % 1000 == 0 {
                    tokio::task::yield_now().await;
                }
            }
            got
        });
        producer.join().unwrap();
        let stats = ring.stats();
        let videos: Vec<_> = got.iter().filter(|p| p.0 == Track::Video).collect();
        let audios: Vec<_> = got.iter().filter(|p| p.0 == Track::Audio).collect();
        assert_eq!(videos.len() as u64 + stats.dropped_video, N as u64);
        assert_eq!(audios.len() as u64 + stats.dropped_audio, N as u64);
        for track in [&videos, &audios] {
            assert!(track.windows(2).all(|w| w[0].1 < w[1].1), "out of order");
        }
        assert!(
            videos.first().is_some_and(|v| v.2),
            "first video is a keyframe"
        );
        for w in videos.windows(2) {
            let expected_next = w[0].1 + 1;
            assert!(
                w[1].1 == expected_next || w[1].2,
                "video jumps from {} to {} without a keyframe",
                w[0].1,
                w[1].1
            );
        }
        assert_eq!(stats.queued, 0);
        assert_eq!(stats.queued_bytes, 0);
    }
}
