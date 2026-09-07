//! Single-threaded Quill/libqsgepaper panel backend.

use std::marker::PhantomData;
use std::ptr::NonNull;
use std::rc::Rc;
use std::slice;
use std::time::{Duration, Instant};

use rm_display_core::{
    GraySurface, PanelBackend, PanelError, PanelInfo, PanelSubmissionMetrics, RefreshDecision,
    Waveform,
};
use rm_display_protocol::{PixelFormat, Rect};

use crate::native_pixels::{write_damage, NativePixelFormat};

unsafe extern "C" {
    fn quill_init() -> i32;
    fn quill_width() -> i32;
    fn quill_height() -> i32;
    fn quill_stride() -> i32;
    fn quill_format() -> i32;
    fn quill_buffer() -> *mut u8;
    fn quill_capabilities() -> u32;
    fn quill_swap_many_ex(
        rects: *const QuillRect,
        count: usize,
        mode: i32,
        full: i32,
        color: i32,
    ) -> i32;
    fn quill_process_events();
}

#[repr(C)]
struct QuillRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

const MAX_BATCH_REGIONS: usize = 32;
const SPARSE_SWAP_COST_PIXELS: u64 = 8_192;

pub struct QuillPanel {
    info: PanelInfo,
    stride: usize,
    format: NativePixelFormat,
    buffer: NonNull<u8>,
    buffer_len: usize,
    last_timing_log: Option<Instant>,
    color_capable: bool,
    _single_thread: PhantomData<Rc<()>>,
}

impl QuillPanel {
    pub fn open() -> Result<Self, PanelError> {
        let status = unsafe { quill_init() };
        if status != 0 {
            return Err(PanelError::Unsupported(format!(
                "quill_init returned {status}"
            )));
        }
        let (width, height, stride, qt_format, capabilities, pointer) = unsafe {
            (
                quill_width(),
                quill_height(),
                quill_stride(),
                quill_format(),
                quill_capabilities(),
                quill_buffer(),
            )
        };
        const QUILL_CAPABILITY_MONO: u32 = 1 << 0;
        const QUILL_CAPABILITY_COLOR: u32 = 1 << 1;
        if capabilities & QUILL_CAPABILITY_MONO == 0 {
            return Err(PanelError::Unsupported(
                "Quill does not report monochrome capability".into(),
            ));
        }
        if width <= 0 || height <= 0 || stride <= 0 {
            return Err(PanelError::Unsupported(
                "Quill returned invalid geometry".into(),
            ));
        }
        let format = NativePixelFormat::from_qt(qt_format)?;
        let minimum_stride = (width as usize)
            .checked_mul(format.bytes_per_pixel())
            .ok_or_else(|| PanelError::Unsupported("Quill stride overflow".into()))?;
        if (stride as usize) < minimum_stride {
            return Err(PanelError::Unsupported(format!(
                "Quill stride {stride} is smaller than {minimum_stride}"
            )));
        }
        let buffer_len = (stride as usize)
            .checked_mul(height as usize)
            .ok_or_else(|| PanelError::Unsupported("Quill buffer length overflow".into()))?;
        let buffer = NonNull::new(pointer)
            .ok_or_else(|| PanelError::Unsupported("Quill returned a null framebuffer".into()))?;
        let native_color = matches!(
            format,
            NativePixelFormat::Rgb565 | NativePixelFormat::Bgra32 | NativePixelFormat::Rgba32
        );
        let color_capable = capabilities & QUILL_CAPABILITY_COLOR != 0;
        let color_rgb565 = native_color && color_capable;
        eprintln!(
            "rm-display-receiver: Quill RGB565 protocol output {}",
            if color_rgb565 { "enabled" } else { "disabled" }
        );
        Ok(Self {
            info: PanelInfo {
                width: width as u32,
                height: height as u32,
                color_rgb565,
            },
            stride: stride as usize,
            format,
            buffer,
            buffer_len,
            last_timing_log: None,
            color_capable,
            _single_thread: PhantomData,
        })
    }
}

impl PanelBackend for QuillPanel {
    fn info(&self) -> PanelInfo {
        self.info
    }

