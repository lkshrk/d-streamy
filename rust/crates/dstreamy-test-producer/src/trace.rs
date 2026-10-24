//! `obs-harness` traces (`obs/tests/traces/*/PROVENANCE.md` has the format) converted by `dstreamy-obs-packet`.

use std::path::Path;

use serde_json::Value;

use dstreamy_obs_packet::{self as packet, Converter, PacketError};

const MAGIC: &[u8; 8] = b"DSTRACE1";
const HEADER: usize = 40;

#[derive(Debug, Clone)]
pub struct TracePacket {
    pub video: bool,
    pub keyframe: bool,
    pub timebase: (i32, i32),
    pub pts: i64,
    pub dts: i64,
    pub dts_usec: i64,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Trace {
    pub meta: Value,
    pub packets: Vec<TracePacket>,
    pub video_extra: Vec<u8>,
    pub audio_extra: Vec<u8>,
}

fn field<T: TryFrom<u64>>(meta: &Value, key: &str) -> Result<T, String> {
    meta[key]
        .as_u64()
        .and_then(|v| T::try_from(v).ok())
        .ok_or_else(|| format!("trace.json: no {key}"))
}

pub fn parse_packets(bytes: &[u8]) -> Result<Vec<TracePacket>, String> {
    let mut rest = bytes.strip_prefix(MAGIC).ok_or("missing DSTRACE1 magic")?;
    let mut packets = Vec::new();
    while !rest.is_empty() {
        let (hdr, tail) = rest
            .split_at_checked(HEADER)
            .ok_or("truncated packet header")?;
        let i32_at = |o: usize| i32::from_le_bytes(hdr[o..o + 4].try_into().unwrap());
        let i64_at = |o: usize| i64::from_le_bytes(hdr[o..o + 8].try_into().unwrap());
        let size = u32::from_le_bytes(hdr[12..16].try_into().unwrap()) as usize;
        let (data, tail) = tail.split_at_checked(size).ok_or("truncated payload")?;
        packets.push(TracePacket {
            video: hdr[0] == 1,
            keyframe: hdr[1] == 1,
            timebase: (i32_at(4), i32_at(8)),
            pts: i64_at(16),
            dts: i64_at(24),
            dts_usec: i64_at(32),
            data: data.to_vec(),
        });
        rest = tail;
    }
    Ok(packets)
}

impl Trace {
    pub fn load(dir: &Path) -> Result<Self, String> {
        let read = |f: &str| {
            std::fs::read(dir.join(f)).map_err(|e| format!("{}: {e}", dir.join(f).display()))
        };
        Ok(Self {
            meta: serde_json::from_slice(&read("trace.json")?)
                .map_err(|e| format!("trace.json: {e}"))?,
            packets: parse_packets(&read("packets.bin")?)?,
            video_extra: read("video-extra.bin")?,
            audio_extra: read("audio-extra.bin")?,
        })
    }

    pub fn width(&self) -> Result<u32, String> {
        field(&self.meta, "width")
    }

    pub fn height(&self) -> Result<u32, String> {
        field(&self.meta, "height")
    }

    pub fn fps(&self) -> Result<u32, String> {
        field(&self.meta, "fps")
    }

    pub fn sample_rate(&self) -> Result<u32, String> {
        field(&self.meta, "sample_rate")
    }

    pub fn audio_frame_size(&self) -> Result<u32, String> {
        field(&self.meta, "audio_frame_size")
    }

    pub fn audio_codec(&self) -> &str {
        self.meta["audio_codec"].as_str().unwrap_or("")
    }

    pub fn video(&self) -> impl Iterator<Item = &TracePacket> {
        self.packets.iter().filter(|p| p.video)
    }

    pub fn audio(&self) -> impl Iterator<Item = &TracePacket> {
        self.packets.iter().filter(|p| !p.video)
    }

    pub fn replay(self) -> Replay {
        Replay {
            trace: self,
            converter: Converter::new(),
            next: 0,
            videos: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Converted {
    Video {
        data: Vec<u8>,
        pts_us: i64,
        keyframe: bool,
    },
    Audio {
        data: Vec<u8>,
        pts_us: i64,
    },
}

/// A packet as the plugin hands it to the IPC writer, with the libobs dts in µs it arrived at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replayed {
    pub dts_usec: i64,
    pub frame: Converted,
}

/// Converts a trace's packets in arrival order, reading the extra data only after the first video packet.
pub struct Replay {
    trace: Trace,
    converter: Converter,
    next: usize,
    videos: usize,
}

impl Iterator for Replay {
    type Item = Result<Replayed, PacketError>;

    fn next(&mut self) -> Option<Self::Item> {
        let p = self.trace.packets.get(self.next)?.clone();
        self.next += 1;
        Some(self.convert(p))
    }
}

impl Replay {
    pub fn trace(&self) -> &Trace {
        &self.trace
    }

    fn convert(&mut self, p: TracePacket) -> Result<Replayed, PacketError> {
        let frame = if p.video {
            if self.videos > 0 && !self.converter.has_video_extra_data() {
                self.converter
                    .set_video_extra_data(&self.trace.video_extra)?;
            }
            self.videos += 1;
            let f = self
                .converter
                .convert_video(&p.data, p.keyframe, p.pts, p.dts, p.timebase)?;
            Converted::Video {
                data: f.data.into_owned(),
                pts_us: f.pts_us,
                keyframe: f.keyframe,
            }
        } else {
            Converted::Audio {
                pts_us: packet::convert_audio(p.pts, p.dts, p.timebase)?,
                data: p.data,
            }
        };
        Ok(Replayed {
            dts_usec: p.dts_usec,
            frame,
        })
    }
}
