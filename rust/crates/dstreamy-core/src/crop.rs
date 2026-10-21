use std::fmt;

use serde::{Deserialize, Serialize};

use crate::stream_config::InvalidStreamConfig;

/// A crop rectangle in source pixels, origin top-left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CropRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl CropRect {
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn validate(self) -> Result<Self, InvalidStreamConfig> {
        if self.width == 0 || self.height == 0 {
            Err(InvalidStreamConfig::EmptyCrop(self))
        } else if self.x.checked_add(self.width).is_none()
            || self.y.checked_add(self.height).is_none()
        {
            Err(InvalidStreamConfig::CropOverflow(self))
        } else {
            Ok(self)
        }
    }
}

impl fmt::Display for CropRect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{},{},{},{}", self.x, self.y, self.width, self.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_non_empty_rects() {
        for rect in [CropRect::new(0, 0, 1, 1), CropRect::new(10, 20, 800, 600)] {
            assert_eq!(rect.validate(), Ok(rect));
        }
    }

    #[test]
    fn validate_rejects_empty_and_overflowing_rects() {
        for rect in [CropRect::new(0, 0, 0, 1), CropRect::new(0, 0, 1, 0)] {
            assert_eq!(rect.validate(), Err(InvalidStreamConfig::EmptyCrop(rect)));
        }
        for rect in [
            CropRect::new(u32::MAX, 0, 1, 1),
            CropRect::new(0, 1, 1, u32::MAX),
        ] {
            assert_eq!(
                rect.validate(),
                Err(InvalidStreamConfig::CropOverflow(rect))
            );
        }
    }
}