    fn submit(
        &mut self,
        frame: &GraySurface,
        damage: &[Rect],
        refresh: RefreshDecision,
    ) -> Result<PanelSubmissionMetrics, PanelError> {
        if frame.width() != self.info.width || frame.height() != self.info.height {
            return Err(PanelError::Unsupported(
                "frame does not match Quill geometry".into(),
            ));
        }
        if damage.is_empty() {
            return Err(PanelError::Unsupported(
                "Quill submission has no damage".into(),
            ));
        }

        let buffer = unsafe { slice::from_raw_parts_mut(self.buffer.as_ptr(), self.buffer_len) };
        let convert_started = Instant::now();
        write_damage(
            buffer,
            self.stride,
            self.info.width,
            self.info.height,
            self.format,
            frame.format(),
            frame.pixels(),
            damage,
        )?;
        let convert_us = duration_us(convert_started.elapsed());
        let submit_started = Instant::now();
        let union =
            union_damage(damage).ok_or_else(|| PanelError::Submit("empty damage".into()))?;
        let damage_pixels = rect_pixels(damage);
        let union_pixels = rect_pixels(std::slice::from_ref(&union));
        let clustered_damage = cluster_damage(damage, MAX_BATCH_REGIONS);
        let clustered_pixels = rect_pixels(&clustered_damage);
        let sparse = use_sparse_damage(
            clustered_damage.len(),
            clustered_pixels,
            union_pixels,
            refresh.waveform,
            refresh.complete_refresh,
        );
        let submit_damage = if sparse {
            clustered_damage.as_slice()
        } else {
            std::slice::from_ref(&union)
        };
        let submitted_pixels = rect_pixels(submit_damage);
        let mode = if self.color_capable {
            refresh.waveform as i32
        } else {
            match refresh.waveform {
                Waveform::Fastest => 0,
                Waveform::Fast | Waveform::Quality | Waveform::FullQuality => 1,
            }
        };
        let swap_started = Instant::now();
        let native_damage = submit_damage
            .iter()
            .map(|rect| QuillRect {
                x: rect.x as i32,
                y: rect.y as i32,
                width: rect.width as i32,
                height: rect.height as i32,
            })
            .collect::<Vec<_>>();
        let accepted = unsafe {
            quill_swap_many_ex(
                native_damage.as_ptr(),
                native_damage.len(),
                mode,
                i32::from(refresh.complete_refresh),
                i32::from(frame.format() == PixelFormat::Rgb565Le),
            )
        };
        if accepted != 1 {
            return Err(PanelError::Submit(format!(
                "quill_swap_many_ex returned status {accepted}"
            )));
        }
        let physical_submissions = 1;
        let swap_us = duration_us(swap_started.elapsed());
        let events_started = Instant::now();
        unsafe { quill_process_events() };
        let events_us = duration_us(events_started.elapsed());
        let submit_us = duration_us(submit_started.elapsed());
        let now = Instant::now();
        if refresh.waveform != Waveform::Fastest
            || self
                .last_timing_log
                .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1))
        {
            let inflation = union_pixels as f64 / damage_pixels.max(1) as f64;
            eprintln!(
                "rm-display-receiver: Quill timing waveform={:?} full={} status={} regions={} swaps={} damage={}px submitted={}px union={}px inflation={:.2}x framebuffer={:.2}ms swap={:.2}ms events={:.2}ms submit={:.2}ms",
                refresh.waveform,
                refresh.complete_refresh,
                accepted,
                damage.len(),
                physical_submissions,
                damage_pixels,
                submitted_pixels,
                union_pixels,
                inflation,
                f64::from(convert_us) / 1_000.0,
                f64::from(swap_us) / 1_000.0,
                f64::from(events_us) / 1_000.0,
                f64::from(submit_us) / 1_000.0,
            );
            self.last_timing_log = Some(now);
        }
        Ok(PanelSubmissionMetrics {
            convert_us,
            submit_us,
            physical_submissions,
        })
    }

    fn pump(&mut self) -> Result<(), PanelError> {
        unsafe { quill_process_events() };
        Ok(())
    }
}

fn duration_us(duration: Duration) -> u32 {
    duration.as_micros().min(u128::from(u32::MAX)) as u32
}

fn rect_pixels(rects: &[Rect]) -> u64 {
    rects.iter().fold(0, |total, rect| {
        total.saturating_add(u64::from(rect.width) * u64::from(rect.height))
    })
}

