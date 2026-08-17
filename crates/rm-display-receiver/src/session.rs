use std::time::{Duration, Instant};

use rand::{rngs::OsRng, RngCore};
use rm_display_core::{
    CoreError, DisplayCore, GraySurface, LocalOverlay, PanelBackend, PresentationOutcome,
    RefreshDebt, RefreshDecision, RefreshPolicyConfig, RefreshProfile as CoreRefreshProfile,
    TerminalFrame, Waveform,
};
use rm_display_protocol::envelope::Body;
use rm_display_protocol::semantic::{validate_and_decode_frame, SemanticError, SurfaceState};
use rm_display_protocol::{
    ClientHello, ContentClass, DisplayInfo, Encoding, Envelope, EpaperProfile,
    EpaperProfileConfiguration, EpaperProfileOperation, EpaperProfileRequest, EpaperProfileResult,
    EpaperProfileResultCode, EpaperProfileState, EpaperRefreshOperation, EpaperRefreshRequest,
    EpaperRefreshResult, EpaperRefreshResultCode, EpaperRefreshState, EpaperWaveform, Frame,
    FrameMetrics, FrameResult, FrameResultCode, FrameResultReason, FullRefreshReason,
    InputCapability, Limits, Orientation, PixelFormat, PointerDevice, PointerPhase, PointerRecord,
    ProducerKind, ProtocolError, ProtocolFeature, Rect, ServerHello, SourceKind, SurfaceOpen,
    SurfaceReady,
};
use thiserror::Error;

use crate::config::ReceiverConfig;
use crate::evdev::{FiveFingerCleanupGesture, PhysicalPointerEvent, PointerPhase as PhysicalPhase};
use crate::local_menu::{LocalMenu, LocalMenuAction};

const MAX_MINOR: u32 = 2;

const MANDATORY_FEATURES: [ProtocolFeature; 4] = [
    ProtocolFeature::AtomicMultiRegion,
    ProtocolFeature::ExactBaseDelta,
    ProtocolFeature::LatestSupersede,
    ProtocolFeature::SettledBarrier,
];

struct ActiveSurface {
    surface_id: u32,
    generation: u32,
    touch_enabled: bool,
    core: DisplayCore,
}

struct LocalFallback {
    base: GraySurface,
    overlay: LocalOverlay,
}

pub struct Session<'a> {
    config: ReceiverConfig,
    panel: &'a mut dyn PanelBackend,
    session_id: u64,
    established: bool,
    closed: bool,
    receiver_exit_requested: bool,
    pairing_reset_requested: bool,
    last_incoming_message_id: u32,
    next_outgoing_message_id: u32,
    next_generation: u32,
    next_input_sequence: u64,
    last_profile_request_id: u32,
    last_refresh_request_id: u32,
    profile_control_enabled: bool,
    custom_profile_control_enabled: bool,
    refresh_control_enabled: bool,
    selected_minor: u32,
    negotiated_encodings: Vec<i32>,
    color_rgb565_enabled: bool,
    byte_credits_enabled: bool,
    cleanup_pending_without_surface: bool,
    refresh_debt_without_surface: RefreshDebt,
    touch_gesture: FiveFingerCleanupGesture,
    local_menu: LocalMenu,
    last_custom_profile: Option<RefreshPolicyConfig>,
    surface: Option<ActiveSurface>,
    fallback: Option<LocalFallback>,
}

impl<'a> Session<'a> {
    pub fn new(config: ReceiverConfig, panel: &'a mut dyn PanelBackend) -> Self {
        Self::new_with_fallback(config, panel, None)
    }

    pub fn new_with_fallback(
        config: ReceiverConfig,
        panel: &'a mut dyn PanelBackend,
        fallback: Option<GraySurface>,
    ) -> Self {
        let fallback = fallback.map(|base| LocalFallback {
            overlay: LocalOverlay::transparent(base.width(), base.height())
                .expect("fallback surface geometry was already validated"),
            base,
        });
        Self {
            config,
            panel,
            session_id: random_nonzero_u64(),
            established: false,
            closed: false,
            receiver_exit_requested: false,
            pairing_reset_requested: false,
            last_incoming_message_id: 0,
            next_outgoing_message_id: 1,
            next_generation: 1,
            next_input_sequence: 1,
            last_profile_request_id: 0,
            last_refresh_request_id: 0,
            profile_control_enabled: false,
            custom_profile_control_enabled: false,
            refresh_control_enabled: false,
            selected_minor: 0,
            negotiated_encodings: Vec::new(),
            color_rgb565_enabled: false,
            byte_credits_enabled: false,
            cleanup_pending_without_surface: false,
            refresh_debt_without_surface: RefreshDebt::default(),
            touch_gesture: FiveFingerCleanupGesture::default(),
            local_menu: LocalMenu::default(),
            last_custom_profile: None,
            surface: None,
            fallback,
        }
    }

