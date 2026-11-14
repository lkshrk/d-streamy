use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dstreamy_media::{
    AudioActivity, AudioClock, CaptureConfig, CropRect, EncodedVideoFrame, FRAME_CHANNEL_CAPACITY,
    FrameReceiver, MediaError, MediaFrame, VideoCodec, send_until_stopped,
};
use gstreamer as gst;

use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use tokio::sync::mpsc;

use crate::encoder::EncoderSpec;

use audio_branch::{AudioBranch, audio_generation, audio_start_error};
use bus::watch_bus;
use caps::{check_crop_fits, crop_caps, parsed_caps, scale_caps};
use elements::{capsfilter, link_chain, make, set_if_present};

mod audio_branch;
mod bus;
mod caps;
mod elements;
#[cfg(test)]
mod test_support;

pub(crate) use audio_branch::AudioOptions;
pub(crate) use caps::validate;
pub(crate) use elements::no_encoder;

/// How long a fresh pipeline gets to fail before it is accepted; a static screen may send no frame.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const VIDEO_SOURCE_NAME: &str = "dstreamy-video-source";
const VIDEO_QUEUE_NAME: &str = "dstreamy-video-queue";
/// Told when the compositor ends the share: the window closed, "stop sharing", or a revoked permission.
pub(crate) const SOURCE_GONE: &str =
    "the shared window or screen went away; start again to pick a new one";

