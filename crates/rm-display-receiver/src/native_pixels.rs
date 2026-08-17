use rm_display_protocol::{PixelFormat, Rect};

use rm_display_core::PanelError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativePixelFormat {
    Gray8,
    Rgb565,
    Bgra32,
    Rgba32,
}

impl NativePixelFormat {
    pub(crate) fn from_qt(format: i32) -> Result<Self, PanelError> {
        match format {
            24 => Ok(Self::Gray8),
            7 => Ok(Self::Rgb565),
            4..=6 => Ok(Self::Bgra32),
            16..=18 => Ok(Self::Rgba32),
            other => Err(PanelError::Unsupported(format!(
                "Qt framebuffer format {other}"
            ))),
        }
    }

    pub(crate) fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Gray8 => 1,
            Self::Rgb565 => 2,
            Self::Bgra32 | Self::Rgba32 => 4,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn write_damage(
    buffer: &mut [u8],
    stride: usize,
    width: u32,
    height: u32,
    format: NativePixelFormat,
    source_format: PixelFormat,
    source: &[u8],
    damage: &[Rect],
) -> Result<(), PanelError> {
    let pixel_count = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| PanelError::Unsupported("panel geometry overflow".into()))?;
    let source_bpp = match source_format {
        PixelFormat::Gray8 => 1,
        PixelFormat::Rgb565Le => 2,
        PixelFormat::Unspecified | PixelFormat::Gray4 => {
            return Err(PanelError::Unsupported("source pixel format".into()));
        }
    };
    if source.len() != pixel_count.saturating_mul(source_bpp) {
        return Err(PanelError::Unsupported(
            "source frame length mismatch".into(),
        ));
    }
    let minimum_stride = (width as usize)
        .checked_mul(format.bytes_per_pixel())
        .ok_or_else(|| PanelError::Unsupported("native stride overflow".into()))?;
    if stride < minimum_stride
        || buffer.len()
            < stride
                .checked_mul(height as usize)
                .ok_or_else(|| PanelError::Unsupported("native buffer length overflow".into()))?
    {
        return Err(PanelError::Unsupported(
            "native framebuffer stride is too small".into(),
        ));
    }

    for rect in damage {
        validate_rect(rect, width, height)?;
        for y in rect.y as usize..(rect.y + rect.height) as usize {
            let left = rect.x as usize;
            let right = (rect.x + rect.width) as usize;
            let source_row = y * width as usize;
            let native_row = y * stride;
            match (source_format, format) {
                (PixelFormat::Gray8, NativePixelFormat::Gray8) => {
                    buffer[native_row + left..native_row + right]
                        .copy_from_slice(&source[source_row + left..source_row + right]);
                }
                (PixelFormat::Rgb565Le, NativePixelFormat::Rgb565) => {
                    let source_start = (source_row + left) * 2;
                    let source_end = (source_row + right) * 2;
                    let native_start = native_row + left * 2;
                    buffer[native_start..native_start + source_end - source_start]
                        .copy_from_slice(&source[source_start..source_end]);
                }
                (PixelFormat::Gray8, NativePixelFormat::Rgb565) => {
                    let gray = &source[source_row + left..source_row + right];
                    let native = &mut buffer[native_row + left * 2..native_row + right * 2];
                    for (luma, output) in gray.iter().copied().zip(native.chunks_exact_mut(2)) {
                        output.copy_from_slice(&pack_rgb565(luma, luma, luma).to_le_bytes());
                    }
                }
                (PixelFormat::Gray8, NativePixelFormat::Bgra32 | NativePixelFormat::Rgba32) => {
                    let gray = &source[source_row + left..source_row + right];
                    let native = &mut buffer[native_row + left * 4..native_row + right * 4];
                    for (luma, output) in gray.iter().copied().zip(native.chunks_exact_mut(4)) {
                        output.copy_from_slice(&[luma, luma, luma, 0xff]);
                    }
                }
                (PixelFormat::Rgb565Le, NativePixelFormat::Gray8) => {
                    let source_start = (source_row + left) * 2;
                    let source_end = (source_row + right) * 2;
                    let packed = &source[source_start..source_end];
                    let native = &mut buffer[native_row + left..native_row + right];
                    for (input, output) in packed.chunks_exact(2).zip(native.iter_mut()) {
                        let (red, green, blue) = unpack_rgb565(input);
                        *output = rgb_luma(red, green, blue);
                    }
                }
                (PixelFormat::Rgb565Le, NativePixelFormat::Bgra32 | NativePixelFormat::Rgba32) => {
                    let source_start = (source_row + left) * 2;
                    let source_end = (source_row + right) * 2;
                    let packed = &source[source_start..source_end];
                    let native = &mut buffer[native_row + left * 4..native_row + right * 4];
                    for (input, output) in packed.chunks_exact(2).zip(native.chunks_exact_mut(4)) {
                        let (red, green, blue) = unpack_rgb565(input);
                        if format == NativePixelFormat::Bgra32 {
                            output.copy_from_slice(&[blue, green, red, 0xff]);
                        } else {
                            output.copy_from_slice(&[red, green, blue, 0xff]);
                        }
                    }
                }
                (PixelFormat::Unspecified | PixelFormat::Gray4, _) => unreachable!(),
            }
        }
    }
    Ok(())
}

