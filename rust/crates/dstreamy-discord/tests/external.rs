use std::sync::Arc;
use std::time::Duration;

use dstreamy_discord::{
    Backoff, DiscordClient, GatewayOptions, Session, SessionEvent, SessionOptions, SessionState,
    StopReason, StreamOptions, StreamStats,
};
use dstreamy_media::{
    Attachment, CaptureConfig, CaptureSource, ExternalHandle, ExternalOptions, ExternalSource,
    IdleMode, OPUS_SILENCE_FRAME, ProducerInfo, TestPatternSource, VideoCodec,
};
use dstreamy_mock_discord::{
    MOCK_GUILD_ID, MOCK_TOKEN, MOCK_VOICE_CHANNEL_ID, MockDiscord, Recorded, RecordedAudioFrame,
    VoiceKind, rtc,
};
use openh264::decoder::Decoder;
use secrecy::SecretString;
use tokio::sync::{broadcast, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

const T: Duration = Duration::from_secs(20);
const FPS: u32 = 30;
const GAP: Duration = Duration::from_millis(1_500);

fn capture() -> CaptureConfig {
    CaptureConfig {
        width: 320,
        height: 180,
        fps: FPS,
        bitrate_bps: 400_000,
        codec: VideoCodec::H264,
        ..CaptureConfig::default()
    }
}

fn producer_info() -> ProducerInfo {
    ProducerInfo {
        codec: VideoCodec::H264,
        fps: FPS,
        width: 320,
        height: 180,
    }
}

async fn client(mock: &MockDiscord) -> Arc<DiscordClient> {
    let mut opts = GatewayOptions::new(SecretString::from(MOCK_TOKEN)).with_url(mock.gateway_url());
    opts.reconnect_delay_min = Duration::from_millis(10);
    opts.reconnect_delay_max = Duration::from_millis(50);
    opts.invalid_session_delay = Duration::from_millis(50);
    let client = timeout(T, DiscordClient::connect(opts))
        .await
        .unwrap()
        .unwrap();
    {
        let mut streamer = client.streamer();
        streamer.set_ice_servers(vec![]);
        streamer.set_setting_engine(rtc::loopback_settings(false));
    }
    Arc::new(client)
}

/// Feeds real-time test-pattern frames into an attachment, forcing a keyframe on every request.
struct Feeder {
    task: JoinHandle<()>,
    stop: Option<oneshot::Sender<()>>,
}

impl Feeder {
    async fn spawn(attachment: Attachment) -> Self {
        let mut pattern = TestPatternSource::new();
        let mut frames = pattern.start(capture()).await.unwrap();
        let mut requests = attachment.keyframe_requests();
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    changed = requests.changed() => {
                        if changed.is_err() { break; }
                        pattern.request_keyframe();
                    }
                    frame = frames.recv() => {
                        let Some(frame) = frame else { break };
                        if attachment.send(frame).is_err() { break; }
                    }
                }
            }
            drop(attachment);
            let _ = pattern.stop().await;
        });
        Self {
            task,
            stop: Some(stop),
        }
    }

    /// Returns when the attachment was told to go, which is no later than when the source saw it drop.
    async fn detach(mut self) -> tokio::time::Instant {
        let at = tokio::time::Instant::now();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        timeout(T, &mut self.task).await.unwrap().unwrap();
        at
    }
}

struct Fixture {
    mock: MockDiscord,
    client: Arc<DiscordClient>,
    session: Session,
    state: watch::Receiver<SessionState>,
    handle: ExternalHandle,
    states: Arc<std::sync::Mutex<Vec<SessionState>>>,
}

