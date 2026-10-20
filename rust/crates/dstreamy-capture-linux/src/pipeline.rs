use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dstreamy_media::{
    AudioEncoder, AudioFrame, CaptureConfig, CropRect, EncodedVideoFrame, FRAME_CHANNEL_CAPACITY,
    FrameReceiver, MediaError, MediaFrame, OPUS_CHANNELS, OPUS_FRAME_DURATION_US,
    OPUS_FRAME_SAMPLES, OPUS_SAMPLE_RATE, OpusEncoder, VideoCodec,
};
use gstreamer as gst;

use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::encoder::{EncoderSettings, EncoderSpec, encoder_candidates};

const AUDIO_BITRATE_BPS: u32 = 128_000;
const SEND_RETRY: Duration = Duration::from_millis(1);
const BUS_POLL: gst::ClockTime = gst::ClockTime::from_mseconds(100);
/// How long a fresh pipeline gets to fail before it is accepted; a static screen may send no frame.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) enum VideoInput {
    PipeWire {
        fd: RawFd,
        node_id: u32,
    },
    #[cfg(test)]
    Test {
        width: u32,
        height: u32,
        frames: i32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioInput {
    /// `pulsesrc` on the default sink's monitor, served by pipewire-pulse.
    PulseMonitor,
    /// `pipewiresrc` capturing the default sink.
    PipeWireSink,
    #[cfg(test)]
    Test { frames: i32 },
}

impl AudioInput {
    /// The system-audio source to use, given the installed element factories.
    pub(crate) fn select<S: AsRef<str>>(available: &[S]) -> Option<Self> {
        let has = |name: &str| available.iter().any(|a| a.as_ref() == name);
        if has("pulsesrc") {
            Some(Self::PulseMonitor)
        } else if has("pipewiresrc") {
            Some(Self::PipeWireSink)
        } else {
            None
        }
    }
}

/// Names of installed element factories that capture or encoder selection may use.
pub(crate) fn available_factories() -> Vec<&'static str> {
    [
        "vah264enc",
        "vaapih264enc",
        "nvh264enc",
        "x264enc",
        "vah265enc",
        "vaapih265enc",
        "nvh265enc",
        "x265enc",
        "pulsesrc",
        "pipewiresrc",
    ]
    .into_iter()
    .filter(|name| gst::ElementFactory::find(name).is_some())
    .collect()
}

pub(crate) fn encoder_settings(config: &CaptureConfig) -> EncoderSettings {
    EncoderSettings {
        codec: config.codec,
        fps: config.fps,
        bitrate_bps: config.bitrate_bps,
    }
}

pub(crate) fn validate(config: &CaptureConfig) -> Result<(), MediaError> {
    if config.fps == 0 || config.fps > 240 {
        return Err(MediaError::InvalidConfig(format!(
            "fps {} is outside 1..=240",
            config.fps
        )));
    }
    if config.width < 2 || config.height < 2 {
        return Err(MediaError::InvalidConfig(format!(
            "output size {}x{} is too small",
            config.width, config.height
        )));
    }
    if let Some(crop) = config.crop
        && (crop.width < 2 || crop.height < 2)
    {
        return Err(MediaError::InvalidConfig(format!(
            "crop {}x{} is too small",
            crop.width, crop.height
        )));
    }
    Ok(())
}

fn even(v: u32) -> i32 {
    i32::try_from(v & !1).unwrap_or(i32::MAX & !1).max(2)
}

/// Caps after `videocrop`: the crop size, made even for 4:2:0 encoders.
pub(crate) fn crop_caps(crop: CropRect) -> gst::Caps {
    gst::Caps::builder("video/x-raw")
        .field("width", even(crop.width))
        .field("height", even(crop.height))
        .build()
}