fn unpack_rgb565(input: &[u8]) -> (u8, u8, u8) {
    let packed = u16::from_le_bytes([input[0], input[1]]);
    (
        expand5((packed >> 11) as u8),
        expand6((packed >> 5) as u8),
        expand5(packed as u8),
    )
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

fn rgb_luma(red: u8, green: u8, blue: u8) -> u8 {
    ((u32::from(red) * 77 + u32::from(green) * 150 + u32::from(blue) * 29 + 128) >> 8) as u8
}

fn validate_rect(rect: &Rect, width: u32, height: u32) -> Result<(), PanelError> {
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
        return Err(PanelError::Unsupported(
            "damage rectangle is outside panel".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_rect() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 2,
            height: 1,
        }
    }

    #[test]
    fn writes_opaque_gray_with_row_padding() {
        let mut native = vec![0xaa; 12];
        write_damage(
            &mut native,
            12,
            2,
            1,
            NativePixelFormat::Bgra32,
            PixelFormat::Gray8,
            &[0x12, 0x80],
            &[full_rect()],
        )
        .unwrap();
        assert_eq!(
            &native[..8],
            &[0x12, 0x12, 0x12, 0xff, 0x80, 0x80, 0x80, 0xff]
        );
        assert_eq!(&native[8..], &[0xaa; 4]);
    }

    #[test]
    fn writes_little_endian_rgb565() {
        let mut native = vec![0; 4];
        write_damage(
            &mut native,
            4,
            2,
            1,
            NativePixelFormat::Rgb565,
            PixelFormat::Gray8,
            &[0, 255],
            &[full_rect()],
        )
        .unwrap();
        assert_eq!(native, [0x00, 0x00, 0xff, 0xff]);
    }

    #[test]
    fn writes_gray8_and_maps_qt_formats() {
        let mut native = vec![0; 2];
        write_damage(
            &mut native,
            2,
            2,
            1,
            NativePixelFormat::from_qt(24).unwrap(),
            PixelFormat::Gray8,
            &[9, 10],
            &[full_rect()],
        )
        .unwrap();
        assert_eq!(native, [9, 10]);
        assert_eq!(
            NativePixelFormat::from_qt(7).unwrap(),
            NativePixelFormat::Rgb565
        );
        assert_eq!(
            NativePixelFormat::from_qt(4).unwrap(),
            NativePixelFormat::Bgra32
        );
        assert_eq!(
            NativePixelFormat::from_qt(16).unwrap(),
            NativePixelFormat::Rgba32
        );
        assert!(NativePixelFormat::from_qt(999).is_err());
    }

    #[test]
    fn preserves_rgb565_color_in_opaque32_buffer() {
        let mut native = vec![0; 12];
        write_damage(
            &mut native,
            12,
            3,
            1,
            NativePixelFormat::Bgra32,
            PixelFormat::Rgb565Le,
            &[0x00, 0xf8, 0xe0, 0x07, 0x1f, 0x00],
            &[Rect {
                x: 0,
                y: 0,
                width: 3,
                height: 1,
            }],
        )
        .unwrap();
        assert_eq!(native, [0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255]);

        let mut rgba = vec![0; 4];
        write_damage(
            &mut rgba,
            4,
            1,
            1,
            NativePixelFormat::Rgba32,
            PixelFormat::Rgb565Le,
            &[0x00, 0xf8],
            &[Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }],
        )
        .unwrap();
        assert_eq!(rgba, [255, 0, 0, 255]);
    }
}
