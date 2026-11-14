use dstreamy_media::{
    CaptureConfig, CropRect, MediaError, OPUS_CHANNELS, OPUS_SAMPLE_RATE, VideoCodec,
};
use gstreamer as gst;

pub(crate) fn validate(config: &CaptureConfig) -> Result<(), MediaError> {
    if config.fps == 0 || config.fps > 240 {
        return Err(MediaError::InvalidConfig(format!(
            "fps {} is outside 1..=240",
            config.fps
        )));
    }
    if config.width < 2 || config.height < 2 {
        return Err(MediaError::InvalidConfig(format!(
            "output size {}x{} is too small",
            config.width, config.height
        )));
    }
    if let Some(crop) = config.crop
        && (crop.width < 2 || crop.height < 2)
    {
        return Err(MediaError::InvalidConfig(format!(
            "crop {}x{} is too small",
            crop.width, crop.height
        )));
    }
    Ok(())
}

/// Errors if the crop, rounded down to even like [`crop_caps`], extends past a `width`x`height` source.
pub(crate) fn check_crop_fits(crop: CropRect, width: u32, height: u32) -> Result<(), MediaError> {
    let fits = |offset: u32, size: u32, limit: u32| {
        u64::from(offset) + u64::from(size & !1).max(2) <= u64::from(limit)
    };
    if fits(crop.x, crop.width, width) && fits(crop.y, crop.height, height) {
        return Ok(());
    }
    Err(MediaError::InvalidConfig(format!(
        "crop {}x{} at ({}, {}) does not fit the {width}x{height} source",
        crop.width, crop.height, crop.x, crop.y
    )))
}

fn even(v: u32) -> i32 {
    i32::try_from(v & !1).unwrap_or(i32::MAX & !1).max(2)
}

/// Caps after `videocrop`: the crop size, made even for 4:2:0 encoders.
pub(crate) fn crop_caps(crop: CropRect) -> gst::Caps {
    gst::Caps::builder("video/x-raw")
        .field("width", even(crop.width))
        .field("height", even(crop.height))
        .build()
}

/// Encoder input caps: fit within the configured size, never upscale, keep the aspect ratio.
pub(crate) fn scale_caps(config: &CaptureConfig, format: &str) -> gst::Caps {
    let fps = i32::try_from(config.fps).unwrap_or(i32::MAX);
    gst::Caps::builder("video/x-raw")
        .field("format", format)
        .field("width", gst::IntRange::with_step(2, even(config.width), 2))
        .field(
            "height",
            gst::IntRange::with_step(2, even(config.height), 2),
        )
        .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
        .field("framerate", gst::Fraction::new(fps, 1))
        .build()
}

pub(super) fn parsed_caps(codec: VideoCodec) -> gst::Caps {
    let name = match codec {
        VideoCodec::H264 => "video/x-h264",
        VideoCodec::H265 => "video/x-h265",
    };
    gst::Caps::builder(name)
        .field("stream-format", "byte-stream")
        .field("alignment", "au")
        .build()
}

pub(super) fn audio_caps() -> gst::Caps {
    gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE")
        .field("rate", OPUS_SAMPLE_RATE as i32)
        .field("channels", OPUS_CHANNELS as i32)
        .field("layout", "interleaved")
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::test_support::{config, init};

    #[test]
    fn crop_caps_round_down_to_even() {
        init();
        let caps = crop_caps(CropRect {
            x: 3,
            y: 5,
            width: 101,
            height: 51,
        });
        let s = caps.structure(0).unwrap();
        assert_eq!(s.get::<i32>("width").unwrap(), 100);
        assert_eq!(s.get::<i32>("height").unwrap(), 50);
    }

    #[test]
    fn crop_must_fit_the_source_after_rounding_to_even() {
        let crop = |x, y, width, height| CropRect {
            x,
            y,
            width,
            height,
        };
        assert!(check_crop_fits(crop(0, 0, 320, 240), 320, 240).is_ok());
        assert!(check_crop_fits(crop(100, 50, 221, 191), 320, 240).is_ok());
        assert!(check_crop_fits(crop(100, 50, 222, 190), 320, 240).is_err());
        assert!(check_crop_fits(crop(0, 1, 320, 240), 320, 240).is_err());
        assert!(check_crop_fits(crop(u32::MAX, 0, 2, 2), 320, 240).is_err());
    }

    #[test]
    fn scale_caps_bound_size_and_fix_rate_and_aspect() {
        init();
        let caps = scale_caps(&config(1281, 720), "I420");
        let s = caps.structure(0).unwrap();
        assert_eq!(s.get::<&str>("format").unwrap(), "I420");
        let w = s.get::<gst::IntRange<i32>>("width").unwrap();
        assert_eq!((w.min(), w.max(), w.step()), (2, 1280, 2));
        let h = s.get::<gst::IntRange<i32>>("height").unwrap();
        assert_eq!((h.min(), h.max()), (2, 720));
        assert_eq!(
            s.get::<gst::Fraction>("pixel-aspect-ratio").unwrap(),
            gst::Fraction::new(1, 1)
        );
        assert_eq!(
            s.get::<gst::Fraction>("framerate").unwrap(),
            gst::Fraction::new(30, 1)
        );
    }

    #[test]
    fn rejects_degenerate_configs() {
        assert!(validate(&config(1280, 720)).is_ok());
        assert!(
            validate(&CaptureConfig {
                fps: 0,
                ..config(1280, 720)
            })
            .is_err()
        );
        assert!(validate(&config(1, 720)).is_err());
        let crop = Some(CropRect {
            x: 0,
            y: 0,
            width: 1,
            height: 100,
        });
        assert!(
            validate(&CaptureConfig {
                crop,
                ..config(1280, 720)
            })
            .is_err()
        );
    }
}
