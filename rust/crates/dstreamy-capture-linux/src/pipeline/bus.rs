use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use dstreamy_media::AUDIO_DEVICE_TARGET;
use gstreamer as gst;
use gstreamer::prelude::*;

use super::audio_branch::{
    AUDIO_ELEMENT_PREFIX, AUDIO_STUCK_WARNING, Attach, AudioBranch, AudioRetry, Restart,
    audio_generation, audio_start_message, is_device_loss,
};
use super::{Output, SOURCE_GONE, bus_error_text, lock, source_path, video_gone};

const BUS_POLL: gst::ClockTime = gst::ClockTime::from_mseconds(100);
const WARNING_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// Lets one pipeline warning through per [`WARNING_LOG_INTERVAL`], counting the ones held back.
#[derive(Debug, Default)]
struct WarningLog {
    last: Option<Instant>,
    held_back: u64,
    latest: Option<String>,
}

impl WarningLog {
    /// The line to log for a new warning, or `None` while it falls within the interval after the last logged one.
    fn note(&mut self, now: Instant, text: String) -> Option<String> {
        if self
            .last
            .is_some_and(|last| now < last + WARNING_LOG_INTERVAL)
        {
            self.held_back += 1;
            self.latest = Some(text);
            return None;
        }
        self.last = Some(now);
        self.latest = None;
        Some(match std::mem::take(&mut self.held_back) {
            0 => text,
            held => format!("{text} ({held} more since the last one logged)"),
        })
    }

    /// The latest held-back warning with their count, once the interval has passed or at `stop`.
    fn flush(&mut self, now: Instant, stop: bool) -> Option<String> {
        let due = stop
            || self
                .last
                .is_none_or(|last| now >= last + WARNING_LOG_INTERVAL);
        if !due || self.held_back == 0 {
            return None;
        }
        self.last = Some(now);
        let held = std::mem::take(&mut self.held_back);
        let text = self.latest.take().unwrap_or_default();
        Some(format!(
            "{text} ({held} like this since the last one logged)"
        ))
    }
}

/// Pipeline warnings, rate-limited separately for the audio branch and everything else.
#[derive(Default)]
struct Warnings {
    audio: WarningLog,
    other: WarningLog,
}

impl Warnings {
    fn note(&mut self, w: &gst::message::Warning) {
        let from_audio = w
            .src()
            .is_some_and(|s| s.name().starts_with(AUDIO_ELEMENT_PREFIX));
        let src = source_path(w.src());
        let log = if from_audio {
            &mut self.audio
        } else {
            &mut self.other
        };
        let detail = w.debug().map(|d| format!(" ({d})")).unwrap_or_default();
        if let Some(line) = log.note(Instant::now(), format!("{src}: {}{detail}", w.error())) {
            tracing::warn!("{line}");
        }
    }

    fn flush(&mut self, stop: bool) {
        let now = Instant::now();
        for line in [self.audio.flush(now, stop), self.other.flush(now, stop)]
            .into_iter()
            .flatten()
        {
            tracing::warn!("{line}");
        }
    }
}

/// Reports frames the leaky queue dropped, at most once per [`WARNING_LOG_INTERVAL`].
struct DropReport {
    reported: u64,
    at: Instant,
}

impl DropReport {
    fn report(&mut self, dropped: u64) {
        if dropped > self.reported && self.at.elapsed() >= WARNING_LOG_INTERVAL {
            tracing::warn!(
                "video: {} frames dropped before encoding; the converter or encoder cannot keep up",
                dropped - self.reported
            );
            (self.reported, self.at) = (dropped, Instant::now());
        }
    }
}

/// Keeps the audio branch running: rebuilds it after a device loss and ends the capture on a fault.
struct AudioSupervisor {
    branch: AudioBranch,
    retry: AudioRetry,
    lost: bool,
    told_blocked: bool,
    buffers_at_attach: u64,
}

impl AudioSupervisor {
    fn new(branch: AudioBranch, output: &Output) -> Self {
        Self {
            branch,
            retry: AudioRetry::default(),
            lost: false,
            told_blocked: false,
            buffers_at_attach: output.audio_buffers.load(Ordering::Relaxed),
        }
    }

    fn lose(&mut self, pipeline: &gst::Pipeline, why: &str) {
        self.branch.lose(pipeline, why, self.lost);
        self.lost = true;
        self.retry.failed(Instant::now());
    }

