use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};
use tracing::warn;

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
    let conn = timeout(step, async {
        let conn = voice.ready().await?;
        // Discord ignores Create Stream until the voice media link is connected.
        while !conn.is_ready().await {
            sleep(POLL).await;
        }
        Ok::<_, StreamError>(conn)
    })
    .await
    .map_err(|_| StreamError::Timeout("joining voice"))??;
    Ok((conn, voice.events))
}

/// Creates Go Live on the joined channel and configures it as the Bun daemon this crate replaced did.
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
            VoiceEvent::MediaFailed { reason } => {
                return Err(ConnectionError::MediaFailed(reason).into());
            }
            _ => {}
        }
    }
    if dave_version == Some(0) {
        warn!("Discord negotiated DAVE protocol 0: this stream is not end-to-end encrypted");
    } else {
        // A failed DAVE handshake should not block the stream; frames then go out unencrypted.
        let ready = timeout(options.dave_timeout, async {
            while !conn.dave_ready() {
                sleep(POLL).await;
            }
        })
        .await;
        if ready.is_err() {
            warn!(
                "DAVE did not come up within {:?}; streaming without end-to-end encryption",
                options.dave_timeout
            );
        }
    }
    if options.require_e2ee {
        conn.require_e2ee()
            .map_err(|_| StreamError::E2eeUnavailable(conn.dave_protocol_version()))?;
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
