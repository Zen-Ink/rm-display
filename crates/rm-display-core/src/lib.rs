//! Hardware-independent Gray8/RGB565 display state and presentation scheduling.

mod panel;
mod refresh;
mod scheduler;
mod surface;

pub use panel::{
    MockPanel, PanelBackend, PanelError, PanelInfo, PanelSubmissionMetrics, RecordedSubmission,
};
pub use refresh::{
    FullRefreshReason, RefreshConfigError, RefreshDebt, RefreshDecision, RefreshPolicy,
    RefreshPolicyConfig, RefreshProfile, Waveform,
};
pub use scheduler::{
    CleanupReport, CommitReport, CoreError, DisplayCore, PresentationMetrics, PresentationOutcome,
    RefreshProfileChange, TerminalFrame,
};
pub use surface::{tile_damage, GraySurface, LocalOverlay, PixelSurface, SurfaceError};
