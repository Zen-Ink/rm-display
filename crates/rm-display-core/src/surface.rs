use rm_display_protocol::{
    semantic::{pixel_format_bytes, DecodedRegion},
    PixelFormat, Rect,
};
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SurfaceError {
    #[error("surface dimensions are zero or overflow addressable memory")]
    InvalidGeometry,
    #[error("region is empty or outside the surface")]
    BadRegion,
    #[error("region pixel length does not match its geometry")]
    BadLength,
    #[error("regions overlap")]
    Overlap,
    #[error("overlay plane length does not match the surface")]
    BadOverlay,
}

/// A tightly packed, top-left-origin pixel surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PixelSurface {
    width: u32,
    height: u32,
    format: PixelFormat,
    pixels: Vec<u8>,
}

/// Backward-compatible name for callers which create the default Gray8 surface.
pub type GraySurface = PixelSurface;

impl PixelSurface {
    pub fn new(width: u32, height: u32, fill: u8) -> Result<Self, SurfaceError> {
        let len = pixel_len(width, height)?;
        Ok(Self {
            width,
            height,
            format: PixelFormat::Gray8,
            pixels: vec![fill; len],
        })
    }

    pub fn new_with_format(
        width: u32,
        height: u32,
        format: PixelFormat,
    ) -> Result<Self, SurfaceError> {
        let bytes_per_pixel = pixel_format_bytes(format).ok_or(SurfaceError::BadLength)?;
        let len = pixel_len(width, height)?
            .checked_mul(bytes_per_pixel)
            .ok_or(SurfaceError::InvalidGeometry)?;
        let mut pixels = vec![0xff; len];
        if format == PixelFormat::Rgb565Le {
            for pixel in pixels.chunks_exact_mut(2) {
                pixel.copy_from_slice(&0xffff_u16.to_le_bytes());
            }
        }
        Ok(Self {
            width,
            height,
            format,
            pixels,
        })
    }

    pub fn from_pixels(width: u32, height: u32, pixels: Vec<u8>) -> Result<Self, SurfaceError> {
        if pixels.len() != pixel_len(width, height)? {
            return Err(SurfaceError::BadLength);
        }
        Ok(Self {
            width,
            height,
            format: PixelFormat::Gray8,
            pixels,
        })
    }

