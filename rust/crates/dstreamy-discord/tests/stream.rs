use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use discord_stream_rs::{rewrite_sps_vui, split_nalu};
use dstreamy_discord::{DiscordClient, GatewayOptions, StreamOptions, StreamStats, start_stream};
use dstreamy_media::{
    AudioEncoder, AudioFrame, CaptureConfig, CaptureSource, CaptureTarget, EncodedVideoFrame,
    FRAME_CHANNEL_CAPACITY, FrameReceiver, MediaError, MediaFrame, OPUS_CHANNELS,
    OPUS_FRAME_DURATION_US, OPUS_FRAME_SAMPLES, OpusEncoder, TestPatternSource, VideoCodec,
};
use dstreamy_mock_discord::depacketize::nal_units;
use dstreamy_mock_discord::opcodes::{gateway as gw, voice as vo};
use dstreamy_mock_discord::{
    MOCK_GUILD_ID, MOCK_TOKEN, MOCK_VOICE_CHANNEL_ID, MockConfig, MockDiscord, Recorded,
    VideoCodec as MockCodec, VoiceKind, rtc,
};
use secrecy::SecretString;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout};

const T: Duration = Duration::from_secs(20);
const H265_FIXTURE: &[u8] = include_bytes!("fixtures/h265-320x240.aus");

async fn client(mock: &MockDiscord) -> Arc<DiscordClient> {
    let opts = GatewayOptions::new(SecretString::from(MOCK_TOKEN)).with_url(mock.gateway_url());
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

fn capture(codec: VideoCodec, width: u32, height: u32) -> CaptureConfig {
    CaptureConfig {
        width,
        height,
        fps: 30,
        bitrate_bps: 400_000,
        codec,
        ..CaptureConfig::default()
    }
}

struct Tee {
    inner: Box<dyn CaptureSource>,
    seen: Arc<Mutex<Vec<MediaFrame>>>,
    forward: Option<JoinHandle<()>>,
}

impl Tee {
    fn new(inner: impl CaptureSource + 'static) -> (Self, Arc<Mutex<Vec<MediaFrame>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let tee = Self {
            inner: Box::new(inner),
            seen: seen.clone(),
            forward: None,
        };
        (tee, seen)
    }
}

#[async_trait]
impl CaptureSource for Tee {
    async fn list_targets(&self) -> Result<Vec<CaptureTarget>, MediaError> {
        self.inner.list_targets().await
    }

    async fn start(&mut self, config: CaptureConfig) -> Result<FrameReceiver, MediaError> {
        let mut rx = self.inner.start(config).await?;
        let (tx, out) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
        let seen = self.seen.clone();
        self.forward = Some(tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                seen.lock().unwrap().push(frame.clone());
                if tx.send(frame).await.is_err() {
                    return;
                }
            }
        }));
        Ok(out)
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        let result = self.inner.stop().await;
        if let Some(forward) = self.forward.take() {
            let _ = forward.await;
        }
        result
    }

    fn request_keyframe(&self) {
        self.inner.request_keyframe();
    }
}

/// Replays recorded access units in real time, looping, with a 440 Hz Opus tone.
struct ReplaySource {
    units: Vec<EncodedVideoFrame>,
    task: Option<JoinHandle<()>>,
    keyframe_requested: Arc<AtomicBool>,
}

impl ReplaySource {
    fn new(codec: VideoCodec, fixture: &[u8]) -> Self {
        let mut units = Vec::new();
        let mut rest = fixture;
        while !rest.is_empty() {
            let pts_us = u64::from_le_bytes(rest[..8].try_into().unwrap());
            let keyframe = rest[8] != 0;
            let len = u32::from_le_bytes(rest[9..13].try_into().unwrap()) as usize;
            units.push(EncodedVideoFrame {
                codec,
                data: rest[13..13 + len].to_vec(),
                pts_us,
                keyframe,
            });
            rest = &rest[13 + len..];
        }
        assert!(units[0].keyframe);
        Self {
            units,
            task: None,
            keyframe_requested: Arc::default(),
        }
    }
}

