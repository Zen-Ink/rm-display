//! Single-threaded Quill/libqsgepaper panel backend.

use std::marker::PhantomData;
use std::ptr::NonNull;
use std::rc::Rc;
use std::slice;
use std::time::{Duration, Instant};

use rm_display_core::{
    GraySurface, PanelBackend, PanelError, PanelInfo, PanelSubmissionMetrics, RefreshDecision,
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
    fn quill_swap_ex(
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        mode: i32,
        full: i32,
        color: i32,
    ) -> u64;
    fn quill_process_events();
}

pub struct QuillPanel {
    info: PanelInfo,
    stride: usize,
    format: NativePixelFormat,
    buffer: NonNull<u8>,
    buffer_len: usize,
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
        let (width, height, stride, qt_format, pointer) = unsafe {
            (
                quill_width(),
                quill_height(),
                quill_stride(),
                quill_format(),
                quill_buffer(),
            )
        };
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
        let color_rgb565 = native_color && is_color_remarkable();
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
            _single_thread: PhantomData,
        })
    }
}

fn is_color_remarkable() -> bool {
    let Ok(machine) = std::fs::read_to_string("/sys/devices/soc0/machine") else {
        return false;
    };
    let machine = machine.to_ascii_lowercase();
    machine.contains("ferrari")
        || machine.contains("chiappa")
        || machine.contains("tatsu")
        || machine.contains("paper pro")
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
        let union =
            union_damage(damage).ok_or_else(|| PanelError::Submit("empty damage".into()))?;
        let submit_started = Instant::now();
        let marker = unsafe {
            quill_swap_ex(
                union.x as i32,
                union.y as i32,
                union.width as i32,
                union.height as i32,
                refresh.waveform as i32,
                i32::from(refresh.complete_refresh),
                i32::from(frame.format() == PixelFormat::Rgb565Le),
            )
        };
        if marker == 0 {
            return Err(PanelError::Submit("quill_swap_ex returned zero".into()));
        }
        unsafe { quill_process_events() };
        Ok(PanelSubmissionMetrics {
            convert_us,
            submit_us: duration_us(submit_started.elapsed()),
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
