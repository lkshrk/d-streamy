use serde::{Deserialize, Serialize};

const BGRA_BYTES_PER_PIXEL: usize = 4;

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

    pub fn fits_within(&self, source_width: u32, source_height: u32) -> bool {
        self.width > 0
            && self.height > 0
            && u64::from(self.x) + u64::from(self.width) <= u64::from(source_width)
            && u64::from(self.y) + u64::from(self.height) <= u64::from(source_height)
    }

    /// Crops a strided BGRA frame into a tightly packed one; `None` if out of bounds.
    pub fn apply_bgra(
        &self,
        src: &[u8],
        source_width: u32,
        source_height: u32,
        bytes_per_row: usize,
    ) -> Option<Vec<u8>> {
        if !self.fits_within(source_width, source_height) {
            return None;
        }
        let row_bytes = self.width as usize * BGRA_BYTES_PER_PIXEL;
        let x_offset = self.x as usize * BGRA_BYTES_PER_PIXEL;
        let last_row = (self.y + self.height - 1) as usize;
        let end = last_row
            .checked_mul(bytes_per_row)?
            .checked_add(x_offset)?
            .checked_add(row_bytes)?;
        if src.len() < end {
            return None;
        }
        let mut out = Vec::with_capacity(row_bytes * self.height as usize);
        for row in self.y as usize..(self.y + self.height) as usize {
            let start = row.checked_mul(bytes_per_row)?.checked_add(x_offset)?;
            out.extend_from_slice(src.get(start..start.checked_add(row_bytes)?)?);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32, stride: usize) -> Vec<u8> {
        let mut buf = vec![0xEE; stride * height as usize];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let px = &mut buf[y * stride + x * 4..][..4];
                px.copy_from_slice(&[x as u8, y as u8, 0, 0xFF]);
            }
        }
        buf
    }

    #[test]
    fn copies_the_requested_region_row_by_row() {
        let src = frame(4, 3, 4 * 4 + 8);
        let out = CropRect::new(1, 1, 2, 2)
            .apply_bgra(&src, 4, 3, 24)
            .unwrap();
        assert_eq!(
            out,
            [
                [1, 1, 0, 0xFF],
                [2, 1, 0, 0xFF],
                [1, 2, 0, 0xFF],
                [2, 2, 0, 0xFF]
            ]
            .concat()
        );
    }

    #[test]
    fn full_frame_crop_strips_row_padding() {
        let src = frame(2, 2, 12);
        let out = CropRect::new(0, 0, 2, 2)
            .apply_bgra(&src, 2, 2, 12)
            .unwrap();
        assert_eq!(out.len(), 16);
        assert!(!out.contains(&0xEE));
    }

    #[test]
    fn rejects_out_of_bounds_and_empty_rects() {
        let src = frame(4, 4, 16);
        for rect in [
            CropRect::new(3, 0, 2, 1),
            CropRect::new(0, 3, 1, 2),
            CropRect::new(0, 0, 0, 1),
            CropRect::new(0, 0, 1, 0),
            CropRect::new(u32::MAX, 0, 1, 1),
        ] {
            assert_eq!(rect.apply_bgra(&src, 4, 4, 16), None, "{rect:?}");
        }
    }

    #[test]
    fn rejects_short_source_buffer() {
        let src = frame(4, 4, 16);
        let rect = CropRect::new(0, 0, 4, 4);
        assert_eq!(rect.apply_bgra(&src[..src.len() - 1], 4, 4, 16), None);
    }
}
