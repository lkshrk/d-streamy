use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dstreamy_media::{
    AudioActivity, AudioEncoder, AudioFrame, CaptureConfig, CropRect, EncodedVideoFrame,
    FRAME_CHANNEL_CAPACITY, FrameReceiver, MediaError, MediaFrame, OPUS_CHANNELS,
    OPUS_FRAME_DURATION_US, OPUS_FRAME_SAMPLES, OPUS_SAMPLE_RATE, OpusEncoder, VideoCodec,
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
const AUDIO_RETRY_FIRST: Duration = Duration::from_millis(500);
const AUDIO_RETRY_MAX: Duration = Duration::from_secs(5);
/// Every audio element's name starts with this, so errors of a branch already replaced are still recognised.
const AUDIO_ELEMENT_PREFIX: &str = "dstreamy-audio-";

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

/// Errors if the crop, rounded down to even like [`crop_caps`], extends past a `width`x`height` source.
pub(crate) fn check_crop_fits(crop: CropRect, width: u32, height: u32) -> Result<(), MediaError> {
    let fits = |offset: u32, size: u32, limit: u32| {
        u64::from(offset) + u64::from(size & !1).max(2) <= u64::from(limit)
    };
    if fits(crop.x, crop.width, width) && fits(crop.y, crop.height, height) {
        return Ok(());
    }
    Err(MediaError::InvalidConfig(format!(
        "crop {}x{} at ({}, {}) does not fit the {width}x{height} source",
        crop.width, crop.height, crop.x, crop.y
    )))
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
        VideoInput::PipeWire { fd, node_id, .. } => {
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

/// Stream properties that keep a chosen device's stream on that device: WirePlumber destroys it instead of moving it.
pub(crate) fn pinned_stream_fields(
    device: Option<&str>,
) -> &'static [(&'static str, &'static str)] {
    match device {
        Some(_) => &[
            ("node.dont-fallback", "true"),
            ("node.dont-reconnect", "true"),
            ("node.dont-move", "true"),
        ],
        None => &[],
    }
}

/// `pulsesrc` stream properties, which pipewire-pulse copies onto the PipeWire node.
pub(crate) fn pulse_stream_properties(device: Option<&str>) -> Option<gst::Structure> {
    let fields = pinned_stream_fields(device);
    (!fields.is_empty()).then(|| {
        fields
            .iter()
            .fold(gst::Structure::builder("props"), |b, (k, v)| {
                b.field(*k, *v)
            })
            .build()
    })
}

pub(crate) fn pipewire_stream_properties(device: Option<&str>) -> gst::Structure {
    let (target, capture_sink) = pipewire_target(device);
    let mut props = gst::Structure::builder("props").field("media.category", "Capture");
    if capture_sink {
        props = props.field("stream.capture.sink", "true");
    }
    if let Some(target) = target {
        props = props.field("target.object", target);
    }
    pinned_stream_fields(device)
        .iter()
        .fold(props, |b, (k, v)| b.field(*k, *v))
        .build()
}

/// Where `pipewiresrc` should capture `device`, a pulse source name: a sink's monitor or a source.
pub(crate) fn pipewire_target(device: Option<&str>) -> (Option<&str>, bool) {
    match device {
        None => (None, true),
        Some(name) => match name.strip_suffix(".monitor") {
            Some(sink) => (Some(sink), true),
            None => (Some(name), false),
        },
    }
}

fn audio_source(input: AudioInput, device: Option<&str>) -> Result<gst::Element, MediaError> {
    match input {
        AudioInput::PulseMonitor => {
            let src = make("pulsesrc")?;
            src.set_property("device", device.unwrap_or("@DEFAULT_MONITOR@"));
            if let Some(props) = pulse_stream_properties(device) {
                src.set_property("stream-properties", props);
            }
            Ok(src)
        }
        AudioInput::PipeWireSink => {
            let src = make("pipewiresrc")?;
            if let Some(target) = pipewire_target(device).0 {
                set_if_present(&src, "target-object", target);
                set_if_present(&src, "on-disconnect", "error");
            }
            src.set_property("stream-properties", pipewire_stream_properties(device));
            Ok(src)
        }
        #[cfg(test)]
        AudioInput::Test { frames } => {
            let src = make("audiotestsrc")?;
            src.set_property("num-buffers", frames);
            src.set_property("is-live", frames < 0);
            Ok(src)
        }
    }
}

/// Where the appsink callbacks deliver frames; closing it closes the receiver.
struct Output {
    tx: Mutex<Option<mpsc::Sender<MediaFrame>>>,
    stop: AtomicBool,
    video_frames: AtomicU64,
    audio_buffers: AtomicU64,
    error: Mutex<Option<String>>,
}

impl Output {
    fn new(tx: mpsc::Sender<MediaFrame>) -> Arc<Self> {
        Arc::new(Self {
            tx: Mutex::new(Some(tx)),
            stop: AtomicBool::new(false),
            video_frames: AtomicU64::new(0),
            audio_buffers: AtomicU64::new(0),
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

fn audio_sink(
    output: Arc<Output>,
    activity: Option<AudioActivity>,
) -> Result<gst_app::AppSink, MediaError> {
    let mut packetizer = AudioPacketizer {
        encoder: OpusEncoder::new(AUDIO_BITRATE_BPS)?,
        pending: Vec::with_capacity(OPUS_FRAME_SAMPLES * OPUS_CHANNELS * 2),
        next_pts_us: None,
    };
    let sink = gst_app::AppSink::builder()
        .sync(false)
        .caps(&audio_caps())
        .callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    output.audio_buffers.fetch_add(1, Ordering::Relaxed);
                    if let Some(activity) = &activity {
                        activity.samples(
                            map.as_slice()
                                .as_chunks::<2>()
                                .0
                                .iter()
                                .map(|b| i16::from_le_bytes(*b)),
                        );
                    }
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
        .build();
    // A replacement branch is added to a playing pipeline, which must not wait for it to preroll.
    sink.set_property("async", false);
    Ok(sink)
}

/// The audio chain, which is rebuilt in place when its source fails so video keeps streaming.
struct AudioBranch {
    input: AudioInput,
    device: Option<String>,
    gain: f64,
    activity: Option<AudioActivity>,
    elements: Vec<gst::Element>,
    generation: u64,
}

impl AudioBranch {
    fn new(input: AudioInput, config: &CaptureConfig, activity: Option<&AudioActivity>) -> Self {
        Self {
            input,
            device: config.audio_source.clone(),
            gain: f64::from(config.audio_gain),
            activity: activity.cloned(),
            elements: Vec::new(),
            generation: 0,
        }
    }

    /// Builds the chain into `pipeline`; the caller brings it to the pipeline's state.
    fn add(&mut self, pipeline: &gst::Pipeline, output: &Arc<Output>) -> Result<(), MediaError> {
        self.generation += 1;
        let volume = make("volume")?;
        volume.set_property("volume", self.gain);
        let source = audio_source(self.input, self.device.as_deref())?;
        // Video would stall on a lost clock when a failed audio source that provided it is removed.
        set_if_present(&source, "provide-clock", "false");
        let chain = vec![
            source,
            make("audioconvert")?,
            volume,
            make("audioresample")?,
            audio_sink(output.clone(), self.activity.clone())?.upcast(),
        ];
        for (i, e) in chain.iter().enumerate() {
            e.set_property(
                "name",
                format!("{AUDIO_ELEMENT_PREFIX}{}-{i}", self.generation),
            );
        }
        pipeline
            .add_many(&chain)
            .map_err(|e| MediaError::Backend(format!("cannot add elements: {e}")))?;
        self.elements = chain;
        gst::Element::link_many(&self.elements)
            .map_err(|e| MediaError::Backend(format!("cannot link pipeline: {e}")))
    }

    fn attach(&mut self, pipeline: &gst::Pipeline, output: &Arc<Output>) -> Result<(), MediaError> {
        self.add(pipeline, output)?;
        for e in &self.elements {
            e.sync_state_with_parent()
                .map_err(|e| MediaError::Backend(format!("cannot start audio: {e}")))?;
        }
        Ok(())
    }

    fn detach(&mut self, pipeline: &gst::Pipeline) {
        for e in self.elements.drain(..) {
            // Ignored: the element is discarded either way.
            let _ = e.set_state(gst::State::Null);
            let _ = pipeline.remove(&e);
        }
    }
}

/// The audio branch generation an error came from, current or already replaced.
fn audio_generation(err: &gst::message::Error) -> Option<u64> {
    let name = err.src()?.name();
    let (generation, _) = name.strip_prefix(AUDIO_ELEMENT_PREFIX)?.split_once('-')?;
    generation.parse().ok()
}

/// When to rebuild a failed audio branch: soon at first, backing off while the device stays away.
#[derive(Debug)]
struct AudioRetry {
    next: Option<Instant>,
    delay: Duration,
}

impl Default for AudioRetry {
    fn default() -> Self {
        Self {
            next: None,
            delay: AUDIO_RETRY_FIRST,
        }
    }
}

impl AudioRetry {
    fn failed(&mut self, now: Instant) {
        self.next = Some(now + self.delay);
        self.delay = (self.delay * 2).min(AUDIO_RETRY_MAX);
    }

    fn due(&mut self, now: Instant) -> bool {
        let due = self.next.is_some_and(|at| now >= at);
        if due {
            self.next = None;
        }
        due
    }

    fn pending(&self) -> bool {
        self.next.is_some()
    }

    fn recovered(&mut self) {
        self.delay = AUDIO_RETRY_FIRST;
    }
}

/// The message for an audio source that failed at start.
fn audio_start_error(device: Option<&str>, err: &str) -> MediaError {
    match device {
        Some(id) => MediaError::Backend(format!(
            "audio source {id:?} is not available ({err}); D-Streamy does not fall back to another device, \
             so pick one from `dstreamy audio-sources`"
        )),
        None => MediaError::Backend(format!("audio capture failed: {err}")),
    }
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
) -> Result<Built, MediaError> {
    let pipeline = gst::Pipeline::new();

    let mut chain = video_source(video, config.fps)?;
    let video_source = chain.last().expect("video source has elements").clone();
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

    let audio = match audio {
        Some(input) => {
            let mut branch = AudioBranch::new(input, config, activity);
            branch.add(&pipeline, output)?;
            Some(branch)
        }
        None => None,
    };
    Ok(Built {
        pipeline,
        video_source,
        size_hint,
        video_sink: sink,
        audio,
    })
}

fn bus_error_text(err: &gst::message::Error) -> String {
    let src = err
        .src()
        .map_or_else(|| "pipeline".to_owned(), |s| s.path_string().to_string());
    format!("{src}: {}", err.error())
}

fn bus_error(err: &gst::message::Error) -> MediaError {
    MediaError::Backend(bus_error_text(err))
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
        StartError::Retry(bus_error(err))
    }
}

/// Waits until the first encoded frame arrives, the stream ends, or the pipeline fails.
fn probe(built: &Built, output: &Output, crop: Option<CropRect>) -> Result<(), StartError> {
    let bus = built.pipeline.bus().expect("pipeline has a bus");
    let types = [gst::MessageType::Error, gst::MessageType::Eos];
    let deadline = Instant::now() + PROBE_TIMEOUT;
    while Instant::now() < deadline && output.video_frames.load(Ordering::Relaxed) == 0 {
        let Some(msg) = bus.timed_pop_filtered(gst::ClockTime::from_mseconds(10), &types) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Error(err) => {
                return Err(classify(err, built, crop));
            }
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
    #[cfg(test)]
    pub(crate) fn start(
        video: VideoInput,
        audio: Option<AudioInput>,
        config: &CaptureConfig,
        candidates: &[EncoderSpec],
    ) -> Result<(Self, FrameReceiver), MediaError> {
        Self::start_with(video, audio, config, candidates, None)
    }

    /// Like `start`; `activity` hears every captured audio sample.
    pub(crate) fn start_with(
        video: VideoInput,
        audio: Option<AudioInput>,
        config: &CaptureConfig,
        candidates: &[EncoderSpec],
        activity: Option<&AudioActivity>,
    ) -> Result<(Self, FrameReceiver), MediaError> {
        if candidates.is_empty() {
            return Err(MediaError::Unsupported(format!(
                "no GStreamer encoder for {:?}",
                config.codec
            )));
        }
        let mut last_err = None;
        for spec in candidates {
            // A channel per candidate, so frames of a failed attempt never reach the receiver.
            let (tx, rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
            match Self::try_start(&video, audio, config, spec, Output::new(tx), activity) {
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
    ) -> Result<Self, StartError> {
        let built = build(video, audio, config, spec, &output, activity).map_err(|e| match e {
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
        capture
            .pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| {
                StartError::Retry(MediaError::Backend(format!(
                    "{} pipeline: {e}",
                    spec.factory
                )))
            })?;
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

/// Closes the frame channel when the pipeline ends or fails; a failed audio branch is rebuilt for the same device instead.
fn watch_bus(pipeline: &gst::Pipeline, output: &Arc<Output>, mut audio: Option<AudioBranch>) {
    let bus = pipeline.bus().expect("pipeline has a bus");
    let types = [gst::MessageType::Error, gst::MessageType::Eos];
    let mut retry = AudioRetry::default();
    let mut buffers_at_attach = output.audio_buffers.load(Ordering::Relaxed);
    while !output.stop.load(Ordering::Relaxed) {
        if let Some(branch) = audio.as_mut() {
            if retry.due(Instant::now()) {
                buffers_at_attach = output.audio_buffers.load(Ordering::Relaxed);
                if branch.attach(pipeline, output).is_err() {
                    branch.detach(pipeline);
                    retry.failed(Instant::now());
                }
            } else if !retry.pending()
                && output.audio_buffers.load(Ordering::Relaxed) > buffers_at_attach
            {
                retry.recovered();
            }
        }
        let Some(msg) = bus.timed_pop_filtered(BUS_POLL, &types) else {
            continue;
        };
        if let gst::MessageView::Error(err) = msg.view() {
            if let Some(generation) = audio_generation(err)
                && let Some(branch) = audio.as_mut()
            {
                if generation == branch.generation && !retry.pending() {
                    branch.detach(pipeline);
                    if let Some(activity) = &branch.activity {
                        activity.lost();
                    }
                    retry.failed(Instant::now());
                }
                continue;
            }
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
    fn crop_must_fit_the_source_after_rounding_to_even() {
        let crop = |x, y, width, height| CropRect {
            x,
            y,
            width,
            height,
        };
        assert!(check_crop_fits(crop(0, 0, 320, 240), 320, 240).is_ok());
        assert!(check_crop_fits(crop(100, 50, 221, 191), 320, 240).is_ok());
        assert!(check_crop_fits(crop(100, 50, 222, 190), 320, 240).is_err());
        assert!(check_crop_fits(crop(0, 1, 320, 240), 320, 240).is_err());
        assert!(check_crop_fits(crop(u32::MAX, 0, 2, 2), 320, 240).is_err());
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
        let (capture, rx) =
            Capture::start_with(video, audio, &cfg, &x264(&cfg), Some(&activity)).unwrap();
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
    fn pipewire_targets_a_sink_for_monitors_and_the_node_for_sources() {
        assert_eq!(pipewire_target(None), (None, true));
        assert_eq!(
            pipewire_target(Some("pipeweaver_music.monitor")),
            (Some("pipeweaver_music"), true)
        );
        assert_eq!(
            pipewire_target(Some("pipeweaver_stream")),
            (Some("pipeweaver_stream"), false)
        );
    }

    fn field(props: &gst::Structure, name: &str) -> Option<String> {
        props.get::<String>(name).ok()
    }

    #[test]
    fn a_chosen_device_is_pinned_and_the_default_is_left_to_follow() {
        init();
        assert!(pinned_stream_fields(None).is_empty());
        assert!(pulse_stream_properties(None).is_none());
        let pulse = pulse_stream_properties(Some("pipeweaver_stream")).unwrap();
        for key in [
            "node.dont-fallback",
            "node.dont-reconnect",
            "node.dont-move",
        ] {
            assert_eq!(field(&pulse, key).as_deref(), Some("true"), "{key}");
        }
        let source = pipewire_stream_properties(Some("pipeweaver_stream"));
        assert_eq!(
            field(&source, "target.object").as_deref(),
            Some("pipeweaver_stream")
        );
        assert_eq!(
            field(&source, "node.dont-fallback").as_deref(),
            Some("true")
        );
        assert_eq!(
            field(&source, "node.dont-reconnect").as_deref(),
            Some("true")
        );
        assert!(!source.has_field("stream.capture.sink"));
        let monitor = pipewire_stream_properties(Some("pipeweaver_music.monitor"));
        assert_eq!(
            field(&monitor, "target.object").as_deref(),
            Some("pipeweaver_music")
        );
        assert_eq!(
            field(&monitor, "stream.capture.sink").as_deref(),
            Some("true")
        );
        assert_eq!(
            field(&monitor, "node.dont-fallback").as_deref(),
            Some("true")
        );
        let default = pipewire_stream_properties(None);
        assert!(!default.has_field("target.object"));
        assert!(!default.has_field("node.dont-fallback"));
        assert!(!default.has_field("node.dont-reconnect"));
    }

    #[test]
    fn audio_elements_carry_the_pinning_properties() {
        init();
        let installed = available_factories();
        if installed.contains(&"pulsesrc") {
            let src = audio_source(AudioInput::PulseMonitor, Some("pipeweaver_stream")).unwrap();
            assert_eq!(
                src.property::<Option<String>>("device").as_deref(),
                Some("pipeweaver_stream")
            );
            let props = src
                .property::<Option<gst::Structure>>("stream-properties")
                .unwrap();
            assert_eq!(field(&props, "node.dont-fallback").as_deref(), Some("true"));
            let default = audio_source(AudioInput::PulseMonitor, None).unwrap();
            assert!(
                default
                    .property::<Option<gst::Structure>>("stream-properties")
                    .is_none()
            );
        }
        if installed.contains(&"pipewiresrc") {
            let src = audio_source(AudioInput::PipeWireSink, Some("pipeweaver_stream")).unwrap();
            let props = src
                .property::<Option<gst::Structure>>("stream-properties")
                .unwrap();
            assert_eq!(
                field(&props, "node.dont-reconnect").as_deref(),
                Some("true")
            );
            let on_disconnect = src.property_value("on-disconnect");
            assert_eq!(on_disconnect.serialize().unwrap().as_str(), "error");
        }
    }

    #[test]
    fn audio_retries_back_off_while_the_device_stays_away_and_reset_once_it_is_back() {
        let t0 = Instant::now();
        let mut r = AudioRetry::default();
        assert!(!r.pending() && !r.due(t0));
        r.failed(t0);
        assert!(r.pending());
        assert!(!r.due(t0 + AUDIO_RETRY_FIRST - Duration::from_millis(1)));
        assert!(r.due(t0 + AUDIO_RETRY_FIRST));
        assert!(!r.pending() && !r.due(t0 + AUDIO_RETRY_FIRST));
        let mut at = t0;
        let mut delays = Vec::new();
        for _ in 0..6 {
            r.failed(at);
            let next = r.next.unwrap();
            delays.push(next - at);
            at = next;
            assert!(r.due(at));
        }
        assert_eq!(delays[0], AUDIO_RETRY_FIRST * 2);
        assert!(delays.windows(2).all(|w| w[1] >= w[0]));
        assert_eq!(*delays.last().unwrap(), AUDIO_RETRY_MAX);
        r.recovered();
        r.failed(at);
        assert_eq!(r.next.unwrap() - at, AUDIO_RETRY_FIRST);
    }

    #[test]
    fn errors_are_told_apart_by_audio_branch_generation() {
        init();
        let error_from = |name: &str| {
            let e = make("fakesrc").unwrap();
            e.set_property("name", name);
            gst::message::Error::builder(gst::ResourceError::NotFound, "gone")
                .src(&e)
                .build()
        };
        let audio = |name: &str| match error_from(name).view() {
            gst::MessageView::Error(err) => audio_generation(err),
            _ => unreachable!(),
        };
        assert_eq!(audio(&format!("{AUDIO_ELEMENT_PREFIX}3-0")), Some(3));
        assert_eq!(audio(&format!("{AUDIO_ELEMENT_PREFIX}12-4")), Some(12));
        assert_eq!(audio("pipewiresrc0"), None);
        assert_eq!(audio(&format!("{AUDIO_ELEMENT_PREFIX}x-0")), None);
    }

    #[test]
    fn a_chosen_device_that_fails_at_start_names_the_device_and_the_way_out() {
        let err = "pulsesrc: Failed to connect stream: No such entity";
        let msg = audio_start_error(Some("pipeweaver_stream"), err).to_string();
        assert!(
            msg.contains("\"pipeweaver_stream\" is not available"),
            "{msg}"
        );
        assert!(msg.contains("No such entity"), "{msg}");
        assert!(msg.contains("does not fall back"), "{msg}");
        assert!(msg.contains("dstreamy audio-sources"), "{msg}");
        let default = audio_start_error(None, err).to_string();
        assert!(
            default.ends_with(
                "audio capture failed: pulsesrc: Failed to connect stream: No such entity"
            ),
            "{default}"
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
        let video = VideoInput::Test {
            width: 320,
            height: 240,
            frames: -1,
        };
        let activity = AudioActivity::default();
        activity.started();
        let audio = Some(AudioInput::Test { frames: -1 });
        let (capture, mut rx) =
            Capture::start_with(video, audio, &cfg, &x264(&cfg), Some(&activity)).unwrap();
        let mut recv = |within: Duration| {
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
        };
        let (video, audio) = recv(Duration::from_millis(300));
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
        let (video, _) = recv(AUDIO_RETRY_FIRST / 2);
        assert!(video > 0, "video stopped with the audio");
        let (video, audio) = recv(AUDIO_RETRY_FIRST * 2);
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
        assert_eq!(capture.error(), None);
        capture.stop();
    }

    /// A throwaway pipewire-pulse null sink, unloaded on drop.
    struct NullSink(Option<String>);

    impl NullSink {
        const NAME: &str = "dstreamy_pin_e2e";

        fn load() -> Self {
            let out = std::process::Command::new("pactl")
                .args([
                    "load-module",
                    "module-null-sink",
                    &format!("sink_name={}", Self::NAME),
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
        init();
        let mut sink = NullSink::load();
        let cfg = CaptureConfig {
            audio: true,
            audio_source: Some(format!("{}.monitor", NullSink::NAME)),
            ..config(320, 240)
        };
        let video = VideoInput::Test {
            width: 320,
            height: 240,
            frames: -1,
        };
        let activity = AudioActivity::default();
        activity.started();
        let (capture, mut rx) = Capture::start_with(
            video,
            Some(AudioInput::PulseMonitor),
            &cfg,
            &x264(&cfg),
            Some(&activity),
        )
        .unwrap();
        let mut recv = |within: Duration| {
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
        };
        let (video, audio) = recv(Duration::from_secs(1));
        assert!(
            video > 0 && audio > 0,
            "{video} video, {audio} audio before"
        );
        sink.unload();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !activity.is_lost() && Instant::now() < deadline {
            recv(Duration::from_millis(20));
        }
        assert!(activity.is_lost(), "the loss was not reported");
        let (video, audio) = recv(Duration::from_secs(3));
        assert!(video > 0, "video stopped with the audio");
        assert_eq!(
            audio, 0,
            "audio came from another device while the chosen one was gone"
        );
        assert!(activity.is_lost());
        let _back = NullSink::load();
        let deadline = Instant::now() + AUDIO_RETRY_MAX * 2;
        let mut resumed = 0;
        while resumed == 0 && Instant::now() < deadline {
            resumed = recv(Duration::from_millis(100)).1;
        }
        assert!(
            resumed > 0,
            "audio did not resume from the returning device"
        );
        assert!(!activity.is_lost());
        assert_eq!(capture.error(), None);
        capture.stop();
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