/// Encoder input caps: fit within the configured size, never upscale, keep the aspect ratio.
pub(crate) fn scale_caps(config: &CaptureConfig, format: &str) -> gst::Caps {
    let fps = i32::try_from(config.fps).unwrap_or(i32::MAX);
    gst::Caps::builder("video/x-raw")
        .field("format", format)
        .field("width", gst::IntRange::with_step(2, even(config.width), 2))
        .field(
            "height",
            gst::IntRange::with_step(2, even(config.height), 2),
        )
        .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
        .field("framerate", gst::Fraction::new(fps, 1))
        .build()
}

fn parsed_caps(codec: VideoCodec) -> gst::Caps {
    let name = match codec {
        VideoCodec::H264 => "video/x-h264",
        VideoCodec::H265 => "video/x-h265",
    };
    gst::Caps::builder(name)
        .field("stream-format", "byte-stream")
        .field("alignment", "au")
        .build()
}

fn audio_caps() -> gst::Caps {
    gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE")
        .field("rate", OPUS_SAMPLE_RATE as i32)
        .field("channels", OPUS_CHANNELS as i32)
        .field("layout", "interleaved")
        .build()
}

fn make(factory: &str) -> Result<gst::Element, MediaError> {
    gst::ElementFactory::make(factory)
        .build()
        .map_err(|e| MediaError::Backend(format!("cannot create {factory}: {e}")))
}

fn capsfilter(caps: gst::Caps) -> Result<gst::Element, MediaError> {
    gst::ElementFactory::make("capsfilter")
        .property("caps", caps)
        .build()
        .map_err(|e| MediaError::Backend(format!("cannot create capsfilter: {e}")))
}

/// Sets a property from its string form; skips it if this element version lacks or rejects it.
fn set_if_present(element: &gst::Element, name: &str, value: &str) -> bool {
    let Some(pspec) = element.find_property(name) else {
        return false;
    };
    if !pspec.flags().contains(glib::ParamFlags::WRITABLE) {
        return false;
    }
    match glib::Value::deserialize(value, pspec.value_type()) {
        Ok(v) if pspec.value_is_valid(&v) => {
            element.set_property_from_value(name, &v);
            true
        }
        _ => false,
    }
}

fn video_source(input: &VideoInput, fps: u32) -> Result<Vec<gst::Element>, MediaError> {
    match input {
        VideoInput::PipeWire { fd, node_id } => {
            let src = make("pipewiresrc")?;
            src.set_property("fd", *fd);
            set_if_present(&src, "path", &node_id.to_string());
            set_if_present(&src, "do-timestamp", "true");
            set_if_present(&src, "keepalive-time", &(1000 / fps.max(1)).to_string());
            Ok(vec![src])
        }
        #[cfg(test)]
        VideoInput::Test {
            width,
            height,
            frames,
        } => {
            let src = make("videotestsrc")?;
            src.set_property("num-buffers", *frames);
            src.set_property("is-live", *frames < 0);
            src.set_property_from_str("pattern", "ball");
            let caps = gst::Caps::builder("video/x-raw")
                .field("width", *width as i32)
                .field("height", *height as i32)
                .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
                .build();
            Ok(vec![src, capsfilter(caps)?])
        }
    }
}

fn audio_source(input: AudioInput) -> Result<gst::Element, MediaError> {
    match input {
        AudioInput::PulseMonitor => {
            let src = make("pulsesrc")?;
            src.set_property("device", "@DEFAULT_MONITOR@");
            Ok(src)
        }
        AudioInput::PipeWireSink => {
            let src = make("pipewiresrc")?;
            let props = gst::Structure::builder("props")
                .field("stream.capture.sink", "true")
                .field("media.category", "Capture")
                .build();
            src.set_property("stream-properties", props);
            Ok(src)
        }
        #[cfg(test)]
        AudioInput::Test { frames } => {
            let src = make("audiotestsrc")?;
            src.set_property("num-buffers", frames);
            Ok(src)
        }
    }
}

/// Where the appsink callbacks deliver frames; closing it closes the receiver.
struct Output {
    tx: Mutex<Option<mpsc::Sender<MediaFrame>>>,
    stop: AtomicBool,
    video_frames: AtomicU64,
    error: Mutex<Option<String>>,
}