    /// One supervision step; a message means the capture must end with it.
    fn tick(&mut self, pipeline: &gst::Pipeline, output: &Arc<Output>) -> Option<String> {
        let flowed = output.audio_buffers.load(Ordering::Relaxed) > 0;
        let moved = lock(&output.moved).take().filter(|(generation, _)| {
            *generation == self.branch.generation && self.branch.active.is_some()
        });
        if let Some((_, current)) = moved {
            let why = format!("the server put the stream on {current:?}");
            if !flowed {
                return Some(audio_start_message(self.branch.device.as_deref(), &why));
            }
            self.lose(pipeline, &why);
        }
        if self.branch.job.is_some() {
            match self.branch.poll(pipeline) {
                Attach::Pending => {}
                Attach::Attached => {
                    self.buffers_at_attach = output.audio_buffers.load(Ordering::Relaxed);
                }
                Attach::Failed(why, Restart::DeviceLoss) => self.lose(pipeline, &why),
                Attach::Failed(why, Restart::Fault) => {
                    return Some(format!("audio capture failed: {why}"));
                }
            }
        } else if self.retry.due(Instant::now()) {
            self.reconnect(output);
        } else if !self.retry.pending()
            && self.branch.active.is_some()
            && output.audio_buffers.load(Ordering::Relaxed) > self.buffers_at_attach
        {
            self.retry.recovered();
            if self.lost {
                self.lost = false;
                tracing::info!(target: AUDIO_DEVICE_TARGET, "audio source {} is back", self.branch.name());
            }
        }
        None
    }

    /// Starts the next generation unless an abandoned start is still stuck in the audio server.
    fn reconnect(&mut self, output: &Arc<Output>) {
        if let Some(blocked) = self.branch.blocked_for() {
            if !self.told_blocked && blocked >= AUDIO_STUCK_WARNING {
                self.told_blocked = true;
                tracing::warn!(
                    "audio source {}: an earlier start is still stuck in the audio server; \
                     trying again once it returns",
                    self.branch.name()
                );
            }
            self.retry.failed(Instant::now());
        } else {
            self.told_blocked = false;
            if let Err(e) = self.branch.begin(output) {
                tracing::debug!(
                    "audio source {}: reconnecting failed: {e}",
                    self.branch.name()
                );
                self.retry.failed(Instant::now());
            }
        }
    }

    /// Handles an error of audio branch `generation`; a message means the capture must end with it.
    fn on_error(
        &mut self,
        pipeline: &gst::Pipeline,
        err: &gst::message::Error,
        generation: u64,
        output: &Output,
    ) -> Option<String> {
        if generation != self.branch.generation || self.branch.active.is_none() {
            tracing::debug!(
                "audio: ignoring an error of a replaced source: {}",
                bus_error_text(err)
            );
            return None;
        }
        let flowed = output.audio_buffers.load(Ordering::Relaxed) > 0;
        if is_device_loss(err) && flowed {
            self.lose(pipeline, &bus_error_text(err));
            return None;
        }
        Some(if flowed {
            format!("audio capture failed: {}", bus_error_text(err))
        } else {
            audio_start_message(self.branch.device.as_deref(), &bus_error_text(err))
        })
    }
}

/// Closes the frame channel when the pipeline ends or fails; a lost audio device is reconnected by name instead.
pub(super) fn watch_bus(
    pipeline: &gst::Pipeline,
    output: &Arc<Output>,
    audio: Option<AudioBranch>,
) {
    let bus = pipeline.bus().expect("pipeline has a bus");
    let types = [
        gst::MessageType::Error,
        gst::MessageType::Eos,
        gst::MessageType::Warning,
    ];
    let mut warnings = Warnings::default();
    let mut audio = audio.map(|branch| AudioSupervisor::new(branch, output));
    let mut drops = DropReport {
        reported: 0,
        at: Instant::now(),
    };
    while !output.stop.load(Ordering::Relaxed) {
        drops.report(output.dropped_frames.load(Ordering::Relaxed));
        if let Some(message) = audio.as_mut().and_then(|a| a.tick(pipeline, output)) {
            output.fail(message);
            continue;
        }
        warnings.flush(false);
        let Some(msg) = bus.timed_pop_filtered(BUS_POLL, &types) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Warning(w) => warnings.note(w),
            gst::MessageView::Error(err) => {
                let fatal = match (audio_generation(err), audio.as_mut()) {
                    (Some(generation), Some(audio)) => {
                        audio.on_error(pipeline, err, generation, output)
                    }
                    _ if video_gone(err) => Some(SOURCE_GONE.to_owned()),
                    _ => Some(bus_error_text(err)),
                };
                if let Some(message) = fatal {
                    output.fail(message);
                }
            }
            _ => output.close(),
        }
    }
    warnings.flush(true);
    if let Some(audio) = audio.as_mut() {
        audio.branch.abandon();
    }
}

#[cfg(test)]
mod tests {
    use dstreamy_media::{CaptureConfig, FRAME_CHANNEL_CAPACITY};
    use gstreamer_app as gst_app;
    use tokio::sync::mpsc;

    use super::*;
    use crate::pipeline::audio_branch::AudioOptions;
    use crate::pipeline::test_support::{BlockingSrc, config, init, post_error_as};
    use crate::pipeline::{AudioInput, Capture};

