use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use dstreamy_media::{
    CaptureConfig, CaptureSource, CaptureTarget, FRAME_CHANNEL_CAPACITY, FrameReceiver, MediaError,
    MediaFrame, OPUS_CHANNELS, OPUS_SAMPLE_RATE, OpusEncoder, TargetKind, VideoEncoder,
};
use objc2_core_video::CVPixelBuffer;
use screencapturekit::cg::CGRect;
use screencapturekit::cm::{CMSampleBuffer, CMSampleBufferSCExt, CMTime, SCFrameStatus};
use screencapturekit::shareable_content::{SCDisplay, SCShareableContent, SCWindow};
use screencapturekit::stream::configuration::{PixelFormat, SCStreamConfiguration};
use screencapturekit::stream::content_filter::SCContentFilter;
use screencapturekit::stream::delegate_trait::SCStreamDelegateTrait;
use screencapturekit::stream::output_trait::SCStreamOutputTrait;
use screencapturekit::stream::output_type::SCStreamOutputType;
use screencapturekit::stream::sc_stream::SCStream;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::audio::{AudioPacketizer, SourceBuffer, stereo_i16_from_f32};
use crate::dimensions::CaptureDimensions;
use crate::targets::{TargetId, is_user_window};
use crate::vt::{VtEncoder, VtEncoderConfig};

const AUDIO_BITRATE_BPS: u32 = 128_000;
const AUDIO_GAIN: f32 = 1.0;

type SharedSender = Arc<Mutex<Option<mpsc::Sender<MediaFrame>>>>;

/// ScreenCaptureKit capture (macOS 14+, Screen Recording permission) encoded with VideoToolbox and Opus.
#[derive(Default)]
pub struct MacCaptureSource {
    running: Option<Running>,
    keyframe_requested: Arc<AtomicBool>,
}

struct Running {
    stream: SCStream,
    pipeline: Arc<Pipeline>,
    stopped: bool,
}

