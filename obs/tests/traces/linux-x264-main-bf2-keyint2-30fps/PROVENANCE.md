# OBS trace

Recorded by `obs/harness/obs-harness.c` with its trace recorder output.

| Field | Value |
|---|---|
| libobs | 32.2.2 |
| OBS commit | ba2f32bdf791005443988a4955e963663e16b1ed |
| CI run | 40028252651 |
| OS | Linux 6.17.0-1022-azure x86_64 |
| Date (UTC) | 2026-10-23T21:40:40Z |
| Graphics module | `libobs-opengl` |
| Video | 1280x720 at 30 fps, NV12, Rec. 709 partial range |
| Audio | 48000 Hz stereo |
| Video encoder | `obs_x264` (codec `h264`) |
| Audio encoder | `ffmpeg_opus` (codec `opus`), frame size 960 |
| Packets | 150 video, 246 audio |
| Frames lagged / skipped while recording | 0 / 0 |
| Note | linux-x264-main-bf2-keyint2-30fps |

Command line:

```
obs-harness --seconds 5 --bitrate 500 --x264opts scenecut=0 --trace-dir traces/linux-x264-main-bf2-keyint2-30fps --note linux-x264-main-bf2-keyint2-30fps --profile main --bf 2 --keyint-sec 2
```

Video encoder settings set by the harness:

```json
{
    "bf": 2,
    "bitrate": 500,
    "keyint_sec": 2,
    "rate_control": "CBR",
    "profile": "main",
    "x264opts": "scenecut=0"
}
```

Video encoder settings including defaults:

```json
{
    "bf": 2,
    "bitrate": 500,
    "use_bufsize": false,
    "buffer_size": 6000,
    "keyint_sec": 2,
    "crf": 23,
    "rate_control": "CBR",
    "preset": "veryfast",
    "profile": "main",
    "tune": "",
    "x264opts": "scenecut=0",
    "repeat_headers": false
}
```

Audio encoder settings including defaults:

```json
{
    "bitrate": 128
}
```

Scene: `color_source_v3` 1280x720, dark (0xFF202020) except one white frame per second; harness audio source: 1 kHz beep for 20 ms at each flash, 440 Hz tone at amplitude 0.05 otherwise.

Files: `packets.bin` (every `encoded_packet` in arrival order until 150 video packets), `video-extra.bin` and `audio-extra.bin` (`obs_encoder_get_extra_data` at each track's first packet), `trace.json`.

`packets.bin`: the 8 bytes `DSTRACE1`, then per packet a 40-byte little-endian header followed by the payload:
type u8 (0 audio, 1 video), keyframe u8, priority u8, drop_priority u8, timebase_num i32, timebase_den i32, size u32, pts i64, dts i64, dts_usec i64.