impl Fixture {
    async fn streaming(external: ExternalOptions) -> Self {
        let mock = MockDiscord::start().await.unwrap();
        let client = client(&mock).await;
        let session = Session::new(client.clone());
        let mut state = session.state();
        let source = ExternalSource::new(external);
        let handle = source.handle();
        let mut options = SessionOptions::new(StreamOptions::new(capture()));
        options.health_interval = Duration::from_millis(100);
        options.backoff = Backoff {
            base: Duration::from_millis(20),
            max: Duration::from_millis(200),
            max_attempts: 5,
        };
        session
            .start(
                Some(MOCK_GUILD_ID.into()),
                MOCK_VOICE_CHANNEL_ID.into(),
                Box::new(source),
                options,
            )
            .await
            .unwrap();
        wait_state(&mut state, |s| *s == SessionState::Streaming).await;
        let states = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut watcher = session.state();
        watcher.borrow_and_update();
        let seen = states.clone();
        tokio::spawn(async move {
            while watcher.changed().await.is_ok() {
                seen.lock()
                    .unwrap()
                    .push(watcher.borrow_and_update().clone());
            }
        });
        Self {
            mock,
            client,
            session,
            state,
            handle,
            states,
        }
    }

    async fn attach(&self) -> Feeder {
        Feeder::spawn(self.handle.attach(producer_info()).unwrap()).await
    }

    async fn frames_after(&self, video: usize, audio: usize) {
        self.mock
            .wait_for(T, |r| {
                (r.video.len() >= video && r.audio.len() >= audio).then_some(())
            })
            .await
            .unwrap_or_else(|| panic!("media stalled before {video} video, {audio} audio"));
    }

    async fn finish(self) {
        let _ = timeout(T, self.session.shutdown()).await.unwrap();
        Arc::try_unwrap(self.client).ok().unwrap().close().await;
        self.mock.shutdown().await;
    }
}

async fn wait_state(state: &mut watch::Receiver<SessionState>, f: impl Fn(&SessionState) -> bool) {
    let waited = timeout(T, state.wait_for(|s| f(s)))
        .await
        .map(|r| r.is_ok());
    assert_eq!(waited, Ok(true), "state stuck at {:?}", *state.borrow());
}

fn options(idle: IdleMode, grace: Duration) -> ExternalOptions {
    ExternalOptions {
        grace,
        stall: Duration::from_secs(2),
        idle,
    }
}

fn stream_video(r: &Recorded) -> Vec<&dstreamy_mock_discord::RecordedVideoFrame> {
    r.video
        .iter()
        .filter(|v| v.kind == VoiceKind::Stream)
        .collect()
}

fn stream_audio(r: &Recorded) -> Vec<&RecordedAudioFrame> {
    r.audio
        .iter()
        .filter(|a| a.kind == VoiceKind::Stream)
        .collect()
}

fn increasing(timestamps: &[u32]) {
    for w in timestamps.windows(2) {
        assert!(
            w[1].wrapping_sub(w[0]) as i32 > 0,
            "RTP timestamps not increasing: {w:?}"
        );
    }
}

fn decode_opus(frames: &[&RecordedAudioFrame]) {
    let mut err = 0;
    // SAFETY: valid constants; the decoder is destroyed below.
    let dec = unsafe { unsafe_libopus::opus_decoder_create(48_000, 2, &mut err) };
    assert_eq!(err, unsafe_libopus::OPUS_OK);
    let mut pcm = vec![0i16; 960 * 2];
    for frame in frames {
        // SAFETY: `pcm` holds one 20 ms stereo frame, which is all one packet may carry here.
        let n = unsafe {
            unsafe_libopus::opus_decode(
                dec,
                frame.data.as_ptr(),
                frame.data.len() as i32,
                pcm.as_mut_ptr(),
                960,
                0,
            )
        };
        assert_eq!(
            n,
            960,
            "Opus packet of {} bytes failed to decode",
            frame.data.len()
        );
    }
    // SAFETY: created above and not used afterwards.
    unsafe { unsafe_libopus::opus_decoder_destroy(dec) };
}

fn is_keyframe(au: &[u8]) -> bool {
    dstreamy_media::annexb::h264_is_keyframe(au)
}

/// Splits stream video at the largest RTP gap; returns the index of the first frame after it.
fn after_gap(video: &[&dstreamy_mock_discord::RecordedVideoFrame]) -> usize {
    let gaps = video
        .windows(2)
        .map(|w| w[1].rtp_timestamp.wrapping_sub(w[0].rtp_timestamp));
    let (i, _) = gaps.enumerate().max_by_key(|&(_, g)| g).unwrap();
    i + 1
}

