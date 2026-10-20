use crate::crop::CropRect;
use crate::stream_config::Resolution;

/// Capture size requested from the OS (`stream_*`) and encoded output size (`output_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureDimensions {
    pub stream_width: u32,
    pub stream_height: u32,
    pub output_width: u32,
    pub output_height: u32,
}

impl CaptureDimensions {
    /// A zero source size means unknown; without a crop the source is only ever scaled down.
    pub fn resolve(
        source_width: u32,
        source_height: u32,
        max: Resolution,
        crop: Option<&CropRect>,
    ) -> Self {
        let has_source = source_width > 0 && source_height > 0;

        let (output_width, output_height) = match crop {
            Some(crop) => (crop.width, crop.height),
            None if has_source => {
                let scale = (f64::from(max.width()) / f64::from(source_width))
                    .min(f64::from(max.height()) / f64::from(source_height))
                    .min(1.0);
                let even = |v: u32| ((f64::from(v) * scale) as u32 & !1).max(2);
                (even(source_width), even(source_height))
            }
            None => (max.width(), max.height()),
        };

        let (stream_width, stream_height) = if crop.is_some() && has_source {
            (source_width, source_height)
        } else {
            (output_width, output_height)
        };

        Self {
            stream_width,
            stream_height,
            output_width,
            output_height,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: Resolution = Resolution::P720;

    #[test]
    fn uncropped_window_captures_scaled_output_size() {
        let d = CaptureDimensions::resolve(2560, 1440, MAX, None);
        assert_eq!(
            d,
            CaptureDimensions {
                stream_width: 1280,
                stream_height: 720,
                output_width: 1280,
                output_height: 720,
            }
        );
    }

    #[test]
    fn crop_uses_full_source_capture_and_crop_sized_output() {
        let crop = CropRect::new(10, 20, 800, 600);
        let d = CaptureDimensions::resolve(2560, 1440, MAX, Some(&crop));
        assert_eq!(
            d,
            CaptureDimensions {
                stream_width: 2560,
                stream_height: 1440,
                output_width: 800,
                output_height: 600,
            }
        );
    }

    #[test]
    fn missing_source_falls_back_to_configured_maximums() {
        let d = CaptureDimensions::resolve(0, 0, MAX, None);
        assert_eq!(
            d,
            CaptureDimensions {
                stream_width: 1280,
                stream_height: 720,
                output_width: 1280,
                output_height: 720,
            }
        );
    }

    #[test]
    fn small_source_is_not_upscaled_and_rounds_down_to_even() {
        let d = CaptureDimensions::resolve(801, 601, MAX, None);
        assert_eq!((d.output_width, d.output_height), (800, 600));
    }

    #[test]
    fn aspect_ratio_limited_by_tighter_axis() {
        let d = CaptureDimensions::resolve(1000, 2000, MAX, None);
        assert_eq!((d.output_width, d.output_height), (360, 720));
    }

    #[test]
    fn tiny_scaled_size_is_at_least_two() {
        let d = CaptureDimensions::resolve(10_000, 1, MAX, None);
        assert_eq!((d.output_width, d.output_height), (1280, 2));
    }

    #[test]
    fn crop_without_known_source_streams_crop_size() {
        let crop = CropRect::new(0, 0, 640, 480);
        let d = CaptureDimensions::resolve(0, 0, MAX, Some(&crop));
        assert_eq!((d.stream_width, d.stream_height), (640, 480));
    }
}