    pub fn is_established(&self) -> bool {
        self.established
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn receiver_exit_requested(&self) -> bool {
        self.receiver_exit_requested
    }

    pub fn pairing_reset_requested(&self) -> bool {
        self.pairing_reset_requested
    }

    pub(crate) fn refresh_policy(&self) -> RefreshPolicyConfig {
        self.config.refresh_policy
    }

    pub(crate) fn close_local_menu(
        &mut self,
        now: Duration,
    ) -> Result<Vec<Envelope>, SessionError> {
        if !self.local_menu.is_visible() {
            return Ok(Vec::new());
        }
        self.local_menu.close();
        self.render_local_menu(now)
    }

    pub fn session_id(&self) -> u64 {
        self.session_id
    }

    pub fn handle(
        &mut self,
        envelope: Envelope,
        now: Duration,
    ) -> Result<Vec<Envelope>, SessionError> {
        self.validate_envelope_header(&envelope)?;
        let related_message_id = envelope.message_id;
        let body = envelope.body.ok_or(SessionError::MissingBody)?;

        let mut responses = if !self.established {
            match body {
                Body::ClientHello(hello) => vec![self.accept_hello(hello)?],
                _ => return Err(SessionError::ExpectedClientHello),
            }
        } else {
            match body {
                Body::Ping(ping) => vec![self.wrap(Body::Pong(rm_display_protocol::Pong {
                    cookie: ping.cookie,
                }))],
                Body::Goodbye(_) => {
                    self.closed = true;
                    Vec::new()
                }
                Body::SurfaceOpen(open) => self.open_surface(open)?,
                Body::SurfaceClose(close) => self.close_surface(close.surface_id, close.generation),
                Body::Frame(frame) => self.accept_frame(frame, now),
                Body::EpaperProfileRequest(request) => self.handle_epaper_profile(request, now)?,
                Body::EpaperRefreshRequest(request) => self.handle_epaper_refresh(request, now)?,
                Body::ActionResult(_) => Vec::new(),
                Body::ClientHello(_)
                | Body::ServerHello(_)
                | Body::Pong(_)
                | Body::Error(_)
                | Body::SurfaceReady(_)
                | Body::FrameResult(_)
                | Body::InputBatch(_)
                | Body::KeyInput(_)
                | Body::TextInput(_)
                | Body::ActionInvoke(_)
                | Body::EpaperProfileResult(_)
                | Body::EpaperRefreshResult(_) => {
                    return Err(SessionError::IllegalDirection);
                }
            }
        };
        responses.extend(self.poll(now)?);
        if self.closed && responses.is_empty() {
            let _ = related_message_id;
        }
        Ok(responses)
    }

    pub fn poll(&mut self, now: Duration) -> Result<Vec<Envelope>, SessionError> {
        let terminals = match self.surface.as_mut() {
            Some(surface) => {
                let terminals = surface.core.tick(now, self.panel)?;
                self.cleanup_pending_without_surface = surface.core.cleanup_pending();
                self.refresh_debt_without_surface = surface.core.refresh_debt();
                terminals
            }
            // There is no Qt/Quill operation to advance until a surface exists.
            // Real submissions pump the event queue synchronously in the backend.
            None => Vec::new(),
        };
        Ok(terminals
            .into_iter()
            .map(|terminal| self.terminal_result(terminal))
            .collect())
    }

    pub fn protocol_error(&mut self, error: &SessionError, related_message_id: u32) -> Envelope {
        self.wrap(Body::Error(ProtocolError {
            code: error.code(),
            fatal: true,
            related_message_id,
            message: error.to_string(),
        }))
    }

    pub fn input_reports(
        &mut self,
        reports: Vec<Vec<PhysicalPointerEvent>>,
        monotonic: Duration,
    ) -> Result<Vec<Envelope>, SessionError> {
        let surface = self.surface.as_ref().map(|surface| {
            (
                surface.surface_id,
                surface.generation,
                surface.touch_enabled,
            )
        });
        let mut envelopes = Vec::new();
        for report in reports.into_iter().filter(|report| !report.is_empty()) {
            if self.local_menu.is_visible() {
                let geometry = self
                    .surface
                    .as_ref()
                    .map(|surface| (surface.core.width(), surface.core.height()))
                    .or_else(|| {
                        self.fallback
                            .as_ref()
                            .map(|fallback| (fallback.base.width(), fallback.base.height()))
                    });
                let action = geometry.and_then(|(width, height)| {
                    self.local_menu.action_for_report(&report, width, height)
                });
                if let Some(action) = action {
                    envelopes.extend(self.handle_local_menu_action(action, monotonic)?);
                }
                continue;
            }
            let gesture = self.touch_gesture.process(
                report,
                surface.is_some_and(|(_, _, enabled)| enabled),
                surface.is_some(),
            );
            if let Some((surface_id, generation, _)) = surface {
                if !gesture.cancel_forwarded.is_empty() {
                    envelopes.push(self.pointer_batch(
                        surface_id,
                        generation,
                        gesture.cancel_forwarded,
                        monotonic,
                    ));
                }
                if !gesture.forward.is_empty() {
                    envelopes.push(self.pointer_batch(
                        surface_id,
                        generation,
                        gesture.forward,
                        monotonic,
                    ));
                }
                if gesture.cleanup_requested {
                    envelopes.extend(self.local_cleanup(monotonic)?);
                }
            }
        }
        Ok(envelopes)
    }

    pub fn power_key_pressed(&mut self, now: Duration) -> Result<Vec<Envelope>, SessionError> {
        if self.surface.is_none() && self.fallback.is_none() {
            return Ok(Vec::new());
        }
        self.touch_gesture.surface_transition();
        self.local_menu.toggle();
        self.render_local_menu(now)
    }

    fn handle_local_menu_action(
        &mut self,
        action: LocalMenuAction,
        now: Duration,
    ) -> Result<Vec<Envelope>, SessionError> {
        match action {
            LocalMenuAction::SetProfile(profile) => {
                let next = if profile == CoreRefreshProfile::Custom {
                    let Some(custom) = self.last_custom_profile else {
                        return self.render_local_menu(now);
                    };
                    custom
                } else {
                    self.config.refresh_policy.switched_to(profile)
                };
                self.config.refresh_policy = next;
                let mut responses = if let Some(surface) = self.surface.as_mut() {
                    surface
                        .core
                        .change_refresh_profile(next, now, self.panel)?
                        .terminals
                        .into_iter()
                        .map(|terminal| self.terminal_result(terminal))
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                responses.extend(self.render_local_menu(now)?);
                Ok(responses)
            }
            LocalMenuAction::TogglePartialRefresh => {
                let mut next = self.config.refresh_policy;
                next.partial_refresh_enabled = !next.partial_refresh_enabled;
                if let Some(surface) = self.surface.as_mut() {
                    surface.core.update_refresh_config(next)?;
                }
                self.config.refresh_policy = next;
                self.render_local_menu(now)
            }
            LocalMenuAction::FullRefresh => self.local_cleanup(now),
            LocalMenuAction::NewPair => {
                self.local_menu.close();
                let _ = self.render_local_menu(now);
                self.closed = true;
                self.pairing_reset_requested = true;
                Ok(Vec::new())
            }
            LocalMenuAction::CloseApp => {
                self.local_menu.close();
                // Closing the receiver is local control flow, not a request that
                // depends on the producer still accepting a terminal response.
                // Try to restore the base frame before dropping the Quill/touch
                // owners, but never leave the app trapped in its accept loop if
                // that final panel operation fails.
                let _ = self.render_local_menu(now);
                self.closed = true;
                self.receiver_exit_requested = true;
                Ok(Vec::new())
            }
            LocalMenuAction::Close => {
                self.local_menu.close();
                self.render_local_menu(now)
            }
        }
    }

    fn render_local_menu(&mut self, now: Duration) -> Result<Vec<Envelope>, SessionError> {
        if let Some(surface) = self.surface.as_mut() {
            let width = surface.core.width();
            let height = surface.core.height();
            self.local_menu.render(
                surface.core.overlay_mut(),
                width,
                height,
                self.config.refresh_policy.profile,
                self.config.refresh_policy.partial_refresh_enabled,
                self.last_custom_profile.is_some(),
            );
            let terminals = surface.core.present_overlay(now, self.panel)?;
            self.cleanup_pending_without_surface = surface.core.cleanup_pending();
            self.refresh_debt_without_surface = surface.core.refresh_debt();
            return Ok(terminals
                .into_iter()
                .map(|terminal| self.terminal_result(terminal))
                .collect());
        }
        self.present_fallback(false)?;
        Ok(Vec::new())
    }

    fn pointer_batch(
        &mut self,
        surface_id: u32,
        generation: u32,
        report: Vec<PhysicalPointerEvent>,
        monotonic: Duration,
    ) -> Envelope {
        let sequence = self.next_input_sequence.max(1);
        self.next_input_sequence = sequence.wrapping_add(1).max(1);
        self.wrap(Body::InputBatch(rm_display_protocol::InputBatch {
            surface_id,
            generation,
            sequence,
            monotonic_us: monotonic.as_micros().min(u64::MAX as u128) as u64,
            records: report.into_iter().map(pointer_record).collect(),
        }))
    }

    fn present_fallback(&mut self, complete_refresh: bool) -> Result<(), SessionError> {
        let Some(fallback) = self.fallback.as_mut() else {
            return Ok(());
        };
        let width = fallback.base.width();
        let height = fallback.base.height();
        self.local_menu.render(
            &mut fallback.overlay,
            width,
            height,
            self.config.refresh_policy.profile,
            self.config.refresh_policy.partial_refresh_enabled,
            self.last_custom_profile.is_some(),
        );
        let composed = fallback.base.compose(&fallback.overlay)?;
        let damage = if complete_refresh {
            Rect {
                x: 0,
                y: 0,
                width,
                height,
            }
        } else {
            Rect {
                x: 0,
                y: 0,
                width,
                height: self.local_menu.damage_height(height),
            }
        };
        self.panel.submit(
            &composed,
            &[damage],
            RefreshDecision {
                waveform: if complete_refresh {
                    Waveform::FullQuality
                } else {
                    Waveform::Quality
                },
                complete_refresh,
                full_refresh_reason: if complete_refresh {
                    rm_display_core::FullRefreshReason::Forced
                } else {
                    rm_display_core::FullRefreshReason::None
                },
            },
        )?;
        Ok(())
    }

    fn local_cleanup(&mut self, now: Duration) -> Result<Vec<Envelope>, SessionError> {
        let Some(surface) = self.surface.as_mut() else {
            self.present_fallback(true)?;
            return Ok(Vec::new());
        };
        let report = surface.core.request_cleanup(now, self.panel)?;
        self.cleanup_pending_without_surface = report.cleanup_pending;
        self.refresh_debt_without_surface = surface.core.refresh_debt();
        Ok(report
            .terminals
            .into_iter()
            .map(|terminal| self.terminal_result(terminal))
            .collect())
    }

    fn validate_envelope_header(&mut self, envelope: &Envelope) -> Result<(), SessionError> {
        if self.last_incoming_message_id == 0 {
            if envelope.message_id != 1 {
                return Err(SessionError::BadMessageOrder);
            }
        } else if envelope.message_id <= self.last_incoming_message_id {
            return Err(SessionError::BadMessageOrder);
        }
        if (!self.established && envelope.session_id != 0)
            || (self.established && envelope.session_id != self.session_id)
        {
            return Err(SessionError::BadSession);
        }
        self.last_incoming_message_id = envelope.message_id;
        Ok(())
    }

    fn accept_hello(&mut self, hello: ClientHello) -> Result<Envelope, SessionError> {
        validate_hello(&hello, &self.config)?;
        self.selected_minor = select_minor(&hello)?;
        self.byte_credits_enabled = self.selected_minor >= 1;
        self.profile_control_enabled = hello
            .features
            .contains(&(ProtocolFeature::EpaperProfileControl as i32));
        self.custom_profile_control_enabled = self.selected_minor >= 2
            && self.profile_control_enabled
            && hello
                .features
                .contains(&(ProtocolFeature::EpaperCustomProfile as i32));
        self.refresh_control_enabled = hello
            .features
            .contains(&(ProtocolFeature::EpaperRefreshControl as i32));
        let info = self.panel.info();
        self.color_rgb565_enabled = self.selected_minor >= 1
            && info.color_rgb565
            && hello
                .features
                .contains(&(ProtocolFeature::ColorRgb565 as i32))
            && hello
                .pixel_formats
                .contains(&(PixelFormat::Rgb565Le as i32));
        self.negotiated_encodings = [Encoding::Raw, Encoding::Zstd, Encoding::Zlib]
            .into_iter()
            .filter(|encoding| hello.encodings.contains(&(*encoding as i32)))
            .map(|encoding| encoding as i32)
            .collect();
        self.established = true;
        let mut features = MANDATORY_FEATURES
            .iter()
            .copied()
            .map(|feature| feature as i32)
            .collect::<Vec<_>>();
        if self.config.input_device.is_some() {
            features.push(ProtocolFeature::PointerInput as i32);
        }
        if self.profile_control_enabled {
            features.push(ProtocolFeature::EpaperProfileControl as i32);
        }
        if self.custom_profile_control_enabled {
            features.push(ProtocolFeature::EpaperCustomProfile as i32);
        }
        if self.refresh_control_enabled {
            features.push(ProtocolFeature::EpaperRefreshControl as i32);
        }
        if self.byte_credits_enabled {
            features.push(ProtocolFeature::ByteCredits as i32);
        }
        if self.color_rgb565_enabled {
            features.push(ProtocolFeature::ColorRgb565 as i32);
        }
        let mut pixel_formats = vec![PixelFormat::Gray8 as i32];
        if self.color_rgb565_enabled {
            pixel_formats.push(PixelFormat::Rgb565Le as i32);
        }
        let hello = ServerHello {
            selected_minor: self.selected_minor,
            features,
            server_id: self.config.server_id.to_vec().into(),
            display: Some(DisplayInfo {
                width: info.width,
                height: info.height,
                orientation: Orientation::Current as i32,
                pixel_formats,
                encodings: self.negotiated_encodings.clone(),
                input_capabilities: if self.config.input_device.is_some() {
                    vec![InputCapability::Touch as i32]
                } else {
                    Vec::new()
                },
            }),
            limits: Some(self.protocol_limits()),
            name: self.config.name.clone(),
        };
        Ok(self.wrap(Body::ServerHello(hello)))
    }

    fn open_surface(&mut self, open: SurfaceOpen) -> Result<Vec<Envelope>, SessionError> {
        if open.surface_id == 0 {
            return Err(SessionError::BadSurface("surface_id must be nonzero"));
        }
        let pixel_format = PixelFormat::try_from(open.pixel_format)
            .map_err(|_| SessionError::BadSurface("unknown pixel format"))?;
        if pixel_format != PixelFormat::Gray8
            && !(pixel_format == PixelFormat::Rgb565Le && self.color_rgb565_enabled)
        {
            return Err(SessionError::BadSurface("pixel format was not negotiated"));
        }
        let source_kind = SourceKind::try_from(open.source_kind)
            .map_err(|_| SessionError::BadSurface("unknown source kind"))?;
        let info = self.panel.info();
        let generation = self.next_generation.max(1);
        self.next_generation = generation.wrapping_add(1).max(1);
        let carry_cleanup = self.cleanup_pending_without_surface
            || self
                .surface
                .as_ref()
                .is_some_and(|surface| surface.core.cleanup_pending());
        let carry_debt = self
            .surface
            .as_ref()
            .map_or(self.refresh_debt_without_surface, |surface| {
                surface.core.refresh_debt()
            });
        let mut core = DisplayCore::new_with_format(
            info.width,
            info.height,
            pixel_format,
            self.config.limits.max_fps_x100,
            Duration::from_millis(self.config.limits.settled_deadline_ms as u64),
            self.config.refresh_policy,
        )?;
        if carry_cleanup {
            core.require_cleanup();
        }
        core.restore_refresh_debt(carry_debt)?;
        self.cleanup_pending_without_surface = carry_cleanup;
        self.refresh_debt_without_surface = carry_debt;
        let touch_enabled = self.config.input_device.is_some()
            && open
                .input_capabilities
                .contains(&(InputCapability::Touch as i32));
        let cancelled = self
            .surface
            .as_mut()
            .and_then(|surface| surface.core.cancel_pending());
        let mut responses = cancelled
            .into_iter()
            .map(|terminal| self.terminal_result(terminal))
            .collect::<Vec<_>>();
        self.touch_gesture.surface_transition();
        self.local_menu.close();
        self.surface = Some(ActiveSurface {
            surface_id: open.surface_id,
            generation,
            touch_enabled,
            core,
        });
        let ready = SurfaceReady {
            surface_id: open.surface_id,
            generation,
            width: info.width,
            height: info.height,
            pixel_format: pixel_format as i32,
            orientation: Orientation::Current as i32,
            source_kind: source_kind as i32,
            input_capabilities: if touch_enabled {
                vec![InputCapability::Touch as i32]
            } else {
                Vec::new()
            },
            action_capabilities: Vec::new(),
            limits: Some(self.protocol_limits()),
        };
        responses.push(self.wrap(Body::SurfaceReady(ready)));
        Ok(responses)
    }

    fn close_surface(&mut self, surface_id: u32, generation: u32) -> Vec<Envelope> {
        if !self.surface.as_ref().is_some_and(|surface| {
            surface.surface_id == surface_id && surface.generation == generation
        }) {
            return Vec::new();
        }
        let cancelled = self
            .surface
            .as_mut()
            .and_then(|surface| surface.core.cancel_pending());
        self.cleanup_pending_without_surface = self
            .surface
            .as_ref()
            .is_some_and(|surface| surface.core.cleanup_pending());
        self.refresh_debt_without_surface = self
            .surface
            .as_ref()
            .map_or(self.refresh_debt_without_surface, |surface| {
                surface.core.refresh_debt()
            });
        let responses = cancelled
            .into_iter()
            .map(|terminal| self.terminal_result(terminal))
            .collect();
        self.touch_gesture.surface_transition();
        self.local_menu.close();
        self.surface = None;
        responses
    }

    fn handle_epaper_profile(
        &mut self,
        request: EpaperProfileRequest,
        now: Duration,
    ) -> Result<Vec<Envelope>, SessionError> {
        let current = self.config.refresh_policy;
        if !self.profile_control_enabled {
            return Ok(vec![self.epaper_profile_result(
                &request,
                EpaperProfileResultCode::Unsupported,
                current,
                false,
                self.current_cleanup_pending(),
                "EPAPER_PROFILE_CONTROL was not negotiated",
            )]);
        }
        if request.request_id == 0 || request.request_id <= self.last_profile_request_id {
            return Ok(vec![self.epaper_profile_result(
                &request,
                EpaperProfileResultCode::Rejected,
                current,
                false,
                self.current_cleanup_pending(),
                "request_id must be nonzero and strictly increasing",
            )]);
        }
        self.last_profile_request_id = request.request_id;

        let operation = EpaperProfileOperation::try_from(request.operation).ok();
        if operation == Some(EpaperProfileOperation::Query)
            && request.requested_profile == EpaperProfile::Unspecified as i32
            && request.custom.is_none()
        {
            return Ok(vec![self.epaper_profile_result(
                &request,
                EpaperProfileResultCode::Unchanged,
                current,
                false,
                self.current_cleanup_pending(),
                "active receiver refresh policy",
            )]);
        }
        if operation != Some(EpaperProfileOperation::Set) {
            return Ok(vec![self.epaper_profile_result(
                &request,
                EpaperProfileResultCode::Rejected,
                current,
                false,
                self.current_cleanup_pending(),
                "operation/profile combination is invalid",
            )]);
        }
        let Some(requested_profile) = EpaperProfile::try_from(request.requested_profile).ok()
        else {
            return Ok(vec![self.epaper_profile_result(
                &request,
                EpaperProfileResultCode::Rejected,
                current,
                false,
                self.current_cleanup_pending(),
                "requested profile is invalid",
            )]);
        };
        let next = if requested_profile == EpaperProfile::Custom {
            if !self.custom_profile_control_enabled {
                return Ok(vec![self.epaper_profile_result(
                    &request,
                    EpaperProfileResultCode::Unsupported,
                    current,
                    false,
                    self.current_cleanup_pending(),
                    "CUSTOM requires protocol v2.2 and EPAPER_CUSTOM_PROFILE",
                )]);
            }
            let Some(custom) = request.custom.as_ref() else {
                return Ok(vec![self.epaper_profile_result(
                    &request,
                    EpaperProfileResultCode::Rejected,
                    current,
                    false,
                    self.current_cleanup_pending(),
                    "CUSTOM requires a complete custom configuration",
                )]);
            };
            match custom_profile_config(custom) {
                Ok(config) => config,
                Err(message) => {
                    return Ok(vec![self.epaper_profile_result(
                        &request,
                        EpaperProfileResultCode::Rejected,
                        current,
                        false,
                        self.current_cleanup_pending(),
                        message,
                    )]);
                }
            }
        } else {
            if request.custom.is_some() {
                return Ok(vec![self.epaper_profile_result(
                    &request,
                    EpaperProfileResultCode::Rejected,
                    current,
                    false,
                    self.current_cleanup_pending(),
                    "custom configuration is valid only with CUSTOM",
                )]);
            }
            let Some(profile) = protocol_profile(request.requested_profile) else {
                return Ok(vec![self.epaper_profile_result(
                    &request,
                    EpaperProfileResultCode::Rejected,
                    current,
                    false,
                    self.current_cleanup_pending(),
                    "requested profile is invalid",
                )]);
            };
            current.switched_to(profile)
        };
        if next.profile == CoreRefreshProfile::Custom {
            self.last_custom_profile = Some(next);
        }
        self.config.refresh_policy = next;
        let (changed, cleanup_performed, cleanup_pending, backend_failed, terminals) =
            if let Some(surface) = self.surface.as_mut() {
                let report = surface.core.change_refresh_profile(next, now, self.panel)?;
                (
                    report.changed,
                    report.cleanup_performed,
                    report.cleanup_pending,
                    report.backend_failed,
                    report.terminals,
                )
            } else {
                let changed = next != current;
                self.cleanup_pending_without_surface |= changed;
                (
                    changed,
                    false,
                    self.cleanup_pending_without_surface,
                    false,
                    Vec::new(),
                )
            };
        self.cleanup_pending_without_surface = cleanup_pending;

        let mut responses = terminals
            .into_iter()
            .map(|terminal| self.terminal_result(terminal))
            .collect::<Vec<_>>();
        let result = if changed {
            EpaperProfileResultCode::Applied
        } else {
            EpaperProfileResultCode::Unchanged
        };
        let message = if backend_failed {
            "policy applied; cleanup failed and remains pending"
        } else if cleanup_performed {
            "policy applied and full-panel cleanup completed"
        } else if cleanup_pending {
            "policy applied; cleanup is armed for the next presentation"
        } else if changed {
            "policy applied"
        } else {
            "requested policy is already active"
        };
        responses.push(self.epaper_profile_result(
            &request,
            result,
            next,
            cleanup_performed,
            cleanup_pending,
            message,
        ));
        Ok(responses)
    }

    fn current_cleanup_pending(&self) -> bool {
        self.surface
            .as_ref()
            .map_or(self.cleanup_pending_without_surface, |surface| {
                surface.core.cleanup_pending()
            })
    }

    fn epaper_profile_result(
        &mut self,
        request: &EpaperProfileRequest,
        result: EpaperProfileResultCode,
        active: RefreshPolicyConfig,
        cleanup_performed: bool,
        cleanup_pending: bool,
        message: &str,
    ) -> Envelope {
        self.wrap(Body::EpaperProfileResult(EpaperProfileResult {
            request_id: request.request_id,
            operation: request.operation,
            requested_profile: request.requested_profile,
            result: result as i32,
            active: Some(profile_state(active)),
            cleanup_performed,
            cleanup_pending,
            message: message.to_owned(),
        }))
    }

    fn handle_epaper_refresh(
        &mut self,
        request: EpaperRefreshRequest,
        now: Duration,
    ) -> Result<Vec<Envelope>, SessionError> {
        if !self.refresh_control_enabled {
            let active = self.current_refresh_state();
            return Ok(vec![self.epaper_refresh_result(
                &request,
                EpaperRefreshResultCode::Unsupported,
                active,
                false,
                "EPAPER_REFRESH_CONTROL was not negotiated",
            )]);
        }
        if request.request_id == 0 || request.request_id <= self.last_refresh_request_id {
            let active = self.current_refresh_state();
            return Ok(vec![self.epaper_refresh_result(
                &request,
                EpaperRefreshResultCode::Rejected,
                active,
                false,
                "request_id must be nonzero and strictly increasing",
            )]);
        }
        self.last_refresh_request_id = request.request_id;

        let operation = EpaperRefreshOperation::try_from(request.operation).ok();
        let has_parameters = request.partial_refresh_enabled.is_some()
            || request.cleanup_after_updates.is_some()
            || request.large_update_threshold_percent.is_some()
            || request.static_cleanup_after_fast_updates.is_some();
        match operation {
            Some(EpaperRefreshOperation::Query) if !has_parameters => {
                let active = self.current_refresh_state();
                Ok(vec![self.epaper_refresh_result(
                    &request,
                    EpaperRefreshResultCode::Unchanged,
                    active,
                    false,
                    "active receiver refresh parameters",
                )])
            }
            Some(EpaperRefreshOperation::Update) if has_parameters => {
                if request
                    .large_update_threshold_percent
                    .is_some_and(|percent| percent > 100)
                {
                    let active = self.current_refresh_state();
                    return Ok(vec![self.epaper_refresh_result(
                        &request,
                        EpaperRefreshResultCode::Rejected,
                        active,
                        false,
                        "large_update_threshold_percent must be between 0 and 100",
                    )]);
                }
                let mut next = self.config.refresh_policy;
                if let Some(enabled) = request.partial_refresh_enabled {
                    next.partial_refresh_enabled = enabled;
                }
                if let Some(interval) = request.cleanup_after_updates {
                    next.cleanup_after_updates = interval;
                }
                if let Some(percent) = request.large_update_threshold_percent {
                    next.large_update_threshold_percent = percent as u8;
                }
                if let Some(count) = request.static_cleanup_after_fast_updates {
                    next.static_cleanup_after_fast_updates = count;
                }
                let changed = if let Some(surface) = self.surface.as_mut() {
                    surface.core.update_refresh_config(next)?
                } else {
                    next != self.config.refresh_policy
                };
                self.config.refresh_policy = next;
                let active = self.current_refresh_state();
                Ok(vec![self.epaper_refresh_result(
                    &request,
                    if changed {
                        EpaperRefreshResultCode::Applied
                    } else {
                        EpaperRefreshResultCode::Unchanged
                    },
                    active,
                    false,
                    if changed {
                        "receiver refresh parameters applied"
                    } else {
                        "requested refresh parameters are already active"
                    },
                )])
            }
            Some(EpaperRefreshOperation::Cleanup) if !has_parameters => {
                let report = if let Some(surface) = self.surface.as_mut() {
                    surface.core.request_cleanup(now, self.panel)?
                } else {
                    self.cleanup_pending_without_surface = true;
                    rm_display_core::CleanupReport {
                        cleanup_performed: false,
                        cleanup_pending: true,
                        backend_failed: false,
                        terminals: Vec::new(),
                    }
                };
                self.cleanup_pending_without_surface = report.cleanup_pending;
                let mut responses = report
                    .terminals
                    .into_iter()
                    .map(|terminal| self.terminal_result(terminal))
                    .collect::<Vec<_>>();
                let active = self.current_refresh_state();
                let result = if report.backend_failed {
                    EpaperRefreshResultCode::Failed
                } else {
                    EpaperRefreshResultCode::Applied
                };
                let message = if report.backend_failed {
                    "full cleanup failed and remains armed"
                } else if report.cleanup_performed {
                    "full cleanup completed"
                } else {
                    "full cleanup is armed for the next presentation"
                };
                responses.push(self.epaper_refresh_result(
                    &request,
                    result,
                    active,
                    report.cleanup_performed,
                    message,
                ));
                Ok(responses)
            }
            _ => {
                let active = self.current_refresh_state();
                Ok(vec![self.epaper_refresh_result(
                    &request,
                    EpaperRefreshResultCode::Rejected,
                    active,
                    false,
                    "operation and optional parameter combination is invalid",
                )])
            }
        }
    }

    fn current_refresh_state(&self) -> EpaperRefreshState {
        let (config, presented_since_full_refresh, cleanup_pending, fast_updates_since_settled) =
            self.surface.as_ref().map_or(
                (
                    self.config.refresh_policy,
                    self.refresh_debt_without_surface
                        .physical_partial_updates_since_cleanup,
                    self.cleanup_pending_without_surface,
                    0,
                ),
                |surface| {
                    (
                        surface.core.refresh_config(),
                        surface.core.presented_since_full_refresh(),
                        surface.core.cleanup_pending(),
                        surface.core.fast_updates_since_settled(),
                    )
                },
            );
        EpaperRefreshState {
            partial_refresh_enabled: config.partial_refresh_enabled,
            cleanup_after_updates: config.cleanup_after_updates,
            large_update_threshold_percent: u32::from(config.large_update_threshold_percent),
            presented_since_full_refresh,
            cleanup_pending,
            static_cleanup_after_fast_updates: config.static_cleanup_after_fast_updates,
            fast_updates_since_settled,
        }
    }

    fn epaper_refresh_result(
        &mut self,
        request: &EpaperRefreshRequest,
        result: EpaperRefreshResultCode,
        active: EpaperRefreshState,
        cleanup_performed: bool,
        message: &str,
    ) -> Envelope {
        self.wrap(Body::EpaperRefreshResult(EpaperRefreshResult {
            request_id: request.request_id,
            operation: request.operation,
            result: result as i32,
            active: Some(active),
            cleanup_performed,
            message: message.to_owned(),
        }))
    }

    fn accept_frame(&mut self, frame: Frame, now: Duration) -> Vec<Envelope> {
        if frame
            .regions
            .iter()
            .any(|region| !self.negotiated_encodings.contains(&region.encoding))
        {
            return vec![self.immediate_frame_result(
                &frame,
                FrameResultCode::Rejected,
                FrameResultReason::Unsupported,
            )];
        }
        if self.credits() == 0 {
            return vec![self.immediate_frame_result(
                &frame,
                FrameResultCode::Rejected,
                FrameResultReason::NoCredit,
            )];
        }
        let decoded_cost = match frame.regions.iter().try_fold(0_u64, |total, region| {
            total.checked_add(u64::from(region.decoded_len))
        }) {
            Some(cost) => cost,
            None => {
                return vec![self.immediate_frame_result(
                    &frame,
                    FrameResultCode::Rejected,
                    FrameResultReason::BadLength,
                )];
            }
        };
        if self.byte_credits_enabled && decoded_cost > self.byte_credits() {
            return vec![self.immediate_frame_result(
                &frame,
                FrameResultCode::Rejected,
                FrameResultReason::NoCredit,
            )];
        }
        let Some(surface) = self.surface.as_mut() else {
            return vec![self.immediate_frame_result(
                &frame,
                FrameResultCode::Rejected,
                FrameResultReason::ProtocolState,
            )];
        };
        let semantic_surface = SurfaceState {
            surface_id: surface.surface_id,
            generation: surface.generation,
            width: surface.core.width(),
            height: surface.core.height(),
            pixel_format: surface.core.pixel_format(),
            logical_frame_id: surface.core.logical_frame_id(),
            max_regions: self.config.limits.max_regions as usize,
            max_frame_bytes: self.config.limits.max_frame_bytes as usize,
        };
        let decode_started = Instant::now();
        let validated = match validate_and_decode_frame(&frame, &semantic_surface) {
            Ok(frame) => frame,
            Err(error) => {
                let (result, reason) = semantic_result(&error);
                return vec![self.immediate_frame_result(&frame, result, reason)];
            }
        };
        let content_class = match ContentClass::try_from(frame.content_class) {
            Ok(ContentClass::Unspecified) | Err(_) => ContentClass::Mixed,
            Ok(class) => class,
        };
        let validation_elapsed = decode_started.elapsed();
        let decode_us = duration_us(validation_elapsed);
        let report = match surface
            .core
            .commit_timed(&validated, content_class, now, decode_us)
        {
            Ok(report) => report,
            Err(CoreError::NeedKeyframe) => {
                return vec![self.immediate_frame_result(
                    &frame,
                    FrameResultCode::NeedKeyframe,
                    FrameResultReason::BadBase,
                )];
            }
            Err(CoreError::SettledBarrier) => {
                return vec![self.immediate_frame_result(
                    &frame,
                    FrameResultCode::Rejected,
                    FrameResultReason::ProtocolState,
                )];
            }
            Err(_) => {
                return vec![self.immediate_frame_result(
                    &frame,
                    FrameResultCode::Rejected,
                    FrameResultReason::BadRegion,
                )];
            }
        };
        report
            .superseded
            .into_iter()
            .map(|terminal| self.terminal_result(terminal))
            .collect()
    }

    fn terminal_result(&mut self, terminal: TerminalFrame) -> Envelope {
        let (result, reason) = match terminal.outcome {
            PresentationOutcome::Presented => (FrameResultCode::Presented, FrameResultReason::None),
            PresentationOutcome::Superseded => {
                (FrameResultCode::Superseded, FrameResultReason::None)
            }
            PresentationOutcome::Cancelled => {
                (FrameResultCode::Rejected, FrameResultReason::ProtocolState)
            }
            PresentationOutcome::BackendFailure => {
                (FrameResultCode::Rejected, FrameResultReason::BackendFailure)
            }
        };
        let (surface_id, generation, logical, presented) = self
            .surface
            .as_ref()
            .map(|surface| {
                (
                    surface.surface_id,
                    surface.generation,
                    surface.core.logical_frame_id(),
                    surface.core.presented_frame_id(),
                )
            })
            .unwrap_or((0, 0, 0, 0));
        self.frame_result(
            surface_id,
            generation,
            terminal.frame_id,
            result,
            reason,
            logical,
            presented,
            Some(terminal.metrics),
        )
    }

    fn immediate_frame_result(
        &mut self,
        frame: &Frame,
        result: FrameResultCode,
        reason: FrameResultReason,
    ) -> Envelope {
        let (logical, presented) = self
            .surface
            .as_ref()
            .map(|surface| {
                (
                    surface.core.logical_frame_id(),
                    surface.core.presented_frame_id(),
                )
            })
            .unwrap_or((0, 0));
        self.frame_result(
            frame.surface_id,
            frame.generation,
            frame.frame_id,
            result,
            reason,
            logical,
            presented,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn frame_result(
        &mut self,
        surface_id: u32,
        generation: u32,
        frame_id: u64,
        result: FrameResultCode,
        reason: FrameResultReason,
        logical_frame_id: u64,
        presented_frame_id: u64,
        metrics: Option<rm_display_core::PresentationMetrics>,
    ) -> Envelope {
        self.wrap(Body::FrameResult(FrameResult {
            surface_id,
            generation,
            frame_id,
            result: result as i32,
            reason: reason as i32,
            credits: self.credits(),
            logical_frame_id,
            presented_frame_id,
            metrics: Some(
                metrics.map_or_else(FrameMetrics::default, |metrics| FrameMetrics {
                    decode_us: metrics.decode_us,
                    queue_us: metrics.queue_us,
                    present_us: metrics.present_us,
                    compose_us: metrics.compose_us,
                    convert_us: metrics.convert_us,
                    submit_us: metrics.submit_us,
                    damage_pixels: metrics.damage_pixels,
                    damage_regions: metrics.damage_regions,
                    waveform: metrics.waveform,
                    complete_refresh: metrics.complete_refresh,
                    full_refresh_reason: match metrics.full_refresh_reason {
                        rm_display_core::FullRefreshReason::None => FullRefreshReason::None,
                        rm_display_core::FullRefreshReason::PartialDisabled => {
                            FullRefreshReason::PartialDisabled
                        }
                        rm_display_core::FullRefreshReason::Forced => FullRefreshReason::Forced,
                        rm_display_core::FullRefreshReason::FirstFrame => {
                            FullRefreshReason::FirstFrame
                        }
                        rm_display_core::FullRefreshReason::Periodic => FullRefreshReason::Periodic,
                        rm_display_core::FullRefreshReason::LargeDamage => {
                            FullRefreshReason::LargeDamage
                        }
                        rm_display_core::FullRefreshReason::StaticFastDebt => {
                            FullRefreshReason::StaticFastDebt
                        }
                    } as i32,
                }),
            ),
            byte_credits: self.byte_credits(),
        }))
    }

    fn credits(&self) -> u32 {
        let pending = u32::from(
            self.surface
                .as_ref()
                .is_some_and(|surface| surface.core.has_pending()),
        );
        self.config.limits.max_inflight.saturating_sub(pending)
    }

    fn byte_credits(&self) -> u64 {
        if !self.byte_credits_enabled {
            return 0;
        }
        let pending = self
            .surface
            .as_ref()
            .map_or(0_u64, |surface| surface.core.pending_bytes() as u64);
        self.config
            .limits
            .max_inflight_bytes
            .saturating_sub(pending)
    }

    fn protocol_limits(&self) -> Limits {
        Limits {
            max_payload: self.config.limits.max_payload,
            max_frame_bytes: self.config.limits.max_frame_bytes,
            max_regions: self.config.limits.max_regions,
            max_inflight: self.config.limits.max_inflight,
            max_fps_x100: self.config.limits.max_fps_x100,
            settled_deadline_ms: self.config.limits.settled_deadline_ms,
            max_inflight_bytes: if self.byte_credits_enabled {
                self.config.limits.max_inflight_bytes
            } else {
                0
            },
        }
    }

    fn wrap(&mut self, body: Body) -> Envelope {
        let message_id = self.next_outgoing_message_id.max(1);
        self.next_outgoing_message_id = message_id.wrapping_add(1).max(1);
        Envelope {
            session_id: self.session_id,
            message_id,
            body: Some(body),
        }
    }
}

fn duration_us(duration: Duration) -> u32 {
    duration.as_micros().min(u128::from(u32::MAX)) as u32
}

fn pointer_record(event: PhysicalPointerEvent) -> PointerRecord {
    let phase = match event.phase {
        PhysicalPhase::Down => PointerPhase::Down,
        PhysicalPhase::Move => PointerPhase::Move,
        PhysicalPhase::Up => PointerPhase::Up,
        PhysicalPhase::Cancel => PointerPhase::Cancel,
    };
    PointerRecord {
        device: PointerDevice::Touch as i32,
        phase: phase as i32,
        flags: 0,
        contact_id: event.contact_id,
        x_16_16: fixed_16_16(event.x),
        y_16_16: fixed_16_16(event.y),
        pressure: 0,
        buttons: 0,
        tilt_x: 0,
        tilt_y: 0,
    }
}

fn fixed_16_16(value: u32) -> u32 {
    value.saturating_mul(1 << 16)
}

fn protocol_profile(value: i32) -> Option<CoreRefreshProfile> {
    match EpaperProfile::try_from(value).ok()? {
        EpaperProfile::Realtime => Some(CoreRefreshProfile::Realtime),
        EpaperProfile::Animate => Some(CoreRefreshProfile::Animate),
        EpaperProfile::Balanced => Some(CoreRefreshProfile::Balanced),
        EpaperProfile::Reading => Some(CoreRefreshProfile::Reading),
        EpaperProfile::Quality => Some(CoreRefreshProfile::Quality),
        EpaperProfile::Unspecified | EpaperProfile::Custom => None,
    }
}

fn epaper_profile(profile: CoreRefreshProfile) -> EpaperProfile {
    match profile {
        CoreRefreshProfile::Realtime => EpaperProfile::Realtime,
        CoreRefreshProfile::Animate => EpaperProfile::Animate,
        CoreRefreshProfile::Balanced => EpaperProfile::Balanced,
        CoreRefreshProfile::Reading => EpaperProfile::Reading,
        CoreRefreshProfile::Quality => EpaperProfile::Quality,
        CoreRefreshProfile::Custom => EpaperProfile::Custom,
    }
}

fn profile_state(config: RefreshPolicyConfig) -> EpaperProfileState {
    EpaperProfileState {
        profile: epaper_profile(config.profile) as i32,
        cleanup_after_updates: config.cleanup_after_updates,
        large_update_threshold_percent: u32::from(config.large_update_threshold_percent),
        damage_tile: config.damage_tile,
        clean_first_frame: config.clean_first_frame,
        static_cleanup_after_fast_updates: config.static_cleanup_after_fast_updates,
        effective: Some(profile_configuration(config)),
    }
}

fn profile_configuration(config: RefreshPolicyConfig) -> EpaperProfileConfiguration {
    EpaperProfileConfiguration {
        latest_text_waveform: protocol_waveform(config.latest_text_waveform) as i32,
        latest_photo_waveform: protocol_waveform(config.latest_photo_waveform) as i32,
        latest_video_waveform: protocol_waveform(config.latest_video_waveform) as i32,
        settled_waveform: protocol_waveform(config.settled_waveform) as i32,
        partial_refresh_enabled: config.partial_refresh_enabled,
        cleanup_after_updates: config.cleanup_after_updates,
        clean_first_frame: config.clean_first_frame,
        large_update_threshold_percent: u32::from(config.large_update_threshold_percent),
        static_cleanup_after_fast_updates: config.static_cleanup_after_fast_updates,
        damage_tile: config.damage_tile,
    }
}

fn protocol_waveform(waveform: Waveform) -> EpaperWaveform {
    match waveform {
        Waveform::Fastest => EpaperWaveform::Fastest,
        Waveform::Fast => EpaperWaveform::Fast,
        Waveform::Quality | Waveform::FullQuality => EpaperWaveform::Quality,
    }
}

fn custom_profile_config(
    custom: &EpaperProfileConfiguration,
) -> Result<RefreshPolicyConfig, &'static str> {
    let waveform = |value| match EpaperWaveform::try_from(value).ok() {
        Some(EpaperWaveform::Fastest) => Ok(Waveform::Fastest),
        Some(EpaperWaveform::Fast) => Ok(Waveform::Fast),
        Some(EpaperWaveform::Quality) => Ok(Waveform::Quality),
        Some(EpaperWaveform::Unspecified) | None => {
            Err("all CUSTOM waveforms must be FASTEST, FAST, or QUALITY")
        }
    };
    if !(8..=512).contains(&custom.damage_tile) || !custom.damage_tile.is_power_of_two() {
        return Err("CUSTOM damage_tile must be a power of two between 8 and 512");
    }
    let large_update_threshold_percent = u8::try_from(custom.large_update_threshold_percent)
        .ok()
        .filter(|threshold| *threshold <= 100)
        .ok_or("CUSTOM large_update_threshold_percent must be between 0 and 100")?;
    let config = RefreshPolicyConfig {
        profile: CoreRefreshProfile::Custom,
        latest_text_waveform: waveform(custom.latest_text_waveform)?,
        latest_photo_waveform: waveform(custom.latest_photo_waveform)?,
        latest_video_waveform: waveform(custom.latest_video_waveform)?,
        settled_waveform: waveform(custom.settled_waveform)?,
        partial_refresh_enabled: custom.partial_refresh_enabled,
        cleanup_after_updates: custom.cleanup_after_updates,
        clean_first_frame: custom.clean_first_frame,
        large_update_threshold_percent,
        static_cleanup_after_fast_updates: custom.static_cleanup_after_fast_updates,
        damage_tile: custom.damage_tile,
    };
    config
        .validate()
        .map_err(|_| "CUSTOM refresh configuration is invalid")?;
    Ok(config)
}

fn validate_hello(hello: &ClientHello, config: &ReceiverConfig) -> Result<(), SessionError> {
    if hello.min_minor > hello.max_minor || hello.min_minor > MAX_MINOR {
        return Err(SessionError::NoCommonVersion);
    }
    if hello.client_id.len() != 16 || hello.token.len() != 32 || hello.client_nonce.len() != 16 {
        return Err(SessionError::BadHello(
            "invalid client id, token, or nonce length",
        ));
    }
    if ProducerKind::try_from(hello.producer_kind).ok() == Some(ProducerKind::Unspecified)
        || ProducerKind::try_from(hello.producer_kind).is_err()
    {
        return Err(SessionError::BadHello("producer kind is unspecified"));
    }
    if !MANDATORY_FEATURES
        .iter()
        .all(|feature| hello.features.contains(&(*feature as i32)))
        || !hello.pixel_formats.contains(&(PixelFormat::Gray8 as i32))
        || !hello.encodings.contains(&(Encoding::Raw as i32))
    {
        return Err(SessionError::MissingCapability);
    }
    if !config.token_verifier.verify(&hello.client_id, &hello.token) {
        return Err(SessionError::Authentication);
    }
    Ok(())
}

fn select_minor(hello: &ClientHello) -> Result<u32, SessionError> {
    if hello.max_minor >= 2
        && hello.min_minor <= 2
        && hello
            .features
            .contains(&(ProtocolFeature::ByteCredits as i32))
        && hello
            .features
            .contains(&(ProtocolFeature::EpaperCustomProfile as i32))
    {
        return Ok(2);
    }
    if hello.max_minor >= 1
        && hello.min_minor <= 1
        && hello
            .features
            .contains(&(ProtocolFeature::ByteCredits as i32))
    {
        return Ok(1);
    }
    if hello.min_minor == 0 {
        Ok(0)
    } else {
        Err(SessionError::MissingCapability)
    }
}

fn semantic_result(error: &SemanticError) -> (FrameResultCode, FrameResultReason) {
    match error {
        SemanticError::BadBase { .. } => {
            (FrameResultCode::NeedKeyframe, FrameResultReason::BadBase)
        }
        SemanticError::WrongSurface => (FrameResultCode::Rejected, FrameResultReason::WrongSurface),
        SemanticError::BadDecodedLength => {
            (FrameResultCode::Rejected, FrameResultReason::BadLength)
        }
        SemanticError::BadCrc => (FrameResultCode::Rejected, FrameResultReason::BadCrc),
        SemanticError::Unsupported => (FrameResultCode::Rejected, FrameResultReason::Unsupported),
        SemanticError::FrameTooLarge => (FrameResultCode::Rejected, FrameResultReason::BadLength),
        SemanticError::BadFrameId | SemanticError::BadIntent => {
            (FrameResultCode::Rejected, FrameResultReason::ProtocolState)
        }
        SemanticError::BadRegionCount
        | SemanticError::BadRegion
        | SemanticError::RegionOverlap
        | SemanticError::BadKeyframe
        | SemanticError::Decompression => (FrameResultCode::Rejected, FrameResultReason::BadRegion),
    }
}

fn random_nonzero_u64() -> u64 {
    loop {
        let value = OsRng.next_u64();
        if value != 0 {
            return value;
        }
    }
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("ClientHello must be the first message")]
    ExpectedClientHello,
    #[error("envelope body is missing")]
    MissingBody,
    #[error("message_id is not strictly increasing from one")]
    BadMessageOrder,
    #[error("session_id is invalid")]
    BadSession,
    #[error("message direction is illegal")]
    IllegalDirection,
    #[error("no common protocol minor version")]
    NoCommonVersion,
    #[error("ClientHello is invalid: {0}")]
    BadHello(&'static str),
    #[error("mandatory protocol capability is absent")]
    MissingCapability,
    #[error("client authentication failed")]
    Authentication,
    #[error("SurfaceOpen is invalid: {0}")]
    BadSurface(&'static str),
    #[error(transparent)]
    Core(#[from] CoreError),
    #[error(transparent)]
    Panel(#[from] rm_display_core::PanelError),
    #[error(transparent)]
    Surface(#[from] rm_display_core::SurfaceError),
}

impl SessionError {
    fn code(&self) -> u32 {
        match self {
            Self::Authentication => 2,
            Self::NoCommonVersion | Self::MissingCapability => 3,
            Self::BadSurface(_) => 4,
            _ => 1,
        }
    }
}
