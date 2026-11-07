//! Bounded packet queue from `encoded_packet` to the IPC writer, with `dstreamy_media::external`'s drop policy.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use tokio::sync::Notify;

/// About 2 s at 60 fps plus 20 ms audio.
pub const CAPACITY: usize = 256;
/// Four maximum-size producer frames.
pub const MAX_BYTES: usize = 32 << 20;
/// A producer frame's largest payload; bigger packets still go to the writer, which refuses them with their size.
pub const MAX_PACKET: usize = (8 << 20) - 10;

/// Which track a packet belongs to.
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
    /// OBS time base; `pts` and `dts` use ticks of 1 / denominator seconds.
    pub timebase: (i32, i32),
    pub data: Vec<u8>,
    /// Arrival at the encoded-packet callback, used for latency rather than render time.
    pub arrived: Instant,
}

/// A packet over `MAX_PACKET`, kept as its size only: never queued, so no eviction can lose it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Oversized {
    pub track: Track,
    pub len: usize,
}

/// Current queued payload and per-track loss totals since the latest `Ring::reopen`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RingStats {
    pub queued: usize,
    pub queued_bytes: usize,
    pub dropped_video: u64,
    pub dropped_audio: u64,
}

#[derive(Debug)]
struct Inner {
    queue: VecDeque<Packet>,
    bytes: usize,
    awaiting_keyframe: bool,
    dropped_video: u64,
    dropped_audio: u64,
    oversized: Option<Oversized>,
    closed: bool,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            bytes: 0,
            awaiting_keyframe: true,
            dropped_video: 0,
            dropped_audio: 0,
            oversized: None,
            closed: false,
        }
    }
}

impl Inner {
    fn full(&self, capacity: usize, max_bytes: usize, incoming: usize) -> bool {
        self.queue.len() >= capacity || self.bytes + incoming > max_bytes
    }

    fn drop_oldest_gop(&mut self) {
        let first = self.queue.iter().position(|p| p.track == Track::Video);
        let next = first.and_then(|first| {
            self.queue
                .iter()
                .enumerate()
                .skip(first + 1)
                .find(|(_, p)| p.track == Track::Video && p.keyframe)
                .map(|(index, _)| index)
        });
        let before = self.queue.len();
        let mut freed = 0;
        let mut index = 0;
        self.queue.retain(|p| {
            let keep = p.track == Track::Audio || next.is_some_and(|next| index >= next);
            index += 1;
            if !keep {
                freed += p.data.len();
            }
            keep
        });
        self.bytes -= freed;
        self.dropped_video += (before - self.queue.len()) as u64;
        self.awaiting_keyframe = next.is_none();
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

    fn take(&mut self) -> Option<Result<Packet, Oversized>> {
        if let Some(oversized) = self.oversized.take() {
            return Some(Err(oversized));
        }
        let packet = self.queue.pop_front()?;
        self.bytes -= packet.data.len();
        Some(Ok(packet))
    }

    fn admit(&mut self, packet: Packet, capacity: usize, max_bytes: usize) {
        let len = packet.data.len();
        if len > MAX_PACKET {
            self.oversized.get_or_insert(Oversized {
                track: packet.track,
                len,
            });
            return;
        }
        if len > max_bytes {
            match packet.track {
                Track::Video => {
                    self.dropped_video += 1;
                    self.awaiting_keyframe = true;
                }
                Track::Audio => self.dropped_audio += 1,
            }
            return;
        }
        match packet.track {
            Track::Video => {
                if self.awaiting_keyframe && !packet.keyframe {
                    self.dropped_video += 1;
                    return;
                }
                while self.full(capacity, max_bytes, len) {
                    let keyframes = self
                        .queue
                        .iter()
                        .filter(|p| p.track == Track::Video && p.keyframe)
                        .count();
                    if keyframes > 1
                        || (packet.keyframe && self.queue.iter().any(|p| p.track == Track::Video))
                    {
                        self.drop_oldest_gop();
                    } else if !self.drop_oldest_audio() {
                        self.drop_oldest_gop();
                        if self.awaiting_keyframe {
                            self.dropped_video += 1;
                            return;
                        }
                    }
                }
                if self.awaiting_keyframe && !packet.keyframe {
                    self.dropped_video += 1;
                    return;
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
                    let keyframes = self
                        .queue
                        .iter()
                        .filter(|p| p.track == Track::Video && p.keyframe)
                        .count();
                    if keyframes < 2 {
                        self.dropped_audio += 1;
                        return;
                    }
                    self.drop_oldest_gop();
                }
            }
        }
        self.bytes += len;
        self.queue.push_back(packet);
    }
}

/// The bounded packet queue of ADR 0009: `encoded_packet` pushes, the writer thread pops.
#[derive(Debug)]
pub struct Ring {
    capacity: usize,
    max_bytes: usize,
    inner: Mutex<Inner>,
    ready: Notify,
    /// Payload bytes written to the service since the last reopen.
    sent: AtomicU64,
}

impl Default for Ring {
    fn default() -> Self {
        Self::new(CAPACITY, MAX_BYTES)
    }
}

impl Ring {
    /// Bounds queued packets and payload bytes; packet capacity is at least one.
    pub fn new(capacity: usize, max_bytes: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            max_bytes,
            inner: Mutex::new(Inner::default()),
            ready: Notify::new(),
            sent: AtomicU64::new(0),
        }
    }