impl Output {
    fn new(tx: mpsc::Sender<MediaFrame>) -> Arc<Self> {
        Arc::new(Self {
            tx: Mutex::new(Some(tx)),
            stop: AtomicBool::new(false),
            video_frames: AtomicU64::new(0),
            error: Mutex::new(None),
        })
    }

    fn sender(&self) -> Option<mpsc::Sender<MediaFrame>> {
        self.tx.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn close(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.tx.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    // Polls instead of `blocking_send` so a stalled receiver cannot block pipeline shutdown.
    fn send(&self, mut frame: MediaFrame) -> bool {
        let Some(tx) = self.sender() else {
            return false;
        };
        loop {
            match tx.try_send(frame) {
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
}

fn running_time_us(sample: &gst::Sample, buffer: &gst::BufferRef) -> Option<u64> {
    let pts = buffer.pts();
    let running = sample
        .segment()
        .and_then(|s| s.downcast_ref::<gst::ClockTime>())
        .and_then(|s| pts.and_then(|p| s.to_running_time(p)));
    running.or(pts).map(gst::ClockTime::useconds)
}

fn link_chain(pipeline: &gst::Pipeline, chain: &[gst::Element]) -> Result<(), MediaError> {
    pipeline
        .add_many(chain)
        .map_err(|e| MediaError::Backend(format!("cannot add elements: {e}")))?;
    gst::Element::link_many(chain)
        .map_err(|e| MediaError::Backend(format!("cannot link pipeline: {e}")))
}

fn video_sink(codec: VideoCodec, output: Arc<Output>) -> gst_app::AppSink {
    let mut last_pts_us = 0;
    gst_app::AppSink::builder()
        .sync(false)
        .callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let pts_us = running_time_us(&sample, buffer)
                        .unwrap_or(last_pts_us)
                        .max(last_pts_us);
                    last_pts_us = pts_us;
                    let frame = EncodedVideoFrame {
                        codec,
                        data: map.as_slice().to_vec(),
                        pts_us,
                        keyframe: !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT),
                    };
                    output.video_frames.fetch_add(1, Ordering::Relaxed);
                    if output.send(MediaFrame::Video(frame)) {
                        Ok(gst::FlowSuccess::Ok)
                    } else {
                        Err(gst::FlowError::Flushing)
                    }
                })
                .build(),
        )
        .build()
}

/// Drift between sample-count pts and buffer timestamps beyond which audio pts are rebased.
const AUDIO_RESYNC_US: u64 = 2 * OPUS_FRAME_DURATION_US;

/// Chops PCM into 20 ms Opus frames timestamped by sample count, rebased after gaps.
struct AudioPacketizer {
    encoder: OpusEncoder,
    pending: Vec<i16>,
    /// Pts of the first sample in `pending`.
    next_pts_us: Option<u64>,
}

impl AudioPacketizer {
    fn push(&mut self, pcm: &[u8], running_us: Option<u64>) -> Result<Vec<AudioFrame>, MediaError> {
        let pending_us =
            (self.pending.len() / OPUS_CHANNELS) as u64 * 1_000_000 / u64::from(OPUS_SAMPLE_RATE);
        let mut next = match (self.next_pts_us, running_us) {
            (Some(next), Some(actual))
                if (next + pending_us).abs_diff(actual) > AUDIO_RESYNC_US =>
            {
                actual.saturating_sub(pending_us)
            }
            (Some(next), _) => next,
            (None, actual) => actual.unwrap_or(0),
        };
        self.pending.extend(
            pcm.as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes(*b)),
        );
        let frame_len = OPUS_FRAME_SAMPLES * OPUS_CHANNELS;
        let mut out = Vec::new();
        let mut consumed = 0;
        while self.pending.len() - consumed >= frame_len {
            out.push(
                self.encoder
                    .encode(&self.pending[consumed..consumed + frame_len], next)?,
            );
            consumed += frame_len;
            next += OPUS_FRAME_DURATION_US;
        }
        self.pending.drain(..consumed);
        self.next_pts_us = Some(next);
        Ok(out)
    }
}

