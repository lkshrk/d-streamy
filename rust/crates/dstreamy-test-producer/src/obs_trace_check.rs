//! Checks an `obs-harness` trace: frame count, flashes, Opus tone and A/V marker alignment.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use dstreamy_test_producer::sync::{
    self, H264LumaDecoder, LumaFrame, OpusMonoDecoder, SAMPLE_RATE, TONE_HZ,
};

const MAGIC: &[u8; 8] = b"DSTRACE1";
const HEADER: usize = 40;

struct Packet<'a> {
    video: bool,
    timebase: (i64, i64),
    pts: i64,
    data: &'a [u8],
}

impl Packet<'_> {
    fn pts_us(&self) -> i64 {
        self.pts * 1_000_000 * self.timebase.0 / self.timebase.1
    }
}

fn parse(bytes: &[u8]) -> Result<Vec<Packet<'_>>, String> {
    let mut rest = bytes.strip_prefix(MAGIC).ok_or("missing DSTRACE1 magic")?;
    let mut packets = Vec::new();
    while !rest.is_empty() {
        let (hdr, tail) = rest
            .split_at_checked(HEADER)
            .ok_or("truncated packet header")?;
        let u32_at = |o: usize| u32::from_le_bytes(hdr[o..o + 4].try_into().unwrap());
        let i64_at = |o: usize| i64::from_le_bytes(hdr[o..o + 8].try_into().unwrap());
        let size = u32_at(12) as usize;
        let (data, tail) = tail.split_at_checked(size).ok_or("truncated payload")?;
        packets.push(Packet {
            video: hdr[0] == 1,
            timebase: (i64::from(u32_at(4)), i64::from(u32_at(8))),
            pts: i64_at(16),
            data,
        });
        rest = tail;
    }
    Ok(packets)
}

struct Args {
    dir: PathBuf,
    fps: i64,
    seconds: i64,
}

fn args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let dir = it
        .next()
        .ok_or("usage: obs-trace-check <trace-dir> --fps N --seconds N")?;
    let (mut fps, mut seconds) = (30, 5);
    while let Some(flag) = it.next() {
        let value: i64 = it
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or(format!("{flag} needs a number"))?;
        match flag.as_str() {
            "--fps" => fps = value,
            "--seconds" => seconds = value,
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    Ok(Args {
        dir: dir.into(),
        fps,
        seconds,
    })
}

fn check(dir: &Path, fps: i64, seconds: i64) -> Result<(), String> {
    let bytes = std::fs::read(dir.join("packets.bin")).map_err(|e| format!("packets.bin: {e}"))?;
    let extra = std::fs::read(dir.join("video-extra.bin")).unwrap_or_default();
    let packets = parse(&bytes)?;

    let mut video = H264LumaDecoder::new().map_err(|e| e.to_string())?;
    let mut frames: Vec<LumaFrame> = Vec::new();
    if !extra.is_empty() {
        video.prime(&extra).map_err(|e| e.to_string())?;
    }
    let mut opus = OpusMonoDecoder::new().map_err(|e| e.to_string())?;
    let mut pcm = Vec::new();
    let mut audio_start_us = None;
    let mut video_packets = 0;
    for p in &packets {
        if p.video {
            video_packets += 1;
            frames.extend(
                video
                    .decode(p.data, p.pts_us())
                    .map_err(|e| e.to_string())?,
            );
        } else {
            audio_start_us.get_or_insert(p.pts_us());
            pcm.extend(opus.decode(p.data).map_err(|e| e.to_string())?);
        }
    }
    frames.extend(video.finish().map_err(|e| e.to_string())?);

    let expected = fps * seconds;
    let decoded = frames.len() as i64;
    println!("video: {video_packets} packets, {decoded} decoded frames (expected {expected} ±1)");
    if (decoded - expected).abs() > 1 {
        return Err(format!("decoded {decoded} frames, expected {expected} ±1"));
    }
    let lumas: Vec<f32> = frames.iter().map(|f| f.mean_luma).collect();
    let (lo, hi) = lumas
        .iter()
        .fold((f32::MAX, f32::MIN), |(lo, hi), &l| (lo.min(l), hi.max(l)));
    println!("video: mean luma ranges {lo:.1}..{hi:.1}");
    let flashes = sync::flash_onsets(&frames);
    println!("video: flashes at {flashes:?} µs");
    if hi - lo < 100.0 || flashes.len() < (seconds as usize).saturating_sub(1) {
        return Err(format!(
            "expected about {seconds} flashes, found {}",
            flashes.len()
        ));
    }

    let start_us = audio_start_us.ok_or("no audio packets")?;
    let beeps = sync::beep_onsets(&pcm, start_us, SAMPLE_RATE);
    println!(
        "audio: {} samples from {start_us} µs, beeps at {beeps:?} µs",
        pcm.len()
    );
    let quiet = sync::quiet_spans(&pcm, start_us, &beeps, SAMPLE_RATE);
    let longest = quiet
        .iter()
        .max_by_key(|s| s.len())
        .ok_or("no audio outside the beeps")?;
    let tone = sync::goertzel_amplitude(longest, TONE_HZ, SAMPLE_RATE);
    let off: Vec<f32> = [220.0, 330.0, 660.0, 880.0]
        .iter()
        .map(|&f| sync::goertzel_amplitude(longest, f, SAMPLE_RATE))
        .collect();
    println!("audio: 440 Hz amplitude {tone:.4}, off-frequency {off:.4?}");
    if tone < 0.02 || off.iter().any(|&o| o > tone / 4.0) {
        return Err(format!("no clean 440 Hz tone (amplitude {tone:.4})"));
    }
    // The first packets carry the encoder's priming, which decodes to a ramp-in.
    let warmup = (2 * 480).min(pcm.len());
    let warmup_end_us = start_us + sync::samples_to_us(warmup, SAMPLE_RATE);
    let dropouts = sync::quiet_spans(&pcm[warmup..], warmup_end_us, &beeps, SAMPLE_RATE)
        .iter()
        .flat_map(|span| span.as_chunks::<480>().0)
        .filter(|w| sync::goertzel_amplitude(&w[..], TONE_HZ, SAMPLE_RATE) < tone / 2.0)
        .count();
    println!("audio: {dropouts} 10 ms windows below half the tone level");
    if dropouts > 0 {
        return Err(format!(
            "{dropouts} audio dropouts (10 ms windows) in the tone"
        ));
    }

    let frame_us = 1_000_000 / fps;
    let span = (
        frames.first().map_or(0, |f| f.pts_us),
        frames.last().map_or(0, |f| f.pts_us),
    );
    let audio_span = (
        start_us,
        start_us + sync::samples_to_us(pcm.len(), SAMPLE_RATE),
    );
    let pairs =
        sync::align(&flashes, &beeps, span, audio_span, frame_us).map_err(|e| e.to_string())?;
    if pairs.len() + 1 < seconds as usize {
        return Err(format!(
            "only {} aligned markers in {seconds} s",
            pairs.len()
        ));
    }
    let offsets: Vec<i64> = pairs.iter().map(|p| p.offset_us()).collect();
    println!("sync: beep - flash offsets {offsets:?} µs (tolerance {frame_us} µs)");
    Ok(())
}

fn main() -> ExitCode {
    let result = args().and_then(|a| check(&a.dir, a.fps, a.seconds));
    match result {
        Ok(()) => {
            println!("PASS");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("FAIL: {e}");
            ExitCode::FAILURE
        }
    }
}
