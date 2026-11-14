mod common;

use async_trait::async_trait;
use common::{annexb, answer_like_discord, loopback_settings};
use discord_stream_rs::dave::DaveHandler;
use discord_stream_rs::voice::webrtc::{MAX_VIDEO_FRAME_BYTES, WebRtcWrapper};
use discord_stream_rs::voice::{WebRtcParams, codec_payload};
use dstreamy_mock_discord::dave::DaveServer;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::num::NonZeroU16;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS, MediaEngine};
use webrtc::ice::udp_mux::{UDPMuxDefault, UDPMuxParams};
use webrtc::ice::udp_network::UDPNetwork;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::rtp::packet::Packet;
use webrtc::rtp::packetizer::Depacketizer;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::util::Conn;

const AUDIO_SSRC: u32 = 1111;
const VIDEO_SSRC: u32 = 2222;
const CLIENT: u64 = 42;
const CHANNEL: u64 = 7;

/// (ssrc, seq, marker, timestamp) of each RTP packet in the order it reached the socket; SRTP leaves the header clear.
struct WireTap {
    socket: UdpSocket,
    wire: mpsc::UnboundedSender<WireKey>,
}

impl WireTap {
    fn note(&self, buf: &[u8]) {
        // RTP/RTCP first byte 128..=191 (RFC 7983 §7); RTCP packet types 192..=223 (RFC 5761 §4).
        if buf.len() >= 12 && (128..=191).contains(&buf[0]) && !(192..=223).contains(&buf[1]) {
            let ssrc = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
            let seq = u16::from_be_bytes([buf[2], buf[3]]);
            let ts = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
            let _ = self.wire.send((ssrc, seq, buf[1] & 0x80 != 0, ts));
        }
    }
}

#[async_trait]
impl Conn for WireTap {
    async fn connect(&self, addr: SocketAddr) -> webrtc::util::Result<()> {
        Ok(self.socket.connect(addr).await?)
    }
    async fn recv(&self, buf: &mut [u8]) -> webrtc::util::Result<usize> {
        let n = self.socket.recv(buf).await?;
        self.note(&buf[..n]);
        Ok(n)
    }
    async fn recv_from(&self, buf: &mut [u8]) -> webrtc::util::Result<(usize, SocketAddr)> {
        let (n, addr) = self.socket.recv_from(buf).await?;
        self.note(&buf[..n]);
        Ok((n, addr))
    }
    async fn send(&self, buf: &[u8]) -> webrtc::util::Result<usize> {
        Ok(self.socket.send(buf).await?)
    }
    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> webrtc::util::Result<usize> {
        Ok(self.socket.send_to(buf, target).await?)
    }
    fn local_addr(&self) -> webrtc::util::Result<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }
    fn remote_addr(&self) -> Option<SocketAddr> {
        None
    }
    async fn close(&self) -> webrtc::util::Result<()> {
        Ok(())
    }
    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

type WireKey = (u32, u16, bool, u32);
type Wire = mpsc::UnboundedReceiver<WireKey>;

async fn tapped_discord_peer() -> (
    Arc<RTCPeerConnection>,
    mpsc::UnboundedReceiver<Packet>,
    Wire,
) {
    let mut m = MediaEngine::default();
    for (mime_type, clock_rate, channels, payload_type, kind) in [
        (
            MIME_TYPE_OPUS,
            48_000,
            2,
            codec_payload::OPUS.payload_type,
            RTPCodecType::Audio,
        ),
        (
            MIME_TYPE_H264,
            90_000,
            0,
            codec_payload::H264.payload_type,
            RTPCodecType::Video,
        ),
    ] {
        m.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: mime_type.to_owned(),
                    clock_rate,
                    channels,
                    ..Default::default()
                },
                payload_type,
                ..Default::default()
            },
            kind,
        )
        .unwrap();
    }
    let (wire_tx, wire_rx) = mpsc::unbounded_channel();
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mux = UDPMuxDefault::new(UDPMuxParams::new(WireTap {
        socket,
        wire: wire_tx,
    }));
    let mut settings = loopback_settings(true);
    settings.set_udp_network(UDPNetwork::Muxed(mux));
    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_setting_engine(settings)
        .build();
    let pc = Arc::new(
        api.new_peer_connection(RTCConfiguration::default())
            .await
            .unwrap(),
    );
    let (tx, rx) = mpsc::unbounded_channel();
    pc.on_track(Box::new(move |track, _, _| {
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Ok((pkt, _)) = track.read_rtp().await {
                let _ = tx.send(pkt);
            }
        });
        Box::pin(async {})
    }));
    (pc, rx, wire_rx)
}

fn ready_dave() -> (Arc<StdMutex<DaveHandler>>, DaveServer) {
    let mut server = DaveServer::new(NonZeroU16::new(1).unwrap(), CLIENT, CHANNEL, 9999).unwrap();
    let mut client = DaveHandler::new(CLIENT, CHANNEL);
    let key_package = client.init(1).unwrap().unwrap();
    client
        .set_external_sender(server.external_sender_package())
        .unwrap();
    let proposals = server.on_key_package(&key_package).unwrap();
    let cw = client
        .process_proposals(proposals[0], &proposals[1..], &[9999])
        .unwrap()
        .unwrap();
    let mut payload = cw.commit;
    payload.extend(cw.welcome.unwrap());
    let (_, announce) = server.on_commit_welcome(&payload).unwrap();
    client.process_commit(&announce[2..]).unwrap();
    assert!(client.is_ready());
    (Arc::new(StdMutex::new(client)), server)
}