    /// The writer sent a packet with `bytes` of payload.
    pub fn count_sent(&self, bytes: usize) {
        self.sent.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Payload bytes written to IPC since reopen, excluding wire headers and queued packets.
    pub fn sent_bytes(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
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
            let was_empty = inner.queue.is_empty() && inner.oversized.is_none();
            inner.admit(packet, self.capacity, self.max_bytes);
            was_empty && (!inner.queue.is_empty() || inner.oversized.is_some())
        };
        if wake {
            self.ready.notify_one();
        }
    }

    /// Discards video before the first queued keyframe while retaining audio and accumulated losses.
    pub fn reattach(&self) {
        let mut inner = self.lock();
        let mut ready = false;
        let mut bytes = 0;
        let mut dropped = 0;
        inner.queue.retain(|packet| {
            if packet.track == Track::Audio {
                return true;
            }
            ready |= packet.keyframe;
            if !ready {
                bytes += packet.data.len();
                dropped += 1;
            }
            ready
        });
        inner.bytes -= bytes;
        inner.dropped_video += dropped;
        inner.awaiting_keyframe = !ready;
    }

    /// Latches a size-only error ahead of queued media without retaining its oversized payload.
    pub fn refuse(&self, oversized: Oversized) {
        let mut inner = self.lock();
        if !inner.closed {
            inner.oversized.get_or_insert(oversized);
            self.ready.notify_one();
        }
    }

    /// An oversized packet comes first: it ends the run, so what is queued behind it no longer matters.
    pub fn pop(&self) -> Option<Result<Packet, Oversized>> {
        self.lock().take()
    }

