use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

use super::{StreamError, StreamOptions};
use crate::DiscordClient;
use crate::transport::{ConnectionError, MediaConnection, VideoAttributes, VoiceEvent};

const POLL: Duration = Duration::from_millis(10);

pub(crate) type Events = mpsc::UnboundedReceiver<VoiceEvent>;

/// Joins the voice channel (op 4) and waits until its connection can carry media.
pub(crate) async fn join_voice(
    client: &DiscordClient,
    guild_id: Option<String>,
    channel_id: String,
    step: Duration,
) -> Result<(MediaConnection, Events), StreamError> {
    let mut voice = client.join_voice(guild_id, channel_id);
    let conn = timeout(step, voice.ready())
        .await
        .map_err(|_| StreamError::Timeout("joining voice"))??;
    Ok((conn, voice.events))
}

/// Creates Go Live on the joined channel and configures it as `src/core/stream.ts` does.
pub(crate) async fn open_stream(
    client: &DiscordClient,
    options: &StreamOptions,
    attrs: VideoAttributes,
) -> Result<(MediaConnection, Events), StreamError> {
    let step = options.step_timeout;
    let mut stream = client.create_stream()?;
    let conn = timeout(step, stream.ready())
        .await
        .map_err(|_| StreamError::Timeout("creating the stream"))??;
    timeout(step, async {
        while !conn.is_ready().await {
            sleep(POLL).await;
        }
    })
    .await
    .map_err(|_| StreamError::Timeout("connecting the peer connection"))?;
    let mut dave_version = None;
    while let Ok(event) = stream.events.try_recv() {
        match event {
            VoiceEvent::SelectProtocolAck {
                dave_version: v, ..
            } => dave_version = Some(v),
            VoiceEvent::Closed { .. } => return Err(ConnectionError::Closed.into()),
            _ => {}
        }
    }
    if dave_version != Some(0) {
        // A failed DAVE handshake should not block the stream; frames then go out unencrypted.
        let _ = timeout(options.dave_timeout, async {
            while !conn.dave_ready() {
                sleep(POLL).await;
            }
        })
        .await;
    }

    conn.set_packetizer(codec_name(options.capture.codec))
        .await?;
    conn.set_speaking(true)?;
    conn.set_video_attributes(Some(attrs))?;
    Ok((conn, stream.events))
}

pub(crate) fn initial_attributes(options: &StreamOptions) -> VideoAttributes {
    VideoAttributes {
        width: options.capture.width,
        height: options.capture.height,
        fps: options.capture.fps,
    }
}

fn codec_name(codec: dstreamy_media::VideoCodec) -> &'static str {
    match codec {
        dstreamy_media::VideoCodec::H264 => "H264",
        dstreamy_media::VideoCodec::H265 => "H265",
    }
}
