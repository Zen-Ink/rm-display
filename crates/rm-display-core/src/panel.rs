use rm_display_protocol::{PixelFormat, Rect};
use thiserror::Error;

use crate::{GraySurface, RefreshDecision};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelInfo {
    pub width: u32,
    pub height: u32,
    /// True only when the physical panel/backend can preserve RGB565 color.
    pub color_rgb565: bool,
}

/// CPU-side timings measured by a panel backend. They end when `submit`
/// returns; they do not claim that the physical ink has settled.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PanelSubmissionMetrics {
    pub convert_us: u32,
    pub submit_us: u32,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PanelError {
    #[error("panel geometry or pixel format is unsupported: {0}")]
    Unsupported(String),
    #[error("panel submission failed: {0}")]
    Submit(String),
    #[error("panel event pump failed: {0}")]
    Pump(String),
}

pub trait PanelBackend {
    fn info(&self) -> PanelInfo;

    /// Submit a fully composed negotiated-format frame. Implementations must write every
    /// region before making any of them visible.
    fn submit(
        &mut self,
        frame: &GraySurface,
        damage: &[Rect],
        refresh: RefreshDecision,
    ) -> Result<PanelSubmissionMetrics, PanelError>;

    fn pump(&mut self) -> Result<(), PanelError> {
        Ok(())
    }
}

impl<T: PanelBackend + ?Sized> PanelBackend for Box<T> {
    fn info(&self) -> PanelInfo {
        (**self).info()
    }

    fn submit(
        &mut self,
        frame: &GraySurface,
        damage: &[Rect],
        refresh: RefreshDecision,
    ) -> Result<PanelSubmissionMetrics, PanelError> {
        (**self).submit(frame, damage, refresh)
    }

    fn pump(&mut self) -> Result<(), PanelError> {
        (**self).pump()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordedSubmission {
    pub pixel_format: PixelFormat,
    pub pixels: Vec<u8>,
    pub damage: Vec<Rect>,
    pub refresh: RefreshDecision,
}

#[derive(Debug)]
pub struct MockPanel {
    info: PanelInfo,
    submissions: Vec<RecordedSubmission>,
    fail_next: Option<String>,
    pump_count: usize,
}

impl MockPanel {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            info: PanelInfo {
                width,
                height,
                color_rgb565: false,
            },
            submissions: Vec::new(),
            fail_next: None,
            pump_count: 0,
        }
    }

    pub fn with_rgb565(mut self) -> Self {
        self.info.color_rgb565 = true;
        self
    }

    pub fn submissions(&self) -> &[RecordedSubmission] {
        &self.submissions
    }

    pub fn fail_next_submit(&mut self, message: impl Into<String>) {
        self.fail_next = Some(message.into());
    }

    pub fn pump_count(&self) -> usize {
        self.pump_count
    }
}

impl PanelBackend for MockPanel {
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
                "frame dimensions do not match panel".into(),
            ));
        }
        if let Some(message) = self.fail_next.take() {
            return Err(PanelError::Submit(message));
        }
        self.submissions.push(RecordedSubmission {
            pixel_format: frame.format(),
            pixels: frame.pixels().to_vec(),
            damage: damage.to_vec(),
            refresh,
        });
        Ok(PanelSubmissionMetrics::default())
    }

    fn pump(&mut self) -> Result<(), PanelError> {
        self.pump_count += 1;
        Ok(())
    }
}