    pub fn from_pixels_with_format(
        width: u32,
        height: u32,
        format: PixelFormat,
        pixels: Vec<u8>,
    ) -> Result<Self, SurfaceError> {
        let bytes_per_pixel = pixel_format_bytes(format).ok_or(SurfaceError::BadLength)?;
        let expected = pixel_len(width, height)?
            .checked_mul(bytes_per_pixel)
            .ok_or(SurfaceError::InvalidGeometry)?;
        if pixels.len() != expected {
            return Err(SurfaceError::BadLength);
        }
        Ok(Self {
            width,
            height,
            format,
            pixels,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn format(&self) -> PixelFormat {
        self.format
    }

    pub fn bytes_per_pixel(&self) -> usize {
        pixel_format_bytes(self.format).expect("PixelSurface only stores supported formats")
    }

    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    pub fn apply_regions_atomic(&mut self, regions: &[DecodedRegion]) -> Result<(), SurfaceError> {
        let bytes_per_pixel = self.bytes_per_pixel();
        validate_regions(self.width, self.height, bytes_per_pixel, regions)?;
        for region in regions {
            let rect = &region.rect;
            let row_width = rect.width as usize * bytes_per_pixel;
            for row in 0..rect.height as usize {
                let source = row * row_width;
                let destination = ((rect.y as usize + row) * self.width as usize + rect.x as usize)
                    * bytes_per_pixel;
                self.pixels[destination..destination + row_width]
                    .copy_from_slice(&region.pixels[source..source + row_width]);
            }
        }
        Ok(())
    }

    pub fn compose(&self, overlay: &LocalOverlay) -> Result<Self, SurfaceError> {
        if overlay.len() != pixel_len(self.width, self.height)? {
            return Err(SurfaceError::BadOverlay);
        }
        if overlay.is_transparent() {
            return Ok(self.clone());
        }
        let mut pixels = self.pixels.clone();
        match self.format {
            PixelFormat::Gray8 => {
                for ((output, foreground), alpha) in pixels
                    .iter_mut()
                    .zip(overlay.luma.iter())
                    .zip(overlay.alpha.iter())
                {
                    *output = blend(*output, *foreground, *alpha);
                }
            }
            PixelFormat::Rgb565Le => {
                for (index, output) in pixels.chunks_exact_mut(2).enumerate() {
                    let packed = u16::from_le_bytes([output[0], output[1]]);
                    let r = expand5((packed >> 11) as u8);
                    let g = expand6((packed >> 5) as u8);
                    let b = expand5(packed as u8);
                    let foreground = overlay.luma[index];
                    let alpha = overlay.alpha[index];
                    let composed = pack_rgb565(
                        blend(r, foreground, alpha),
                        blend(g, foreground, alpha),
                        blend(b, foreground, alpha),
                    )
                    .to_le_bytes();
                    output.copy_from_slice(&composed);
                }
            }
            PixelFormat::Unspecified | PixelFormat::Gray4 => unreachable!(),
        }
        Self::from_pixels_with_format(self.width, self.height, self.format, pixels)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalOverlay {
    luma: Vec<u8>,
    alpha: Vec<u8>,
    transparent: bool,
}

impl LocalOverlay {
    pub fn transparent(width: u32, height: u32) -> Result<Self, SurfaceError> {
        let len = pixel_len(width, height)?;
        Ok(Self {
            luma: vec![0; len],
            alpha: vec![0; len],
            transparent: true,
        })
    }

    pub fn len(&self) -> usize {
        self.luma.len()
    }

    pub fn is_empty(&self) -> bool {
        self.luma.is_empty()
    }

    pub fn is_transparent(&self) -> bool {
        self.transparent
    }

    pub fn clear(&mut self) {
        self.alpha.fill(0);
        self.transparent = true;
    }

    pub fn replace_planes(&mut self, luma: &[u8], alpha: &[u8]) -> Result<(), SurfaceError> {
        if luma.len() != self.len() || alpha.len() != self.len() {
            return Err(SurfaceError::BadOverlay);
        }
        self.luma.copy_from_slice(luma);
        self.alpha.copy_from_slice(alpha);
        self.transparent = !alpha.iter().any(|value| *value != 0);
        Ok(())
    }
}

pub fn tile_damage(previous: &PixelSurface, current: &PixelSurface, tile: u32) -> Vec<Rect> {
    if previous.width != current.width
        || previous.height != current.height
        || previous.format != current.format
        || tile == 0
    {
        return vec![Rect {
            x: 0,
            y: 0,
            width: current.width,
            height: current.height,
        }];
    }

    let mut damage = Vec::new();
    let width = current.width as usize;
    let bytes_per_pixel = current.bytes_per_pixel();
    let tile = tile as usize;
    for y in (0..current.height as usize).step_by(tile) {
        let height = tile.min(current.height as usize - y);
        for x in (0..width).step_by(tile) {
            let rect_width = tile.min(width - x);
            let changed = (0..height).any(|row| {
                let offset = ((y + row) * width + x) * bytes_per_pixel;
                let byte_width = rect_width * bytes_per_pixel;
                previous.pixels[offset..offset + byte_width]
                    != current.pixels[offset..offset + byte_width]
            });
            if changed {
                damage.push(Rect {
                    x: x as u32,
                    y: y as u32,
                    width: rect_width as u32,
                    height: height as u32,
                });
            }
        }
    }
    damage
}

fn pixel_len(width: u32, height: u32) -> Result<usize, SurfaceError> {
    if width == 0 || height == 0 {
        return Err(SurfaceError::InvalidGeometry);
    }
    (width as usize)
        .checked_mul(height as usize)
        .ok_or(SurfaceError::InvalidGeometry)
}

fn validate_regions(
    width: u32,
    height: u32,
    bytes_per_pixel: usize,
    regions: &[DecodedRegion],
) -> Result<(), SurfaceError> {
    for region in regions {
        let rect = &region.rect;
        if rect.width == 0
            || rect.height == 0
            || rect
                .x
                .checked_add(rect.width)
                .is_none_or(|right| right > width)
            || rect
                .y
                .checked_add(rect.height)
                .is_none_or(|bottom| bottom > height)
        {
            return Err(SurfaceError::BadRegion);
        }
        let expected = (rect.width as usize)
            .checked_mul(rect.height as usize)
            .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
            .ok_or(SurfaceError::BadLength)?;
        if region.pixels.len() != expected {
            return Err(SurfaceError::BadLength);
        }
    }
    for (index, left) in regions.iter().enumerate() {
        for right in regions.iter().skip(index + 1) {
            if overlaps(&left.rect, &right.rect) {
                return Err(SurfaceError::Overlap);
            }
        }
    }
    Ok(())
}

fn blend(background: u8, foreground: u8, alpha: u8) -> u8 {
    let alpha = alpha as u16;
    (((foreground as u16 * alpha) + (background as u16 * (255 - alpha)) + 127) / 255) as u8
}

fn expand5(value: u8) -> u8 {
    let value = value & 0x1f;
    (value << 3) | (value >> 2)
}

fn expand6(value: u8) -> u8 {
    let value = value & 0x3f;
    (value << 2) | (value >> 4)
}

fn pack_rgb565(red: u8, green: u8, blue: u8) -> u16 {
    ((red as u16 & 0xf8) << 8) | ((green as u16 & 0xfc) << 3) | (blue as u16 >> 3)
}

fn overlaps(left: &Rect, right: &Rect) -> bool {
    left.x < right.x + right.width
        && right.x < left.x + left.width
        && left.y < right.y + right.height
        && right.y < left.y + left.height
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(rect: Rect, pixels: &[u8]) -> DecodedRegion {
        DecodedRegion {
            rect,
            pixels: pixels.to_vec(),
        }
    }

    #[test]
    fn invalid_atomic_update_leaves_surface_unchanged() {
        let mut surface = GraySurface::new(4, 2, 7).unwrap();
        let original = surface.clone();
        let updates = [
            region(
                Rect {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 1,
                },
                &[1, 2],
            ),
            region(
                Rect {
                    x: 3,
                    y: 0,
                    width: 2,
                    height: 1,
                },
                &[3, 4],
            ),
        ];
        assert_eq!(
            surface.apply_regions_atomic(&updates),
            Err(SurfaceError::BadRegion)
        );
        assert_eq!(surface, original);
    }

    #[test]
    fn overlay_is_separate_from_base() {
        let base = GraySurface::new(2, 1, 200).unwrap();
        let mut overlay = LocalOverlay::transparent(2, 1).unwrap();
        assert!(overlay.is_transparent());
        overlay.replace_planes(&[0, 255], &[255, 128]).unwrap();
        assert!(!overlay.is_transparent());
        let composed = base.compose(&overlay).unwrap();
        assert_eq!(composed.pixels(), &[0, 228]);
        assert_eq!(base.pixels(), &[200, 200]);
        overlay.clear();
        assert!(overlay.is_transparent());
        assert_eq!(base.compose(&overlay).unwrap(), base);
    }

    #[test]
    fn rgb565_regions_and_overlay_preserve_color_layout() {
        let mut base = PixelSurface::new_with_format(2, 1, PixelFormat::Rgb565Le).unwrap();
        base.apply_regions_atomic(&[region(
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            },
            &[0x00, 0xf8, 0x1f, 0x00],
        )])
        .unwrap();
        assert_eq!(base.pixels(), &[0x00, 0xf8, 0x1f, 0x00]);

        let mut overlay = LocalOverlay::transparent(2, 1).unwrap();
        overlay.replace_planes(&[255, 0], &[255, 0]).unwrap();
        let composed = base.compose(&overlay).unwrap();
        assert_eq!(composed.pixels(), &[0xff, 0xff, 0x1f, 0x00]);
    }
}