fn audio_sink(output: Arc<Output>) -> Result<gst_app::AppSink, MediaError> {
    let mut packetizer = AudioPacketizer {
        encoder: OpusEncoder::new(AUDIO_BITRATE_BPS)?,
        pending: Vec::with_capacity(OPUS_FRAME_SAMPLES * OPUS_CHANNELS * 2),
        next_pts_us: None,
    };
    Ok(gst_app::AppSink::builder()
        .sync(false)
        .caps(&audio_caps())
        .callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let frames = packetizer
                        .push(map.as_slice(), running_time_us(&sample, buffer))
                        .map_err(|_| gst::FlowError::Error)?;
                    for frame in frames {
                        if !output.send(MediaFrame::Audio(frame)) {
                            return Err(gst::FlowError::Flushing);
                        }
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        )
        .build())
}

fn build(
    video: &VideoInput,
    audio: Option<AudioInput>,
    config: &CaptureConfig,
    spec: &EncoderSpec,
    output: &Arc<Output>,
) -> Result<(gst::Pipeline, gst_app::AppSink), MediaError> {
    let pipeline = gst::Pipeline::new();

    let mut chain = video_source(video, config.fps)?;
    chain.push(
        gst::ElementFactory::make("queue")
            .property("max-size-buffers", 3u32)
            .property("max-size-bytes", 0u32)
            .property("max-size-time", 0u64)
            .property_from_str("leaky", "downstream")
            .build()
            .map_err(|e| MediaError::Backend(format!("cannot create queue: {e}")))?,
    );
    chain.push(make("videorate")?);
    chain.push(make("videoconvert")?);
    if let Some(crop) = config.crop {
        let videocrop = make("videocrop")?;
        videocrop.set_property("left", i32::try_from(crop.x).unwrap_or(i32::MAX));
        videocrop.set_property("top", i32::try_from(crop.y).unwrap_or(i32::MAX));
        // -1 lets videocrop derive the far edges from the downstream crop caps.
        videocrop.set_property("right", -1i32);
        videocrop.set_property("bottom", -1i32);
        chain.push(videocrop);
        chain.push(capsfilter(crop_caps(crop))?);
    }
    chain.push(make("videoscale")?);
    chain.push(capsfilter(scale_caps(config, spec.input_format))?);
    let encoder = make(spec.factory)?;
    for (name, value) in &spec.properties {
        set_if_present(&encoder, name, value);
    }
    chain.push(encoder);
    chain.push(capsfilter(spec.output_caps.parse::<gst::Caps>().map_err(
        |e| MediaError::Backend(format!("bad encoder caps: {e}")),
    )?)?);
    let parser = make(match config.codec {
        VideoCodec::H264 => "h264parse",
        VideoCodec::H265 => "h265parse",
    })?;
    parser.set_property("config-interval", -1i32);
    chain.push(parser);
    chain.push(capsfilter(parsed_caps(config.codec))?);
    let sink = video_sink(config.codec, output.clone());
    chain.push(sink.clone().upcast());
    link_chain(&pipeline, &chain)?;

    if let Some(input) = audio {
        let chain = [
            audio_source(input)?,
            make("audioconvert")?,
            make("audioresample")?,
            audio_sink(output.clone())?.upcast(),
        ];
        link_chain(&pipeline, &chain)?;
    }
    Ok((pipeline, sink))
}

fn bus_error(err: &gst::message::Error) -> MediaError {
    let src = err
        .src()
        .map_or_else(|| "pipeline".to_owned(), |s| s.path_string().to_string());
    MediaError::Backend(format!("{src}: {}", err.error()))
}

