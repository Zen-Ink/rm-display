use std::time::{Duration, Instant};

use rm_display_protocol::{semantic::ValidatedFrame, ContentClass, FrameIntent, PixelFormat, Rect};
use thiserror::Error;

use crate::surface::tile_damage_regions;
use crate::{
    tile_damage, FullRefreshReason, GraySurface, LocalOverlay, PanelBackend, PanelError,
    RefreshConfigError, RefreshDebt, RefreshDecision, RefreshPolicy, RefreshPolicyConfig,
    SurfaceError, Waveform,
};

const REALTIME_CLEANUP_IDLE: Duration = Duration::from_secs(3);
const REALTIME_CLEANUP_SCREEN_EQUIVALENTS: u64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentationOutcome {
    Presented,
    Superseded,
    Cancelled,
    BackendFailure,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PresentationMetrics {
    pub decode_us: u32,
    pub queue_us: u32,
    pub present_us: u32,
    pub compose_us: u32,
    pub convert_us: u32,
    pub submit_us: u32,
    pub damage_pixels: u32,
    pub damage_regions: u32,
    pub waveform: u32,
    pub complete_refresh: bool,
    pub full_refresh_reason: FullRefreshReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalFrame {
    pub frame_id: u64,
    pub outcome: PresentationOutcome,
    pub metrics: PresentationMetrics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitReport {
    pub logical_frame_id: u64,
    pub superseded: Option<TerminalFrame>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshProfileChange {
    pub config: RefreshPolicyConfig,
    pub changed: bool,
    pub cleanup_performed: bool,
    pub cleanup_pending: bool,
    pub backend_failed: bool,
    pub terminals: Vec<TerminalFrame>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupReport {
    pub cleanup_performed: bool,
    pub cleanup_pending: bool,
    pub backend_failed: bool,
    pub terminals: Vec<TerminalFrame>,
}

#[derive(Debug, Error)]
pub enum CoreError {
    #[error(transparent)]
    Surface(#[from] SurfaceError),
    #[error(transparent)]
    RefreshConfig(#[from] RefreshConfigError),
    #[error("a SETTLED presentation barrier is active")]
    SettledBarrier,
    #[error("frame id must be nonzero and newer than the logical frame")]
    BadFrameId,
    #[error("delta base is invalid; a keyframe is required")]
    NeedKeyframe,
    #[error("frame pixel format does not match the surface")]
    PixelFormat,
    #[error(transparent)]
    Panel(#[from] PanelError),
}

#[derive(Debug, Clone)]
struct PendingPresentation {
    frame_id: u64,
    intent: FrameIntent,
    content_class: ContentClass,
    accepted_at: Duration,
    force_full_damage: bool,
    force_cleanup: bool,
    decode_us: u32,
    decoded_bytes: usize,
    damage_hint: Vec<Rect>,
}

/// Owns the logical remote base, local overlay, one pending presentation, and
/// the software state known to have been successfully submitted to the panel.
pub struct DisplayCore {
    base: GraySurface,
    overlay: LocalOverlay,
    presented: GraySurface,
    working: GraySurface,
    logical_frame_id: u64,
    presented_frame_id: u64,
    base_valid: bool,
    pending: Option<PendingPresentation>,
    last_present_at: Option<Duration>,
    frame_interval: Duration,
    settled_deadline: Duration,
    refresh_policy: RefreshPolicy,
    damage_tile: u32,
    panel_state_uncertain: bool,
    /// A profile change is a receiver-side policy barrier. It remains armed
    /// across frame superseding and backend failure until a complete refresh
    /// has actually succeeded.
    force_cleanup_next: bool,
    /// Tile-exact damage refreshed with a fast waveform since the last quality
    /// or complete presentation. A quality SETTLED frame repaints these tiles
    /// even when their pixels equal the latest frame.
    settle_damage: Vec<Rect>,
    fast_updates_since_settled: u32,
    partial_damage_pixels_since_cleanup: u64,
    last_partial_at: Option<Duration>,
}

impl DisplayCore {
    pub fn new(
        width: u32,
        height: u32,
        max_fps_x100: u32,
        settled_deadline: Duration,
        refresh_config: RefreshPolicyConfig,
    ) -> Result<Self, CoreError> {
        Self::new_with_format(
            width,
            height,
            PixelFormat::Gray8,
            max_fps_x100,
            settled_deadline,
            refresh_config,
        )
    }

    pub fn new_with_format(
        width: u32,
        height: u32,
        pixel_format: PixelFormat,
        max_fps_x100: u32,
        settled_deadline: Duration,
        refresh_config: RefreshPolicyConfig,
    ) -> Result<Self, CoreError> {
        let fps_x100 = max_fps_x100.max(100);
        let interval_us = 100_000_000_u64.div_ceil(fps_x100 as u64);
        let refresh_policy = RefreshPolicy::new(refresh_config)?;
        Ok(Self {
            base: GraySurface::new_with_format(width, height, pixel_format)?,
            overlay: LocalOverlay::transparent(width, height)?,
            presented: GraySurface::new_with_format(width, height, pixel_format)?,
            working: GraySurface::new_with_format(width, height, pixel_format)?,
            logical_frame_id: 0,
            presented_frame_id: 0,
            base_valid: false,
            pending: None,
            last_present_at: None,
            frame_interval: Duration::from_micros(interval_us),
            settled_deadline,
            refresh_policy,
            damage_tile: refresh_config.damage_tile,
            panel_state_uncertain: false,
            force_cleanup_next: false,
            settle_damage: Vec::new(),
            fast_updates_since_settled: 0,
            partial_damage_pixels_since_cleanup: 0,
            last_partial_at: None,
        })
    }

    pub fn width(&self) -> u32 {
        self.base.width()
    }

    pub fn height(&self) -> u32 {
        self.base.height()
    }

    pub fn logical_frame_id(&self) -> u64 {
        self.logical_frame_id
    }

    pub fn presented_frame_id(&self) -> u64 {
        self.presented_frame_id
    }

    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn pending_is_settled(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.intent == FrameIntent::Settled)
    }

    pub fn pending_bytes(&self) -> usize {
        self.pending
            .as_ref()
            .map_or(0, |pending| pending.decoded_bytes)
    }

    pub fn pixel_format(&self) -> PixelFormat {
        self.base.format()
    }

    pub fn base_pixels(&self) -> &[u8] {
        self.base.pixels()
    }

    pub fn refresh_config(&self) -> RefreshPolicyConfig {
        self.refresh_policy.config()
    }

    pub fn cleanup_pending(&self) -> bool {
        self.force_cleanup_next
    }

    pub fn presented_since_full_refresh(&self) -> u32 {
        self.refresh_policy.presented_since_cleanup()
    }

    pub fn fast_updates_since_settled(&self) -> u32 {
        self.fast_updates_since_settled
    }

    pub fn refresh_debt(&self) -> RefreshDebt {
        self.refresh_policy.debt()
    }

    pub fn restore_refresh_debt(&mut self, debt: RefreshDebt) -> Result<(), CoreError> {
        self.refresh_policy.restore_debt(debt)?;
        Ok(())
    }

    pub fn require_cleanup(&mut self) {
        self.force_cleanup_next = true;
    }

    fn record_panel_damage(&mut self, decision: RefreshDecision, damage: &[Rect], now: Duration) {
        if decision.complete_refresh {
            self.partial_damage_pixels_since_cleanup = 0;
            self.last_partial_at = None;
        } else {
            self.partial_damage_pixels_since_cleanup = self
                .partial_damage_pixels_since_cleanup
                .saturating_add(damage_pixels(damage));
            self.last_partial_at = Some(now);
        }
    }

    fn realtime_cleanup_due(&self, now: Duration) -> bool {
        self.refresh_policy.config().profile == crate::RefreshProfile::Realtime
            && self.presented_frame_id != 0
            && self.partial_damage_pixels_since_cleanup
                >= u64::from(self.width())
                    .saturating_mul(u64::from(self.height()))
                    .saturating_mul(REALTIME_CLEANUP_SCREEN_EQUIVALENTS)
            && self
                .last_partial_at
                .is_some_and(|last| now.saturating_sub(last) >= REALTIME_CLEANUP_IDLE)
    }

    pub fn update_refresh_config(
        &mut self,
        config: RefreshPolicyConfig,
    ) -> Result<bool, CoreError> {
        let changed = config != self.refresh_policy.config();
        self.refresh_policy.reconfigure(config)?;
        self.damage_tile = config.damage_tile;
        Ok(changed)
    }

    pub fn overlay_mut(&mut self) -> &mut LocalOverlay {
        &mut self.overlay
    }

    /// Immediately presents a receiver-owned overlay change without inventing
    /// a remote frame ID. Pending remote work is first completed so local UI
    /// can never make unacknowledged base pixels visible. Clearing the overlay
    /// therefore restores the last remote base deterministically.
    pub fn present_overlay(
        &mut self,
        now: Duration,
        panel: &mut dyn PanelBackend,
    ) -> Result<Vec<TerminalFrame>, CoreError> {
        let terminals = if self.pending.is_some() {
            self.tick_inner(now, panel, true)?
        } else {
            Vec::new()
        };
        let composed = self.base.compose(&self.overlay)?;
        let damage = tile_damage(&self.presented, &composed, self.damage_tile);
        if damage.is_empty() {
            return Ok(terminals);
        }
        let decision = self.refresh_policy.decide(
            FrameIntent::Settled,
            ContentClass::TextUi,
            damage_pixels(&damage),
            u64::from(self.width()) * u64::from(self.height()),
            false,
        );
        let damage =
            self.refresh_policy
                .damage_for_decision(decision, self.width(), self.height(), damage);
        let panel_metrics = panel.submit(&composed, &damage, decision)?;
        self.record_panel_damage(decision, &damage, now);
        self.presented = composed;
        self.working.clone_from(&self.presented);
        self.last_present_at = Some(now);
        self.panel_state_uncertain = false;
        self.refresh_policy
            .presented_submissions(decision, panel_metrics.physical_submissions);
        if decision.complete_refresh {
            self.force_cleanup_next = false;
        }
        if decision.complete_refresh || decision.waveform != Waveform::Fastest {
            self.settle_damage.clear();
            self.fast_updates_since_settled = 0;
        } else {
            self.settle_damage = merge_tiled_damage(
                self.width(),
                self.height(),
                self.damage_tile,
                &self.settle_damage,
                &damage,
            );
            self.fast_updates_since_settled = self
                .fast_updates_since_settled
                .saturating_add(panel_metrics.physical_submissions);
        }
        Ok(terminals)
    }

    pub fn commit(
        &mut self,
        frame: &ValidatedFrame,
        content_class: ContentClass,
        now: Duration,
    ) -> Result<CommitReport, CoreError> {
        self.commit_timed(frame, content_class, now, 0)
    }

    pub fn commit_timed(
        &mut self,
        frame: &ValidatedFrame,
        content_class: ContentClass,
        now: Duration,
        decode_us: u32,
    ) -> Result<CommitReport, CoreError> {
        if self.pending_is_settled() {
            return Err(CoreError::SettledBarrier);
        }
        if frame.frame_id == 0 || frame.frame_id <= self.logical_frame_id {
            return Err(CoreError::BadFrameId);
        }
        if frame.pixel_format != self.base.format() {
            return Err(CoreError::PixelFormat);
        }
        let keyframe = frame.base_frame_id == 0;
        let recovering_base = !self.base_valid;
        if !keyframe && (!self.base_valid || frame.base_frame_id != self.logical_frame_id) {
            return Err(CoreError::NeedKeyframe);
        }

        let apply_started = Instant::now();
        self.base.apply_regions_atomic(&frame.regions)?;
        let apply_elapsed = apply_started.elapsed();
        let decode_us = decode_us.saturating_add(duration_us(apply_elapsed));
        self.logical_frame_id = frame.frame_id;
        self.base_valid = true;

        let old_pending = self.pending.take();
        let carry_full_damage = old_pending
            .as_ref()
            .is_some_and(|pending| pending.force_full_damage);
        let carry_cleanup = old_pending
            .as_ref()
            .is_some_and(|pending| pending.force_cleanup);
        let mut damage_hint = frame
            .regions
            .iter()
            .map(|region| region.rect.clone())
            .collect::<Vec<_>>();
        if let Some(old) = old_pending.as_ref() {
            damage_hint.extend(old.damage_hint.iter().cloned());
        }
        let superseded = old_pending.map(|old| TerminalFrame {
            frame_id: old.frame_id,
            outcome: PresentationOutcome::Superseded,
            metrics: PresentationMetrics {
                decode_us: old.decode_us,
                queue_us: duration_us(now.saturating_sub(old.accepted_at)),
                ..PresentationMetrics::default()
            },
        });
        self.pending = Some(PendingPresentation {
            frame_id: frame.frame_id,
            intent: frame.intent,
            content_class,
            accepted_at: now,
            force_full_damage: carry_full_damage
                || keyframe && (self.presented_frame_id == 0 || recovering_base),
            force_cleanup: carry_cleanup || keyframe && self.panel_state_uncertain,
            decode_us,
            decoded_bytes: frame.decoded_bytes,
            damage_hint,
        });
        Ok(CommitReport {
            logical_frame_id: self.logical_frame_id,
            superseded,
        })
    }

    /// Abandon work when its surface is explicitly closed or replaced.  The
    /// caller must still emit the returned terminal result before forgetting
    /// the old surface identity.
    pub fn cancel_pending(&mut self) -> Option<TerminalFrame> {
        self.pending.take().map(|pending| TerminalFrame {
            frame_id: pending.frame_id,
            outcome: PresentationOutcome::Cancelled,
            metrics: PresentationMetrics {
                decode_us: pending.decode_us,
                ..PresentationMetrics::default()
            },
        })
    }

    pub fn next_deadline(&self) -> Option<Duration> {
        let pending = self.pending.as_ref()?;
        let fps_due = self.last_present_at.map_or(pending.accepted_at, |last| {
            last.saturating_add(self.frame_interval)
        });
        if pending.intent == FrameIntent::Settled {
            Some(fps_due.min(pending.accepted_at.saturating_add(self.settled_deadline)))
        } else {
            Some(fps_due)
        }
    }

    /// Apply a receiver-owned refresh profile while preserving frame ordering.
    /// Any pending LATEST or SETTLED presentation is flushed immediately as a
    /// complete refresh. With no pending frame, the currently displayed image
    /// is resubmitted as a maintenance refresh. If no image exists yet or the
    /// backend fails, cleanup remains armed for the next valid presentation.
    pub fn change_refresh_profile(
        &mut self,
        config: RefreshPolicyConfig,
        now: Duration,
        panel: &mut dyn PanelBackend,
    ) -> Result<RefreshProfileChange, CoreError> {
        let changed = self.update_refresh_config(config)?;
        let cleanup = if changed || self.force_cleanup_next {
            self.request_cleanup(now, panel)?
        } else {
            CleanupReport {
                cleanup_performed: false,
                cleanup_pending: false,
                backend_failed: false,
                terminals: Vec::new(),
            }
        };
        Ok(RefreshProfileChange {
            config,
            changed,
            cleanup_performed: cleanup.cleanup_performed,
            cleanup_pending: cleanup.cleanup_pending,
            backend_failed: cleanup.backend_failed,
            terminals: cleanup.terminals,
        })
    }

    /// Request one receiver-decided complete refresh without exposing a native
    /// waveform. Pending frame barriers are completed immediately; otherwise
    /// the current presented image is resubmitted. With no image, the request
    /// remains armed for the next valid frame.
    pub fn request_cleanup(
        &mut self,
        now: Duration,
        panel: &mut dyn PanelBackend,
    ) -> Result<CleanupReport, CoreError> {
        self.force_cleanup_next = true;
        let mut report = CleanupReport {
            cleanup_performed: false,
            cleanup_pending: true,
            backend_failed: false,
            terminals: Vec::new(),
        };
        if self.pending.is_some() {
            report.terminals = self.tick_inner(now, panel, true)?;
            report.cleanup_performed = report.terminals.iter().any(|terminal| {
                terminal.outcome == PresentationOutcome::Presented
                    && terminal.metrics.complete_refresh
            });
            report.backend_failed = report
                .terminals
                .iter()
                .any(|terminal| terminal.outcome == PresentationOutcome::BackendFailure);
            report.cleanup_pending = self.force_cleanup_next;
            return Ok(report);
        }

        if self.presented_frame_id == 0 {
            return Ok(report);
        }

        if panel.pump().is_err() {
            self.mark_panel_failure();
            report.backend_failed = true;
            report.cleanup_pending = true;
            return Ok(report);
        }
        let decision = self.refresh_policy.decide(
            FrameIntent::Settled,
            ContentClass::Mixed,
            u64::from(self.width()) * u64::from(self.height()),
            u64::from(self.width()) * u64::from(self.height()),
            true,
        );
        let damage = full_damage(self.width(), self.height());
        if panel.submit(&self.presented, &damage, decision).is_err() {
            self.mark_panel_failure();
            report.backend_failed = true;
            report.cleanup_pending = true;
            return Ok(report);
        }
        self.last_present_at = Some(now);
        self.panel_state_uncertain = false;
        self.refresh_policy.presented(decision);
        self.record_panel_damage(decision, &damage, now);
        self.force_cleanup_next = false;
        self.settle_damage.clear();
        self.fast_updates_since_settled = 0;
        report.cleanup_performed = true;
        report.cleanup_pending = false;
        Ok(report)
    }

    pub fn tick(
        &mut self,
        now: Duration,
        panel: &mut dyn PanelBackend,
    ) -> Result<Vec<TerminalFrame>, CoreError> {
        if self.pending.is_none() && self.realtime_cleanup_due(now) {
            return Ok(self.request_cleanup(now, panel)?.terminals);
        }
        self.tick_inner(now, panel, false)
    }

    fn tick_inner(
        &mut self,
        now: Duration,
        panel: &mut dyn PanelBackend,
        ignore_deadline: bool,
    ) -> Result<Vec<TerminalFrame>, CoreError> {
        // An idle core has no Qt/Quill work to advance. In particular, do not
        // call QCoreApplication::processEvents at the server's network timeout
        // cadence: every actual Quill submission already pumps once.
        let Some(deadline) = self.next_deadline() else {
            return Ok(Vec::new());
        };
        if !ignore_deadline && now < deadline {
            return Ok(Vec::new());
        }
        if panel.pump().is_err() {
            self.mark_panel_failure();
            return Ok(self
                .pending
                .take()
                .map(|pending| {
                    vec![TerminalFrame {
                        frame_id: pending.frame_id,
                        outcome: PresentationOutcome::BackendFailure,
                        metrics: PresentationMetrics {
                            decode_us: pending.decode_us,
                            queue_us: duration_us(now.saturating_sub(pending.accepted_at)),
                            ..PresentationMetrics::default()
                        },
                    }]
                })
                .unwrap_or_default());
        }
        let pending = self
            .pending
            .take()
            .expect("deadline requires pending frame");
        let present_started = Instant::now();
        let compose_started = Instant::now();
        let mut compose_regions = if pending.force_full_damage {
            full_damage(self.width(), self.height())
        } else {
            pending.damage_hint.clone()
        };
        if pending.intent == FrameIntent::Settled {
            compose_regions.extend(self.settle_damage.iter().cloned());
        }
        self.base
            .compose_regions_into(&self.overlay, &mut self.working, &compose_regions)?;
        let damage = if pending.force_full_damage {
            full_damage(self.width(), self.height())
        } else {
            tile_damage_regions(
                &self.presented,
                &self.working,
                self.damage_tile,
                &compose_regions,
            )
        };
        let decision_damage = if pending.intent == FrameIntent::Settled {
            merge_tiled_damage(
                self.width(),
                self.height(),
                self.damage_tile,
                &self.settle_damage,
                &damage,
            )
        } else {
            damage.clone()
        };
        let force_cleanup = pending.force_cleanup || self.force_cleanup_next;
        let static_cleanup_due = !force_cleanup
            && pending.intent == FrameIntent::Settled
            && !self.settle_damage.is_empty()
            && self
                .refresh_policy
                .config()
                .static_cleanup_after_fast_updates
                > 0
            && self.fast_updates_since_settled
                >= self
                    .refresh_policy
                    .config()
                    .static_cleanup_after_fast_updates;
        let mut decision = self.refresh_policy.decide(
            pending.intent,
            pending.content_class,
            damage_pixels(&decision_damage),
            u64::from(self.width()) * u64::from(self.height()),
            force_cleanup || static_cleanup_due,
        );
        if static_cleanup_due && decision.full_refresh_reason == FullRefreshReason::Forced {
            decision.full_refresh_reason = FullRefreshReason::StaticFastDebt;
        }
        // A Fastest SETTLED frame is only an ordering barrier. Do
        // not repaint historical fast tiles with the same fast waveform.
        let damage =
            if pending.intent == FrameIntent::Settled && decision.waveform != Waveform::Fastest {
                decision_damage
            } else {
                damage
            };
        let mut damage =
            self.refresh_policy
                .damage_for_decision(decision, self.width(), self.height(), damage);
        if decision.complete_refresh {
            let full = full_damage(self.width(), self.height());
            self.base
                .compose_regions_into(&self.overlay, &mut self.working, &full)?;
            damage = full;
        }
        let compose_us = elapsed_us(compose_started);
        let damage_pixels = damage.iter().fold(0_u64, |total, rect| {
            total.saturating_add(u64::from(rect.width) * u64::from(rect.height))
        });
        let mut metrics = PresentationMetrics {
            decode_us: pending.decode_us,
            queue_us: duration_us(now.saturating_sub(pending.accepted_at)),
            compose_us,
            damage_pixels: damage_pixels.min(u64::from(u32::MAX)) as u32,
            damage_regions: damage.len().min(u32::MAX as usize) as u32,
            waveform: decision.waveform as u32,
            complete_refresh: decision.complete_refresh,
            full_refresh_reason: decision.full_refresh_reason,
            ..PresentationMetrics::default()
        };

        // A frame whose composed pixels already match the panel is logically
        // presented without asking backends to accept an empty update. Fast
        // waveform debt prevents this shortcut from swallowing the final
        // high-quality SETTLED repaint.
        if damage.is_empty() {
            metrics.present_us = elapsed_us(present_started);
            self.presented_frame_id = pending.frame_id;
            self.last_present_at = Some(now);
            return Ok(vec![TerminalFrame {
                frame_id: pending.frame_id,
                outcome: PresentationOutcome::Presented,
                metrics,
            }]);
        }

        let panel_metrics = match panel.submit(&self.working, &damage, decision) {
            Ok(panel_metrics) => panel_metrics,
            Err(_) => {
                metrics.present_us = elapsed_us(present_started);
                self.working.clone_from(&self.presented);
                self.mark_panel_failure();
                return Ok(vec![TerminalFrame {
                    frame_id: pending.frame_id,
                    outcome: PresentationOutcome::BackendFailure,
                    metrics,
                }]);
            }
        };
        self.record_panel_damage(decision, &damage, now);
        metrics.convert_us = panel_metrics.convert_us;
        metrics.submit_us = panel_metrics.submit_us;
        metrics.present_us = elapsed_us(present_started);

        std::mem::swap(&mut self.presented, &mut self.working);
        self.working.copy_regions_from(&self.presented, &damage)?;
        self.presented_frame_id = pending.frame_id;
        self.last_present_at = Some(now);
        self.panel_state_uncertain = false;
        self.refresh_policy
            .presented_submissions(decision, panel_metrics.physical_submissions);
        if decision.complete_refresh
            || pending.intent == FrameIntent::Settled && decision.waveform != Waveform::Fastest
        {
            self.settle_damage.clear();
            self.fast_updates_since_settled = 0;
        } else if matches!(decision.waveform, Waveform::Fastest | Waveform::Fast) {
            self.settle_damage = merge_tiled_damage(
                self.width(),
                self.height(),
                self.damage_tile,
                &self.settle_damage,
                &damage,
            );
            self.fast_updates_since_settled = self
                .fast_updates_since_settled
                .saturating_add(panel_metrics.physical_submissions);
        }
        if decision.complete_refresh {
            self.force_cleanup_next = false;
        }
        Ok(vec![TerminalFrame {
            frame_id: pending.frame_id,
            outcome: PresentationOutcome::Presented,
            metrics,
        }])
    }

    fn mark_panel_failure(&mut self) {
        self.base_valid = false;
        self.logical_frame_id = 0;
        self.panel_state_uncertain = true;
        self.settle_damage.clear();
        self.fast_updates_since_settled = 0;
    }
}

fn full_damage(width: u32, height: u32) -> Vec<Rect> {
    vec![Rect {
        x: 0,
        y: 0,
        width,
        height,
    }]
}

fn damage_pixels(damage: &[Rect]) -> u64 {
    damage.iter().fold(0, |total, rect| {
        total.saturating_add(u64::from(rect.width) * u64::from(rect.height))
    })
}

fn merge_tiled_damage(
    width: u32,
    height: u32,
    tile: u32,
    existing: &[Rect],
    damage: &[Rect],
) -> Vec<Rect> {
    let tile = tile.max(1);
    let tiles_x = width.div_ceil(tile);
    let tiles_y = height.div_ceil(tile);
    let mut dirty = vec![false; (tiles_x * tiles_y) as usize];
    for rect in existing.iter().chain(damage) {
        let right = rect.x.saturating_add(rect.width).min(width);
        let bottom = rect.y.saturating_add(rect.height).min(height);
        if rect.x >= right || rect.y >= bottom {
            continue;
        }
        for tile_y in rect.y / tile..=(bottom - 1) / tile {
            for tile_x in rect.x / tile..=(right - 1) / tile {
                dirty[(tile_y * tiles_x + tile_x) as usize] = true;
            }
        }
    }
    dirty
        .into_iter()
        .enumerate()
        .filter(|(_, dirty)| *dirty)
        .map(|(index, _)| {
            let tile_x = index as u32 % tiles_x;
            let tile_y = index as u32 / tiles_x;
            let x = tile_x * tile;
            let y = tile_y * tile;
            Rect {
                x,
                y,
                width: tile.min(width - x),
                height: tile.min(height - y),
            }
        })
        .collect()
}

fn duration_us(duration: Duration) -> u32 {
    duration.as_micros().min(u128::from(u32::MAX)) as u32
}

fn elapsed_us(started: Instant) -> u32 {
    duration_us(started.elapsed())
}

#[cfg(test)]
mod tests {
    use rm_display_protocol::{semantic::DecodedRegion, Rect};

    use super::*;
    use crate::{MockPanel, RefreshProfile};

    fn frame(id: u64, base: u64, intent: FrameIntent, pixels: Vec<u8>) -> ValidatedFrame {
        ValidatedFrame {
            frame_id: id,
            base_frame_id: base,
            intent,
            pixel_format: PixelFormat::Gray8,
            decoded_bytes: pixels.len(),
            regions: vec![DecodedRegion {
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                },
                pixels,
            }],
        }
    }

    fn region_frame(
        id: u64,
        base: u64,
        intent: FrameIntent,
        rect: Rect,
        pixels: Vec<u8>,
    ) -> ValidatedFrame {
        ValidatedFrame {
            frame_id: id,
            base_frame_id: base,
            intent,
            pixel_format: PixelFormat::Gray8,
            decoded_bytes: pixels.len(),
            regions: vec![DecodedRegion { rect, pixels }],
        }
    }

    fn core() -> DisplayCore {
        DisplayCore::new(
            2,
            2,
            400,
            Duration::from_millis(200),
            RefreshPolicyConfig::default(),
        )
        .unwrap()
    }

    #[test]
    fn settled_damage_keeps_distant_tiles_sparse() {
        let damage = merge_tiled_damage(
            8,
            2,
            2,
            &[Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            }],
            &[Rect {
                x: 6,
                y: 0,
                width: 2,
                height: 2,
            }],
        );
        assert_eq!(damage.len(), 2);
        assert_eq!(damage_pixels(&damage), 8);
    }

    #[test]
    fn latest_replaces_exactly_one_pending_frame() {
        let mut core = core();
        core.commit(
            &frame(1, 0, FrameIntent::Latest, vec![1; 4]),
            ContentClass::TextUi,
            Duration::ZERO,
        )
        .unwrap();
        let report = core
            .commit(
                &frame(2, 1, FrameIntent::Latest, vec![2; 4]),
                ContentClass::TextUi,
                Duration::from_millis(1),
            )
            .unwrap();
        let superseded = report.superseded.expect("pending frame was superseded");
        assert_eq!(superseded.frame_id, 1);
        assert_eq!(superseded.outcome, PresentationOutcome::Superseded);
        assert!(superseded.metrics.queue_us <= 1_100);
        assert_eq!(core.logical_frame_id(), 2);
    }

    #[test]
    fn superseded_delta_keeps_all_unpresented_damage_hints() {
        let config = RefreshPolicyConfig {
            damage_tile: 2,
            clean_first_frame: false,
            ..RefreshPolicyConfig::default()
        };
        let mut core = DisplayCore::new(4, 4, 400, Duration::from_millis(200), config).unwrap();
        let mut panel = MockPanel::new(4, 4);
        core.commit(
            &region_frame(
                1,
                0,
                FrameIntent::Latest,
                Rect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 4,
                },
                vec![0; 16],
            ),
            ContentClass::TextUi,
            Duration::ZERO,
        )
        .unwrap();
        core.tick(Duration::ZERO, &mut panel).unwrap();
        core.commit(
            &region_frame(
                2,
                1,
                FrameIntent::Latest,
                Rect {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                },
                vec![1; 4],
            ),
            ContentClass::TextUi,
            Duration::from_millis(10),
        )
        .unwrap();
        core.commit(
            &region_frame(
                3,
                2,
                FrameIntent::Latest,
                Rect {
                    x: 2,
                    y: 2,
                    width: 2,
                    height: 2,
                },
                vec![2; 4],
            ),
            ContentClass::TextUi,
            Duration::from_millis(20),
        )
        .unwrap();
        core.tick(Duration::from_millis(250), &mut panel).unwrap();
        assert_eq!(panel.submissions()[1].damage.len(), 2);
        assert_eq!(panel.submissions()[1].pixels[0], 1);
        assert_eq!(panel.submissions()[1].pixels[15], 2);
    }

    #[test]
    fn idle_and_not_yet_due_ticks_do_not_pump_panel_events() {
        let mut core = core();
        let mut panel = MockPanel::new(2, 2);
        assert!(core.tick(Duration::ZERO, &mut panel).unwrap().is_empty());
        assert_eq!(panel.pump_count(), 0);

        core.commit(
            &frame(1, 0, FrameIntent::Latest, vec![1; 4]),
            ContentClass::TextUi,
            Duration::from_millis(100),
        )
        .unwrap();
        assert!(core
            .tick(Duration::from_millis(99), &mut panel)
            .unwrap()
            .is_empty());
        assert_eq!(panel.pump_count(), 0);

        assert_eq!(
            core.tick(Duration::from_millis(100), &mut panel)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(panel.pump_count(), 1);
    }

    #[test]
    fn settled_is_a_barrier_and_is_presented_by_deadline() {
        let mut core = core();
        let mut panel = MockPanel::new(2, 2);
        core.commit(
            &frame(1, 0, FrameIntent::Settled, vec![0; 4]),
            ContentClass::TextUi,
            Duration::ZERO,
        )
        .unwrap();
        assert!(matches!(
            core.commit(
                &frame(2, 1, FrameIntent::Latest, vec![1; 4]),
                ContentClass::TextUi,
                Duration::from_millis(1),
            ),
            Err(CoreError::SettledBarrier)
        ));
        let terminal = core.tick(Duration::ZERO, &mut panel).unwrap();
        assert_eq!(terminal[0].outcome, PresentationOutcome::Presented);
        assert_eq!(panel.submissions().len(), 1);
        assert_eq!(panel.submissions()[0].damage[0].width, 2);
    }

    #[test]
    fn fps_limit_keeps_final_latest_frame_pending() {
        let mut core = core();
        let mut panel = MockPanel::new(2, 2);
        core.commit(
            &frame(1, 0, FrameIntent::Latest, vec![1; 4]),
            ContentClass::TextUi,
            Duration::ZERO,
        )
        .unwrap();
        core.tick(Duration::ZERO, &mut panel).unwrap();
        core.commit(
            &frame(2, 1, FrameIntent::Latest, vec![2; 4]),
            ContentClass::TextUi,
            Duration::from_millis(10),
        )
        .unwrap();
        assert!(core
            .tick(Duration::from_millis(100), &mut panel)
            .unwrap()
            .is_empty());
        assert!(core.has_pending());
        let terminal = core.tick(Duration::from_millis(250), &mut panel).unwrap();
        assert_eq!(terminal[0].frame_id, 2);
    }

    #[test]
    fn backend_failure_does_not_advance_presented_and_requires_keyframe() {
        let mut core = core();
        let mut panel = MockPanel::new(2, 2);
        panel.fail_next_submit("injected");
        core.commit(
            &frame(1, 0, FrameIntent::Latest, vec![1; 4]),
            ContentClass::TextUi,
            Duration::ZERO,
        )
        .unwrap();
        let terminal = core.tick(Duration::ZERO, &mut panel).unwrap();
        assert_eq!(terminal[0].outcome, PresentationOutcome::BackendFailure);
        assert_eq!(core.presented_frame_id(), 0);
        assert_eq!(core.logical_frame_id(), 0);
        assert!(matches!(
            core.commit(
                &frame(2, 1, FrameIntent::Latest, vec![2; 4]),
                ContentClass::TextUi,
                Duration::from_millis(1),
            ),
            Err(CoreError::NeedKeyframe)
        ));
        core.commit(
            &frame(3, 0, FrameIntent::Latest, vec![3; 4]),
            ContentClass::TextUi,
            Duration::from_millis(2),
        )
        .unwrap();
        assert!(core.pending.as_ref().unwrap().force_full_damage);
    }

    #[test]
    fn recovery_forces_cleanup_but_initial_full_damage_respects_policy() {
        let config = RefreshPolicyConfig {
            cleanup_after_updates: 0,
            clean_first_frame: false,
            ..RefreshPolicyConfig::default()
        };
        let mut core = DisplayCore::new(2, 2, 400, Duration::from_millis(200), config).unwrap();
        let mut panel = MockPanel::new(2, 2);
        core.commit(
            &frame(1, 0, FrameIntent::Latest, vec![1; 4]),
            ContentClass::TextUi,
            Duration::ZERO,
        )
        .unwrap();
        core.tick(Duration::ZERO, &mut panel).unwrap();
        assert!(!panel.submissions()[0].refresh.complete_refresh);
        assert_eq!(panel.submissions()[0].damage, full_damage(2, 2));

        panel.fail_next_submit("injected");
        core.commit(
            &frame(2, 1, FrameIntent::Latest, vec![2; 4]),
            ContentClass::TextUi,
            Duration::from_millis(250),
        )
        .unwrap();
        core.tick(Duration::from_millis(250), &mut panel).unwrap();
        core.commit(
            &frame(3, 0, FrameIntent::Latest, vec![3; 4]),
            ContentClass::TextUi,
            Duration::from_millis(500),
        )
        .unwrap();
        core.tick(Duration::from_millis(500), &mut panel).unwrap();
        assert!(panel.submissions()[1].refresh.complete_refresh);
    }

    #[test]
    fn cancelling_pending_returns_one_terminal_result() {
        let mut core = core();
        core.commit(
            &frame(1, 0, FrameIntent::Latest, vec![1; 4]),
            ContentClass::TextUi,
            Duration::ZERO,
        )
        .unwrap();
        let cancelled = core.cancel_pending().expect("pending frame was cancelled");
        assert_eq!(cancelled.frame_id, 1);
        assert_eq!(cancelled.outcome, PresentationOutcome::Cancelled);
        assert_eq!(core.cancel_pending(), None);
    }

    #[test]
    fn realtime_settled_repaints_fastest_damage_with_fast_waveform() {
        let config = RefreshPolicyConfig {
            damage_tile: 2,
            ..RefreshPolicyConfig::for_profile(RefreshProfile::Realtime)
        };
        let mut core = DisplayCore::new(4, 4, 400, Duration::from_millis(200), config).unwrap();
        let mut panel = MockPanel::new(4, 4);

        core.commit(
            &region_frame(
                1,
                0,
                FrameIntent::Latest,
                Rect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 4,
                },
                vec![1; 16],
            ),
            ContentClass::TextUi,
            Duration::ZERO,
        )
        .unwrap();
        core.tick(Duration::ZERO, &mut panel).unwrap();

        core.commit(
            &region_frame(
                2,
                1,
                FrameIntent::Settled,
                Rect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 4,
                },
                vec![1; 16],
            ),
            ContentClass::TextUi,
            Duration::from_millis(10),
        )
        .unwrap();
        core.tick(Duration::from_millis(210), &mut panel).unwrap();
        assert_eq!(panel.submissions()[1].refresh.waveform, Waveform::Fast);

        let quadrant = Rect {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        core.commit(
            &region_frame(3, 2, FrameIntent::Latest, quadrant.clone(), vec![2; 4]),
            ContentClass::TextUi,
            Duration::from_millis(220),
        )
        .unwrap();
        core.tick(Duration::from_millis(460), &mut panel).unwrap();
        assert_eq!(panel.submissions()[2].refresh.waveform, Waveform::Fastest);

        core.commit(
            &region_frame(4, 3, FrameIntent::Settled, quadrant.clone(), vec![2; 4]),
            ContentClass::TextUi,
            Duration::from_millis(470),
        )
        .unwrap();
        core.tick(Duration::from_millis(710), &mut panel).unwrap();
        assert_eq!(panel.submissions().len(), 4);
        assert_eq!(panel.submissions()[3].damage, vec![quadrant.clone()]);
        assert_eq!(panel.submissions()[3].refresh.waveform, Waveform::Fast);
        assert!(!panel.submissions()[3].refresh.complete_refresh);

        // Repeating an already-settled image is a logical presentation only,
        // not an accidental full-screen refresh.
        core.commit(
            &region_frame(5, 4, FrameIntent::Settled, quadrant, vec![2; 4]),
            ContentClass::TextUi,
            Duration::from_millis(720),
        )
        .unwrap();
        let terminal = core.tick(Duration::from_millis(970), &mut panel).unwrap();
        assert_eq!(terminal[0].metrics.damage_pixels, 0);
        assert_eq!(panel.submissions().len(), 4);
        assert_eq!(core.presented_frame_id(), 5);
    }

    #[test]
    fn fastest_settled_is_a_barrier_without_repainting_fast_damage() {
        let config = RefreshPolicyConfig {
            settled_waveform: Waveform::Fastest,
            clean_first_frame: false,
            damage_tile: 2,
            ..RefreshPolicyConfig::default()
        };
        let mut core = DisplayCore::new(2, 2, 400, Duration::from_millis(200), config).unwrap();
        let mut panel = MockPanel::new(2, 2);

        core.commit(
            &frame(1, 0, FrameIntent::Latest, vec![1; 4]),
            ContentClass::TextUi,
            Duration::ZERO,
        )
        .unwrap();
        core.tick(Duration::ZERO, &mut panel).unwrap();
        assert_eq!(panel.submissions().len(), 1);
        assert_eq!(core.fast_updates_since_settled(), 1);

        core.commit(
            &frame(2, 1, FrameIntent::Settled, vec![1; 4]),
            ContentClass::TextUi,
            Duration::from_millis(10),
        )
        .unwrap();
        let terminal = core.tick(Duration::from_millis(210), &mut panel).unwrap();
        assert_eq!(terminal[0].metrics.damage_pixels, 0);
        assert_eq!(panel.submissions().len(), 1);
        assert_eq!(core.fast_updates_since_settled(), 1);
    }

    #[test]
    fn realtime_cleanup_uses_actual_damage_and_an_idle_deadline() {
        let config = RefreshPolicyConfig {
            damage_tile: 2,
            ..RefreshPolicyConfig::for_profile(RefreshProfile::Realtime)
        };
        let mut core = DisplayCore::new(2, 2, 400, Duration::from_millis(200), config).unwrap();
        let mut panel = MockPanel::new(2, 2);

        core.commit(
            &frame(1, 0, FrameIntent::Latest, vec![1; 4]),
            ContentClass::TextUi,
            Duration::ZERO,
        )
        .unwrap();
        core.tick(Duration::ZERO, &mut panel).unwrap();
        core.commit(
            &frame(2, 1, FrameIntent::Latest, vec![2; 4]),
            ContentClass::TextUi,
            Duration::from_millis(250),
        )
        .unwrap();
        core.tick(Duration::from_millis(250), &mut panel).unwrap();

        core.tick(Duration::from_millis(3_249), &mut panel).unwrap();
        assert_eq!(panel.submissions().len(), 2);
        core.tick(Duration::from_millis(3_250), &mut panel).unwrap();
        assert_eq!(panel.submissions().len(), 3);
        assert!(panel.submissions()[2].refresh.complete_refresh);
        assert_eq!(core.partial_damage_pixels_since_cleanup, 0);
    }

    #[test]
    fn settled_cleans_accumulated_fast_waveform_debt_once() {
        let config = RefreshPolicyConfig {
            cleanup_after_updates: 0,
            clean_first_frame: false,
            large_update_threshold_percent: 0,
            static_cleanup_after_fast_updates: 2,
            ..RefreshPolicyConfig::default()
        };
        let mut core = DisplayCore::new(2, 2, 400, Duration::from_millis(200), config).unwrap();
        let mut panel = MockPanel::new(2, 2);

        core.commit(
            &frame(1, 0, FrameIntent::Latest, vec![1; 4]),
            ContentClass::TextUi,
            Duration::ZERO,
        )
        .unwrap();
        core.tick(Duration::ZERO, &mut panel).unwrap();
        core.commit(
            &frame(2, 1, FrameIntent::Latest, vec![2; 4]),
            ContentClass::TextUi,
            Duration::from_millis(250),
        )
        .unwrap();
        core.tick(Duration::from_millis(250), &mut panel).unwrap();
        assert_eq!(core.fast_updates_since_settled(), 2);
        assert!(panel
            .submissions()
            .iter()
            .all(|submission| !submission.refresh.complete_refresh));

        core.commit(
            &frame(3, 2, FrameIntent::Settled, vec![2; 4]),
            ContentClass::TextUi,
            Duration::from_millis(500),
        )
        .unwrap();
        let terminal = core.tick(Duration::from_millis(700), &mut panel).unwrap();
        assert!(terminal[0].metrics.complete_refresh);
        assert_eq!(
            terminal[0].metrics.full_refresh_reason,
            FullRefreshReason::StaticFastDebt
        );
        assert_eq!(core.fast_updates_since_settled(), 0);

        core.commit(
            &frame(4, 3, FrameIntent::Settled, vec![2; 4]),
            ContentClass::TextUi,
            Duration::from_millis(710),
        )
        .unwrap();
        let terminal = core.tick(Duration::from_millis(910), &mut panel).unwrap();
        assert!(!terminal[0].metrics.complete_refresh);
        assert_eq!(panel.submissions().len(), 3);
    }

    #[test]
    fn profile_switch_flushes_pending_settled_as_complete_refresh() {
        let mut config = RefreshPolicyConfig::for_profile(RefreshProfile::Animate);
        config.clean_first_frame = false;
        let mut core = DisplayCore::new(2, 2, 400, Duration::from_millis(200), config).unwrap();
        let mut panel = MockPanel::new(2, 2);
        core.commit(
            &frame(1, 0, FrameIntent::Settled, vec![7; 4]),
            ContentClass::TextUi,
            Duration::ZERO,
        )
        .unwrap();

        let next = core.refresh_config().switched_to(RefreshProfile::Quality);
        let report = core
            .change_refresh_profile(next, Duration::from_millis(1), &mut panel)
            .unwrap();

        assert!(report.changed);
        assert!(report.cleanup_performed);
        assert!(!report.cleanup_pending);
        assert_eq!(report.terminals.len(), 1);
        assert_eq!(report.terminals[0].frame_id, 1);
        assert_eq!(report.terminals[0].outcome, PresentationOutcome::Presented);
        assert_eq!(panel.submissions().len(), 1);
        assert!(panel.submissions()[0].refresh.complete_refresh);
        assert_eq!(
            panel.submissions()[0].refresh.waveform,
            Waveform::FullQuality
        );
    }

    #[test]
    fn profile_switch_repaints_current_frame_and_clears_fast_damage_debt() {
        let mut config = RefreshPolicyConfig::for_profile(RefreshProfile::Animate);
        config.clean_first_frame = false;
        let mut core = DisplayCore::new(2, 2, 400, Duration::from_millis(200), config).unwrap();
        let mut panel = MockPanel::new(2, 2);
        core.commit(
            &frame(1, 0, FrameIntent::Latest, vec![4; 4]),
            ContentClass::TextUi,
            Duration::ZERO,
        )
        .unwrap();
        core.tick(Duration::ZERO, &mut panel).unwrap();
        assert_eq!(panel.submissions()[0].refresh.waveform, Waveform::Fastest);
        assert!(!panel.submissions()[0].refresh.complete_refresh);

        let next = core.refresh_config().switched_to(RefreshProfile::Balanced);
        let report = core
            .change_refresh_profile(next, Duration::from_millis(10), &mut panel)
            .unwrap();
        assert!(report.cleanup_performed);
        assert!(!core.cleanup_pending());
        assert_eq!(panel.submissions().len(), 2);
        assert!(panel.submissions()[1].refresh.complete_refresh);
        assert_eq!(panel.submissions()[1].damage, full_damage(2, 2));

        core.commit(
            &frame(2, 1, FrameIntent::Settled, vec![4; 4]),
            ContentClass::TextUi,
            Duration::from_millis(20),
        )
        .unwrap();
        core.tick(Duration::from_millis(260), &mut panel).unwrap();
        assert_eq!(panel.submissions().len(), 2);
    }

    #[test]
    fn profile_switch_before_first_frame_arms_cleanup_until_presented() {
        let mut config = RefreshPolicyConfig::for_profile(RefreshProfile::Animate);
        config.clean_first_frame = false;
        let mut core = DisplayCore::new(2, 2, 400, Duration::from_millis(200), config).unwrap();
        let mut panel = MockPanel::new(2, 2);
        let next = core.refresh_config().switched_to(RefreshProfile::Quality);
        let report = core
            .change_refresh_profile(next, Duration::ZERO, &mut panel)
            .unwrap();
        assert!(report.cleanup_pending);
        assert!(panel.submissions().is_empty());

        core.commit(
            &frame(1, 0, FrameIntent::Latest, vec![1; 4]),
            ContentClass::TextUi,
            Duration::from_millis(1),
        )
        .unwrap();
        core.tick(Duration::from_millis(1), &mut panel).unwrap();
        assert!(panel.submissions()[0].refresh.complete_refresh);
        assert_eq!(
            panel.submissions()[0].refresh.waveform,
            Waveform::FullQuality
        );
        assert!(!core.cleanup_pending());
    }

    #[test]
    fn cleanup_interval_counts_physical_partial_submissions_not_zero_damage() {
        let mut config = RefreshPolicyConfig::for_profile(RefreshProfile::Balanced);
        config.clean_first_frame = false;
        config.cleanup_after_updates = 2;
        let mut core = DisplayCore::new(2, 2, 400, Duration::from_millis(200), config).unwrap();
        let mut panel = MockPanel::new(2, 2);

        core.commit(
            &frame(1, 0, FrameIntent::Latest, vec![1; 4]),
            ContentClass::TextUi,
            Duration::ZERO,
        )
        .unwrap();
        core.tick(Duration::ZERO, &mut panel).unwrap();
        assert_eq!(core.presented_since_full_refresh(), 1);

        core.commit(
            &frame(2, 1, FrameIntent::Latest, vec![1; 4]),
            ContentClass::TextUi,
            Duration::from_millis(250),
        )
        .unwrap();
        core.tick(Duration::from_millis(250), &mut panel).unwrap();
        assert_eq!(panel.submissions().len(), 1);
        assert_eq!(core.presented_since_full_refresh(), 1);

        core.commit(
            &frame(3, 2, FrameIntent::Latest, vec![2; 4]),
            ContentClass::TextUi,
            Duration::from_millis(500),
        )
        .unwrap();
        core.tick(Duration::from_millis(500), &mut panel).unwrap();
        assert!(!panel.submissions()[1].refresh.complete_refresh);
        assert_eq!(core.presented_since_full_refresh(), 2);

        core.commit(
            &frame(4, 3, FrameIntent::Latest, vec![3; 4]),
            ContentClass::TextUi,
            Duration::from_millis(750),
        )
        .unwrap();
        core.tick(Duration::from_millis(750), &mut panel).unwrap();
        assert!(panel.submissions()[2].refresh.complete_refresh);
        assert_eq!(core.presented_since_full_refresh(), 0);
    }

    #[test]
    fn restored_debt_reaches_threshold_with_clean_first_frame_disabled() {
        let mut config = RefreshPolicyConfig::for_profile(RefreshProfile::Balanced);
        config.clean_first_frame = false;
        config.cleanup_after_updates = 2;
        let mut core = DisplayCore::new(2, 2, 400, Duration::from_millis(200), config).unwrap();
        core.restore_refresh_debt(RefreshDebt {
            has_presented: true,
            physical_partial_updates_since_cleanup: 2,
        })
        .unwrap();
        let mut panel = MockPanel::new(2, 2);
        core.commit(
            &frame(1, 0, FrameIntent::Latest, vec![3; 4]),
            ContentClass::TextUi,
            Duration::ZERO,
        )
        .unwrap();
        core.tick(Duration::ZERO, &mut panel).unwrap();
        assert!(panel.submissions()[0].refresh.complete_refresh);
        assert_eq!(core.presented_since_full_refresh(), 0);
    }

    #[test]
    fn explicit_cleanup_repaints_current_frame_and_leaves_no_pending_debt() {
        let mut config = RefreshPolicyConfig::for_profile(RefreshProfile::Animate);
        config.clean_first_frame = false;
        let mut core = DisplayCore::new(2, 2, 400, Duration::from_millis(200), config).unwrap();
        let mut panel = MockPanel::new(2, 2);
        core.commit(
            &frame(1, 0, FrameIntent::Latest, vec![8; 4]),
            ContentClass::TextUi,
            Duration::ZERO,
        )
        .unwrap();
        core.tick(Duration::ZERO, &mut panel).unwrap();

        let report = core
            .request_cleanup(Duration::from_millis(10), &mut panel)
            .unwrap();
        assert!(report.cleanup_performed);
        assert!(!report.cleanup_pending);
        assert!(report.terminals.is_empty());
        assert_eq!(panel.submissions().len(), 2);
        assert!(panel.submissions()[1].refresh.complete_refresh);
        assert_eq!(panel.submissions()[1].damage, full_damage(2, 2));
    }
}
