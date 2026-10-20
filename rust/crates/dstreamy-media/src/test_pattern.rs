use std::f32::consts::TAU;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::{
    AudioEncoder, CaptureConfig, CaptureSource, CaptureTarget, FRAME_CHANNEL_CAPACITY,
    FrameReceiver, I420Frame, MediaError, MediaFrame, OPUS_CHANNELS, OPUS_FRAME_DURATION_US,
    OPUS_FRAME_SAMPLES, OPUS_SAMPLE_RATE, OpenH264Encoder, OpusEncoder, TargetKind, VideoCodec,
    VideoEncoder,
};

pub const TEST_PATTERN_TARGET_ID: &str = "test-pattern";

const TONE_HZ: f32 = 440.0;
const TONE_AMPLITUDE: f32 = 0.2;
const AUDIO_BITRATE_BPS: u32 = 128_000;
const SEND_RETRY: Duration = Duration::from_millis(1);

/// Synthetic source: a moving pattern encoded as H.264 Baseline plus a 440 Hz tone as Opus.
pub struct TestPatternSource {
    realtime: bool,
    running: Option<Running>,
    keyframe_requested: Arc<AtomicBool>,
}

struct Running {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl Default for TestPatternSource {
    fn default() -> Self {
        Self::new()
    }
}

impl TestPatternSource {
    /// Paces frames in real time, like a capture device.
    pub fn new() -> Self {
        Self {
            realtime: true,
            running: None,
            keyframe_requested: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Generates frames as fast as the receiver consumes them.
    pub fn unpaced() -> Self {
        let mut source = Self::new();
        source.realtime = false;
        source
    }
}

#[async_trait]
impl CaptureSource for TestPatternSource {
    async fn list_targets(&self) -> Result<Vec<CaptureTarget>, MediaError> {
        Ok(vec![CaptureTarget {
            id: TEST_PATTERN_TARGET_ID.to_owned(),
            kind: TargetKind::Display,
            title: "Test pattern".to_owned(),
            app: None,
            width: 1280,
            height: 720,
        }])
    }

    async fn start(&mut self, config: CaptureConfig) -> Result<FrameReceiver, MediaError> {
        if self.running.is_some() {
            return Err(MediaError::AlreadyRunning);
        }
        if config.codec != VideoCodec::H264 {
            return Err(MediaError::Unsupported(format!(
                "test pattern encodes H.264 only, not {:?}",
                config.codec
            )));
        }
        if config.fps == 0 {
            return Err(MediaError::InvalidConfig("fps must be positive".to_owned()));
        }
        let video = OpenH264Encoder::new(
            config.width,
            config.height,
            config.fps,
            config.bitrate_bps,
            config.keyframe_interval_frames(),
        )?;
        let audio = if config.audio {
            Some(OpusEncoder::new(AUDIO_BITRATE_BPS)?)
        } else {
            None
        };
        let (tx, rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let generator = Generator {
            tx,
            stop: stop.clone(),
            keyframe_requested: self.keyframe_requested.clone(),
            realtime: self.realtime,
            fps: config.fps,
            frame: I420Frame::new(config.width, config.height),
            video,
            audio,
        };
        let thread = std::thread::Builder::new()
            .name("test-pattern".to_owned())
            .spawn(move || generator.run())
            .map_err(|e| MediaError::Backend(e.to_string()))?;
        self.running = Some(Running { stop, thread });
        Ok(rx)
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        let Some(running) = self.running.take() else {
            return Ok(());
        };
        running.stop.store(true, Ordering::Relaxed);
        tokio::task::spawn_blocking(move || running.thread.join())
            .await
            .map_err(|e| MediaError::Backend(e.to_string()))?
            .map_err(|_| MediaError::Backend("test pattern thread panicked".to_owned()))
    }

    fn request_keyframe(&self) {
        self.keyframe_requested.store(true, Ordering::Relaxed);
    }
}

impl Drop for TestPatternSource {
    fn drop(&mut self) {
        if let Some(running) = &self.running {
            running.stop.store(true, Ordering::Relaxed);
        }
    }
}

struct Generator {
    tx: mpsc::Sender<MediaFrame>,
    stop: Arc<AtomicBool>,
    keyframe_requested: Arc<AtomicBool>,
    realtime: bool,
    fps: u32,
    frame: I420Frame,
    video: OpenH264Encoder,
    audio: Option<OpusEncoder>,
}

impl Generator {
    fn run(mut self) {
        let started = Instant::now();
        let mut video_index: u64 = 0;
        let mut audio_index: u64 = 0;
        let mut pcm = vec![0i16; OPUS_FRAME_SAMPLES * OPUS_CHANNELS];
        while !self.stop.load(Ordering::Relaxed) {
            let video_pts = video_index * 1_000_000 / u64::from(self.fps);
            let audio_pts = audio_index * OPUS_FRAME_DURATION_US;
            let audio_due = self.audio.is_some() && audio_pts < video_pts;
            let pts = if audio_due { audio_pts } else { video_pts };
            if self.realtime {
                let due = started + Duration::from_micros(pts);
                if let Some(wait) = due.checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
            }
            let frames = if audio_due {
                fill_tone(&mut pcm, audio_index);
                audio_index += 1;
                self.encode_audio(&pcm, pts)
            } else {
                draw_pattern(&mut self.frame, video_index);
                video_index += 1;
                let force = self.keyframe_requested.swap(false, Ordering::Relaxed);
                self.video
                    .encode(&self.frame, pts, force)
                    .map(|v| v.into_iter().map(MediaFrame::Video).collect())
            };
            // An encoder failure ends the stream; consumers observe it as the channel closing.
            let Ok(frames) = frames else {
                break;
            };
            for frame in frames {
                if !self.send(frame) {
                    return;
                }
            }
        }
    }

    // Polls instead of `blocking_send` so a stalled receiver cannot block `stop`.
    fn send(&self, mut frame: MediaFrame) -> bool {
        loop {
            match self.tx.try_send(frame) {
                Ok(()) => return true,
                Err(TrySendError::Closed(_)) => return false,
                Err(TrySendError::Full(f)) => {
                    if self.stop.load(Ordering::Relaxed) {
                        return false;
                    }
                    frame = f;
                    std::thread::sleep(SEND_RETRY);
                }
            }
        }
    }

    fn encode_audio(&mut self, pcm: &[i16], pts: u64) -> Result<Vec<MediaFrame>, MediaError> {
        match &mut self.audio {
            Some(enc) => Ok(vec![MediaFrame::Audio(enc.encode(pcm, pts)?)]),
            None => Ok(Vec::new()),
        }
    }
}

fn fill_tone(pcm: &mut [i16], frame_index: u64) {
    let first = frame_index * OPUS_FRAME_SAMPLES as u64;
    for (i, pair) in pcm
        .as_chunks_mut::<OPUS_CHANNELS>()
        .0
        .iter_mut()
        .enumerate()
    {
        let n = (first + i as u64) % u64::from(OPUS_SAMPLE_RATE);
        let phase = TAU * TONE_HZ * n as f32 / OPUS_SAMPLE_RATE as f32;
        let sample = (phase.sin() * TONE_AMPLITUDE * f32::from(i16::MAX)) as i16;
        pair.fill(sample);
    }
}

fn draw_pattern(frame: &mut I420Frame, index: u64) {
    let (w, h) = (frame.width as usize, frame.height as usize);
    let box_size = (h / 4).max(2);
    let box_x = (index as usize * 8) % w.saturating_sub(box_size).max(1);
    let box_y = (h - box_size) / 2;
    let shift = index as usize * 2;
    let (y, u, v) = frame.planes_mut();
    for (row, line) in y.chunks_exact_mut(w).enumerate() {
        let in_rows = (box_y..box_y + box_size).contains(&row);
        for (col, px) in line.iter_mut().enumerate() {
            *px = if in_rows && (box_x..box_x + box_size).contains(&col) {
                235
            } else {
                (16 + ((col + shift) * 200 / w) % 200) as u8
            };
        }
    }
    let band = (index / 30) as u8;
    u.fill(128u8.wrapping_add(band.wrapping_mul(16)));
    v.fill(128u8.wrapping_sub(band.wrapping_mul(16)));
}