    #[test]
    fn stop_returns_while_an_audio_rebuild_is_blocked_in_its_state_change() {
        init();
        let pipeline = gst::Pipeline::new();
        pipeline.set_state(gst::State::Playing).unwrap();
        let (tx, _rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
        let output = Output::new(tx);
        let mut branch = AudioBranch::new(
            AudioInput::Blocking,
            &CaptureConfig {
                audio: true,
                ..config(320, 240)
            },
            None,
            AudioOptions::default(),
        );
        branch.begin(&output).unwrap();
        let (source, bin) = {
            let job = branch.job.as_ref().unwrap();
            (
                job.source.clone().downcast::<BlockingSrc>().unwrap(),
                job.bin.clone(),
            )
        };
        std::thread::sleep(Duration::from_millis(200));
        assert!(matches!(branch.poll(&pipeline), Attach::Pending));
        let bus_thread = {
            let (pipeline, output) = (pipeline.clone(), output.clone());
            std::thread::spawn(move || watch_bus(&pipeline, &output, Some(branch)))
        };
        let capture = Capture {
            pipeline,
            video_sink: gst_app::AppSink::builder().build(),
            output,
            bus_thread: Some(bus_thread),
            encoder: "none",
        };
        std::thread::sleep(BUS_POLL.into());
        let stopping = Instant::now();
        capture.stop().unwrap();
        assert!(
            stopping.elapsed() < Duration::from_secs(1),
            "stop took {:?}",
            stopping.elapsed()
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while bin.current_state() != gst::State::Null && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            source.was_flushed(),
            "the abandoned start was never flushed"
        );
        assert_eq!(
            bin.current_state(),
            gst::State::Null,
            "the abandoned bin was left open"
        );
    }

    #[test]
    fn pipeline_warnings_are_logged_at_most_once_per_interval_with_a_count() {
        let t0 = Instant::now();
        let mut log = WarningLog::default();
        let w = |n: u32| format!("warning {n}");
        assert_eq!(log.note(t0, w(1)), Some(w(1)));
        assert_eq!(log.note(t0 + Duration::from_secs(1), w(2)), None);
        assert_eq!(log.note(t0 + Duration::from_secs(2), w(3)), None);
        assert_eq!(
            log.note(t0 + WARNING_LOG_INTERVAL, w(4)).as_deref(),
            Some("warning 4 (2 more since the last one logged)")
        );
        assert_eq!(log.note(t0 + WARNING_LOG_INTERVAL * 2, w(5)), Some(w(5)));
    }

    #[test]
    fn held_back_warnings_are_logged_with_the_latest_text_after_the_interval_or_at_stop() {
        let t0 = Instant::now();
        let mut log = WarningLog::default();
        let w = |n: u32| format!("warning {n}");
        assert_eq!(log.flush(t0, true), None);
        log.note(t0, w(1));
        log.note(t0 + Duration::from_secs(1), w(2));
        log.note(t0 + Duration::from_secs(2), w(3));
        assert_eq!(log.flush(t0 + Duration::from_secs(3), false), None);
        assert_eq!(
            log.flush(t0 + WARNING_LOG_INTERVAL, false).as_deref(),
            Some("warning 3 (2 like this since the last one logged)")
        );
        assert_eq!(log.flush(t0 + WARNING_LOG_INTERVAL * 2, false), None);
        log.note(t0 + WARNING_LOG_INTERVAL + Duration::from_secs(1), w(4));
        assert_eq!(
            log.flush(t0 + WARNING_LOG_INTERVAL + Duration::from_secs(2), true)
                .as_deref(),
            Some("warning 4 (1 like this since the last one logged)")
        );
    }

    #[test]
    fn the_bus_is_watched_while_an_audio_rebuild_is_blocked() {
        init();
        let pipeline = gst::Pipeline::new();
        pipeline.set_state(gst::State::Playing).unwrap();
        let (tx, _rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
        let output = Output::new(tx);
        let mut branch = AudioBranch::new(
            AudioInput::Blocking,
            &CaptureConfig {
                audio: true,
                ..config(320, 240)
            },
            None,
            AudioOptions::default(),
        );
        branch.begin(&output).unwrap();
        let bus_thread = {
            let (pipeline, output) = (pipeline.clone(), output.clone());
            std::thread::spawn(move || watch_bus(&pipeline, &output, Some(branch)))
        };
        std::thread::sleep(Duration::from_millis(200));
        post_error_as(&pipeline, "videosrc", gst::StreamError::Failed);
        let deadline = Instant::now() + Duration::from_secs(1);
        while !output.stop.load(Ordering::Relaxed) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let error = output.error.lock().unwrap().clone();
        assert!(
            error.is_some_and(|e| e.contains("injected")),
            "the video error went unhandled"
        );
        bus_thread.join().unwrap();
        let _ = pipeline.set_state(gst::State::Null);
    }
}