/// Waits until the first encoded frame arrives, the stream ends, or the pipeline fails.
fn probe(pipeline: &gst::Pipeline, output: &Output) -> Result<(), MediaError> {
    let bus = pipeline.bus().expect("pipeline has a bus");
    let types = [gst::MessageType::Error, gst::MessageType::Eos];
    let deadline = Instant::now() + PROBE_TIMEOUT;
    while Instant::now() < deadline && output.video_frames.load(Ordering::Relaxed) == 0 {
        let Some(msg) = bus.timed_pop_filtered(gst::ClockTime::from_mseconds(10), &types) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Error(err) => return Err(bus_error(err)),
            _ => {
                output.close();
                return Ok(());
            }
        }
    }
    Ok(())
}

/// A running GStreamer capture pipeline feeding a frame channel.
pub(crate) struct Capture {
    pipeline: gst::Pipeline,
    video_sink: gst_app::AppSink,
    output: Arc<Output>,
    bus_thread: Option<JoinHandle<()>>,
    encoder: &'static str,
}

impl Capture {
    /// Blocking: builds and starts the pipeline, falling back through `candidates` on failure.
    pub(crate) fn start(
        video: VideoInput,
        audio: Option<AudioInput>,
        config: &CaptureConfig,
        candidates: &[EncoderSpec],
    ) -> Result<(Self, FrameReceiver), MediaError> {
        if candidates.is_empty() {
            return Err(MediaError::Unsupported(format!(
                "no GStreamer encoder for {:?}",
                config.codec
            )));
        }
        let (tx, rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
        let mut last_err = None;
        for spec in candidates {
            let output = Output::new(tx.clone());
            match Self::try_start(&video, audio, config, spec, output) {
                Ok(capture) => return Ok((capture, rx)),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.expect("at least one candidate was tried"))
    }

    fn try_start(
        video: &VideoInput,
        audio: Option<AudioInput>,
        config: &CaptureConfig,
        spec: &EncoderSpec,
        output: Arc<Output>,
    ) -> Result<Self, MediaError> {
        let (pipeline, video_sink) = build(video, audio, config, spec, &output)?;
        let mut capture = Self {
            pipeline,
            video_sink,
            output,
            bus_thread: None,
            encoder: spec.factory,
        };
        capture
            .pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| MediaError::Backend(format!("{} pipeline: {e}", spec.factory)))?;
        probe(&capture.pipeline, &capture.output)?;
        let bus = capture.pipeline.bus().expect("pipeline has a bus");
        let output = capture.output.clone();
        capture.bus_thread = Some(
            std::thread::Builder::new()
                .name("gst-bus".to_owned())
                .spawn(move || watch_bus(&bus, &output))
                .map_err(|e| MediaError::Backend(e.to_string()))?,
        );
        Ok(capture)
    }

    pub(crate) fn encoder(&self) -> &'static str {
        self.encoder
    }

