# OBS trace

Recorded by `obs/harness/obs-harness.c` with its trace recorder output.

| Field | Value |
|---|---|
| libobs | 32.2.2 |
| OS | Linux 6.17.0-1022-azure x86_64 |
| Date (UTC) | 2026-10-23T21:03:33Z |
| Graphics module | `libobs-opengl` |
| Video | 1280x720 at 30 fps, NV12, Rec. 709 partial range |
| Audio | 48000 Hz stereo |
| Video encoder | `obs_x264` (codec `h264`) |
| Audio encoder | `ffmpeg_opus` (codec `opus`), frame size 960 |
| Packets | 90 video, 149 audio |
| Note | linux-ffmpeg_opus-defaults |

Command line:

```
obs-harness --seconds 3 --bitrate 500 --trace-dir traces/linux-ffmpeg_opus-defaults --note linux-ffmpeg_opus-defaults --profile main --bf 0 --keyint-sec 2 --audio-encoder ffmpeg_opus
```

Video encoder settings set by the harness:

```json
{
    "bf": 0,
    "bitrate": 500,
    "keyint_sec": 2,
    "rate_control": "CBR",
    "profile": "main"
}
```

Video encoder settings including defaults:

```json
{
    "bf": 0,
    "bitrate": 500,
    "use_bufsize": false,
    "buffer_size": 6000,
    "keyint_sec": 2,
    "crf": 23,
    "rate_control": "CBR",
    "preset": "veryfast",
    "profile": "main",
    "tune": "",
    "x264opts": "",
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

Files: `packets.bin` (every `encoded_packet` in arrival order until 90 video packets), `video-extra.bin` and `audio-extra.bin` (`obs_encoder_get_extra_data` at each track's first packet), `trace.json`.

`packets.bin`: the 8 bytes `DSTRACE1`, then per packet a 40-byte little-endian header followed by the payload:
type u8 (0 audio, 1 video), keyframe u8, priority u8, drop_priority u8, timebase_num i32, timebase_den i32, size u32, pts i64, dts i64, dts_usec i64.