pub(crate) enum VideoInput {
    PipeWire {
        fd: RawFd,
        node_id: u32,
        size: Option<(u32, u32)>,
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
    /// `pulsesrc`, served by pipewire-pulse; captures the default sink's monitor unless a device is chosen.
    PulseMonitor,
    /// `pipewiresrc`; captures the default sink unless a device is chosen.
    PipeWireSink,
    #[cfg(test)]
    Test { frames: i32 },
    /// Blocks in READY to PAUSED until its source pad is flushed.
    #[cfg(test)]
    Blocking,
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

fn video_source(input: &VideoInput, fps: u32) -> Result<Vec<gst::Element>, MediaError> {
    match input {
        VideoInput::PipeWire { fd, node_id, .. } => {
            let src = make("pipewiresrc")?;
            src.set_property("fd", *fd);
            set_if_present(&src, "path", &node_id.to_string());
            set_if_present(&src, "do-timestamp", "true");
            set_if_present(&src, "keepalive-time", &(1000 / fps.max(1)).to_string());
            // Without it the source resends the last frame forever once the compositor drops the link.
            set_if_present(&src, "on-disconnect", "error");
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

/// Where the appsink callbacks deliver frames; closing it closes the receiver.
struct Output {
    tx: Mutex<Option<mpsc::Sender<MediaFrame>>>,
    stop: AtomicBool,
    video_frames: AtomicU64,
    /// Frames the leaky queue threw away because conversion or encoding fell behind.
    dropped_frames: AtomicU64,
    audio_buffers: AtomicU64,
    /// The audio branch generation whose stream the server moved off the chosen device, and where to.
    moved: Mutex<Option<(u64, String)>>,
    audio_clock: Arc<Mutex<AudioClock>>,
    error: Mutex<Option<String>>,
}

impl Output {
    fn new(tx: mpsc::Sender<MediaFrame>) -> Arc<Self> {
        Arc::new(Self {
            tx: Mutex::new(Some(tx)),
            stop: AtomicBool::new(false),
            video_frames: AtomicU64::new(0),
            dropped_frames: AtomicU64::new(0),
            audio_buffers: AtomicU64::new(0),
            moved: Mutex::new(None),
            audio_clock: Arc::default(),
            error: Mutex::new(None),
        })
    }

    fn sender(&self) -> Option<mpsc::Sender<MediaFrame>> {
        lock(&self.tx).clone()
    }

    fn close(&self) {
        self.stop.store(true, Ordering::Relaxed);
        lock(&self.tx).take();
    }

    /// Ends the capture with `message` as its error; nothing once it is stopped.
    fn fail(&self, message: String) {
        if self.stop.load(Ordering::Relaxed) {
            return;
        }
        tracing::error!("{message}");
        *lock(&self.error) = Some(message);
        self.close();
    }

    fn send(&self, frame: MediaFrame) -> bool {
        self.sender()
            .is_some_and(|tx| send_until_stopped(&tx, frame, &self.stop))
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

struct Built {
    pipeline: gst::Pipeline,
    video_source: gst::Element,
    /// Only used to explain a failure, never to reject a crop up front: the portal may report logical size.
    size_hint: Option<(u32, u32)>,
    video_sink: gst_app::AppSink,
    audio: Option<AudioBranch>,
}

/// The frame size `source` can produce, if it is a single fixed size regardless of downstream.
fn fixed_source_size(source: &gst::Element) -> Option<(u32, u32)> {
    let caps = source.static_pad("src")?.query_caps(None);
    let s = caps.structure(0)?;
    let width = u32::try_from(s.get::<i32>("width").ok()?).ok()?;
    let height = u32::try_from(s.get::<i32>("height").ok()?).ok()?;
    Some((width, height))
}

fn build(
    video: &VideoInput,
    audio: Option<AudioInput>,
    config: &CaptureConfig,
    spec: &EncoderSpec,
    output: &Arc<Output>,
    activity: Option<&AudioActivity>,
    options: &AudioOptions,
) -> Result<Built, MediaError> {
    let pipeline = gst::Pipeline::new();

    let mut chain = video_source(video, config.fps)?;
    chain[0].set_property("name", VIDEO_SOURCE_NAME);
    let video_source = chain.last().expect("video source has elements").clone();
    let queue = gst::ElementFactory::make("queue")
        .name(VIDEO_QUEUE_NAME)
        .property("max-size-buffers", 3u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 0u64)
        .property_from_str("leaky", "downstream")
        .build()
        .map_err(|e| MediaError::Backend(format!("cannot create queue: {e}")))?;
    // A leaky queue drops one buffer per overrun; nothing else reports those drops.
    queue.connect("overrun", false, {
        let output = Arc::clone(output);
        move |_| {
            output.dropped_frames.fetch_add(1, Ordering::Relaxed);
            None
        }
    });
    chain.push(queue);
    chain.push(make("videorate")?);
    // Cropping first keeps the conversion to the rest of the frame and works on the compositor's RGB.
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
    chain.push(make("videoconvert")?);
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
    let size_hint = match video {
        VideoInput::PipeWire { size, .. } => *size,
        #[cfg(test)]
        VideoInput::Test { .. } => None,
    };
    if let Err(e) = link_chain(&pipeline, &chain) {
        // A crop larger than fixed source caps already fails here, at link time.
        if let Some(crop) = config.crop
            && let Some((width, height)) = fixed_source_size(&video_source).or(size_hint)
        {
            check_crop_fits(crop, width, height)?;
        }
        return Err(e);
    }

    let audio = audio.map(|input| AudioBranch::new(input, config, activity, options.clone()));
    Ok(Built {
        pipeline,
        video_source,
        size_hint,
        video_sink: sink,
        audio,
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn init_gstreamer() -> Result<(), MediaError> {
    gst::init().map_err(|e| MediaError::Backend(format!("GStreamer init: {e}")))
}

/// The element path a message came from, or "pipeline" when it has no source.
fn source_path(src: Option<&gst::Object>) -> String {
    src.map_or_else(|| "pipeline".to_owned(), |s| s.path_string().to_string())
}

fn bus_error_text(err: &gst::message::Error) -> String {
    format!("{}: {}", source_path(err.src()), err.error())
}

/// `pipewiresrc` with `on-disconnect=error` posts this once the compositor destroyed the stream's link.
fn video_gone(err: &gst::message::Error) -> bool {
    err.src().is_some_and(|s| s.name() == VIDEO_SOURCE_NAME)
        && err.error().matches(gst::ResourceError::NotFound)
}

/// Why a candidate pipeline failed to start: only `Retry` moves on to the next encoder.
enum StartError {
    Retry(MediaError),
    Fatal(MediaError),
}

fn classify(err: &gst::message::Error, built: &Built, crop: Option<CropRect>) -> StartError {
    if let Some(crop) = crop
        && let Some((width, height)) = fixed_source_size(&built.video_source).or(built.size_hint)
        && let Err(e) = check_crop_fits(crop, width, height)
    {
        return StartError::Fatal(e);
    }
    if video_gone(err) {
        return StartError::Fatal(MediaError::Backend(SOURCE_GONE.to_owned()));
    }
    if let Some(audio) = built
        .audio
        .as_ref()
        .filter(|_| audio_generation(err).is_some())
    {
        StartError::Fatal(audio_start_error(
            audio.device.as_deref(),
            &bus_error_text(err),
        ))
    } else {
        StartError::Retry(MediaError::Backend(bus_error_text(err)))
    }
}

/// The error a failed state change posted, audio first: an unusable audio device is not the encoder's fault.
fn posted_error(built: &Built, crop: Option<CropRect>) -> Option<StartError> {
    let bus = built.pipeline.bus().expect("pipeline has a bus");
    let mut errors = Vec::new();
    while let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error]) {
        errors.push(msg);
    }
    let error = |m: &gst::Message| match m.view() {
        gst::MessageView::Error(err) => Some(classify(err, built, crop)),
        _ => None,
    };
    let audio = errors.iter().find(|m| match m.view() {
        gst::MessageView::Error(err) => audio_generation(err).is_some(),
        _ => false,
    });
    audio.or(errors.first()).and_then(error)
}

/// Waits until the first encoded frame and audio buffer arrive, the stream ends, or the pipeline fails.
fn probe(built: &Built, output: &Output, crop: Option<CropRect>) -> Result<(), StartError> {
    let bus = built.pipeline.bus().expect("pipeline has a bus");
    let types = [gst::MessageType::Error, gst::MessageType::Eos];
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let started = || {
        output.video_frames.load(Ordering::Relaxed) > 0
            && (built.audio.is_none() || output.audio_buffers.load(Ordering::Relaxed) > 0)
    };
    while Instant::now() < deadline && !started() {
        if output.audio_buffers.load(Ordering::Relaxed) == 0
            && let Some(audio) = &built.audio
            && let Some((_, current)) = lock(&output.moved).take()
        {
            return Err(StartError::Fatal(audio_start_error(
                audio.device.as_deref(),
                &format!("the server put the stream on {current:?}"),
            )));
        }
        let Some(msg) = bus.timed_pop_filtered(gst::ClockTime::from_mseconds(10), &types) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Error(err) => {
                return Err(classify(err, built, crop));
            }
            _ if started() => {
                output.close();
                return Ok(());
            }
            _ => {
                return Err(StartError::Fatal(MediaError::Backend(
                    "the capture ended before the first frame".to_owned(),
                )));
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
    #[cfg(test)]
    pub(crate) fn start(
        video: VideoInput,
        audio: Option<AudioInput>,
        config: &CaptureConfig,
        candidates: &[EncoderSpec],
    ) -> Result<(Self, FrameReceiver), MediaError> {
        Self::start_with(
            video,
            audio,
            config,
            candidates,
            None,
            &AudioOptions::default(),
        )
    }

    /// Blocking: builds and starts the pipeline, falling back through `candidates` on failure; `activity` hears every captured audio sample.
    pub(crate) fn start_with(
        video: VideoInput,
        audio: Option<AudioInput>,
        config: &CaptureConfig,
        candidates: &[EncoderSpec],
        activity: Option<&AudioActivity>,
        options: &AudioOptions,
    ) -> Result<(Self, FrameReceiver), MediaError> {
        if candidates.is_empty() {
            return Err(no_encoder(config.codec));
        }
        let mut last_err = None;
        for spec in candidates {
            // A channel per candidate, so frames of a failed attempt never reach the receiver.
            let (tx, rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
            match Self::try_start(
                &video,
                audio,
                config,
                spec,
                Output::new(tx),
                activity,
                options,
            ) {
                Ok(capture) => return Ok((capture, rx)),
                Err(StartError::Fatal(e)) => return Err(e),
                Err(StartError::Retry(e)) => last_err = Some(e),
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
        activity: Option<&AudioActivity>,
        options: &AudioOptions,
    ) -> Result<Self, StartError> {
        let mut built =
            build(video, audio, config, spec, &output, activity, options).map_err(|e| match e {
                MediaError::InvalidConfig(_) => StartError::Fatal(e),
                e => StartError::Retry(e),
            })?;
        let mut capture = Self {
            pipeline: built.pipeline.clone(),
            video_sink: built.video_sink.clone(),
            output,
            bus_thread: None,
            encoder: spec.factory,
        };
        if let Err(e) = capture.pipeline.set_state(gst::State::Playing) {
            return Err(posted_error(&built, config.crop).unwrap_or_else(|| {
                StartError::Retry(MediaError::Backend(format!(
                    "{} pipeline: {e}",
                    spec.factory
                )))
            }));
        }
        if let Some(branch) = built.audio.as_mut() {
            branch
                .start(&capture.pipeline, &capture.output)
                .map_err(StartError::Fatal)?;
        }
        probe(&built, &capture.output, config.crop)?;
        let pipeline = capture.pipeline.clone();
        let output = capture.output.clone();
        let audio = built.audio;
        capture.bus_thread = Some(
            std::thread::Builder::new()
                .name("gst-bus".to_owned())
                .spawn(move || watch_bus(&pipeline, &output, audio))
                .map_err(|e| StartError::Retry(MediaError::Backend(e.to_string())))?,
        );
        Ok(capture)
    }

    pub(crate) fn encoder(&self) -> &'static str {
        self.encoder
    }

    #[cfg(test)]
    pub(crate) fn dropped_frames(&self) -> u64 {
        self.output.dropped_frames.load(Ordering::Relaxed)
    }

    /// The pipeline error that ended the stream, if any.
    pub(crate) fn error(&self) -> Option<String> {
        lock(&self.output.error).clone()
    }

    /// A handle that ends the capture from outside the pipeline, e.g. when the portal session closes.
    pub(crate) fn ender(&self) -> Ender {
        Ender(self.output.clone())
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
    pub(crate) fn stop(mut self) -> Result<(), MediaError> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Result<(), MediaError> {
        self.output.close();
        // Joined first so a rebuild in progress cannot leave playing elements in a stopped pipeline.
        if let Some(thread) = self.bus_thread.take() {
            let _ = thread.join();
        }
        self.pipeline
            .set_state(gst::State::Null)
            .map_err(|e| MediaError::Backend(format!("stopping capture pipeline: {e}")))?;
        let (result, state, _) = self.pipeline.state(gst::ClockTime::ZERO);
        if result.is_err() || state != gst::State::Null {
            return Err(MediaError::Backend(
                "capture pipeline did not reach NULL; source loss counts remain a lower bound"
                    .into(),
            ));
        }
        Ok(())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

pub(crate) struct Ender(Arc<Output>);

impl Ender {
    pub(crate) fn dropped_frames(&self) -> u64 {
        self.0.dropped_frames.load(Ordering::Relaxed)
    }

    /// Closes the frame channel with `message` as the capture's error; nothing once it is stopped.
    pub(crate) fn fail(&self, message: &str) {
        self.0.fail(message.to_owned());
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use dstreamy_media::annexb::{self, H264_NAL_IDR, H264_NAL_PPS, H264_NAL_SPS};
    use dstreamy_media::{AudioFrame, OPUS_FRAME_DURATION_US};
    use openh264::decoder::Decoder;
    use openh264::formats::YUVSource;

    use super::*;

    use super::audio_branch::{AUDIO_ELEMENT_PREFIX, AUDIO_RETRY_FIRST, AUDIO_RETRY_MAX};
    use super::test_support::*;
    use crate::encoder::{available_factories, encoder_candidates, encoder_settings};

    fn live_test_video() -> VideoInput {
        VideoInput::Test {
            width: 320,
            height: 240,
            frames: -1,
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
        let (capture, rx) = Capture::start(video, audio, config, &x264(config)).unwrap();
        let encoder = capture.encoder();
        (encoder, drain(capture, rx))
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
    fn test_source_through_x264_yields_decodable_baseline_aus() {
        let cfg = config(1280, 720);
        let input = VideoInput::Test {
            width: 1280,
            height: 720,
            frames: 121,
        };
        let (encoder, frames) = run(input, None, &cfg);
        assert_eq!(encoder, "x264enc");
        let video = video_frames(&frames);
        let max_gop = cfg.fps as usize * 2;
        assert!(video.len() > max_gop, "only {} AUs", video.len());

        assert!(video[0].keyframe);
        assert_eq!(video[0].codec, VideoCodec::H264);
        let keyframes: Vec<_> = video.iter().filter(|v| v.keyframe).collect();
        assert!(!keyframes.is_empty());
        for keyframe in keyframes {
            let types: Vec<u8> = annexb::nal_units(&keyframe.data)
                .filter_map(annexb::h264_nal_type)
                .collect();
            for t in [H264_NAL_SPS, H264_NAL_PPS, H264_NAL_IDR] {
                assert!(types.contains(&t), "keyframe NAL types {types:?}");
            }
        }
        for pair in video.windows(2) {
            assert!(pair[1].pts_us > pair[0].pts_us);
        }
        for au in &video {
            assert!(au.data.starts_with(&annexb::START_CODE));
            assert_eq!(au.keyframe, annexb::h264_is_keyframe(&au.data));
        }
        let keyframe_positions: Vec<_> = video
            .iter()
            .enumerate()
            .filter_map(|(i, frame)| frame.keyframe.then_some(i))
            .collect();
        assert!(keyframe_positions.len() >= 2);
        assert!(
            keyframe_positions
                .windows(2)
                .all(|pair| pair[1] - pair[0] <= max_gop)
        );
        assert!(video.len() - 1 - keyframe_positions.last().unwrap() <= max_gop);

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

    /// How long the audio had been silent when a tone capture after `quiet` of silence ended.
    fn silence_after_tone(gain: f32, quiet: Duration) -> (Duration, Duration) {
        init();
        let cfg = CaptureConfig {
            audio: true,
            audio_gain: gain,
            ..config(320, 240)
        };
        let video = VideoInput::Test {
            width: 320,
            height: 240,
            frames: 30,
        };
        let activity = AudioActivity::default();
        activity.started();
        let begun = Instant::now();
        std::thread::sleep(quiet);
        let audio = Some(AudioInput::Test { frames: 50 });
        let (capture, rx) = Capture::start_with(
            video,
            audio,
            &cfg,
            &x264(&cfg),
            Some(&activity),
            &AudioOptions::default(),
        )
        .unwrap();
        drain(capture, rx);
        (activity.silent_for().unwrap(), begun.elapsed())
    }

    #[test]
    fn audio_activity_hears_the_tone_but_not_a_muted_one() {
        let quiet = Duration::from_millis(300);
        let (silent, elapsed) = silence_after_tone(1.0, quiet);
        assert!(silent + quiet < elapsed, "{silent:?} of {elapsed:?}");
        let (silent, elapsed) = silence_after_tone(0.0, quiet);
        assert!(
            silent + Duration::from_millis(50) >= elapsed,
            "{silent:?} of {elapsed:?}"
        );
    }

    #[test]
    fn a_failed_audio_source_is_rebuilt_while_video_keeps_streaming() {
        init();
        let cfg = CaptureConfig {
            audio: true,
            audio_source: Some("pipeweaver_stream".into()),
            ..config(320, 240)
        };
        let video = live_test_video();
        let activity = AudioActivity::default();
        activity.started();
        let audio = Some(AudioInput::Test { frames: -1 });
        let (capture, mut rx) = Capture::start_with(
            video,
            audio,
            &cfg,
            &x264(&cfg),
            Some(&activity),
            &AudioOptions::default(),
        )
        .unwrap();
        let (video, audio) = recv_audio(&mut rx, Duration::from_millis(300));
        assert!(
            video > 0 && audio > 0,
            "{video} video, {audio} audio before"
        );
        let first = capture
            .pipeline
            .by_name(&format!("{AUDIO_ELEMENT_PREFIX}1-0"))
            .unwrap();
        first
            .post_message(
                gst::message::Error::builder(gst::ResourceError::NotFound, "target not found")
                    .src(&first)
                    .build(),
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !activity.is_lost() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(activity.is_lost(), "the loss was not reported");
        let (video, _) = recv_audio(&mut rx, AUDIO_RETRY_FIRST / 2);
        assert!(video > 0, "video stopped with the audio");
        let (video, audio) = recv_audio(&mut rx, AUDIO_RETRY_FIRST * 2);
        assert!(video > 0 && audio > 0, "{video} video, {audio} audio after");
        assert!(!activity.is_lost());
        assert!(
            capture
                .pipeline
                .by_name(&format!("{AUDIO_ELEMENT_PREFIX}1-0"))
                .is_none()
        );
        assert!(
            capture
                .pipeline
                .by_name(&format!("{AUDIO_ELEMENT_PREFIX}2-0"))
                .is_some()
        );
        post_error_as(
            &capture.pipeline,
            &format!("{AUDIO_ELEMENT_PREFIX}1-0"),
            gst::ResourceError::NotFound,
        );
        let (video, audio) = recv_audio(&mut rx, AUDIO_RETRY_FIRST * 2);
        assert!(
            video > 0 && audio > 0,
            "{video} video, {audio} audio after a stale error"
        );
        assert!(
            !activity.is_lost(),
            "a replaced source's error was taken for a new loss"
        );
        assert!(
            capture
                .pipeline
                .by_name(&format!("{AUDIO_ELEMENT_PREFIX}2-0"))
                .is_some()
        );
        assert_eq!(capture.error(), None);
        capture.stop().unwrap();
    }

    fn live_capture_with_test_audio() -> (Capture, FrameReceiver, AudioActivity) {
        init();
        let cfg = CaptureConfig {
            audio: true,
            ..config(320, 240)
        };
        let video = live_test_video();
        let activity = AudioActivity::default();
        activity.started();
        let audio = Some(AudioInput::Test { frames: -1 });
        let (capture, rx) = Capture::start_with(
            video,
            audio,
            &cfg,
            &x264(&cfg),
            Some(&activity),
            &AudioOptions::default(),
        )
        .unwrap();
        (capture, rx, activity)
    }

    #[test]
    fn audio_faults_other_than_a_lost_device_end_the_capture_with_their_cause() {
        let (capture, mut rx, activity) = live_capture_with_test_audio();
        post_error_as(
            &capture.pipeline,
            &format!("{AUDIO_ELEMENT_PREFIX}1-4"),
            gst::StreamError::Failed,
        );
        wait_closed(&mut rx);
        let error = capture.error().expect("the capture ended without an error");
        assert!(error.contains("audio capture failed: "), "{error}");
        assert!(error.contains("injected"), "{error}");
        assert!(!activity.is_lost());
        capture.stop().unwrap();
    }

    pub(crate) fn live_video_capture() -> (Capture, FrameReceiver) {
        init();
        let cfg = config(320, 240);
        let input = live_test_video();
        Capture::start(input, None, &cfg, &x264(&cfg)).unwrap()
    }

    fn wait_closed(rx: &mut FrameReceiver) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !rx.is_closed() && Instant::now() < deadline {
            while rx.try_recv().is_ok() {}
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(rx.is_closed(), "the capture did not end");
    }

    #[test]
    fn a_video_source_whose_link_was_destroyed_ends_the_capture_saying_the_share_went_away() {
        let (capture, mut rx) = live_video_capture();
        post_error_as(
            &capture.pipeline,
            VIDEO_SOURCE_NAME,
            gst::ResourceError::NotFound,
        );
        wait_closed(&mut rx);
        assert_eq!(capture.error().as_deref(), Some(SOURCE_GONE));
        capture.stop().unwrap();
    }

    #[test]
    fn other_pipeline_errors_keep_their_own_text() {
        let (capture, mut rx) = live_video_capture();
        post_error_as(&capture.pipeline, "enc", gst::ResourceError::NotFound);
        wait_closed(&mut rx);
        let error = capture.error().expect("the capture ended without an error");
        assert!(error.contains("injected"), "{error}");
        assert!(!error.contains(SOURCE_GONE), "{error}");
        capture.stop().unwrap();
    }

    #[test]
    fn an_ender_closes_the_channel_with_the_first_message_it_is_given() {
        let (capture, mut rx) = live_video_capture();
        let ender = capture.ender();
        ender.fail("the portal session closed");
        ender.fail("too late");
        wait_closed(&mut rx);
        assert_eq!(
            capture.error().as_deref(),
            Some("the portal session closed")
        );
        capture.stop().unwrap();
    }

    #[test]
    fn queue_overruns_count_as_dropped_frames() {
        let (capture, _rx) = live_video_capture();
        assert_eq!(capture.dropped_frames(), 0);
        let queue = capture.pipeline.by_name(VIDEO_QUEUE_NAME).unwrap();
        queue.emit_by_name::<()>("overrun", &[]);
        queue.emit_by_name::<()>("overrun", &[]);
        assert_eq!(capture.dropped_frames(), 2);
        capture.stop().unwrap();
    }

    #[tokio::test]
    async fn retained_source_counters_include_overruns_during_shutdown_and_survive_failed_start() {
        use dstreamy_media::CaptureSource;
        let (mut capture, mut rx) = live_video_capture();
        let queue = capture.pipeline.by_name(VIDEO_QUEUE_NAME).unwrap();
        let old_bus = capture.bus_thread.take().unwrap();
        capture.bus_thread = Some(std::thread::spawn(move || {
            old_bus.join().unwrap();
            queue.emit_by_name::<()>("overrun", &[]);
            queue.emit_by_name::<()>("overrun", &[]);
        }));
        let ender = capture.ender();
        let mut source = crate::source::LinuxCaptureSource::new();
        source.retain_drops(ender);
        let stopping = tokio::task::spawn_blocking(move || capture.stop());
        while rx.recv().await.is_some() {}
        stopping.await.unwrap().unwrap();
        assert_eq!(source.dropped_frames(), 2);
        source.stop().await.unwrap();
        assert_eq!(source.dropped_frames(), 2);
        let failed = source
            .start(CaptureConfig {
                target_id: Some("missing".into()),
                ..CaptureConfig::default()
            })
            .await;
        assert!(failed.is_err());
        assert_eq!(source.capture_drops().video_frames, 2);
    }

    #[test]
    fn a_failed_null_transition_is_reported_as_incomplete_teardown() {
        use glib::subclass::types::ObjectSubclassIsExt;
        init();
        let source = glib::Object::new::<BlockingSrc>();
        let pipeline = gst::Pipeline::new();
        pipeline.add(&source).unwrap();
        pipeline.set_state(gst::State::Ready).unwrap();
        source.imp().fail_stop.store(true, Ordering::Relaxed);
        let (tx, _rx) = mpsc::channel(1);
        let capture = Capture {
            pipeline: pipeline.clone(),
            video_sink: gst_app::AppSink::builder().build(),
            output: Output::new(tx),
            bus_thread: None,
            encoder: "none",
        };
        let error = capture.stop().unwrap_err().to_string();
        assert!(error.contains("stopping capture pipeline"), "{error}");
        source.imp().fail_stop.store(false, Ordering::Relaxed);
        pipeline.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn a_stream_that_ends_before_its_first_frame_fails_the_start() {
        init();
        let cfg = config(320, 240);
        let input = VideoInput::Test {
            width: 320,
            height: 240,
            frames: 0,
        };
        match Capture::start(input, None, &cfg, &x264(&cfg)) {
            Err(MediaError::Backend(msg)) => {
                assert!(msg.contains("before the first frame"), "{msg}")
            }
            Err(e) => panic!("unexpected error {e}"),
            Ok(_) => panic!("an empty stream started"),
        }
    }

    #[test]
    fn an_unusable_chosen_device_fails_the_start_with_the_device_not_the_encoder() {
        init();
        for (input, factory) in REAL_INPUTS {
            if !available_factories().contains(&factory) {
                continue;
            }
            let cfg = CaptureConfig {
                audio: true,
                audio_source: Some("dstreamy_no_such_source_test".into()),
                ..config(320, 240)
            };
            let video = live_test_video();
            let started = Instant::now();
            let err = Capture::start(video, Some(input), &cfg, &x264(&cfg))
                .err()
                .unwrap_or_else(|| panic!("{factory} started on a missing device"))
                .to_string();
            assert!(
                err.contains("\"dstreamy_no_such_source_test\" is not available"),
                "{factory}: {err}"
            );
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{factory}: {:?}",
                started.elapsed()
            );
        }
    }

    fn recv_audio(rx: &mut FrameReceiver, within: Duration) -> (usize, usize) {
        let deadline = Instant::now() + within;
        let (mut video, mut audio) = (0, 0);
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(MediaFrame::Video(_)) => video += 1,
                Ok(MediaFrame::Audio(_)) => audio += 1,
                Err(mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(mpsc::error::TryRecvError::Disconnected) => panic!("the capture ended"),
            }
        }
        (video, audio)
    }

    fn start_pinned_test_audio(
        options: &AudioOptions,
        activity: &AudioActivity,
    ) -> Result<(Capture, FrameReceiver), MediaError> {
        init();
        let cfg = CaptureConfig {
            audio: true,
            audio_source: Some("chosen".into()),
            ..config(320, 240)
        };
        let video = live_test_video();
        activity.started();
        Capture::start_with(
            video,
            Some(AudioInput::Test { frames: -1 }),
            &cfg,
            &x264(&cfg),
            Some(activity),
            options,
        )
    }

    #[test]
    fn a_pinned_source_the_server_reports_elsewhere_is_dropped_and_rebuilt() {
        let current = Arc::new(Mutex::new("chosen".to_owned()));
        let activity = AudioActivity::default();
        let (capture, mut rx) = start_pinned_test_audio(&fake_device(&current), &activity).unwrap();
        let (_, audio) = recv_audio(&mut rx, Duration::from_millis(300));
        assert!(audio > 0, "no audio while on the chosen device");
        *current.lock().unwrap() = "elsewhere".into();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !activity.is_lost() && Instant::now() < deadline {
            recv_audio(&mut rx, Duration::from_millis(10));
        }
        assert!(activity.is_lost(), "the move was not reported");
        let (video, audio) = recv_audio(&mut rx, AUDIO_RETRY_FIRST * 3);
        assert!(video > 0, "video stopped with the audio");
        assert_eq!(
            audio, 0,
            "audio passed while the server reported another device"
        );
        *current.lock().unwrap() = "chosen".into();
        let deadline = Instant::now() + AUDIO_RETRY_MAX * 2;
        let mut resumed = 0;
        while resumed == 0 && Instant::now() < deadline {
            resumed = recv_audio(&mut rx, Duration::from_millis(50)).1;
        }
        assert!(
            resumed > 0,
            "audio did not resume once back on the chosen device"
        );
        assert!(!activity.is_lost());
        assert_eq!(capture.error(), None);
        capture.stop().unwrap();
    }

    #[test]
    fn a_pinned_source_on_another_device_from_the_start_fails_the_start() {
        let current = Arc::new(Mutex::new("elsewhere".to_owned()));
        let activity = AudioActivity::default();
        let err = start_pinned_test_audio(&fake_device(&current), &activity)
            .err()
            .expect("started on another device")
            .to_string();
        assert!(
            err.contains(
                "\"chosen\" is not available (the server put the stream on \"elsewhere\")"
            ),
            "{err}"
        );
    }

    /// Held by each real-device test so their device changes never overlap.
    fn real_audio_lock() -> std::sync::MutexGuard<'static, ()> {
        static REAL_AUDIO: Mutex<()> = Mutex::new(());
        lock(&REAL_AUDIO)
    }

    /// A throwaway pipewire-pulse null sink, unloaded on drop.
    struct NullSink(Option<String>);

    impl NullSink {
        const NAME: &str = "dstreamy_pin_e2e";

        fn load() -> Self {
            Self::load_named(Self::NAME)
        }

        fn load_named(name: &str) -> Self {
            let out = std::process::Command::new("pactl")
                .args([
                    "load-module",
                    "module-null-sink",
                    &format!("sink_name={name}"),
                    "sink_properties=priority.session=0 priority.driver=0",
                ])
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
            Self(Some(
                String::from_utf8(out.stdout).unwrap().trim().to_owned(),
            ))
        }

        fn unload(&mut self) {
            if let Some(id) = self.0.take() {
                let _ = std::process::Command::new("pactl")
                    .args(["unload-module", &id])
                    .status();
            }
        }
    }

    impl Drop for NullSink {
        fn drop(&mut self) {
            self.unload();
        }
    }

    /// Run with `DSTREAMY_REAL_AUDIO_TEST=1 cargo test -- --ignored`: it adds and removes a null sink.
    #[test]
    #[ignore = "needs pipewire-pulse and creates a throwaway null sink"]
    fn a_real_device_that_disappears_is_never_replaced_and_comes_back_by_name() {
        if std::env::var_os("DSTREAMY_REAL_AUDIO_TEST").is_none() {
            return;
        }
        let _real = real_audio_lock();
        init();
        let mut sink = NullSink::load();
        let cfg = CaptureConfig {
            audio: true,
            audio_source: Some(format!("{}.monitor", NullSink::NAME)),
            ..config(320, 240)
        };
        let video = live_test_video();
        let activity = AudioActivity::default();
        activity.started();
        let (capture, mut rx) = Capture::start_with(
            video,
            Some(AudioInput::PulseMonitor),
            &cfg,
            &x264(&cfg),
            Some(&activity),
            &AudioOptions::default(),
        )
        .unwrap();
        let (video, audio) = recv_audio(&mut rx, Duration::from_secs(1));
        assert!(
            video > 0 && audio > 0,
            "{video} video, {audio} audio before"
        );
        sink.unload();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !activity.is_lost() && Instant::now() < deadline {
            recv_audio(&mut rx, Duration::from_millis(20));
        }
        assert!(activity.is_lost(), "the loss was not reported");
        let (video, audio) = recv_audio(&mut rx, Duration::from_secs(3));
        assert!(video > 0, "video stopped with the audio");
        assert_eq!(
            audio, 0,
            "audio came from another device while the chosen one was gone"
        );
        assert!(activity.is_lost());
        let back = NullSink::load();
        let deadline = Instant::now() + AUDIO_RETRY_MAX * 2;
        let mut resumed = 0;
        while resumed == 0 && Instant::now() < deadline {
            resumed = recv_audio(&mut rx, Duration::from_millis(100)).1;
        }
        assert!(
            resumed > 0,
            "audio did not resume from the returning device"
        );
        assert!(!activity.is_lost());
        assert_eq!(capture.error(), None);
        capture.stop().unwrap();
        drop(back);
    }

    /// Run with `DSTREAMY_REAL_AUDIO_TEST=1 cargo test -- --ignored`: it adds and removes two null sinks.
    #[test]
    #[ignore = "needs pipewire-pulse and creates throwaway null sinks"]
    fn a_stream_the_server_moves_off_the_chosen_device_is_dropped_and_reconnected() {
        if std::env::var_os("DSTREAMY_REAL_AUDIO_TEST").is_none() {
            return;
        }
        let _real = real_audio_lock();
        init();
        let chosen = "dstreamy_unpinned_a".to_owned();
        let other = "dstreamy_unpinned_b".to_owned();
        let a = NullSink::load_named(&chosen);
        let b = NullSink::load_named(&other);
        let cfg = CaptureConfig {
            audio: true,
            audio_source: Some(format!("{chosen}.monitor")),
            ..config(320, 240)
        };
        let video = live_test_video();
        let activity = AudioActivity::default();
        activity.started();
        let (capture, mut rx) = Capture::start_with(
            video,
            Some(AudioInput::PulseMonitor),
            &cfg,
            &x264(&cfg),
            Some(&activity),
            // Without the node properties pipewire-pulse lets the stream move, as plain PulseAudio does.
            &AudioOptions {
                pin_properties: false,
                device_query: None,
            },
        )
        .unwrap();
        assert!(
            recv_audio(&mut rx, Duration::from_secs(1)).1 > 0,
            "no audio before the move"
        );
        let source = capture
            .pipeline
            .by_name(&format!("{AUDIO_ELEMENT_PREFIX}1-0"))
            .unwrap();
        let index = source.property::<u32>("source-output-index");
        let moved = std::process::Command::new("pactl")
            .args([
                "move-source-output",
                &index.to_string(),
                &format!("{other}.monitor"),
            ])
            .status()
            .unwrap();
        assert!(moved.success());
        let deadline = Instant::now() + Duration::from_secs(2);
        while !activity.is_lost() && Instant::now() < deadline {
            recv_audio(&mut rx, Duration::from_millis(20));
        }
        assert!(activity.is_lost(), "the move was not reported");
        assert!(
            capture
                .pipeline
                .by_name(&format!("{AUDIO_ELEMENT_PREFIX}1-0"))
                .is_none()
        );
        let deadline = Instant::now() + AUDIO_RETRY_MAX * 2;
        let mut resumed = 0;
        while resumed == 0 && Instant::now() < deadline {
            resumed = recv_audio(&mut rx, Duration::from_millis(100)).1;
        }
        assert!(resumed > 0, "audio did not resume from the chosen device");
        let source = capture
            .pipeline
            .by_name(&format!("{AUDIO_ELEMENT_PREFIX}2-0"))
            .unwrap();
        assert_eq!(
            source.property::<String>("current-device"),
            format!("{chosen}.monitor")
        );
        assert!(!activity.is_lost());
        capture.stop().unwrap();
        drop((a, b));
    }

    fn drain(capture: Capture, mut rx: FrameReceiver) -> Vec<MediaFrame> {
        let mut frames = Vec::new();
        while let Some(frame) = rx.blocking_recv() {
            frames.push(frame);
        }
        capture.stop().unwrap();
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
    fn crop_outside_the_source_is_a_config_error_without_fallback() {
        init();
        let cfg = CaptureConfig {
            crop: Some(CropRect {
                x: 200,
                y: 0,
                width: 200,
                height: 100,
            }),
            ..config(320, 240)
        };
        let input = VideoInput::Test {
            width: 320,
            height: 240,
            frames: 10,
        };
        let candidates = [x264(&cfg).remove(0), x264(&cfg).remove(0)];
        let started = Instant::now();
        match Capture::start(input, None, &cfg, &candidates) {
            Err(MediaError::InvalidConfig(msg)) => assert!(msg.contains("320x240"), "{msg}"),
            Err(e) => panic!("expected a crop error, got {e}"),
            Ok(_) => panic!("crop outside the source started"),
        }
        assert!(started.elapsed() < PROBE_TIMEOUT);
    }

    #[test]
    fn keyframe_request_forces_an_early_idr_and_stop_closes_the_channel() {
        init();
        let cfg = config(320, 240);
        let input = live_test_video();
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
        capture.stop().unwrap();
        while rx.blocking_recv().is_some() {}
    }
}