    /// The pipeline error that ended the stream, if any.
    pub(crate) fn error(&self) -> Option<String> {
        self.output
            .error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn request_keyframe(&self) {
        let event = gstreamer_video::UpstreamForceKeyUnitEvent::builder()
            .all_headers(true)
            .build();
        if let Some(pad) = self.video_sink.static_pad("sink") {
            pad.push_event(event);
        }
    }

    /// Blocking: tears the pipeline down and closes the frame channel.
    pub(crate) fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.output.close();
        // Ignored: a failed transition to Null still releases the elements on drop.
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(thread) = self.bus_thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Closes the frame channel when the pipeline ends or fails, so consumers see the stream end.
fn watch_bus(bus: &gst::Bus, output: &Output) {
    let types = [gst::MessageType::Error, gst::MessageType::Eos];
    while !output.stop.load(Ordering::Relaxed) {
        let Some(msg) = bus.timed_pop_filtered(BUS_POLL, &types) else {
            continue;
        };
        if let gst::MessageView::Error(err) = msg.view() {
            *output.error.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(bus_error(err).to_string());
        }
        output.close();
    }
}

/// All encoder candidates for `config` that are installed on this system.
pub(crate) fn installed_candidates(config: &CaptureConfig) -> Vec<EncoderSpec> {
    encoder_candidates(encoder_settings(config), &available_factories())
}

#[cfg(test)]
mod tests {
    use dstreamy_media::annexb::{self, H264_NAL_IDR, H264_NAL_PPS, H264_NAL_SPS};
    use openh264::decoder::Decoder;
    use openh264::formats::YUVSource;

    use super::*;

    fn init() {
        gst::init().unwrap();
    }

    fn config(width: u32, height: u32) -> CaptureConfig {
        CaptureConfig {
            width,
            height,
            fps: 30,
            bitrate_bps: 1_000_000,
            audio: false,
            ..CaptureConfig::default()
        }
    }

    fn x264(config: &CaptureConfig) -> Vec<EncoderSpec> {
        encoder_candidates(encoder_settings(config), &["x264enc"])
    }

    fn run(
        video: VideoInput,
        audio: Option<AudioInput>,
        config: &CaptureConfig,
    ) -> (&'static str, Vec<MediaFrame>) {
        init();
        let (capture, mut rx) = Capture::start(video, audio, config, &x264(config)).unwrap();
        let encoder = capture.encoder();
        let mut frames = Vec::new();
        while let Some(frame) = rx.blocking_recv() {
            frames.push(frame);
        }
        capture.stop();
        (encoder, frames)
    }

    fn video_frames(frames: &[MediaFrame]) -> Vec<&EncodedVideoFrame> {
        frames
            .iter()
            .filter_map(|f| match f {
                MediaFrame::Video(v) => Some(v),
                MediaFrame::Audio(_) => None,
            })
            .collect()
    }

    fn decode_all(video: &[&EncodedVideoFrame]) -> (usize, (usize, usize)) {
        let mut decoder = Decoder::new().unwrap();
        let mut size = (0, 0);
        for (i, au) in video.iter().enumerate() {
            let picture = decoder
                .decode(&au.data)
                .unwrap_or_else(|e| panic!("AU {i} failed to decode: {e}"))
                .unwrap_or_else(|| panic!("AU {i} produced no picture"));
            size = picture.dimensions();
        }
        (video.len(), size)
    }

    #[test]
    fn crop_caps_round_down_to_even() {
        init();
        let caps = crop_caps(CropRect {
            x: 3,
            y: 5,
            width: 101,
            height: 51,
        });
        let s = caps.structure(0).unwrap();
        assert_eq!(s.get::<i32>("width").unwrap(), 100);
        assert_eq!(s.get::<i32>("height").unwrap(), 50);
    }

    #[test]
    fn scale_caps_bound_size_and_fix_rate_and_aspect() {
        init();
        let caps = scale_caps(&config(1281, 720), "I420");
        let s = caps.structure(0).unwrap();
        assert_eq!(s.get::<&str>("format").unwrap(), "I420");
        let w = s.get::<gst::IntRange<i32>>("width").unwrap();
        assert_eq!((w.min(), w.max(), w.step()), (2, 1280, 2));
        let h = s.get::<gst::IntRange<i32>>("height").unwrap();
        assert_eq!((h.min(), h.max()), (2, 720));
        assert_eq!(
            s.get::<gst::Fraction>("pixel-aspect-ratio").unwrap(),
            gst::Fraction::new(1, 1)
        );
        assert_eq!(
            s.get::<gst::Fraction>("framerate").unwrap(),
            gst::Fraction::new(30, 1)
        );
    }

    #[test]
    fn unknown_or_invalid_encoder_properties_are_skipped() {
        init();
        let enc = make("x264enc").unwrap();
        assert!(!set_if_present(&enc, "no-such-property", "1"));
        assert!(!set_if_present(&enc, "pass", "not-a-mode"));
        assert!(!set_if_present(&enc, "bframes", "9999"));
        assert!(set_if_present(&enc, "pass", "cbr"));
        assert!(set_if_present(&enc, "key-int-max", "60"));
        assert_eq!(enc.property::<u32>("key-int-max"), 60);
    }

    #[test]
    fn audio_pts_follow_sample_count_and_rebase_after_a_gap() {
        let mut p = AudioPacketizer {
            encoder: OpusEncoder::new(AUDIO_BITRATE_BPS).unwrap(),
            pending: Vec::new(),
            next_pts_us: None,
        };
        let half_frame = vec![0u8; OPUS_FRAME_SAMPLES * OPUS_CHANNELS];
        let pts = |frames: Vec<AudioFrame>| frames.iter().map(|f| f.pts_us).collect::<Vec<_>>();
        assert!(pts(p.push(&half_frame, Some(1_000)).unwrap()).is_empty());
        assert_eq!(pts(p.push(&half_frame, Some(11_000)).unwrap()), [1_000]);
        assert!(pts(p.push(&half_frame, Some(21_500)).unwrap()).is_empty());
        assert_eq!(pts(p.push(&half_frame, None).unwrap()), [21_000]);
        assert!(pts(p.push(&half_frame, Some(500_000)).unwrap()).is_empty());
        assert_eq!(pts(p.push(&half_frame, Some(510_000)).unwrap()), [500_000]);
    }

    #[test]
    fn audio_source_prefers_pulse_monitor() {
        assert_eq!(
            AudioInput::select(&["pipewiresrc", "pulsesrc"]),
            Some(AudioInput::PulseMonitor)
        );
        assert_eq!(
            AudioInput::select(&["pipewiresrc"]),
            Some(AudioInput::PipeWireSink)
        );
        assert_eq!(AudioInput::select::<&str>(&[]), None);
    }

    #[test]
    fn rejects_degenerate_configs() {
        assert!(validate(&config(1280, 720)).is_ok());
        assert!(
            validate(&CaptureConfig {
                fps: 0,
                ..config(1280, 720)
            })
            .is_err()
        );
        assert!(validate(&config(1, 720)).is_err());
        let crop = Some(CropRect {
            x: 0,
            y: 0,
            width: 1,
            height: 100,
        });
        assert!(
            validate(&CaptureConfig {
                crop,
                ..config(1280, 720)
            })
            .is_err()
        );
    }

    #[test]
    fn test_source_through_x264_yields_decodable_baseline_aus() {
        let cfg = config(1280, 720);
        let input = VideoInput::Test {
            width: 1280,
            height: 720,
            frames: 60,
        };
        let (encoder, frames) = run(input, None, &cfg);
        assert_eq!(encoder, "x264enc");
        let video = video_frames(&frames);
        assert!(video.len() >= 30, "only {} AUs", video.len());

        let first = video[0];
        assert!(first.keyframe);
        assert_eq!(first.codec, VideoCodec::H264);
        let types: Vec<u8> = annexb::nal_units(&first.data)
            .filter_map(annexb::h264_nal_type)
            .collect();
        for t in [H264_NAL_SPS, H264_NAL_PPS, H264_NAL_IDR] {
            assert!(types.contains(&t), "keyframe NAL types {types:?}");
        }
        for pair in video.windows(2) {
            assert!(pair[1].pts_us > pair[0].pts_us);
        }
        for au in &video {
            assert!(au.data.starts_with(&annexb::START_CODE));
            assert_eq!(au.keyframe, annexb::h264_is_keyframe(&au.data));
        }
        let keyframes = video.iter().filter(|v| v.keyframe).count();
        assert_eq!(keyframes, 1, "one keyframe per fps*2 frames");

        let (decoded, size) = decode_all(&video);
        assert!(decoded >= 30);
        assert_eq!(size, (1280, 720));
    }

    #[test]
    fn scaling_preserves_aspect_without_upscaling() {
        let cfg = config(1280, 720);
        let square = VideoInput::Test {
            width: 1000,
            height: 1000,
            frames: 5,
        };
        let (_, frames) = run(square, None, &cfg);
        assert_eq!(decode_all(&video_frames(&frames)).1, (720, 720));

        let small = VideoInput::Test {
            width: 640,
            height: 360,
            frames: 5,
        };
        let (_, frames) = run(small, None, &cfg);
        assert_eq!(decode_all(&video_frames(&frames)).1, (640, 360));
    }

    #[test]
    fn crop_selects_the_rectangle_before_scaling() {
        let crop = Some(CropRect {
            x: 100,
            y: 50,
            width: 321,
            height: 241,
        });
        let cfg = CaptureConfig {
            crop,
            ..config(1280, 720)
        };
        let input = VideoInput::Test {
            width: 1920,
            height: 1080,
            frames: 5,
        };
        let (_, frames) = run(input, None, &cfg);
        assert_eq!(decode_all(&video_frames(&frames)).1, (320, 240));
    }

    #[test]
    fn audio_branch_emits_contiguous_opus_frames() {
        let cfg = CaptureConfig {
            audio: true,
            ..config(320, 240)
        };
        let input = VideoInput::Test {
            width: 320,
            height: 240,
            frames: 30,
        };
        let (_, frames) = run(input, Some(AudioInput::Test { frames: 50 }), &cfg);
        let audio: Vec<&AudioFrame> = frames
            .iter()
            .filter_map(|f| match f {
                MediaFrame::Audio(a) => Some(a),
                MediaFrame::Video(_) => None,
            })
            .collect();
        assert!(audio.len() >= 20, "only {} Opus frames", audio.len());
        for pair in audio.windows(2) {
            assert_eq!(pair[1].pts_us - pair[0].pts_us, OPUS_FRAME_DURATION_US);
        }
        assert!(audio.iter().all(|a| !a.data.is_empty()));
    }

    fn drain(capture: Capture, mut rx: FrameReceiver) -> Vec<MediaFrame> {
        let mut frames = Vec::new();
        while let Some(frame) = rx.blocking_recv() {
            frames.push(frame);
        }
        capture.stop();
        frames
    }

    #[test]
    fn falls_back_when_an_encoder_is_missing_or_fails_to_negotiate() {
        init();
        let cfg = config(320, 240);
        let input = || VideoInput::Test {
            width: 320,
            height: 240,
            frames: 10,
        };
        let good = x264(&cfg).remove(0);
        let mut missing = good.clone();
        missing.factory = "dstreamy-missing-enc";
        let mut unnegotiable = good.clone();
        unnegotiable.output_caps = "video/x-h264,width=(int)3".into();

        let err = Capture::start(input(), None, &cfg, std::slice::from_ref(&unnegotiable));
        assert!(err.is_err(), "odd width must fail negotiation");

        let candidates = [missing, unnegotiable, good];
        let (capture, rx) = Capture::start(input(), None, &cfg, &candidates).unwrap();
        let frames = drain(capture, rx);
        let (decoded, size) = decode_all(&video_frames(&frames));
        assert!(decoded > 0);
        assert_eq!(size, (320, 240));
    }

    #[test]
    fn keyframe_request_forces_an_early_idr_and_stop_closes_the_channel() {
        init();
        let cfg = config(320, 240);
        let input = VideoInput::Test {
            width: 320,
            height: 240,
            frames: -1,
        };
        let (capture, mut rx) = Capture::start(input, None, &cfg, &x264(&cfg)).unwrap();
        let mut next_video = || loop {
            match rx.blocking_recv().expect("stream ended early") {
                MediaFrame::Video(v) => return v,
                MediaFrame::Audio(_) => {}
            }
        };
        assert!(next_video().keyframe);
        for _ in 0..5 {
            assert!(!next_video().keyframe);
        }
        capture.request_keyframe();
        let forced = (0..20).any(|_| next_video().keyframe);
        assert!(forced, "no keyframe within 20 frames of the request");
        capture.stop();
        while rx.blocking_recv().is_some() {}
    }
}