#[async_trait]
impl CaptureSource for ReplaySource {
    async fn list_targets(&self) -> Result<Vec<CaptureTarget>, MediaError> {
        Ok(Vec::new())
    }

    async fn start(&mut self, config: CaptureConfig) -> Result<FrameReceiver, MediaError> {
        let (tx, rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
        let units = self.units.clone();
        let keyframe_requested = self.keyframe_requested.clone();
        let mut opus = OpusEncoder::new(64_000)?;
        let frame_us = 1_000_000 / u64::from(config.fps);
        self.task = Some(tokio::spawn(async move {
            let started = Instant::now();
            let mut pcm = vec![0i16; OPUS_FRAME_SAMPLES * OPUS_CHANNELS];
            let mut sample = 0u64;
            let (mut index, mut base, mut video_pts, mut audio_pts) = (0, 0, 0, 0);
            loop {
                let frame = if audio_pts < video_pts {
                    for pair in pcm.chunks_mut(OPUS_CHANNELS) {
                        let phase = sample as f32 * 440.0 / 48_000.0 * std::f32::consts::TAU;
                        pair.fill((phase.sin() * 8000.0) as i16);
                        sample += 1;
                    }
                    let frame = MediaFrame::Audio(opus.encode(&pcm, audio_pts).unwrap());
                    audio_pts += OPUS_FRAME_DURATION_US;
                    frame
                } else {
                    if keyframe_requested.swap(false, Ordering::Relaxed) || index == units.len() {
                        base = video_pts;
                        index = 0;
                    }
                    let mut unit = units[index].clone();
                    unit.pts_us += base;
                    video_pts = unit.pts_us + frame_us;
                    index += 1;
                    MediaFrame::Video(unit)
                };
                tokio::time::sleep_until(started + Duration::from_micros(frame.pts_us())).await;
                if let Err(mpsc::error::TrySendError::Closed(_)) = tx.try_send(frame) {
                    return;
                }
            }
        }));
        Ok(rx)
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
        Ok(())
    }

    fn request_keyframe(&self) {
        self.keyframe_requested.store(true, Ordering::Relaxed);
    }
}

fn expected_nalus(codec: VideoCodec, au: &[u8]) -> Vec<Vec<u8>> {
    split_nalu(au)
        .into_iter()
        .map(|n| match codec {
            VideoCodec::H264 if n[0] & 0x1F == 7 => rewrite_sps_vui(n),
            _ => n.to_vec(),
        })
        .collect()
}

fn stream_ops(recorded: &Recorded, op: u8) -> Vec<Value> {
    recorded
        .ops
        .iter()
        .filter(|o| o.is(Some(VoiceKind::Stream), op))
        .filter_map(|o| o.json().cloned())
        .collect()
}

fn has_video_attributes(recorded: &Recorded, width: u32, height: u32, fps: u32) -> bool {
    stream_ops(recorded, vo::OP_VIDEO).iter().any(|d| {
        let s = &d["streams"][0];
        s["max_resolution"]["width"] == width
            && s["max_resolution"]["height"] == height
            && s["max_framerate"] == fps
            && s["active"] == true
    })
}

async fn wait_sent(stats: impl Fn() -> StreamStats, video: u64) {
    timeout(T, async {
        while stats().video_frames_sent < video {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("stream stalled: {:?}", stats()));
}

/// Received items found in `sent` in the same order; gaps on either side are skipped.
fn matched_in_order<T: PartialEq>(sent: &[T], got: &[T]) -> usize {
    let mut next = 0;
    let mut matched = 0;
    for item in got {
        if let Some(i) = sent[next..].iter().position(|s| s == item) {
            next += i + 1;
            matched += 1;
        }
    }
    matched
}

fn matched_video(codec: VideoCodec, sent: &[EncodedVideoFrame], recorded: &Recorded) -> usize {
    let want: Vec<Vec<Vec<u8>>> = sent
        .iter()
        .map(|f| expected_nalus(codec, &f.data))
        .collect();
    let got: Vec<Vec<Vec<u8>>> = recorded
        .video
        .iter()
        .filter(|f| f.kind == VoiceKind::Stream)
        .map(|f| nal_units(&f.data).into_iter().map(<[u8]>::to_vec).collect())
        .collect();
    matched_in_order(&want, &got)
}

async fn streams_end_to_end(
    mock_codec: MockCodec,
    source: Tee,
    seen: Arc<Mutex<Vec<MediaFrame>>>,
    config: CaptureConfig,
) {
    let mock = MockDiscord::start_with(MockConfig {
        video_codec: mock_codec,
        ..MockConfig::default()
    })
    .await
    .unwrap();
    let client = client(&mock).await;
    let codec = config.codec;
    let (width, height) = (config.width, config.height);
    let handle = timeout(
        T,
        start_stream(
            client.clone(),
            Some(MOCK_GUILD_ID.into()),
            MOCK_VOICE_CHANNEL_ID.into(),
            Box::new(source),
            StreamOptions::new(config),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(mock.dave_active(VoiceKind::Stream).await);
    assert_eq!(handle.video_attributes().width, width);

    wait_sent(|| handle.stats(), 45).await;
    handle.update_video(width / 2, height / 2, 15).unwrap();
    assert_eq!(handle.video_attributes().fps, 15);
    mock.wait_for(T, |r| {
        has_video_attributes(r, width / 2, height / 2, 15).then_some(())
    })
    .await
    .expect("op 12 with the updated attributes");
    wait_sent(|| handle.stats(), 75).await;

    let stats = handle.stats();
    assert!((20.0..=40.0).contains(&stats.fps), "{stats:?}");
    assert!(stats.audio_frames_sent > 50, "{stats:?}");
    assert!(stats.bytes_sent > 0);
    assert!(!stats.connection_closed);
    let recorded = mock
        .wait_for(T, |r| {
            let video = r
                .video
                .iter()
                .filter(|f| f.kind == VoiceKind::Stream)
                .count();
            let audio = r
                .audio
                .iter()
                .filter(|f| f.kind == VoiceKind::Stream)
                .count();
            // davey drops frames whose nonce precedes the first it decrypted; audio can overtake video.
            let lost = r.dropped_frames.len();
            (video + audio + lost >= (stats.video_frames_sent + stats.audio_frames_sent) as usize)
                .then(|| r.clone())
        })
        .await
        .unwrap_or_else(|| {
            panic!(
                "frames missing: {stats:?} vs {:?}",
                mock.recorded().dropped_frames
            )
        });
    let final_stats = timeout(T, handle.stop()).await.unwrap().unwrap();
    assert!(final_stats.video_frames_sent >= stats.video_frames_sent);
    mock.wait_for_op(None, gw::OP_STREAM_DELETE, 0, T)
        .await
        .expect("STREAM_DELETE");

    let seen = seen.lock().unwrap().clone();
    let sent_video: Vec<EncodedVideoFrame> = seen
        .iter()
        .filter_map(|f| match f {
            MediaFrame::Video(v) => Some(v.clone()),
            MediaFrame::Audio(_) => None,
        })
        .collect();
    let matched = matched_video(codec, &sent_video, &recorded);
    assert!(
        matched as f64 >= 0.95 * stats.video_frames_sent as f64,
        "{matched} bit-identical of {stats:?}"
    );
    assert!(
        stats.video_frames_dropped * 20 <= stats.video_frames_sent,
        "{stats:?}"
    );

    let rec_video: Vec<_> = recorded
        .video
        .iter()
        .filter(|f| f.kind == VoiceKind::Stream)
        .collect();
    assert!(rec_video.iter().all(|f| f.e2ee && f.codec == mock_codec));
    assert!(
        rec_video
            .windows(2)
            .all(|w| w[1].rtp_timestamp.wrapping_sub(w[0].rtp_timestamp) as i32 > 0),
        "video RTP timestamps not increasing"
    );

    let sent_audio: Vec<&[u8]> = seen
        .iter()
        .filter_map(|f| match f {
            MediaFrame::Audio(AudioFrame { data, .. }) => Some(&data[..]),
            MediaFrame::Video(_) => None,
        })
        .collect();
    let rec_audio: Vec<_> = recorded
        .audio
        .iter()
        .filter(|f| f.kind == VoiceKind::Stream)
        .collect();
    assert!(rec_audio.iter().all(|f| f.e2ee));
    let got_audio: Vec<&[u8]> = rec_audio.iter().map(|f| &f.data[..]).collect();
    let matched = matched_in_order(&sent_audio, &got_audio);
    assert!(
        matched as f64 >= 0.95 * stats.audio_frames_sent as f64,
        "{matched} audio frames in order of {stats:?}"
    );

    assert_eq!(stream_ops(&recorded, vo::OP_SPEAKING)[0]["speaking"], 2);
    assert!(has_video_attributes(&recorded, width, height, 30));
    assert!(mock.streams().is_empty());

    Arc::try_unwrap(client)
        .ok()
        .expect("handle released the client")
        .close()
        .await;
    mock.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn h264_test_pattern_streams_through_the_mock() {
    let (source, seen) = Tee::new(TestPatternSource::new());
    streams_end_to_end(
        MockCodec::H264,
        source,
        seen,
        capture(VideoCodec::H264, 320, 180),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn h265_replay_streams_through_the_mock() {
    let (source, seen) = Tee::new(ReplaySource::new(VideoCodec::H265, H265_FIXTURE));
    streams_end_to_end(
        MockCodec::H265,
        source,
        seen,
        capture(VideoCodec::H265, 320, 240),
    )
    .await;
}

/// Synthetic H.264 AUs of `size` bytes, as fast as the receiver takes them; keyframe on request.
struct FloodSource {
    size: usize,
    task: Option<JoinHandle<()>>,
    keyframe_requests: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
}

impl FloodSource {
    fn new(size: usize) -> Self {
        Self {
            size,
            task: None,
            keyframe_requests: Arc::default(),
            stopped: Arc::default(),
        }
    }
}

#[async_trait]
impl CaptureSource for FloodSource {
    async fn list_targets(&self) -> Result<Vec<CaptureTarget>, MediaError> {
        Ok(Vec::new())
    }

    async fn start(&mut self, _config: CaptureConfig) -> Result<FrameReceiver, MediaError> {
        let (tx, rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
        let size = self.size;
        let requests = self.keyframe_requests.clone();
        self.task = Some(tokio::spawn(async move {
            let mut served = 0;
            for index in 0u64.. {
                let wanted = requests.load(Ordering::Relaxed);
                let keyframe = index % 300 == 0 || wanted > served;
                served = wanted;
                let mut data = vec![0, 0, 0, 1, if keyframe { 0x65 } else { 0x41 }];
                data.resize(size, 0x55);
                let frame = MediaFrame::Video(EncodedVideoFrame {
                    codec: VideoCodec::H264,
                    data,
                    pts_us: index * 33_333,
                    keyframe,
                });
                if tx.send(frame).await.is_err() {
                    return;
                }
                sleep(Duration::from_millis(1)).await;
            }
        }));
        Ok(rx)
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
        self.stopped.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn request_keyframe(&self) {
        self.keyframe_requests.fetch_add(1, Ordering::Relaxed);
    }
}

async fn start(
    client: &Arc<DiscordClient>,
    source: impl CaptureSource + 'static,
) -> dstreamy_discord::StreamHandle {
    timeout(
        T,
        start_stream(
            client.clone(),
            Some(MOCK_GUILD_ID.into()),
            MOCK_VOICE_CHANNEL_ID.into(),
            Box::new(source),
            StreamOptions::new(capture(VideoCodec::H264, 320, 180)),
        ),
    )
    .await
    .unwrap()
    .unwrap()
}

async fn left_voice(mock: &MockDiscord) {
    mock.wait_for(T, |r| {
        r.ops
            .iter()
            .filter(|o| o.is(None, gw::OP_VOICE_STATE_UPDATE))
            .any(|o| o.json().unwrap()["channel_id"].is_null())
            .then_some(())
    })
    .await
    .expect("voice left");
}

#[tokio::test(flavor = "multi_thread")]
async fn transport_lag_drops_video_until_a_requested_keyframe() {
    let mock = MockDiscord::start().await.unwrap();
    let client = client(&mock).await;
    let source = FloodSource::new(100_000);
    let requests = source.keyframe_requests.clone();
    let handle = start(&client, source).await;
    timeout(T, async {
        while handle.stats().keyframe_requests == 0 {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no keyframe requested: {:?}", handle.stats()));
    let lagged = handle.stats();
    assert!(lagged.video_frames_dropped > 0, "{lagged:?}");
    assert!(requests.load(Ordering::Relaxed) > 0);
    wait_sent(|| handle.stats(), lagged.video_frames_sent + 5).await;

    let stats = timeout(T, handle.stop()).await.unwrap().unwrap();
    assert!(stats.video_frames_sent > lagged.video_frames_sent);
    let keyframes = mock
        .recorded()
        .video
        .iter()
        .filter(|f| nal_units(&f.data).first().is_some_and(|n| n[0] & 0x1F == 5))
        .count();
    assert!(keyframes >= 1);
    Arc::try_unwrap(client).ok().unwrap().close().await;
    mock.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_handle_ends_go_live_and_stops_the_source() {
    let mock = MockDiscord::start().await.unwrap();
    let client = client(&mock).await;
    let source = FloodSource::new(2_000);
    let stopped = source.stopped.clone();
    let handle = start(&client, source).await;
    wait_sent(|| handle.stats(), 5).await;
    drop(handle);
    mock.wait_for_op(None, gw::OP_STREAM_DELETE, 0, T)
        .await
        .expect("STREAM_DELETE");
    left_voice(&mock).await;
    timeout(T, async {
        while !stopped.load(Ordering::Relaxed) {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("source stopped");
    Arc::try_unwrap(client).ok().unwrap().close().await;
    mock.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_start_leaves_voice() {
    let mock = MockDiscord::start().await.unwrap();
    let client = client(&mock).await;
    let started = timeout(
        Duration::from_millis(1),
        start_stream(
            client.clone(),
            Some(MOCK_GUILD_ID.into()),
            MOCK_VOICE_CHANNEL_ID.into(),
            Box::new(FloodSource::new(2_000)),
            StreamOptions::default(),
        ),
    )
    .await;
    assert!(started.is_err(), "start finished within 1 ms");
    left_voice(&mock).await;
    assert!(mock.streams().is_empty());
    Arc::try_unwrap(client).ok().unwrap().close().await;
    mock.shutdown().await;
}

#[test]
fn stop_ends_every_task_it_started() {
    let mock_runtime = tokio::runtime::Runtime::new().unwrap();
    let mock = mock_runtime.block_on(MockDiscord::start()).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let client = client(&mock).await;
        let metrics = tokio::runtime::Handle::current().metrics();
        sleep(Duration::from_millis(200)).await;
        let baseline = metrics.num_alive_tasks();

        let (source, _) = Tee::new(TestPatternSource::new());
        let handle = start(&client, source).await;
        wait_sent(|| handle.stats(), 10).await;
        assert!(metrics.num_alive_tasks() > baseline);
        timeout(T, handle.stop()).await.unwrap().unwrap();

        timeout(T, async {
            while metrics.num_alive_tasks() > baseline {
                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{} tasks alive after stop, {baseline} before start",
                metrics.num_alive_tasks()
            )
        });
        Arc::try_unwrap(client).ok().unwrap().close().await;
    });
    mock_runtime.block_on(mock.shutdown());
}