fn use_sparse_damage(
    region_count: usize,
    submitted_pixels: u64,
    union_pixels: u64,
    waveform: Waveform,
    complete_refresh: bool,
) -> bool {
    !complete_refresh
        && waveform == Waveform::Fastest
        && region_count >= 2
        && union_pixels
            > submitted_pixels
                .saturating_add((region_count as u64).saturating_mul(SPARSE_SWAP_COST_PIXELS))
}

fn cluster_damage(rects: &[Rect], limit: usize) -> Vec<Rect> {
    if limit == 0 {
        return Vec::new();
    }
    let mut sorted = rects.to_vec();
    sorted.sort_unstable_by_key(|rect| (rect.y, rect.x, rect.height, rect.width));

    let mut clusters: Vec<Rect> = Vec::with_capacity(sorted.len().min(limit));
    for rect in sorted {
        if let Some(last) = clusters.last_mut() {
            let combined = union_pair(last, &rect);
            if rect_pixels(std::slice::from_ref(&combined))
                <= rect_pixels(std::slice::from_ref(last))
                    .saturating_add(rect_pixels(std::slice::from_ref(&rect)))
            {
                *last = combined;
                continue;
            }
        }
        clusters.push(rect);
    }

    // ponytail: O(n²) adjacent clustering is bounded by the panel tile count;
    // replace it with a heap only if profiling shows this CPU work matters.
    while clusters.len() > limit {
        let (index, combined) = clusters
            .windows(2)
            .enumerate()
            .map(|(index, pair)| {
                let combined = union_pair(&pair[0], &pair[1]);
                let added =
                    rect_pixels(std::slice::from_ref(&combined)).saturating_sub(rect_pixels(pair));
                (index, combined, added)
            })
            .min_by_key(|(_, _, added)| *added)
            .map(|(index, combined, _)| (index, combined))
            .expect("more than one cluster");
        clusters[index] = combined;
        clusters.remove(index + 1);
    }
    clusters
}

fn union_pair(left: &Rect, right: &Rect) -> Rect {
    let x = left.x.min(right.x);
    let y = left.y.min(right.y);
    let far_x = left
        .x
        .saturating_add(left.width)
        .max(right.x.saturating_add(right.width));
    let far_y = left
        .y
        .saturating_add(left.height)
        .max(right.y.saturating_add(right.height));
    Rect {
        x,
        y,
        width: far_x - x,
        height: far_y - y,
    }
}

fn union_damage(rects: &[Rect]) -> Option<Rect> {
    let first = rects.first()?;
    let (mut left, mut top) = (first.x, first.y);
    let (mut right, mut bottom) = (first.x + first.width, first.y + first.height);
    for rect in &rects[1..] {
        left = left.min(rect.x);
        top = top.min(rect.y);
        right = right.max(rect.x + rect.width);
        bottom = bottom.max(rect.y + rect.height);
    }
    Some(Rect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_submission_avoids_large_fast_union_only() {
        assert!(use_sparse_damage(
            15,
            15_360,
            466_944,
            Waveform::Fastest,
            false
        ));
        assert!(!use_sparse_damage(
            15,
            15_360,
            46_000,
            Waveform::Fastest,
            false
        ));
        assert!(!use_sparse_damage(
            15,
            15_360,
            466_944,
            Waveform::Quality,
            false
        ));
        assert!(!use_sparse_damage(
            15,
            15_360,
            466_944,
            Waveform::Fastest,
            true
        ));
        assert!(!use_sparse_damage(
            1,
            1_024,
            100_000,
            Waveform::Fastest,
            false
        ));
    }

    #[test]
    fn clustering_has_no_region_count_cliff() {
        let tiles: Vec<_> = (0..261)
            .map(|index| Rect {
                x: (index % 20) * 64,
                y: (index / 20) * 64,
                width: 32,
                height: 32,
            })
            .collect();
        let clusters = cluster_damage(&tiles, MAX_BATCH_REGIONS);
        assert_eq!(clusters.len(), MAX_BATCH_REGIONS);
        assert!(use_sparse_damage(
            clusters.len(),
            rect_pixels(&tiles),
            rect_pixels(&[union_damage(&tiles).unwrap()]),
            Waveform::Fastest,
            false
        ));

        let adjacent = [
            Rect {
                x: 0,
                y: 0,
                width: 32,
                height: 32,
            },
            Rect {
                x: 32,
                y: 0,
                width: 32,
                height: 32,
            },
        ];
        assert_eq!(cluster_damage(&adjacent, MAX_BATCH_REGIONS).len(), 1);
    }
}