/// The truncated nonce of a DAVE frame: LEB128 right after the 8-byte tag (davey `cryptor/encryptor.rs:196-224`).
fn dave_nonce(frame: &[u8]) -> u64 {
    assert_eq!(frame[frame.len() - 2..], [0xFA, 0xFA], "not a DAVE frame");
    let supplemental = frame[frame.len() - 3] as usize;
    let mut value = 0u64;
    for (i, b) in frame[frame.len() - supplemental + 8..].iter().enumerate() {
        value |= u64::from(b & 0x7F) << (7 * i);
        if b & 0x80 == 0 {
            return value;
        }
    }
    panic!("unterminated LEB128 nonce");
}

#[tokio::test(flavor = "multi_thread")]
async fn frames_complete_on_the_wire_in_dave_nonce_order() {
    let params = WebRtcParams {
        address: "127.0.0.1".into(),
        port: 0,
        audio_ssrc: AUDIO_SSRC,
        video_ssrc: VIDEO_SSRC,
        rtx_ssrc: VIDEO_SSRC + 1,
        supported_encryption_modes: vec![],
    };
    let (dave, mut server_dave) = ready_dave();
    let mut client = WebRtcWrapper::new(dave);
    client.set_ice_servers(vec![]);
    client.set_setting_engine(loopback_settings(false));
    let client_pc = client.init(&params).await.unwrap();
    let offer = client.create_offer().await.unwrap();
    let (server, mut rtp_rx, mut wire) = tapped_discord_peer().await;
    let discord_sdp = answer_like_discord(&server, offer).await;
    client.set_remote_answer(&discord_sdp).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while client_pc.connection_state() != RTCPeerConnectionState::Connected {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("peer connection did not connect");
    client.set_packetizer(&params, "H264").unwrap();

    const FRAMES: usize = 3;
    // About a second on the wire, so each audio frame is sent while its keyframe is still being paced.
    let mut idr = vec![0x65, 0x88];
    idr.extend((0..MAX_VIDEO_FRAME_BYTES as u32 - 1024).map(|i| (i % 251) as u8 + 1));
    let au = annexb(&[&[0x09, 0xF0], &idr]);
    let opus = [0xFC, 0xFF, 0xFE, 0x01];
    let mut order: Vec<WireKey> = Vec::new();
    let mut previous_ts = None;
    // Each keyframe is DAVE-encrypted whole when the pacer starts it, about 0.5 s of CPU in a debug build.
    const FIRST_PACKET: Duration = Duration::from_secs(30);
    for i in 0..FRAMES as u64 {
        client
            .send_video_frame_at(&au, Duration::from_millis(33 * i))
            .await
            .unwrap();
        let ts = tokio::time::timeout(FIRST_PACKET, async {
            loop {
                let key = wire.recv().await.unwrap();
                order.push(key);
                if key.0 == VIDEO_SSRC && Some(key.3) != previous_ts {
                    return key.3;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("keyframe {i} did not reach the wire within {FIRST_PACKET:?}"));
        previous_ts = Some(ts);
        client
            .send_audio_frame_at(&opus, Duration::from_millis(20 * i))
            .await
            .unwrap();
    }

    let mut packets: HashMap<(u32, u16), Packet> = HashMap::new();
    tokio::time::timeout(Duration::from_secs(30), async {
        while order.iter().filter(|&&(_, _, m, _)| m).count() < 2 * FRAMES
            || packets.len() < order.len()
        {
            tokio::select! {
                Some(key) = wire.recv() => order.push(key),
                Some(p) = rtp_rx.recv() => {
                    packets.insert((p.header.ssrc, p.header.sequence_number), p);
                }
            }
        }
    })
    .await
    .expect("media not received");

    let mut depacketizer = webrtc::rtp::codecs::h264::H264Packet::default();
    let mut video = Vec::new();
    let mut completed = Vec::new();
    for (ssrc, seq, _, _) in &order {
        let p = &packets[&(*ssrc, *seq)];
        match *ssrc {
            AUDIO_SSRC => completed.push(("audio", p.payload.to_vec())),
            VIDEO_SSRC => {
                video.extend(depacketizer.depacketize(&p.payload).unwrap());
                if p.header.marker {
                    completed.push(("video", std::mem::take(&mut video)));
                }
            }
            other => panic!("unexpected ssrc {other}"),
        }
    }
    let nonces: Vec<(&str, u64)> = completed.iter().map(|(t, f)| (*t, dave_nonce(f))).collect();
    assert!(
        nonces.windows(2).all(|w| w[0].1 < w[1].1),
        "frames completed on the wire out of nonce order: {nonces:?}"
    );
    for (track, frame) in &completed {
        let media = if *track == "audio" {
            davey::MediaType::AUDIO
        } else {
            davey::MediaType::VIDEO
        };
        server_dave
            .decrypt(media, frame)
            .unwrap_or_else(|e| panic!("{track} frame failed to decrypt in wire order: {e}"));
    }

    client.close().await;
    server.close().await.unwrap();
}