    /// The next packet or oversized marker; `None` once the ring is closed and drained.
    pub async fn next(&self) -> Option<Result<Packet, Oversized>> {
        loop {
            {
                let mut inner = self.lock();
                if let Some(item) = inner.take() {
                    return Some(item);
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
        self.sent.store(0, Ordering::Relaxed);
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
            .map(|p| p.expect("no oversized packet"))
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
    fn waits_for_a_keyframe_after_creation_and_reopen() {
        let ring = Ring::new(8, 1 << 20);
        ring.push(video(0, false));
        ring.push(audio(0));
        ring.push(video(1, true));
        assert_eq!(drain(&ring), [(Track::Audio, 0), (Track::Video, 1)]);

        ring.close();
        ring.reopen();
        ring.push(video(2, false));
        ring.push(audio(1));
        ring.push(video(3, true));
        assert_eq!(drain(&ring), [(Track::Audio, 1), (Track::Video, 3)]);
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
    fn overflow_of_a_partially_consumed_gop_never_skips_a_delta() {
        let ring = Ring::new(3, 1 << 20);
        ring.push(video(0, true));
        ring.pop();
        ring.push(video(1, false));
        ring.push(video(2, true));
        ring.push(video(3, false));
        ring.push(video(4, false));
        ring.push(video(5, false));
        let got = drain(&ring);
        assert!(
            got == [(Track::Video, 2), (Track::Video, 3), (Track::Video, 4)] || got.is_empty(),
            "{got:?}"
        );
    }

    #[test]
    fn overflow_evicts_the_oldest_gop_and_keeps_the_newest_keyframe() {
        let ring = Ring::new(5, 1 << 20);
        ring.push(video(0, true));
        ring.push(video(1, false));
        ring.push(video(2, true));
        ring.push(video(3, false));
        ring.push(video(4, false));
        ring.push(video(5, false));
        assert_eq!(
            drain(&ring),
            [
                (Track::Video, 2),
                (Track::Video, 3),
                (Track::Video, 4),
                (Track::Video, 5)
            ]
        );
        assert_eq!(ring.stats().dropped_video, 2);
    }

    #[test]
    fn a_single_gop_uses_audio_space_then_waits_for_the_next_keyframe() {
        let ring = Ring::new(3, 1 << 20);
        ring.push(video(0, true));
        ring.push(video(1, false));
        ring.push(audio(0));
        ring.push(video(2, false));
        assert_eq!(ring.stats().dropped_audio, 1);
        assert_eq!(ring.stats().dropped_video, 0);
        ring.push(video(3, false));
        assert!(drain(&ring).is_empty());
        ring.push(video(4, true));
        ring.push(video(5, false));
        assert_eq!(drain(&ring), [(Track::Video, 4), (Track::Video, 5)]);
        assert_eq!(ring.stats().dropped_video, 4);
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
    fn audio_does_not_evict_the_only_decodable_gop() {
        let ring = Ring::new(2, 1 << 20);
        ring.push(video(0, true));
        ring.push(video(1, false));
        ring.push(audio(0));
        assert_eq!(drain(&ring), [(Track::Video, 0), (Track::Video, 1)]);
        assert_eq!(ring.stats().dropped_video, 0);
        assert_eq!(ring.stats().dropped_audio, 1);
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
        ring.push(audio(2));
        let mut big = video(3, true);
        big.data = vec![0; 26];
        ring.push(big);
        ring.push(video(4, false));
        assert_eq!(
            drain(&ring),
            [(Track::Audio, 2)],
            "an oversized packet is dropped alone, and video waits for a keyframe"
        );
        assert_eq!(ring.stats().queued_bytes, 0);
    }

    #[test]
    fn a_packet_over_the_frame_limit_survives_later_pushes_and_evictions() {
        let ring = Ring::new(2, 1 << 20);
        let mut big = video(0, true);
        big.data = vec![0; MAX_PACKET + 1];
        ring.push(big);
        for i in 1..20 {
            ring.push(audio(i));
            ring.push(video(i, i % 5 == 0));
        }
        assert_eq!(
            ring.pop(),
            Some(Err(Oversized {
                track: Track::Video,
                len: MAX_PACKET + 1
            }))
        );
        assert!(
            ring.stats().queued_bytes < MAX_PACKET,
            "the marker holds no bytes"
        );
        assert!(
            matches!(ring.pop(), Some(Ok(_))),
            "queued packets follow the marker"
        );
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
                    got.push(p.unwrap().pts);
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

    fn produce(ring: &Ring, n: i64) {
        for i in 0..n {
            ring.push(video(i, i % 30 == 0));
            ring.push(audio(i));
        }
        ring.close();
    }

    fn consume(ring: &Ring) -> Vec<(Track, i64, bool)> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut got = Vec::new();
            while let Some(p) = ring.next().await {
                let p = p.unwrap();
                got.push((p.track, p.pts, p.keyframe));
                if got.len() % 1000 == 0 {
                    tokio::task::yield_now().await;
                }
            }
            got
        })
    }

    fn assert_stream_follows_the_policy(got: &[(Track, i64, bool)], stats: RingStats, n: i64) {
        let videos: Vec<_> = got.iter().filter(|p| p.0 == Track::Video).collect();
        let audios: Vec<_> = got.iter().filter(|p| p.0 == Track::Audio).collect();
        assert_eq!(videos.len() as u64 + stats.dropped_video, n as u64);
        assert_eq!(audios.len() as u64 + stats.dropped_audio, n as u64);
        for track in [&videos, &audios] {
            assert!(track.windows(2).all(|w| w[0].1 < w[1].1), "out of order");
        }
        assert!(
            videos.first().is_none_or(|v| v.2),
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
        assert!(
            stats.dropped_video + stats.dropped_audio > 0,
            "the consumer never fell behind, so the drop policy was not exercised"
        );
    }

    /// One thread pushes as `encoded_packet` does while another drains: no packet is lost or reordered beyond the policy.
    #[test]
    fn two_threads_keep_order_and_account_for_every_packet() {
        const N: i64 = 200_000;
        let ring = Arc::new(Ring::new(32, 1 << 20));
        let producer = std::thread::spawn({
            let ring = ring.clone();
            move || produce(&ring, N)
        });
        let got = consume(&ring);
        producer.join().unwrap();
        assert_stream_follows_the_policy(&got, ring.stats(), N);
    }

    #[test]
    fn a_consumer_that_starts_after_the_producer_closes_follows_the_policy() {
        const N: i64 = 1_000;
        let ring = Ring::new(32, 1 << 20);
        produce(&ring, N);
        let got = consume(&ring);
        assert!(
            got.iter().any(|p| p.0 == Track::Video && p.2),
            "the newest GOP survives"
        );
        assert_stream_follows_the_policy(&got, ring.stats(), N);
    }

    #[test]
    fn sent_bytes_count_what_the_writer_sent_until_a_reopen() {
        let ring = Ring::default();
        assert_eq!(ring.sent_bytes(), 0);
        ring.count_sent(1000);
        ring.count_sent(500);
        assert_eq!(ring.sent_bytes(), 1500);
        ring.reopen();
        assert_eq!(ring.sent_bytes(), 0);
    }
}