#[tokio::test(flavor = "multi_thread")]
async fn a_producer_swap_keeps_streaming_with_continuous_decodable_media() {
    let f = Fixture::streaming(options(IdleMode::Documented, Duration::from_secs(30))).await;
    let feeder = f.attach().await;
    f.frames_after(40, 60).await;
    feeder.detach().await;
    sleep(GAP).await;
    let before = f.mock.recorded();
    let feeder = f.attach().await;
    let (v0, a0) = (before.video.len(), before.audio.len());
    f.frames_after(v0 + 40, a0 + 60).await;
    feeder.detach().await;

    let recorded = f.mock.recorded();
    let video = stream_video(&recorded);
    let audio = stream_audio(&recorded);
    increasing(&video.iter().map(|v| v.rtp_timestamp).collect::<Vec<_>>());
    increasing(&audio.iter().map(|a| a.rtp_timestamp).collect::<Vec<_>>());

    let split = after_gap(&video);
    assert_eq!(
        recorded.dropped_frames.len(),
        before.dropped_frames.len(),
        "frames lost after the reattach: {:?}",
        &recorded.dropped_frames[before.dropped_frames.len()..]
    );
    // davey may reject the first keyframe after DAVE comes up; video then resumes at the next one.
    let start = video.iter().position(|v| is_keyframe(&v.data)).unwrap();
    if start > 0 {
        assert!(
            !before.dropped_frames.is_empty()
                && before
                    .dropped_frames
                    .iter()
                    .all(|d| d.starts_with("DAVE decrypt (video)")),
            "video before the first keyframe; drops: {:?}",
            before.dropped_frames
        );
    }
    assert!(split > start);
    assert!(
        is_keyframe(&video[split].data),
        "first video after the gap is not a keyframe"
    );
    let gap_ticks = video[split]
        .rtp_timestamp
        .wrapping_sub(video[split - 1].rtp_timestamp);
    let gap = Duration::from_micros(u64::from(gap_ticks) * 1_000 / 90);
    assert!(
        gap >= GAP && gap < GAP + Duration::from_secs(1),
        "video RTP gap {gap:?}"
    );

    let mut decoder = Decoder::new().unwrap();
    for (i, au) in video.iter().enumerate().skip(start) {
        decoder
            .decode(&au.data)
            .unwrap_or_else(|e| panic!("video {i} failed to decode: {e}"));
    }
    let media: Vec<&RecordedAudioFrame> = audio
        .iter()
        .copied()
        .filter(|a| a.data != OPUS_SILENCE_FRAME)
        .collect();
    decode_opus(&media);

    assert_eq!(*f.state.borrow(), SessionState::Streaming);
    assert!(
        f.states.lock().unwrap().is_empty(),
        "{:?}",
        f.states.lock().unwrap()
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_reattach_the_session_stops_with_capture_ended_after_grace() {
    let grace = Duration::from_secs(2);
    let mut f = Fixture::streaming(options(IdleMode::Documented, grace)).await;
    let feeder = f.attach().await;
    f.frames_after(20, 30).await;
    let detached = feeder.detach().await;
    wait_state(&mut f.state, |s| {
        *s == SessionState::Stopped {
            reason: StopReason::CaptureEnded,
        }
    })
    .await;
    let elapsed = detached.elapsed();
    assert!(
        elapsed >= grace && elapsed < grace + Duration::from_secs(2),
        "{elapsed:?}"
    );
    f.finish().await;
}

async fn health_where(
    events: &mut broadcast::Receiver<SessionEvent>,
    f: impl Fn(&StreamStats) -> bool,
) -> StreamStats {
    timeout(T, async {
        loop {
            match events.recv().await {
                Ok(SessionEvent::Health(h)) if f(&h.stats) => return h.stats,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("session events: {e}"),
            }
        }
    })
    .await
    .expect("health event")
}

// Source: vendor discord-stream-rs `voice/webrtc.rs:332-344` (patch f5b44f0); RFC 4585 §6.3.1, RFC 5104 §4.3.1.
#[tokio::test(flavor = "multi_thread")]
async fn a_viewer_pli_reaches_the_attachment_keyframe_requests() {
    let f = Fixture::streaming(options(IdleMode::Documented, Duration::from_secs(30))).await;
    let mut events = f.session.subscribe();
    let attachment = f.handle.attach(producer_info()).unwrap();
    let mut requests = attachment.keyframe_requests();
    let feeder = Feeder::spawn(attachment).await;
    f.frames_after(20, 30).await;
    let before = health_where(&mut events, |s| s.dave_keyframe_requests == 1).await;
    sleep(Duration::from_millis(600)).await;
    let before_pli = *requests.borrow_and_update();
    assert!(f.mock.send_pli(VoiceKind::Stream).await);
    timeout(T, requests.changed()).await.unwrap().unwrap();
    let stats = health_where(&mut events, |s| s.viewer_keyframe_requests == 1).await;
    assert_eq!(*requests.borrow(), before_pli + 1);
    assert_eq!(
        stats.dave_keyframe_requests, before.dave_keyframe_requests,
        "{stats:?}"
    );
    feeder.detach().await;
    f.finish().await;
}

/// The idle silence of one gap: a fresh libopus encoder's first packet is also `F8 FF FE`, so only the run ending at the reattach counts.
async fn silence_during_gap(idle: IdleMode) -> (Vec<u32>, Duration) {
    let f = Fixture::streaming(options(idle, Duration::from_secs(30))).await;
    let feeder = f.attach().await;
    f.frames_after(20, 30).await;
    let detached = feeder.detach().await;
    sleep(GAP).await;
    let sent_before = f.mock.recorded().audio.len();
    let feeder = f.attach().await;
    let reattached = detached.elapsed();
    let producer_audio =
        |a: &RecordedAudioFrame| a.kind == VoiceKind::Stream && a.data != OPUS_SILENCE_FRAME;
    let gap = f
        .mock
        .wait_for(T, |r| {
            r.audio[sent_before..]
                .iter()
                .any(producer_audio)
                .then(|| r.clone())
        })
        .await
        .expect("producer audio after the reattach");
    let resumed = sent_before
        + gap.audio[sent_before..]
            .iter()
            .position(producer_audio)
            .unwrap();
    f.frames_after(gap.video.len() + 10, gap.audio.len() + 20)
        .await;
    feeder.detach().await;
    let in_gap: Vec<&RecordedAudioFrame> = gap.audio[..resumed]
        .iter()
        .filter(|a| a.kind == VoiceKind::Stream)
        .collect();
    let mut silence: Vec<u32> = in_gap
        .iter()
        .rev()
        .take_while(|a| a.data == OPUS_SILENCE_FRAME)
        .map(|a| a.rtp_timestamp)
        .collect();
    silence.reverse();
    for pair in silence.windows(2) {
        assert_eq!(
            pair[1].wrapping_sub(pair[0]),
            960,
            "silence frames 20 ms apart (RFC 6716 config 31)"
        );
    }
    let recorded = f.mock.recorded();
    let audio = stream_audio(&recorded);
    increasing(&audio.iter().map(|a| a.rtp_timestamp).collect::<Vec<_>>());
    assert_eq!(*f.state.borrow(), SessionState::Streaming);
    f.finish().await;
    (silence, reattached)
}

#[tokio::test(flavor = "multi_thread")]
async fn documented_idle_sends_exactly_five_silence_frames_in_a_gap() {
    assert_eq!(silence_during_gap(IdleMode::Documented).await.0.len(), 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn no_idle_sends_no_silence_in_a_gap() {
    assert!(silence_during_gap(IdleMode::None).await.0.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn continuous_idle_sends_silence_every_20ms_through_a_gap() {
    let (silence, reattached) = silence_during_gap(IdleMode::Continuous).await;
    let (first, last) = (silence[0], *silence.last().unwrap());
    let covered = last.wrapping_sub(first) / 960 + 1;
    let at_least = (GAP.as_millis() / 20) as u32 - 1;
    let at_most = (reattached.as_millis() / 20) as u32 + 1;
    assert!(
        (at_least..=at_most).contains(&covered),
        "silence covers {covered} frames; gap held {at_least}..={at_most}"
    );
}
