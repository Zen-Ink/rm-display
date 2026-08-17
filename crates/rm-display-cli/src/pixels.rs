use std::io::{self, Read};
use std::path::Path;

use image::imageops::{crop_imm, overlay, resize, FilterType};
use image::{GrayImage, ImageError, Luma};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum FitMode {
    Contain,
    Cover,
    Stretch,
}

#[derive(Debug, Error)]
pub enum PixelError {
    #[error("image decode failed: {0}")]
    Image(#[from] ImageError),
    #[error("target dimensions must be nonzero")]
    ZeroGeometry,
    #[error("raw stream dimensions overflow")]
    GeometryOverflow,
    #[error("raw stream ends with a partial frame ({actual} of {expected} bytes)")]
    PartialFrame { actual: usize, expected: usize },
    #[error("raw stream contains no frames")]
    EmptyStream,
    #[error("raw stream read failed: {0}")]
    Io(#[from] io::Error),
}

pub fn load_image_gray8(
    path: &Path,
    target_width: u32,
    target_height: u32,
    fit: FitMode,
) -> Result<Vec<u8>, PixelError> {
    if target_width == 0 || target_height == 0 {
        return Err(PixelError::ZeroGeometry);
    }
    let source = image::open(path)?.to_luma8();
    Ok(fit_gray8(&source, target_width, target_height, fit).into_raw())
}

pub fn fit_raw_gray8(
    pixels: &[u8],
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
    fit: FitMode,
) -> Result<Vec<u8>, PixelError> {
    let expected = frame_len(source_width, source_height)?;
    if pixels.len() != expected {
        return Err(PixelError::PartialFrame {
            actual: pixels.len(),
            expected,
        });
    }
    let source = GrayImage::from_raw(source_width, source_height, pixels.to_vec())
        .ok_or(PixelError::GeometryOverflow)?;
    Ok(fit_gray8(&source, target_width, target_height, fit).into_raw())
}

fn fit_gray8(source: &GrayImage, width: u32, height: u32, fit: FitMode) -> GrayImage {
    match fit {
        FitMode::Stretch => resize(source, width, height, FilterType::Triangle),
        FitMode::Contain => {
            let (scaled_width, scaled_height) =
                contain_size(source.width(), source.height(), width, height);
            let scaled = resize(source, scaled_width, scaled_height, FilterType::Triangle);
            let mut canvas = GrayImage::from_pixel(width, height, Luma([255]));
            overlay(
                &mut canvas,
                &scaled,
                i64::from((width - scaled_width) / 2),
                i64::from((height - scaled_height) / 2),
            );
            canvas
        }
        FitMode::Cover => {
            let (scaled_width, scaled_height) =
                cover_size(source.width(), source.height(), width, height);
            let scaled = resize(source, scaled_width, scaled_height, FilterType::Triangle);
            crop_imm(
                &scaled,
                (scaled_width - width) / 2,
                (scaled_height - height) / 2,
                width,
                height,
            )
            .to_image()
        }
    }
}

fn contain_size(sw: u32, sh: u32, tw: u32, th: u32) -> (u32, u32) {
    if u64::from(tw) * u64::from(sh) <= u64::from(th) * u64::from(sw) {
        (
            tw,
            ((u64::from(sh) * u64::from(tw)) / u64::from(sw)).max(1) as u32,
        )
    } else {
        (
            ((u64::from(sw) * u64::from(th)) / u64::from(sh)).max(1) as u32,
            th,
        )
    }
}

fn cover_size(sw: u32, sh: u32, tw: u32, th: u32) -> (u32, u32) {
    if u64::from(tw) * u64::from(sh) >= u64::from(th) * u64::from(sw) {
        (
            tw,
            div_ceil(u64::from(sh) * u64::from(tw), u64::from(sw)) as u32,
        )
    } else {
        (
            div_ceil(u64::from(sw) * u64::from(th), u64::from(sh)) as u32,
            th,
        )
    }
}

fn div_ceil(value: u64, divisor: u64) -> u64 {
    value.div_ceil(divisor).max(1)
}

pub fn frame_len(width: u32, height: u32) -> Result<usize, PixelError> {
    if width == 0 || height == 0 {
        return Err(PixelError::ZeroGeometry);
    }
    (width as usize)
        .checked_mul(height as usize)
        .ok_or(PixelError::GeometryOverflow)
}

pub fn stream_raw_gray8<R, F>(
    reader: &mut R,
    width: u32,
    height: u32,
    mut submit: F,
) -> Result<u64, PixelError>
where
    R: Read,
    F: FnMut(&[u8], bool) -> Result<(), PixelError>,
{
    let length = frame_len(width, height)?;
    let mut frame = vec![0u8; length];
    let mut count = 0u64;
    loop {
        let mut read = 0usize;
        while read < length {
            match reader.read(&mut frame[read..]) {
                Ok(0) if read == 0 => break,
                Ok(0) => {
                    return Err(PixelError::PartialFrame {
                        actual: read,
                        expected: length,
                    })
                }
                Ok(amount) => read += amount,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(PixelError::Io(error)),
            }
        }
        if read == 0 {
            break;
        }
        submit(&frame, false)?;
        count += 1;
    }
    if count == 0 {
        return Err(PixelError::EmptyStream);
    }
    // EOF is a semantic event: repeat the newest pixels as an exact-base
    // SETTLED frame and wait for its terminal PRESENTED result.
    submit(&frame, true)?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contain_uses_white_letterbox() {
        let source = GrayImage::from_raw(2, 1, vec![0, 0]).unwrap();
        let output = fit_gray8(&source, 2, 2, FitMode::Contain);
        assert_eq!(output.as_raw(), &[0, 0, 255, 255]);
    }

    #[test]
    fn stream_marks_only_the_eof_repeat_settled() {
        let mut input: &[u8] = &[1, 2, 3, 4, 5, 6, 7, 8];
        let mut submissions = Vec::new();
        let count = stream_raw_gray8(&mut input, 2, 2, |frame, settled| {
            submissions.push((frame.to_vec(), settled));
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 2);
        assert_eq!(submissions.len(), 3);
        assert!(!submissions[0].1);
        assert!(!submissions[1].1);
        assert!(submissions[2].1);
        assert_eq!(submissions[1].0, submissions[2].0);
    }

    #[test]
    fn partial_frame_is_rejected() {
        let mut input: &[u8] = &[1, 2, 3];
        assert!(matches!(
            stream_raw_gray8(&mut input, 2, 2, |_, _| Ok(())),
            Err(PixelError::PartialFrame {
                actual: 3,
                expected: 4
            })
        ));
    }
}