impl Running {
    fn shutdown(&mut self) -> Result<(), MediaError> {
        if std::mem::replace(&mut self.stopped, true) {
            return Ok(());
        }
        let stopped = self.stream.stop_capture();
        self.pipeline.finish();
        match self.pipeline.failure() {
            Some(error) => Err(MediaError::Backend(error)),
            None => stopped.map_err(backend),
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

impl MacCaptureSource {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl CaptureSource for MacCaptureSource {
    async fn list_targets(&self) -> Result<Vec<CaptureTarget>, MediaError> {
        blocking(list_targets).await
    }

    async fn start(&mut self, config: CaptureConfig) -> Result<FrameReceiver, MediaError> {
        match &self.running {
            Some(running) if running.pipeline.failure().is_none() => {
                return Err(MediaError::AlreadyRunning);
            }
            Some(_) => {
                if let Some(mut dead) = self.running.take() {
                    let _ = blocking(move || dead.shutdown()).await;
                }
            }
            None => {}
        }
        let keyframe_requested = Arc::clone(&self.keyframe_requested);
        let (running, rx) = blocking(move || start(config, keyframe_requested)).await?;
        self.running = Some(running);
        Ok(rx)
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        let Some(mut running) = self.running.take() else {
            return Ok(());
        };
        blocking(move || running.shutdown()).await
    }

    fn request_keyframe(&self) {
        self.keyframe_requested.store(true, Ordering::Relaxed);
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, MediaError> + Send + 'static,
) -> Result<T, MediaError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| MediaError::Backend(format!("capture task failed: {e}")))?
}

fn backend(e: impl std::fmt::Display) -> MediaError {
    MediaError::Backend(e.to_string())
}

fn shareable_content() -> Result<SCShareableContent, MediaError> {
    // SAFETY: forces CoreGraphics initialisation; avoids CGS_REQUIRE_INIT aborts in CLI processes.
    unsafe { screencapturekit::ffi::sc_initialize_core_graphics() };
    SCShareableContent::create()
        .with_exclude_desktop_windows(false)
        .with_on_screen_windows_only(true)
        .get()
        .map_err(|e| {
            MediaError::Backend(format!(
                "SCShareableContent (Screen Recording permission?): {e}"
            ))
        })
}

fn pixel_size(filter: &SCContentFilter) -> (u32, u32) {
    let rect = filter.content_rect();
    let scale = f64::from(filter.point_pixel_scale());
    (
        (rect.size.width * scale).round() as u32,
        (rect.size.height * scale).round() as u32,
    )
}

fn display_filter(display: &SCDisplay) -> Result<SCContentFilter, MediaError> {
    SCContentFilter::create()
        .with_display(display)
        .with_excluding_windows(&[])
        .build()
        .map_err(backend)
}

fn window_filter(window: &SCWindow) -> Result<SCContentFilter, MediaError> {
    SCContentFilter::create()
        .with_window(window)
        .build()
        .map_err(backend)
}

fn list_targets() -> Result<Vec<CaptureTarget>, MediaError> {
    let content = shareable_content()?;
    let mut targets = Vec::new();
    for display in content.displays() {
        let (width, height) = pixel_size(&display_filter(&display)?);
        targets.push(CaptureTarget {
            id: TargetId::Display(display.display_id()).to_string(),
            kind: TargetKind::Display,
            title: format!("Display {}", display.display_id()),
            app: None,
            width,
            height,
        });
    }
    for window in content.windows() {
        let Some(app) = window.owning_application() else {
            continue;
        };
        let title = window.title().unwrap_or_default();
        let app_name = app.application_name();
        let frame = window.frame();
        if !is_user_window(
            &app_name,
            &app.bundle_identifier(),
            &title,
            frame.size.width,
            frame.size.height,
        ) {
            continue;
        }
        let (width, height) = pixel_size(&window_filter(&window)?);
        targets.push(CaptureTarget {
            id: TargetId::Window(window.window_id()).to_string(),
            kind: TargetKind::Window,
            title,
            app: Some(app_name),
            width,
            height,
        });
    }
    Ok(targets)
}

fn resolve_filter(
    content: &SCShareableContent,
    target: Option<&str>,
) -> Result<SCContentFilter, MediaError> {
    let not_found = |id: &str| MediaError::InvalidConfig(format!("capture target {id} not found"));
    match target.map(str::parse::<TargetId>).transpose()? {
        None => {
            let display = content
                .displays()
                .into_iter()
                .next()
                .ok_or_else(|| not_found("main display"))?;
            display_filter(&display)
        }
        Some(id @ TargetId::Display(display_id)) => {
            let display = content
                .displays()
                .into_iter()
                .find(|d| d.display_id() == display_id)
                .ok_or_else(|| not_found(&id.to_string()))?;
            display_filter(&display)
        }
        Some(id @ TargetId::Window(window_id)) => {
            let window = content
                .windows()
                .into_iter()
                .find(|w| w.window_id() == window_id)
                .ok_or_else(|| not_found(&id.to_string()))?;
            window_filter(&window)
        }
    }
}

fn start(
    config: CaptureConfig,
    keyframe_requested: Arc<AtomicBool>,
) -> Result<(Running, FrameReceiver), MediaError> {
    if config.fps == 0 || config.bitrate_bps == 0 {
        return Err(MediaError::InvalidConfig(format!(
            "fps {} and bitrate {} must be non-zero",
            config.fps, config.bitrate_bps
        )));
    }
    let content = shareable_content()?;
    let filter = resolve_filter(&content, config.target_id.as_deref())?;
    let (source_width, source_height) = pixel_size(&filter);
    let dims = CaptureDimensions::resolve(
        source_width,
        source_height,
        config.width,
        config.height,
        config.crop,
    )?;

    let (tx, rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
    let sender: SharedSender = Arc::new(Mutex::new(Some(tx)));
    let encoder = VtEncoder::with_sink(
        VtEncoderConfig {
            width: dims.output_width,
            height: dims.output_height,
            fps: config.fps,
            bitrate_bps: config.bitrate_bps,
            codec: config.codec,
        },
        {
            let sender = Arc::clone(&sender);
            let keyframe_requested = Arc::clone(&keyframe_requested);
            move |frame| {
                if !send(&sender, MediaFrame::Video(frame)) {
                    keyframe_requested.store(true, Ordering::Relaxed);
                }
            }
        },
    )?;
    keyframe_requested.store(true, Ordering::Relaxed);
    let pipeline = Arc::new(Pipeline {
        sender,
        keyframe_requested,
        clock: Mutex::new(None),
        failure: Mutex::new(None),
        video: Mutex::new(Some(encoder)),
        audio: Mutex::new(AudioPacketizer::new(OpusEncoder::new(AUDIO_BITRATE_BPS)?)),
    });

    let mut stream_config = SCStreamConfiguration::new()
        .with_width(dims.output_width)
        .with_height(dims.output_height)
        .with_pixel_format(PixelFormat::BGRA)
        .with_scales_to_fit(true)
        .with_shows_cursor(false)
        .with_minimum_frame_interval(&CMTime::new(
            1,
            i32::try_from(config.fps).unwrap_or(i32::MAX),
        ))
        .with_captures_audio(config.audio)
        .with_sample_rate(OPUS_SAMPLE_RATE as i32)
        .with_channel_count(OPUS_CHANNELS as i32)
        .with_excludes_current_process_audio(true);
    if let Some(crop) = dims.source_rect {
        let scale = f64::from(filter.point_pixel_scale()).max(1.0);
        stream_config.set_source_rect(CGRect::new(
            f64::from(crop.x) / scale,
            f64::from(crop.y) / scale,
            f64::from(crop.width) / scale,
            f64::from(crop.height) / scale,
        ));
    }

    let mut stream =
        SCStream::new_with_delegate(&filter, &stream_config, StopOnError(Arc::clone(&pipeline)))
            .map_err(backend)?;
    stream
        .add_output_handler(Output(Arc::clone(&pipeline)), SCStreamOutputType::Screen)
        .map_err(backend)?;
    if config.audio {
        stream
            .add_output_handler(Output(Arc::clone(&pipeline)), SCStreamOutputType::Audio)
            .map_err(backend)?;
    }
    stream.start_capture().map_err(backend)?;
    Ok((
        Running {
            stream,
            pipeline,
            stopped: false,
        },
        rx,
    ))
}

/// Returns false when the frame was dropped because the receiver is full.
fn send(sender: &SharedSender, frame: MediaFrame) -> bool {
    let guard = sender.lock().unwrap_or_else(PoisonError::into_inner);
    match guard.as_ref().map(|tx| tx.try_send(frame)) {
        Some(Err(TrySendError::Full(_))) => false,
        Some(Ok(())) | Some(Err(TrySendError::Closed(_))) | None => true,
    }
}

struct Pipeline {
    sender: SharedSender,
    keyframe_requested: Arc<AtomicBool>,
    clock: Mutex<Option<i128>>,
    failure: Mutex<Option<String>>,
    video: Mutex<Option<VtEncoder>>,
    audio: Mutex<AudioPacketizer<OpusEncoder>>,
}

impl Pipeline {
    /// Microseconds since the first sample of either kind.
    fn pts_us(&self, time: CMTime) -> Option<u64> {
        if time.flags & 1 == 0 || time.timescale <= 0 {
            return None;
        }
        let us = i128::from(time.value) * 1_000_000 / i128::from(time.timescale);
        let mut clock = self.clock.lock().unwrap_or_else(PoisonError::into_inner);
        let base = *clock.get_or_insert(us);
        // The other output queue may deliver a slightly earlier first sample.
        Some(u64::try_from(us - base).unwrap_or(0))
    }

    fn on_video(&self, sample: &CMSampleBuffer) {
        if sample.frame_status() != Some(SCFrameStatus::Complete) {
            return;
        }
        let image = sample.image_buffer_ptr_borrowed();
        if image.is_null() {
            return;
        }
        let Some(pts_us) = self.pts_us(sample.presentation_timestamp()) else {
            return;
        };
        // SAFETY: the sample buffer retains its image buffer for as long as `sample` is borrowed.
        let frame = unsafe { &*image.cast::<CVPixelBuffer>() };
        let force = self.keyframe_requested.swap(false, Ordering::Relaxed);
        let mut video = self.video.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(encoder) = video.as_mut()
            && encoder.encode(frame, pts_us, force).is_err()
        {
            self.keyframe_requested.store(true, Ordering::Relaxed);
        }
    }

    fn on_audio(&self, sample: &CMSampleBuffer) {
        let Some(pts_us) = self.pts_us(sample.presentation_timestamp()) else {
            return;
        };
        let Ok(frames) = usize::try_from(sample.num_samples()) else {
            return;
        };
        let Ok(list) = sample.audio_buffer_list() else {
            return;
        };
        let floats: Vec<(usize, Vec<f32>)> = list
            .iter()
            .map(|buffer| {
                let samples = buffer
                    .data()
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f32::from_ne_bytes(*b))
                    .collect();
                (buffer.number_channels as usize, samples)
            })
            .collect();
        let buffers: Vec<SourceBuffer<'_>> = floats
            .iter()
            .map(|(channels, samples)| SourceBuffer {
                channels: *channels,
                samples,
            })
            .collect();
        let Some(pcm) = stereo_i16_from_f32(frames, &buffers, AUDIO_GAIN) else {
            return;
        };
        let packets = self
            .audio
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(&pcm, pts_us);
        for packet in packets {
            send(&self.sender, MediaFrame::Audio(packet));
        }
    }

    fn failure(&self) -> Option<String> {
        self.failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Flushes the encoder and closes the frame channel.
    fn finish(&self) {
        let encoder = self
            .video
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(mut encoder) = encoder {
            let _ = encoder.flush();
        }
        self.sender
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
}

struct Output(Arc<Pipeline>);

impl SCStreamOutputTrait for Output {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, kind: SCStreamOutputType) {
        match kind {
            SCStreamOutputType::Screen => self.0.on_video(&sample),
            SCStreamOutputType::Audio => self.0.on_audio(&sample),
            SCStreamOutputType::Microphone => {}
        }
    }
}

struct StopOnError(Arc<Pipeline>);

impl SCStreamDelegateTrait for StopOnError {
    fn did_stop_with_error(&self, error: screencapturekit::error::SCError) {
        *self
            .0
            .failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(format!("capture stopped: {error}"));
        self.0.finish();
    }
}
