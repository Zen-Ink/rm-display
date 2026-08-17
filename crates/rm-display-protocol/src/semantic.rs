use std::io::Read;

use bytes::Bytes;
use crc32fast::hash;
use flate2::read::ZlibDecoder;
use thiserror::Error;
use zstd::stream::read::Decoder as ZstdDecoder;

use crate::{Encoding, Frame, FrameIntent, FrameRegion, PixelFormat, Rect};

#[derive(Debug, Clone)]
pub struct SurfaceState {
    pub surface_id: u32,
    pub generation: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: PixelFormat,
    pub logical_frame_id: u64,
    pub max_regions: usize,
    pub max_frame_bytes: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecodedRegion {
    pub rect: Rect,
    pub pixels: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedFrame {
    pub frame_id: u64,
    pub base_frame_id: u64,
    pub intent: FrameIntent,
    pub pixel_format: PixelFormat,
    /// Sum of all decoded region payloads. This is the v2.1 byte-credit cost.
    pub decoded_bytes: usize,
    pub regions: Vec<DecodedRegion>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SemanticError {
    #[error("frame targets the wrong surface or generation")]
    WrongSurface,
    #[error("frame id must be nonzero and newer than the logical frame")]
    BadFrameId,
    #[error("delta base {actual} does not match logical frame {expected}")]
    BadBase { expected: u64, actual: u64 },
    #[error("unsupported pixel format or encoding")]
    Unsupported,
    #[error("frame has an invalid region count")]
    BadRegionCount,
    #[error("region is missing or outside the surface")]
    BadRegion,
    #[error("frame regions overlap")]
    RegionOverlap,
    #[error("decoded length does not match region geometry")]
    BadDecodedLength,
    #[error("encoded or decoded frame exceeds the negotiated limit")]
    FrameTooLarge,
    #[error("compressed region cannot be decoded")]
    Decompression,
    #[error("decoded region CRC32 does not match")]
    BadCrc,
    #[error("v2 keyframes must be one full-surface region")]
    BadKeyframe,
    #[error("frame intent is unspecified or unknown")]
    BadIntent,
}

pub fn validate_and_decode_frame(
    frame: &Frame,
    surface: &SurfaceState,
) -> Result<ValidatedFrame, SemanticError> {
    if frame.surface_id != surface.surface_id || frame.generation != surface.generation {
        return Err(SemanticError::WrongSurface);
    }
    if frame.frame_id == 0 || frame.frame_id <= surface.logical_frame_id {
        return Err(SemanticError::BadFrameId);
    }
    let bytes_per_pixel =
        pixel_format_bytes(surface.pixel_format).ok_or(SemanticError::Unsupported)?;
    let intent = FrameIntent::try_from(frame.intent).map_err(|_| SemanticError::BadIntent)?;
    if intent == FrameIntent::Unspecified {
        return Err(SemanticError::BadIntent);
    }
    if frame.regions.is_empty()
        || frame.regions.len() > surface.max_regions
        || frame.regions.len() > u16::MAX as usize
    {
        return Err(SemanticError::BadRegionCount);
    }
    if frame.base_frame_id != 0 && frame.base_frame_id != surface.logical_frame_id {
        return Err(SemanticError::BadBase {
            expected: surface.logical_frame_id,
            actual: frame.base_frame_id,
        });
    }

    let mut decoded_total = 0usize;
    let mut encoded_total = 0usize;
    let mut decoded = Vec::with_capacity(frame.regions.len());
    for region in &frame.regions {
        let rect = validated_rect(region, surface)?;
        let expected = (rect.width as usize)
            .checked_mul(rect.height as usize)
            .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
            .ok_or(SemanticError::FrameTooLarge)?;
        if region.decoded_len as usize != expected {
            return Err(SemanticError::BadDecodedLength);
        }
        decoded_total = decoded_total
            .checked_add(expected)
            .ok_or(SemanticError::FrameTooLarge)?;
        encoded_total = encoded_total
            .checked_add(region.data.len())
            .ok_or(SemanticError::FrameTooLarge)?;
        if decoded_total > surface.max_frame_bytes || encoded_total > surface.max_frame_bytes {
            return Err(SemanticError::FrameTooLarge);
        }
        let pixels = decode_region(region, expected)?;
        if hash(&pixels) != region.decoded_crc32 {
            return Err(SemanticError::BadCrc);
        }
        decoded.push(DecodedRegion { rect, pixels });
    }

    for (index, left) in decoded.iter().enumerate() {
        for right in decoded.iter().skip(index + 1) {
            if overlaps(&left.rect, &right.rect) {
                return Err(SemanticError::RegionOverlap);
            }
        }
    }

    if frame.base_frame_id == 0
        && (decoded.len() != 1
            || decoded[0].rect
                != (Rect {
                    x: 0,
                    y: 0,
                    width: surface.width,
                    height: surface.height,
                }))
    {
        return Err(SemanticError::BadKeyframe);
    }

    Ok(ValidatedFrame {
        frame_id: frame.frame_id,
        base_frame_id: frame.base_frame_id,
        intent,
        pixel_format: surface.pixel_format,
        decoded_bytes: decoded_total,
        regions: decoded,
    })
}

pub fn apply_validated_frame(surface: &mut [u8], width: u32, frame: &ValidatedFrame) {
    let bytes_per_pixel = pixel_format_bytes(frame.pixel_format)
        .expect("validated frame has a supported pixel format");
    let row_width = width as usize * bytes_per_pixel;
    for region in &frame.regions {
        let region_width = region.rect.width as usize * bytes_per_pixel;
        for row in 0..region.rect.height as usize {
            let src = row * region_width;
            let dst = (region.rect.y as usize + row) * row_width
                + region.rect.x as usize * bytes_per_pixel;
            surface[dst..dst + region_width]
                .copy_from_slice(&region.pixels[src..src + region_width]);
        }
    }
}

/// Bytes per tightly packed pixel for formats implemented by protocol v2.
/// Gray4 remains reserved and deliberately returns `None`.
pub fn pixel_format_bytes(format: PixelFormat) -> Option<usize> {
    match format {
        PixelFormat::Gray8 => Some(1),
        PixelFormat::Rgb565Le => Some(2),
        PixelFormat::Unspecified | PixelFormat::Gray4 => None,
    }
}

fn validated_rect(region: &FrameRegion, surface: &SurfaceState) -> Result<Rect, SemanticError> {
    let rect = region.rect.clone().ok_or(SemanticError::BadRegion)?;
    if rect.width == 0 || rect.height == 0 {
        return Err(SemanticError::BadRegion);
    }
    let right = rect
        .x
        .checked_add(rect.width)
        .ok_or(SemanticError::BadRegion)?;
    let bottom = rect
        .y
        .checked_add(rect.height)
        .ok_or(SemanticError::BadRegion)?;
    if right > surface.width || bottom > surface.height {
        return Err(SemanticError::BadRegion);
    }
    Ok(rect)
}

fn decode_region(region: &FrameRegion, expected: usize) -> Result<Vec<u8>, SemanticError> {
    let encoding = Encoding::try_from(region.encoding).map_err(|_| SemanticError::Unsupported)?;
    match encoding {
        Encoding::Raw => {
            if region.data.len() != expected {
                return Err(SemanticError::BadDecodedLength);
            }
            Ok(region.data.to_vec())
        }
        Encoding::Zlib => {
            let mut decoder = ZlibDecoder::new(region.data.as_ref());
            decode_exact(&mut decoder, expected)
        }
        Encoding::Zstd => {
            let mut decoder =
                ZstdDecoder::new(region.data.as_ref()).map_err(|_| SemanticError::Decompression)?;
            decode_exact(&mut decoder, expected)
        }
        Encoding::Unspecified => Err(SemanticError::Unsupported),
    }
}

fn decode_exact(decoder: &mut impl Read, expected: usize) -> Result<Vec<u8>, SemanticError> {
    let mut output = Vec::with_capacity(expected);
    decoder
        .take(expected as u64 + 1)
        .read_to_end(&mut output)
        .map_err(|_| SemanticError::Decompression)?;
    if output.len() != expected {
        return Err(SemanticError::BadDecodedLength);
    }
    Ok(output)
}

fn overlaps(left: &Rect, right: &Rect) -> bool {
    left.x < right.x + right.width
        && right.x < left.x + left.width
        && left.y < right.y + right.height
        && right.y < left.y + left.height
}

pub fn raw_region(rect: Rect, pixels: Vec<u8>) -> FrameRegion {
    FrameRegion {
        rect: Some(rect),
        encoding: Encoding::Raw as i32,
        decoded_len: pixels.len() as u32,
        decoded_crc32: hash(&pixels),
        data: Bytes::from(pixels),
    }
}
