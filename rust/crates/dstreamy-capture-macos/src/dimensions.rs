use dstreamy_media::{CropRect, MediaError};

/// Encoder output size and the source region (in source pixels) it is scaled from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureDimensions {
    pub output_width: u32,
    pub output_height: u32,
    pub source_rect: Option<CropRect>,
}

impl CaptureDimensions {
    /// Fits the source or crop into the maximum size, keeping aspect, never upscaling, even-sized.
    pub fn resolve(
        source_width: u32,
        source_height: u32,
        max_width: u32,
        max_height: u32,
        crop: Option<CropRect>,
    ) -> Result<Self, MediaError> {
        if max_width == 0 || max_height == 0 {
            return Err(MediaError::InvalidConfig(format!(
                "output size {max_width}x{max_height}"
            )));
        }
        let source_known = source_width > 0 && source_height > 0;
        if let Some(c) = crop {
            let outside = source_known
                && (u64::from(c.x) + u64::from(c.width) > u64::from(source_width)
                    || u64::from(c.y) + u64::from(c.height) > u64::from(source_height));
            if c.width == 0 || c.height == 0 || outside {
                return Err(MediaError::InvalidConfig(format!(
                    "crop {}x{}+{}+{} outside source {source_width}x{source_height}",
                    c.width, c.height, c.x, c.y
                )));
            }
        }
        let region = match crop {
            Some(c) => Some((c.width, c.height)),
            None if source_known => Some((source_width, source_height)),
            None => None,
        };
        let (output_width, output_height) = match region {
            Some((w, h)) => fit(w, h, max_width, max_height),
            None => (max_width, max_height),
        };
        Ok(Self {
            output_width,
            output_height,
            source_rect: crop,
        })
    }
}

fn fit(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    let scale = (f64::from(max_width) / f64::from(width))
        .min(f64::from(max_height) / f64::from(height))
        .min(1.0);
    let even = |v: u32| ((f64::from(v) * scale) as u32 & !1).max(2);
    (even(width), even(height))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crop(x: u32, y: u32, width: u32, height: u32) -> Option<CropRect> {
        Some(CropRect {
            x,
            y,
            width,
            height,
        })
    }

    #[test]
    fn uncropped_source_is_scaled_to_fit() {
        let d = CaptureDimensions::resolve(2560, 1440, 1280, 720, None).unwrap();
        assert_eq!(
            (d.output_width, d.output_height, d.source_rect),
            (1280, 720, None)
        );
    }

    #[test]
    fn crop_keeps_its_size_when_it_fits() {
        let d = CaptureDimensions::resolve(2560, 1440, 1280, 720, crop(10, 20, 800, 600)).unwrap();
        assert_eq!((d.output_width, d.output_height), (800, 600));
        assert_eq!(d.source_rect, crop(10, 20, 800, 600));
    }

    #[test]
    fn large_crop_is_scaled_preserving_aspect() {
        let d = CaptureDimensions::resolve(3840, 2160, 1280, 720, crop(0, 0, 2000, 2000)).unwrap();
        assert_eq!((d.output_width, d.output_height), (720, 720));
    }

    #[test]
    fn missing_source_falls_back_to_configured_maximums() {
        let d = CaptureDimensions::resolve(0, 0, 1280, 720, None).unwrap();
        assert_eq!((d.output_width, d.output_height), (1280, 720));
    }

    #[test]
    fn odd_sizes_round_down_to_even() {
        let d = CaptureDimensions::resolve(1001, 701, 1920, 1080, None).unwrap();
        assert_eq!((d.output_width, d.output_height), (1000, 700));
    }

    #[test]
    fn smaller_source_is_not_upscaled() {
        let d = CaptureDimensions::resolve(640, 480, 1280, 720, None).unwrap();
        assert_eq!((d.output_width, d.output_height), (640, 480));
    }

    #[test]
    fn crop_outside_source_is_rejected() {
        assert!(
            CaptureDimensions::resolve(1920, 1080, 1280, 720, crop(1800, 0, 200, 100)).is_err()
        );
        assert!(CaptureDimensions::resolve(1920, 1080, 1280, 720, crop(0, 0, 0, 100)).is_err());
    }
}
