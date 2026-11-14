use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dstreamy_media::{
    AUDIO_DEVICE_TARGET, AudioActivity, AudioClock, AudioEncoder, AudioFrame, CaptureConfig,
    MediaError, MediaFrame, OPUS_BITRATE_BPS, OPUS_CHANNELS, OPUS_FRAME_SAMPLES, OpusEncoder,
};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use super::caps::audio_caps;
use super::elements::{link_chain, make, set_if_present};
use super::{AudioInput, Output, bus_error_text, lock, running_time_us};

pub(super) const AUDIO_RETRY_FIRST: Duration = Duration::from_millis(500);
pub(super) const AUDIO_RETRY_MAX: Duration = Duration::from_secs(5);
const AUDIO_START_TIMEOUT: Duration = Duration::from_secs(10);
/// An abandoned start usually returns within milliseconds; only one blocked this long is reported.
pub(super) const AUDIO_STUCK_WARNING: Duration = Duration::from_secs(5);
/// Longer than pipewiresrc's own 30 s start wait, which its flush cuts short.
const AUDIO_ABANDON_FLUSH: Duration = Duration::from_secs(35);
/// Every audio element's name starts with this, so errors of a branch already replaced are still recognised.
pub(super) const AUDIO_ELEMENT_PREFIX: &str = "dstreamy-audio-";

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

fn audio_source(
    input: AudioInput,
    device: Option<&str>,
    pin_properties: bool,
) -> Result<gst::Element, MediaError> {
    match input {
        AudioInput::PulseMonitor => {
            let src = make("pulsesrc")?;
            src.set_property("device", device.unwrap_or("@DEFAULT_MONITOR@"));
            if let Some(props) = pulse_stream_properties(device.filter(|_| pin_properties)) {
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
        #[cfg(test)]
        AudioInput::Blocking => {
            Ok(glib::Object::new::<super::test_support::BlockingSrc>().upcast())
        }
    }
}

/// Chops PCM into 20 ms Opus frames timestamped by sample count, realigned after gaps.
struct AudioPacketizer {
    encoder: OpusEncoder,
    pending: Vec<i16>,
    /// Shared by every audio branch generation, so a rebuilt branch cannot stamp below the last.
    clock: Arc<Mutex<AudioClock>>,
}

impl AudioPacketizer {
    fn new(clock: Arc<Mutex<AudioClock>>) -> Result<Self, MediaError> {
        Ok(Self {
            encoder: OpusEncoder::new(OPUS_BITRATE_BPS)?,
            pending: Vec::with_capacity(OPUS_FRAME_SAMPLES * OPUS_CHANNELS * 2),
            clock,
        })
    }

    fn push(&mut self, pcm: &[u8], running_us: Option<u64>) -> Result<Vec<AudioFrame>, MediaError> {
        let samples = pcm.as_chunks::<2>().0;
        let mut clock = lock(&self.clock);
        let drop = clock.admit(
            self.pending.len() / OPUS_CHANNELS,
            samples.len() / OPUS_CHANNELS,
            running_us,
        );
        self.pending.extend(
            samples[drop * OPUS_CHANNELS..]
                .iter()
                .map(|b| i16::from_le_bytes(*b)),
        );
        let frame_len = OPUS_FRAME_SAMPLES * OPUS_CHANNELS;
        let mut out = Vec::new();
        let mut consumed = 0;
        while self.pending.len() - consumed >= frame_len {
            let pts = clock.advance();
            out.push(
                self.encoder
                    .encode(&self.pending[consumed..consumed + frame_len], pts)?,
            );
            consumed += frame_len;
        }
        self.pending.drain(..consumed);
        Ok(out)
    }
}

/// Asks where an audio source's stream reads from now; tests swap in a fake.
type DeviceQuery = Arc<dyn Fn(&gst::Element) -> Option<String> + Send + Sync>;

fn pulse_current_device(source: &gst::Element) -> Option<String> {
    source.property::<Option<String>>("current-device")
}

/// How a branch makes its audio source.
#[derive(Clone)]
pub(crate) struct AudioOptions {
    pub(super) pin_properties: bool,
    pub(super) device_query: Option<DeviceQuery>,
}

impl Default for AudioOptions {
    fn default() -> Self {
        Self {
            pin_properties: true,
            device_query: None,
        }
    }
}

/// An audio source that must stay on `device`; plain PulseAudio moves it to the default when the device goes.
struct Pin {
    source: glib::WeakRef<gst::Element>,
    device: String,
    generation: u64,
    query: DeviceQuery,
    /// Set at the first mismatch; nothing from this source instance passes after it.
    tainted: AtomicBool,
}

impl Pin {
    /// Whether a buffer reaching the sink now may pass; on a mismatch, where the server says the stream is.
    fn admit(&self) -> Result<(), String> {
        if self.tainted.load(Ordering::Relaxed) {
            return Err(String::new());
        }
        let current = self.source.upgrade().and_then(|s| (self.query)(&s));
        if current.as_deref() == Some(self.device.as_str()) {
            return Ok(());
        }
        self.tainted.store(true, Ordering::Relaxed);
        Err(current
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "an unknown device".to_owned()))
    }
}

fn audio_sink(
    output: Arc<Output>,
    activity: Option<AudioActivity>,
    pin: Option<Pin>,
) -> Result<gst_app::AppSink, MediaError> {
    let mut packetizer = AudioPacketizer::new(output.audio_clock.clone())?;
    let sink = gst_app::AppSink::builder()
        .sync(false)
        .caps(&audio_caps())
        .callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if let Some(pin) = &pin
                        && let Err(current) = pin.admit()
                    {
                        if !current.is_empty() {
                            let mut moved = lock(&output.moved);
                            moved.get_or_insert((pin.generation, current));
                        }
                        return Ok(gst::FlowSuccess::Ok);
                    }
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

/// A branch being brought up outside the pipeline, so a source that blocks in its state change blocks only this.
pub(super) struct AttachJob {
    pub(super) bin: gst::Bin,
    bus: gst::Bus,
    pub(super) source: gst::Element,
    state: Arc<Mutex<JobState>>,
    started_at: Instant,
}

/// Who owns the bin once its state change returns: decided under the mutex, so exactly one side stops it.
enum JobState {
    Running,
    /// Given up while running; the start thread stops the bin when its state change returns.
    Abandoned,
    Done(Result<(), gst::StateChangeError>),
    Taken,
}

impl AttachJob {
    /// Gives the job up without waiting for it: flushes its source until its state change returns.
    fn abandon(self) {
        let previous = std::mem::replace(&mut *lock(&self.state), JobState::Abandoned);
        if let JobState::Done(_) = previous {
            *lock(&self.state) = JobState::Taken;
            let bin = self.bin;
            std::thread::spawn(move || {
                let _ = bin.set_state(gst::State::Null);
            });
            return;
        }
        let (source, state) = (self.source, self.state);
        std::thread::spawn(move || {
            let deadline = Instant::now() + AUDIO_ABANDON_FLUSH;
            let running = || matches!(*lock(&state), JobState::Abandoned);
            // Repeated: a flush sent before the source's pad is active is dropped by that pad.
            while running() && Instant::now() < deadline {
                if let Some(pad) = source.static_pad("src") {
                    pad.send_event(gst::event::FlushStart::new());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
    }

    /// The state change's result once it returned, handing the bin to the caller.
    fn take_result(&self) -> Option<Result<(), gst::StateChangeError>> {
        let mut state = lock(&self.state);
        match std::mem::replace(&mut *state, JobState::Taken) {
            JobState::Done(result) => Some(result),
            other => {
                *state = other;
                None
            }
        }
    }

    fn errors(&self) -> Option<gst::Message> {
        let mut first = None;
        while let Some(msg) = self.bus.pop_filtered(&[gst::MessageType::Error]) {
            first.get_or_insert(msg);
        }
        first
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Restart {
    DeviceLoss,
    Fault,
}

/// A device loss for a source error that means the device went away; any other failure is a fault.
fn restart_for(msg: Option<&gst::Message>) -> Restart {
    match msg.map(|m| m.view()) {
        Some(gst::MessageView::Error(err)) if is_device_loss(err) => Restart::DeviceLoss,
        _ => Restart::Fault,
    }
}

pub(super) enum Attach {
    Pending,
    Attached,
    /// Why, and whether retrying can help: a lost device or a start that never returned, not a fault.
    Failed(String, Restart),
}

/// The audio chain, which is rebuilt when its source fails so video keeps streaming.
pub(super) struct AudioBranch {
    input: AudioInput,
    pub(super) device: Option<String>,
    gain: f64,
    activity: Option<AudioActivity>,
    options: AudioOptions,
    pub(super) generation: u64,
    pub(super) active: Option<gst::Bin>,
    pub(super) job: Option<AttachJob>,
    /// The last abandoned start and when it was given up; no new start begins while it is inside its state change.
    blocked: Option<(Arc<Mutex<JobState>>, Instant)>,
}

impl AudioBranch {
    pub(super) fn new(
        input: AudioInput,
        config: &CaptureConfig,
        activity: Option<&AudioActivity>,
        options: AudioOptions,
    ) -> Self {
        Self {
            input,
            device: config.audio_source.clone(),
            gain: f64::from(config.audio_gain),
            activity: activity.cloned(),
            options,
            generation: 0,
            active: None,
            job: None,
            blocked: None,
        }
    }

    /// The next generation's chain in a bin of its own, not yet started.
    fn build(&mut self, output: &Arc<Output>) -> Result<(gst::Bin, gst::Element), MediaError> {
        self.generation += 1;
        let volume = make("volume")?;
        volume.set_property("volume", self.gain);
        let source = audio_source(
            self.input,
            self.device.as_deref(),
            self.options.pin_properties,
        )?;
        // Video would stall on a lost clock when a failed audio source that provided it is removed.
        set_if_present(&source, "provide-clock", "false");
        let query = match (self.input, &self.options.device_query) {
            (_, Some(query)) => Some(query.clone()),
            (AudioInput::PulseMonitor, None) => Some(Arc::new(pulse_current_device) as DeviceQuery),
            _ => None,
        };
        let pin = match (query, &self.device) {
            (Some(query), Some(device)) => Some(Pin {
                source: source.downgrade(),
                device: device.clone(),
                generation: self.generation,
                query,
                tainted: AtomicBool::new(false),
            }),
            _ => None,
        };
        let chain = [
            source.clone(),
            make("audioconvert")?,
            volume,
            make("audioresample")?,
            audio_sink(output.clone(), self.activity.clone(), pin)?.upcast(),
        ];
        for (i, e) in chain.iter().enumerate() {
            e.set_property(
                "name",
                format!("{AUDIO_ELEMENT_PREFIX}{}-{i}", self.generation),
            );
        }
        let bin = gst::Bin::builder()
            .name(format!("{AUDIO_ELEMENT_PREFIX}{}", self.generation))
            .build();
        link_chain(&bin, &chain)?;
        Ok((bin, source))
    }

    /// Starts bringing up the next generation on a thread of its own; [`Self::poll`] finishes it.
    pub(super) fn begin(&mut self, output: &Arc<Output>) -> Result<(), MediaError> {
        let (bin, source) = self.build(output)?;
        let bus = gst::Bus::new();
        bin.set_bus(Some(&bus));
        let state = Arc::new(Mutex::new(JobState::Running));
        let thread = {
            let (bin, state) = (bin.clone(), state.clone());
            move || {
                let started = bin.set_state(gst::State::Paused).map(drop);
                let mut state = lock(&state);
                if let JobState::Abandoned = *state {
                    *state = JobState::Taken;
                    drop(state);
                    let _ = bin.set_state(gst::State::Null);
                } else {
                    *state = JobState::Done(started);
                }
            }
        };
        std::thread::Builder::new()
            .name("gst-audio-start".to_owned())
            .spawn(thread)
            .map_err(|e| MediaError::Backend(format!("cannot start audio: {e}")))?;
        self.job = Some(AttachJob {
            bin,
            bus,
            source,
            state,
            started_at: Instant::now(),
        });
        Ok(())
    }

    /// Adds a started generation to `pipeline`, or reports why it failed; never blocks on the source.
    pub(super) fn poll(&mut self, pipeline: &gst::Pipeline) -> Attach {
        let Some(job) = self.job.as_ref() else {
            return Attach::Pending;
        };
        if let Some(msg) = job.errors() {
            let why = message_text(&msg);
            self.abandon();
            return Attach::Failed(why, restart_for(Some(&msg)));
        }
        let Some(started) = job.take_result() else {
            if job.started_at.elapsed() >= AUDIO_ABANDON_FLUSH {
                self.abandon();
                return Attach::Failed(
                    format!(
                        "it did not start within {} s",
                        AUDIO_ABANDON_FLUSH.as_secs()
                    ),
                    Restart::DeviceLoss,
                );
            }
            return Attach::Pending;
        };
        let started = started.map_err(|e| format!("cannot start audio: {e}"));
        let job = self.job.take().expect("job checked above");
        if let Err(why) = started {
            let posted = job.errors();
            let _ = job.bin.set_state(gst::State::Null);
            let restart = restart_for(posted.as_ref());
            return match posted.as_ref().map(|m| m.view()) {
                Some(gst::MessageView::Error(err)) => Attach::Failed(bus_error_text(err), restart),
                _ => Attach::Failed(why, restart),
            };
        }
        if let Some(msg) = job.errors() {
            let _ = job.bin.set_state(gst::State::Null);
            return Attach::Failed(message_text(&msg), restart_for(Some(&msg)));
        }
        if let Err(e) = pipeline.add(&job.bin) {
            let _ = job.bin.set_state(gst::State::Null);
            return Attach::Failed(format!("cannot add audio: {e}"), Restart::Fault);
        }
        if let Err(e) = job.bin.sync_state_with_parent() {
            let _ = job.bin.set_state(gst::State::Null);
            let _ = pipeline.remove(&job.bin);
            return Attach::Failed(format!("cannot start audio: {e}"), Restart::Fault);
        }
        self.active = Some(job.bin);
        Attach::Attached
    }

    /// Gives up a generation still starting, without waiting for it.
    pub(super) fn abandon(&mut self) {
        if let Some(job) = self.job.take() {
            self.blocked = Some((job.state.clone(), Instant::now()));
            job.abandon();
        }
    }

    /// How long the abandoned start has been inside its state change, if it still is.
    pub(super) fn blocked_for(&self) -> Option<Duration> {
        let (state, since) = self.blocked.as_ref()?;
        let running = matches!(*lock(state), JobState::Abandoned);
        running.then(|| since.elapsed())
    }

    pub(super) fn name(&self) -> String {
        self.device
            .as_deref()
            .map_or_else(|| "(the default output)".to_owned(), |d| format!("{d:?}"))
    }

    /// Stops capturing from a device that went away; reported once per loss.
    pub(super) fn lose(&mut self, pipeline: &gst::Pipeline, why: &str, already_lost: bool) {
        self.detach(pipeline);
        if already_lost {
            tracing::debug!("audio source {} still unavailable: {why}", self.name());
            return;
        }
        tracing::warn!(
            target: AUDIO_DEVICE_TARGET,
            "audio source {} lost ({why}); reconnecting to it",
            self.name()
        );
        if let Some(activity) = &self.activity {
            activity.lost();
        }
    }

    fn detach(&mut self, pipeline: &gst::Pipeline) {
        if let Some(bin) = self.active.take() {
            // Ignored: the bin is discarded either way.
            let _ = bin.set_state(gst::State::Null);
            let _ = pipeline.remove(&bin);
        }
    }

    /// Brings the first generation up at start, failing with the device's own error.
    pub(super) fn start(
        &mut self,
        pipeline: &gst::Pipeline,
        output: &Arc<Output>,
    ) -> Result<(), MediaError> {
        self.begin(output)
            .map_err(|e| audio_start_error(self.device.as_deref(), &e.to_string()))?;
        let deadline = Instant::now() + AUDIO_START_TIMEOUT;
        loop {
            match self.poll(pipeline) {
                Attach::Attached => return Ok(()),
                Attach::Failed(why, _) => {
                    return Err(audio_start_error(self.device.as_deref(), &why));
                }
                Attach::Pending if Instant::now() >= deadline => {
                    self.abandon();
                    return Err(audio_start_error(
                        self.device.as_deref(),
                        &format!(
                            "it did not start within {} s",
                            AUDIO_START_TIMEOUT.as_secs()
                        ),
                    ));
                }
                Attach::Pending => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

fn message_text(msg: &gst::Message) -> String {
    match msg.view() {
        gst::MessageView::Error(err) => bus_error_text(err),
        _ => "audio source failed".to_owned(),
    }
}

/// The audio branch generation and chain position an error came from, current or already replaced.
fn audio_origin(err: &gst::message::Error) -> Option<(u64, usize)> {
    let name = err.src()?.name();
    let (generation, index) = name.strip_prefix(AUDIO_ELEMENT_PREFIX)?.split_once('-')?;
    Some((generation.parse().ok()?, index.parse().ok()?))
}

pub(super) fn audio_generation(err: &gst::message::Error) -> Option<u64> {
    audio_origin(err).map(|(generation, _)| generation)
}

/// Whether an audio error means the device went away, as opposed to a fault that retrying cannot fix.
pub(super) fn is_device_loss(err: &gst::message::Error) -> bool {
    use gst::ResourceError as R;
    matches!(audio_origin(err), Some((_, 0)))
        && matches!(
            err.error().kind::<R>(),
            Some(
                R::Failed
                    | R::NotFound
                    | R::Busy
                    | R::OpenRead
                    | R::OpenReadWrite
                    | R::Read
                    | R::Close
            )
        )
}

/// When to rebuild a failed audio branch: soon at first, backing off while the device stays away.
#[derive(Debug)]
pub(super) struct AudioRetry {
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
    pub(super) fn failed(&mut self, now: Instant) {
        self.next = Some(now + self.delay);
        self.delay = (self.delay * 2).min(AUDIO_RETRY_MAX);
    }

    pub(super) fn due(&mut self, now: Instant) -> bool {
        let due = self.next.is_some_and(|at| now >= at);
        if due {
            self.next = None;
        }
        due
    }

    pub(super) fn pending(&self) -> bool {
        self.next.is_some()
    }

    pub(super) fn recovered(&mut self) {
        self.delay = AUDIO_RETRY_FIRST;
    }
}

/// The message for an audio source that failed at start.
pub(super) fn audio_start_message(device: Option<&str>, err: &str) -> String {
    match device {
        Some(id) => format!(
            "audio source {id:?} is not available ({err}); D-Streamy does not fall back to another device, \
             so pick one from `dstreamy audio-sources`"
        ),
        None => format!("audio capture failed: {err}"),
    }
}

pub(super) fn audio_start_error(device: Option<&str>, err: &str) -> MediaError {
    MediaError::Backend(audio_start_message(device, err))
}

#[cfg(test)]
mod tests {
    use dstreamy_media::FRAME_CHANNEL_CAPACITY;
    use tokio::sync::mpsc;

    use super::*;
    use crate::encoder::available_factories;
    use crate::pipeline::test_support::{REAL_INPUTS, config, fake_device, init};

    #[test]
    fn audio_pts_follow_sample_count_and_rebase_after_a_gap() {
        let mut p = AudioPacketizer::new(Arc::default()).unwrap();
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
    fn audio_pts_never_step_back_when_the_clock_falls_behind() {
        let mut p = AudioPacketizer::new(Arc::default()).unwrap();
        let frame = vec![0u8; OPUS_FRAME_SAMPLES * OPUS_CHANNELS * 2];
        let pts = |frames: Vec<AudioFrame>| frames.iter().map(|f| f.pts_us).collect::<Vec<_>>();
        assert_eq!(pts(p.push(&frame, Some(100_000)).unwrap()), [100_000]);
        assert_eq!(pts(p.push(&frame, Some(120_000)).unwrap()), [120_000]);
        assert!(pts(p.push(&frame, Some(90_000)).unwrap()).is_empty());
        assert_eq!(pts(p.push(&frame, Some(110_000)).unwrap()), [140_000]);
        assert!(pts(p.push(&frame.repeat(3), Some(60_000)).unwrap()).is_empty());
        assert_eq!(pts(p.push(&frame, Some(180_000)).unwrap()), [160_000]);
    }

    #[test]
    fn a_rebuilt_audio_branch_continues_after_the_last_pts() {
        let clock = Arc::new(Mutex::new(AudioClock::default()));
        let generation = || AudioPacketizer::new(clock.clone()).unwrap();
        let frame = vec![0u8; OPUS_FRAME_SAMPLES * OPUS_CHANNELS * 2];
        let pts = |frames: Vec<AudioFrame>| frames.iter().map(|f| f.pts_us).collect::<Vec<_>>();
        let mut first = generation();
        assert_eq!(pts(first.push(&frame, Some(100_000)).unwrap()), [100_000]);
        assert_eq!(pts(first.push(&frame, Some(110_000)).unwrap()), [120_000]);
        let mut second = generation();
        assert_eq!(pts(second.push(&frame, Some(115_000)).unwrap()), [140_000]);
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
            let src =
                audio_source(AudioInput::PulseMonitor, Some("pipeweaver_stream"), true).unwrap();
            assert_eq!(
                src.property::<Option<String>>("device").as_deref(),
                Some("pipeweaver_stream")
            );
            let props = src
                .property::<Option<gst::Structure>>("stream-properties")
                .unwrap();
            assert_eq!(field(&props, "node.dont-fallback").as_deref(), Some("true"));
            let default = audio_source(AudioInput::PulseMonitor, None, true).unwrap();
            assert!(
                default
                    .property::<Option<gst::Structure>>("stream-properties")
                    .is_none()
            );
        }
        if installed.contains(&"pipewiresrc") {
            let src =
                audio_source(AudioInput::PipeWireSink, Some("pipeweaver_stream"), true).unwrap();
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
    fn only_resource_errors_of_the_audio_source_count_as_a_lost_device() {
        init();
        let loss = |name: &str, err: gst::glib::Error| {
            let fake = make("fakesrc").unwrap();
            fake.set_property("name", name);
            let msg = gst::message::Error::builder_from_error(err)
                .src(&fake)
                .build();
            match msg.view() {
                gst::MessageView::Error(e) => is_device_loss(e),
                _ => unreachable!(),
            }
        };
        let source = format!("{AUDIO_ELEMENT_PREFIX}2-0");
        let sink = format!("{AUDIO_ELEMENT_PREFIX}2-4");
        let resource = |code| gst::glib::Error::new(code, "x");
        for code in [
            gst::ResourceError::Failed,
            gst::ResourceError::NotFound,
            gst::ResourceError::OpenRead,
            gst::ResourceError::Read,
            gst::ResourceError::Busy,
            gst::ResourceError::OpenReadWrite,
            gst::ResourceError::Close,
        ] {
            assert!(loss(&source, resource(code)), "{code:?}");
        }
        assert!(!loss(&source, resource(gst::ResourceError::Settings)));
        assert!(!loss(&source, resource(gst::ResourceError::NotAuthorized)));
        assert!(!loss(
            &source,
            gst::glib::Error::new(gst::StreamError::Failed, "x")
        ));
        assert!(!loss(
            &source,
            gst::glib::Error::new(gst::CoreError::Negotiation, "x")
        ));
        assert!(!loss(&sink, resource(gst::ResourceError::Failed)));
        assert!(!loss("pulsesrc0", resource(gst::ResourceError::Failed)));
    }

    #[test]
    fn audio_passes_only_while_the_server_reports_the_chosen_device() {
        init();
        let source = make("fakesrc").unwrap();
        let admit = |current: Option<&'static str>| {
            Pin {
                source: source.downgrade(),
                device: "pipeweaver_stream".into(),
                generation: 1,
                query: Arc::new(move |_| current.map(str::to_owned)),
                tainted: AtomicBool::new(false),
            }
            .admit()
        };
        assert_eq!(admit(Some("pipeweaver_stream")), Ok(()));
        assert_eq!(
            admit(Some("alsa_input.mic")),
            Err("alsa_input.mic".to_owned())
        );
        assert_eq!(admit(Some("")), Err("an unknown device".to_owned()));
        assert_eq!(admit(None), Err("an unknown device".to_owned()));
    }

    #[test]
    fn audio_sources_never_provide_the_pipeline_clock() {
        init();
        for (input, factory) in REAL_INPUTS {
            if !available_factories().contains(&factory) {
                continue;
            }
            let (tx, _rx) = mpsc::channel(1);
            let mut branch = AudioBranch::new(
                input,
                &CaptureConfig {
                    audio_source: Some("pipeweaver_stream".into()),
                    ..config(320, 240)
                },
                None,
                AudioOptions::default(),
            );
            let (_bin, source) = branch.build(&Output::new(tx)).unwrap();
            assert!(!source.property::<bool>("provide-clock"), "{factory}");
        }
    }

    #[test]
    fn a_start_abandoned_after_it_returned_is_stopped_too() {
        init();
        let (tx, _rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
        let output = Output::new(tx);
        let mut branch = AudioBranch::new(
            AudioInput::Test { frames: -1 },
            &CaptureConfig {
                audio: true,
                ..config(320, 240)
            },
            None,
            AudioOptions::default(),
        );
        branch.begin(&output).unwrap();
        let bin = branch.job.as_ref().unwrap().bin.clone();
        let deadline = Instant::now() + Duration::from_secs(2);
        while bin.current_state() != gst::State::Paused && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(bin.current_state(), gst::State::Paused);
        branch.abandon();
        let deadline = Instant::now() + Duration::from_secs(2);
        while bin.current_state() != gst::State::Null && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(bin.current_state(), gst::State::Null);
    }

    #[test]
    fn a_tainted_pin_keeps_dropping_after_the_server_reports_the_chosen_device_again() {
        init();
        let current = Arc::new(Mutex::new("chosen".to_owned()));
        let source = make("fakesrc").unwrap();
        let pin = Pin {
            source: source.downgrade(),
            device: "chosen".into(),
            generation: 1,
            query: fake_device(&current).device_query.unwrap(),
            tainted: AtomicBool::new(false),
        };
        assert_eq!(pin.admit(), Ok(()));
        *current.lock().unwrap() = "elsewhere".into();
        assert_eq!(pin.admit(), Err("elsewhere".to_owned()));
        *current.lock().unwrap() = "chosen".into();
        assert!(
            pin.admit().is_err(),
            "audio passed again from a source that was moved"
        );
        assert!(pin.admit().is_err());
    }

    #[test]
    fn only_device_loss_errors_restart_a_failed_rebuild() {
        init();
        let message = |name: &str, err: gst::glib::Error| {
            let fake = make("fakesrc").unwrap();
            fake.set_property("name", name);
            gst::message::Error::builder_from_error(err)
                .src(&fake)
                .build()
        };
        let source = format!("{AUDIO_ELEMENT_PREFIX}3-0");
        let gone = message(
            &source,
            gst::glib::Error::new(gst::ResourceError::NotFound, "x"),
        );
        let caps = message(
            &source,
            gst::glib::Error::new(gst::CoreError::Negotiation, "x"),
        );
        assert_eq!(restart_for(Some(&gone)), Restart::DeviceLoss);
        assert_eq!(restart_for(Some(&caps)), Restart::Fault);
        assert_eq!(restart_for(None), Restart::Fault);
    }

    #[test]
    fn no_new_start_while_an_abandoned_one_is_still_in_its_state_change() {
        init();
        let mut branch = AudioBranch::new(
            AudioInput::Test { frames: -1 },
            &config(320, 240),
            None,
            AudioOptions::default(),
        );
        assert!(branch.blocked_for().is_none());
        let state = Arc::new(Mutex::new(JobState::Abandoned));
        let since = Instant::now()
            .checked_sub(Duration::from_secs(2))
            .unwrap_or_else(Instant::now);
        branch.blocked = Some((state.clone(), since));
        assert!(branch.blocked_for().is_some());
        let blocked = branch.blocked_for().unwrap();
        assert!(blocked >= Duration::from_secs(2) && blocked < AUDIO_STUCK_WARNING);
        *state.lock().unwrap() = JobState::Taken;
        assert!(branch.blocked_for().is_none());
    }
}
