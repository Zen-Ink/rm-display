use std::collections::VecDeque;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use rand::RngCore;
use rm_display_protocol::envelope;
use rm_display_protocol::semantic::raw_region;
use rm_display_protocol::wire::{WireCodec, WireError, HEADER_LEN, MAGIC};
use rm_display_protocol::{
    ActionId, ActionResult, ActionStatus, ClientHello, ContentClass, Encoding, Envelope,
    EpaperProfile, EpaperProfileConfiguration, EpaperProfileOperation, EpaperProfileRequest,
    EpaperProfileResult, EpaperProfileResultCode, EpaperRefreshOperation, EpaperRefreshRequest,
    EpaperRefreshResult, EpaperRefreshResultCode, Frame, FrameIntent, FrameResult, FrameResultCode,
    Goodbye, InputCapability, Orientation, PixelFormat, Pong, ProducerKind, ProtocolFeature, Rect,
    ServerHello, SourceKind, SurfaceOpen, SurfaceReady,
};
use thiserror::Error;

use crate::events::write_event_jsonl;
const MIN_MINOR: u32 = 0;
const MAX_MINOR: u32 = 2;
const SURFACE_ID: u32 = 1;
const MAX_DELTA_REGIONS: usize = 64;

pub trait ReadWrite: Read + Write + Send {}
impl<T: Read + Write + Send> ReadWrite for T {}

#[derive(Debug, Clone)]
pub struct Surface {
    pub id: u32,
    pub generation: u32,
    pub width: u32,
    pub height: u32,
    pub max_frame_bytes: usize,
    pub max_regions: u32,
    pub max_inflight: u32,
    pub max_inflight_bytes: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ProducerFrameMetrics {
    pub attempts: u32,
    pub build_us: u64,
    pub wire_encode_us: u64,
    pub write_us: u64,
    pub wait_us: u64,
    pub total_us: u64,
    pub wire_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct FrameReport {
    pub result: FrameResult,
    pub producer: ProducerFrameMetrics,
}

#[derive(Debug, Clone, Copy)]
struct SendMeasurement {
    wire_encode_us: u64,
    write_us: u64,
    wire_bytes: u64,
}

struct PendingFrame {
    frame_id: u64,
    decoded_bytes: u64,
    started: Instant,
    producer: ProducerFrameMetrics,
}

#[derive(Debug, Error)]
pub enum ProducerError {
    #[error("transport I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("wire framing failed: {0}")]
    Wire(#[from] WireError),
    #[error("receiver closed the connection")]
    Closed,
    #[error("receiver sent an unexpected message")]
    UnexpectedMessage,
    #[error("receiver sent a non-increasing message id")]
    MessageOrder,
    #[error("receiver message has the wrong session id")]
    WrongSession,
    #[error("server hello is invalid: {0}")]
    BadHello(&'static str),
    #[error("surface response is invalid: {0}")]
    BadSurface(&'static str),
    #[error("frame contains {actual} bytes but surface limit is {limit}")]
    FrameTooLarge { actual: usize, limit: usize },
    #[error("surface pixels have length {actual}; expected {expected}")]
    BadPixelLength { actual: usize, expected: usize },
    #[error("receiver advertised zero frame credit")]
    NoCredit,
    #[error("frame {frame_id} was rejected: result={result} reason={reason}")]
    FrameRejected {
        frame_id: u64,
        result: i32,
        reason: i32,
    },
    #[error("receiver reported a fatal error: {0}")]
    Remote(String),
    #[error("receiver did not negotiate EPAPER_PROFILE_CONTROL")]
    ProfileControlUnsupported,
    #[error("receiver rejected e-paper profile request: status={status} message={message}")]
    ProfileRequestRejected { status: i32, message: String },
    #[error("receiver did not negotiate EPAPER_REFRESH_CONTROL")]
    RefreshControlUnsupported,
    #[error("receiver rejected e-paper refresh request: status={status} message={message}")]
    RefreshRequestRejected { status: i32, message: String },
    #[error("event JSON output failed: {0}")]
    Event(#[from] anyhow::Error),
}

pub struct ProducerClient {
    io: Box<dyn ReadWrite>,
    codec: WireCodec,
    session_id: u64,
    next_message_id: u32,
    last_received_id: u32,
    next_frame_id: u64,
    next_profile_request_id: u32,
    next_refresh_request_id: u32,
    logical_frame_id: u64,
    credits: u32,
    byte_credits: u64,
    credit_limit: u32,
    byte_credit_limit: u64,
    pending_frames: VecDeque<PendingFrame>,
    selected_minor: u32,
    zstd_enabled: bool,
    client_id: [u8; 16],
    server_hello: Option<ServerHello>,
    event_output: Option<Box<dyn Write + Send>>,
    actions_enabled: bool,
}

impl ProducerClient {
    pub fn new(io: Box<dyn ReadWrite>, client_id: [u8; 16]) -> Self {
        Self {
            io,
            codec: WireCodec::pre_handshake(),
            session_id: 0,
            next_message_id: 1,
            last_received_id: 0,
            next_frame_id: 1,
            next_profile_request_id: 1,
            next_refresh_request_id: 1,
            logical_frame_id: 0,
            credits: 0,
            byte_credits: 0,
            credit_limit: 0,
            byte_credit_limit: 0,
            pending_frames: VecDeque::new(),
            selected_minor: 0,
            zstd_enabled: false,
            client_id,
            server_hello: None,
            event_output: None,
            actions_enabled: false,
        }
    }

    pub fn set_event_output(&mut self, output: Box<dyn Write + Send>) {
        self.event_output = Some(output);
    }

    pub fn hello(&mut self, name: &str) -> Result<&ServerHello, ProducerError> {
        let mut nonce = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let hello = ClientHello {
            min_minor: MIN_MINOR,
            max_minor: MAX_MINOR,
            producer_kind: ProducerKind::LinuxCli as i32,
            features: vec![
                ProtocolFeature::AtomicMultiRegion as i32,
                ProtocolFeature::ExactBaseDelta as i32,
                ProtocolFeature::LatestSupersede as i32,
                ProtocolFeature::SettledBarrier as i32,
                ProtocolFeature::PointerInput as i32,
                ProtocolFeature::KeyInput as i32,
                ProtocolFeature::TextInput as i32,
                ProtocolFeature::Actions as i32,
                ProtocolFeature::EpaperProfileControl as i32,
                ProtocolFeature::EpaperRefreshControl as i32,
                ProtocolFeature::ByteCredits as i32,
                ProtocolFeature::EpaperCustomProfile as i32,
            ],
            pixel_formats: vec![PixelFormat::Gray8 as i32],
            encodings: vec![Encoding::Raw as i32, Encoding::Zstd as i32],
            client_id: Bytes::copy_from_slice(&self.client_id),
            token: Bytes::from_static(&[0; 32]),
            client_nonce: Bytes::copy_from_slice(&nonce),
            name: name.to_owned(),
        };
        self.send(envelope::Body::ClientHello(hello))?;
        let envelope = self.receive()?;
        if envelope.session_id == 0 {
            return Err(ProducerError::BadHello("session id is zero"));
        }
        let server = match envelope.body {
            Some(envelope::Body::ServerHello(server)) => server,
            Some(envelope::Body::Error(error)) => return Err(ProducerError::Remote(error.message)),
            _ => return Err(ProducerError::UnexpectedMessage),
        };
        validate_server_hello(&server)?;
        self.zstd_enabled = server
            .display
            .as_ref()
            .is_some_and(|display| display.encodings.contains(&(Encoding::Zstd as i32)));
        self.selected_minor = server.selected_minor;
        self.session_id = envelope.session_id;
        let limit = server
            .limits
            .as_ref()
            .expect("validated limits")
            .max_payload as usize;
        self.codec.set_max_payload(limit);
        self.server_hello = Some(server);
        Ok(self.server_hello.as_ref().unwrap())
    }

    pub fn server_hello(&self) -> Option<&ServerHello> {
        self.server_hello.as_ref()
    }

    pub fn query_epaper_profile(&mut self) -> Result<EpaperProfileResult, ProducerError> {
        self.epaper_profile_request(
            EpaperProfileOperation::Query,
            EpaperProfile::Unspecified,
            None,
        )
    }

    pub fn request_epaper_profile(
        &mut self,
        profile: EpaperProfile,
    ) -> Result<EpaperProfileResult, ProducerError> {
        if matches!(profile, EpaperProfile::Unspecified | EpaperProfile::Custom) {
            return Err(ProducerError::ProfileRequestRejected {
                status: EpaperProfileResultCode::Rejected as i32,
                message: "preset SET requires realtime, animate, balanced, reading, or quality"
                    .to_owned(),
            });
        }
        self.epaper_profile_request(EpaperProfileOperation::Set, profile, None)
    }

    pub fn request_custom_epaper_profile(
        &mut self,
        custom: EpaperProfileConfiguration,
    ) -> Result<EpaperProfileResult, ProducerError> {
        let supported = self.server_hello.as_ref().is_some_and(|hello| {
            hello.selected_minor >= 2
                && hello
                    .features
                    .contains(&(ProtocolFeature::EpaperCustomProfile as i32))
        });
        if !supported {
            return Err(ProducerError::ProfileControlUnsupported);
        }
        self.epaper_profile_request(
            EpaperProfileOperation::Set,
            EpaperProfile::Custom,
            Some(custom),
        )
    }

    fn epaper_profile_request(
        &mut self,
        operation: EpaperProfileOperation,
        requested_profile: EpaperProfile,
        custom: Option<EpaperProfileConfiguration>,
    ) -> Result<EpaperProfileResult, ProducerError> {
        self.drain_queued_frame_reports()?;
        let supported = self.server_hello.as_ref().is_some_and(|hello| {
            hello
                .features
                .contains(&(ProtocolFeature::EpaperProfileControl as i32))
        });
        if !supported {
            return Err(ProducerError::ProfileControlUnsupported);
        }
        let request_id = self.next_profile_request_id;
        self.next_profile_request_id = request_id
            .checked_add(1)
            .ok_or(ProducerError::MessageOrder)?;
        self.send(envelope::Body::EpaperProfileRequest(EpaperProfileRequest {
            request_id,
            operation: operation as i32,
            requested_profile: requested_profile as i32,
            custom,
        }))?;
        loop {
            let envelope = self.receive()?;
            match envelope.body {
                Some(envelope::Body::EpaperProfileResult(result))
                    if result.request_id == request_id =>
                {
                    let code = EpaperProfileResultCode::try_from(result.result)
                        .unwrap_or(EpaperProfileResultCode::Unspecified);
                    if matches!(
                        code,
                        EpaperProfileResultCode::Applied | EpaperProfileResultCode::Unchanged
                    ) && result.active.is_some()
                    {
                        return Ok(result);
                    }
                    return Err(ProducerError::ProfileRequestRejected {
                        status: result.result,
                        message: result.message,
                    });
                }
                Some(envelope::Body::Error(error)) if error.fatal => {
                    return Err(ProducerError::Remote(error.message));
                }
                Some(body) => self.handle_auxiliary(body)?,
                None => return Err(ProducerError::UnexpectedMessage),
            }
        }
    }

    pub fn query_epaper_refresh(&mut self) -> Result<EpaperRefreshResult, ProducerError> {
        self.epaper_refresh_request(EpaperRefreshRequest {
            request_id: 0,
            operation: EpaperRefreshOperation::Query as i32,
            partial_refresh_enabled: None,
            cleanup_after_updates: None,
            large_update_threshold_percent: None,
            static_cleanup_after_fast_updates: None,
        })
    }

    pub fn update_epaper_refresh(
        &mut self,
        partial_refresh_enabled: Option<bool>,
        cleanup_after_updates: Option<u32>,
        large_update_threshold_percent: Option<u32>,
        static_cleanup_after_fast_updates: Option<u32>,
    ) -> Result<EpaperRefreshResult, ProducerError> {
        self.epaper_refresh_request(EpaperRefreshRequest {
            request_id: 0,
            operation: EpaperRefreshOperation::Update as i32,
            partial_refresh_enabled,
            cleanup_after_updates,
            large_update_threshold_percent,
            static_cleanup_after_fast_updates,
        })
    }

    pub fn request_epaper_cleanup(&mut self) -> Result<EpaperRefreshResult, ProducerError> {
        self.epaper_refresh_request(EpaperRefreshRequest {
            request_id: 0,
            operation: EpaperRefreshOperation::Cleanup as i32,
            partial_refresh_enabled: None,
            cleanup_after_updates: None,
            large_update_threshold_percent: None,
            static_cleanup_after_fast_updates: None,
        })
    }

    fn epaper_refresh_request(
        &mut self,
        mut request: EpaperRefreshRequest,
    ) -> Result<EpaperRefreshResult, ProducerError> {
        self.drain_queued_frame_reports()?;
        let supported = self.server_hello.as_ref().is_some_and(|hello| {
            hello
                .features
                .contains(&(ProtocolFeature::EpaperRefreshControl as i32))
        });
        if !supported {
            return Err(ProducerError::RefreshControlUnsupported);
        }
        if request.operation == EpaperRefreshOperation::Update as i32
            && request.partial_refresh_enabled.is_none()
            && request.cleanup_after_updates.is_none()
            && request.large_update_threshold_percent.is_none()
            && request.static_cleanup_after_fast_updates.is_none()
        {
            return Err(ProducerError::RefreshRequestRejected {
                status: EpaperRefreshResultCode::Rejected as i32,
                message: "UPDATE requires at least one parameter".to_owned(),
            });
        }
        let request_id = self.next_refresh_request_id;
        self.next_refresh_request_id = request_id
            .checked_add(1)
            .ok_or(ProducerError::MessageOrder)?;
        request.request_id = request_id;
        self.send(envelope::Body::EpaperRefreshRequest(request))?;
        loop {
            let envelope = self.receive()?;
            match envelope.body {
                Some(envelope::Body::EpaperRefreshResult(result))
                    if result.request_id == request_id =>
                {
                    let code = EpaperRefreshResultCode::try_from(result.result)
                        .unwrap_or(EpaperRefreshResultCode::Unspecified);
                    if matches!(
                        code,
                        EpaperRefreshResultCode::Applied | EpaperRefreshResultCode::Unchanged
                    ) && result.active.is_some()
                    {
                        return Ok(result);
                    }
                    return Err(ProducerError::RefreshRequestRejected {
                        status: result.result,
                        message: result.message,
                    });
                }
                Some(envelope::Body::Error(error)) if error.fatal => {
                    return Err(ProducerError::Remote(error.message));
                }
                Some(body) => self.handle_auxiliary(body)?,
                None => return Err(ProducerError::UnexpectedMessage),
            }
        }
    }

    pub fn open_surface(
        &mut self,
        desired_width: u32,
        desired_height: u32,
        source_kind: SourceKind,
        accept_input: bool,
        label: &str,
    ) -> Result<Surface, ProducerError> {
        let input_capabilities = if accept_input {
            vec![
                InputCapability::Touch as i32,
                InputCapability::Pen as i32,
                InputCapability::Mouse as i32,
                InputCapability::Key as i32,
                InputCapability::Text as i32,
            ]
        } else {
            Vec::new()
        };
        let action_capabilities = if accept_input {
            vec![
                ActionId::Back as i32,
                ActionId::Forward as i32,
                ActionId::Reload as i32,
                ActionId::Home as i32,
                ActionId::Menu as i32,
                ActionId::PreviousPage as i32,
                ActionId::NextPage as i32,
            ]
        } else {
            Vec::new()
        };
        self.actions_enabled = accept_input && self.event_output.is_some();
        self.send(envelope::Body::SurfaceOpen(SurfaceOpen {
            surface_id: SURFACE_ID,
            desired_width,
            desired_height,
            pixel_format: PixelFormat::Gray8 as i32,
            orientation: Orientation::Current as i32,
            source_kind: source_kind as i32,
            input_capabilities,
            action_capabilities,
            label: label.to_owned(),
        }))?;
        loop {
            let envelope = self.receive()?;
            match envelope.body {
                Some(envelope::Body::SurfaceReady(ready)) => return self.accept_surface(ready),
                Some(envelope::Body::Error(error)) => {
                    return Err(ProducerError::Remote(error.message))
                }
                Some(body) => self.handle_auxiliary(body)?,
                None => return Err(ProducerError::UnexpectedMessage),
            }
        }
    }

    fn accept_surface(&mut self, ready: SurfaceReady) -> Result<Surface, ProducerError> {
        if ready.surface_id != SURFACE_ID || ready.generation == 0 {
            return Err(ProducerError::BadSurface("wrong id or zero generation"));
        }
        if ready.width == 0 || ready.height == 0 || ready.pixel_format != PixelFormat::Gray8 as i32
        {
            return Err(ProducerError::BadSurface(
                "invalid geometry or pixel format",
            ));
        }
        let limits = ready
            .limits
            .ok_or(ProducerError::BadSurface("limits missing"))?;
        if limits.max_frame_bytes == 0 || limits.max_regions == 0 || limits.max_inflight == 0 {
            return Err(ProducerError::BadSurface("zero frame limit"));
        }
        self.actions_enabled &= !ready.action_capabilities.is_empty();
        self.next_frame_id = 1;
        self.logical_frame_id = 0;
        self.credits = limits.max_inflight;
        self.credit_limit = limits.max_inflight;
        self.byte_credits = if self.selected_minor >= 1 {
            limits.max_inflight_bytes
        } else {
            u64::MAX
        };
        self.byte_credit_limit = self.byte_credits;
        self.pending_frames.clear();
        Ok(Surface {
            id: ready.surface_id,
            generation: ready.generation,
            width: ready.width,
            height: ready.height,
            max_frame_bytes: limits.max_frame_bytes as usize,
            max_regions: limits.max_regions,
            max_inflight: limits.max_inflight,
            max_inflight_bytes: self.byte_credits,
        })
    }

    pub fn send_frame(
        &mut self,
        surface: &Surface,
        pixels: &[u8],
        intent: FrameIntent,
        content_class: ContentClass,
    ) -> Result<FrameResult, ProducerError> {
        Ok(self
            .send_frame_report(surface, pixels, intent, content_class)?
            .result)
    }

    pub fn send_delta_frame(
        &mut self,
        surface: &Surface,
        previous: &[u8],
        pixels: &[u8],
        tile: u32,
        intent: FrameIntent,
        content_class: ContentClass,
    ) -> Result<FrameResult, ProducerError> {
        Ok(self
            .send_delta_frame_report(surface, previous, pixels, tile, intent, content_class)?
            .result)
    }

    pub fn send_delta_frame_report(
        &mut self,
        surface: &Surface,
        previous: &[u8],
        pixels: &[u8],
        tile: u32,
        intent: FrameIntent,
        content_class: ContentClass,
    ) -> Result<FrameReport, ProducerError> {
        self.send_frame_report_inner(
            surface,
            pixels,
            Some((previous, tile)),
            intent,
            content_class,
        )
    }

    pub fn queue_delta_frame_report(
        &mut self,
        surface: &Surface,
        previous: &[u8],
        pixels: &[u8],
        tile: u32,
        content_class: ContentClass,
    ) -> Result<Option<FrameReport>, ProducerError> {
        if self.logical_frame_id == 0 {
            return self
                .send_frame_report(surface, pixels, FrameIntent::Latest, content_class)
                .map(Some);
        }
        validate_frame_pixels(surface, pixels, Some(previous))?;
        if self.credits == 0 {
            return Err(ProducerError::NoCredit);
        }
        let started = Instant::now();
        let build_started = Instant::now();
        let regions = encode_delta_regions(
            previous,
            pixels,
            surface.width,
            surface.height,
            tile,
            (surface.max_regions as usize).min(MAX_DELTA_REGIONS),
            self.zstd_enabled,
        );
        let decoded_bytes = regions
            .iter()
            .map(|region| u64::from(region.decoded_len))
            .sum::<u64>();
        if decoded_bytes > self.byte_credits {
            return Err(ProducerError::NoCredit);
        }
        let frame_id = self.next_frame_id;
        self.next_frame_id = self
            .next_frame_id
            .checked_add(1)
            .ok_or(ProducerError::MessageOrder)?;
        let frame = Frame {
            surface_id: surface.id,
            generation: surface.generation,
            frame_id,
            base_frame_id: self.logical_frame_id,
            intent: FrameIntent::Latest as i32,
            content_class: content_class as i32,
            regions,
            source_timestamp_us: 0,
        };
        let mut producer = ProducerFrameMetrics {
            attempts: 1,
            build_us: elapsed_us(build_started),
            ..ProducerFrameMetrics::default()
        };
        self.credits -= 1;
        self.byte_credits = self.byte_credits.saturating_sub(decoded_bytes);
        let sent = self.send_measured(envelope::Body::Frame(frame))?;
        producer.wire_encode_us = sent.wire_encode_us;
        producer.write_us = sent.write_us;
        producer.wire_bytes = sent.wire_bytes;
        self.logical_frame_id = frame_id;
        self.pending_frames.push_back(PendingFrame {
            frame_id,
            decoded_bytes,
            started,
            producer,
        });
        Ok(None)
    }

    pub fn wait_for_frame_credit_report(&mut self) -> Result<Option<FrameReport>, ProducerError> {
        if self.credits > 0 || self.pending_frames.is_empty() {
            Ok(None)
        } else {
            self.reap_queued_frame_report().map(Some)
        }
    }

    pub fn pending_frame_count(&self) -> usize {
        self.pending_frames.len()
    }

    pub fn send_frame_report(
        &mut self,
        surface: &Surface,
        pixels: &[u8],
        intent: FrameIntent,
        content_class: ContentClass,
    ) -> Result<FrameReport, ProducerError> {
        self.send_frame_report_inner(surface, pixels, None, intent, content_class)
    }

    fn send_frame_report_inner(
        &mut self,
        surface: &Surface,
        pixels: &[u8],
        delta: Option<(&[u8], u32)>,
        intent: FrameIntent,
        content_class: ContentClass,
    ) -> Result<FrameReport, ProducerError> {
        self.drain_queued_frame_reports()?;
        let total_started = Instant::now();
        let mut producer = ProducerFrameMetrics::default();
        validate_frame_pixels(surface, pixels, delta.map(|(previous, _)| previous))?;
        let mut force_keyframe = self.logical_frame_id == 0;
        loop {
            let build_started = Instant::now();
            let regions = if force_keyframe {
                vec![encode_region(
                    Rect {
                        x: 0,
                        y: 0,
                        width: surface.width,
                        height: surface.height,
                    },
                    pixels,
                    self.zstd_enabled,
                )]
            } else if let Some((previous, tile)) = delta {
                encode_delta_regions(
                    previous,
                    pixels,
                    surface.width,
                    surface.height,
                    tile,
                    (surface.max_regions as usize).min(MAX_DELTA_REGIONS),
                    self.zstd_enabled,
                )
            } else {
                vec![encode_region(
                    Rect {
                        x: 0,
                        y: 0,
                        width: surface.width,
                        height: surface.height,
                    },
                    pixels,
                    self.zstd_enabled,
                )]
            };
            let decoded_bytes = regions
                .iter()
                .map(|region| u64::from(region.decoded_len))
                .sum::<u64>();
            if self.credits == 0 {
                return Err(ProducerError::NoCredit);
            }
            if decoded_bytes > self.byte_credits {
                return Err(ProducerError::NoCredit);
            }
            let frame_id = self.next_frame_id;
            self.next_frame_id =
                self.next_frame_id
                    .checked_add(1)
                    .ok_or(ProducerError::FrameRejected {
                        frame_id,
                        result: FrameResultCode::Rejected as i32,
                        reason: 0,
                    })?;
            let base_frame_id = if force_keyframe {
                0
            } else {
                self.logical_frame_id
            };
            let frame = Frame {
                surface_id: surface.id,
                generation: surface.generation,
                frame_id,
                base_frame_id,
                intent: intent as i32,
                content_class: content_class as i32,
                regions,
                source_timestamp_us: 0,
            };
            producer.build_us = producer.build_us.saturating_add(elapsed_us(build_started));
            producer.attempts = producer.attempts.saturating_add(1);
            self.credits -= 1;
            self.byte_credits = self.byte_credits.saturating_sub(decoded_bytes);
            let sent = self.send_measured(envelope::Body::Frame(frame))?;
            producer.wire_encode_us = producer.wire_encode_us.saturating_add(sent.wire_encode_us);
            producer.write_us = producer.write_us.saturating_add(sent.write_us);
            producer.wire_bytes = producer.wire_bytes.saturating_add(sent.wire_bytes);
            let wait_started = Instant::now();
            let result = self.wait_for_frame_result(frame_id)?;
            producer.wait_us = producer.wait_us.saturating_add(elapsed_us(wait_started));
            self.credits = result.credits;
            if self.selected_minor >= 1 {
                self.byte_credits = result.byte_credits;
            }
            let code =
                FrameResultCode::try_from(result.result).unwrap_or(FrameResultCode::Unspecified);
            match code {
                FrameResultCode::Presented | FrameResultCode::Superseded => {
                    self.logical_frame_id = result.logical_frame_id;
                    producer.total_us = elapsed_us(total_started);
                    return Ok(FrameReport { result, producer });
                }
                FrameResultCode::NeedKeyframe if !force_keyframe => {
                    self.logical_frame_id = result.logical_frame_id;
                    force_keyframe = true;
                }
                _ => {
                    return Err(ProducerError::FrameRejected {
                        frame_id,
                        result: result.result,
                        reason: result.reason,
                    })
                }
            }
        }
    }

    pub fn goodbye(&mut self) -> Result<(), ProducerError> {
        self.send(envelope::Body::Goodbye(Goodbye {
            reason: 1,
            message: "producer completed".to_owned(),
        }))
    }

    fn drain_queued_frame_reports(&mut self) -> Result<(), ProducerError> {
        while !self.pending_frames.is_empty() {
            self.reap_queued_frame_report()?;
        }
        Ok(())
    }

    fn reap_queued_frame_report(&mut self) -> Result<FrameReport, ProducerError> {
        loop {
            let envelope = self.receive()?;
            match envelope.body {
                Some(envelope::Body::FrameResult(result)) => {
                    let Some(index) = self
                        .pending_frames
                        .iter()
                        .position(|pending| pending.frame_id == result.frame_id)
                    else {
                        return Err(ProducerError::UnexpectedMessage);
                    };
                    let mut pending = self.pending_frames.remove(index).unwrap();
                    self.credits = self.credits.saturating_add(1).min(self.credit_limit);
                    self.byte_credits = self
                        .byte_credits
                        .saturating_add(pending.decoded_bytes)
                        .min(self.byte_credit_limit);
                    pending.producer.wait_us = elapsed_us(pending.started);
                    pending.producer.total_us = pending.producer.wait_us;
                    let code = FrameResultCode::try_from(result.result)
                        .unwrap_or(FrameResultCode::Unspecified);
                    if matches!(
                        code,
                        FrameResultCode::Presented | FrameResultCode::Superseded
                    ) {
                        self.logical_frame_id = self.logical_frame_id.max(result.logical_frame_id);
                        return Ok(FrameReport {
                            result,
                            producer: pending.producer,
                        });
                    }
                    return Err(ProducerError::FrameRejected {
                        frame_id: result.frame_id,
                        result: result.result,
                        reason: result.reason,
                    });
                }
                Some(envelope::Body::Error(error)) if error.fatal => {
                    return Err(ProducerError::Remote(error.message))
                }
                Some(body) => self.handle_auxiliary(body)?,
                None => return Err(ProducerError::UnexpectedMessage),
            }
        }
    }

    pub fn pump_once(&mut self) -> Result<(), ProducerError> {
        let envelope = self.receive()?;
        match envelope.body {
            Some(body) => self.handle_auxiliary(body),
            None => Err(ProducerError::UnexpectedMessage),
        }
    }

    fn wait_for_frame_result(&mut self, frame_id: u64) -> Result<FrameResult, ProducerError> {
        loop {
            let envelope = self.receive()?;
            match envelope.body {
                Some(envelope::Body::FrameResult(result)) if result.frame_id == frame_id => {
                    return Ok(result)
                }
                Some(envelope::Body::Error(error)) if error.fatal => {
                    return Err(ProducerError::Remote(error.message))
                }
                Some(body) => self.handle_auxiliary(body)?,
                None => return Err(ProducerError::UnexpectedMessage),
            }
        }
    }

    fn handle_auxiliary(&mut self, body: envelope::Body) -> Result<(), ProducerError> {
        match body {
            envelope::Body::Ping(ping) => self.send(envelope::Body::Pong(Pong {
                cookie: ping.cookie,
            })),
            envelope::Body::InputBatch(batch) => self.emit_event(envelope::Body::InputBatch(batch)),
            envelope::Body::KeyInput(key) => self.emit_event(envelope::Body::KeyInput(key)),
            envelope::Body::TextInput(text) => self.emit_event(envelope::Body::TextInput(text)),
            envelope::Body::ActionInvoke(action) => {
                let surface_id = action.surface_id;
                let generation = action.generation;
                let invocation_id = action.invocation_id;
                let handled = self.actions_enabled && self.event_output.is_some();
                self.emit_event(envelope::Body::ActionInvoke(action))?;
                self.send(envelope::Body::ActionResult(ActionResult {
                    surface_id,
                    generation,
                    invocation_id,
                    status: if handled {
                        ActionStatus::Ok as i32
                    } else {
                        ActionStatus::Unsupported as i32
                    },
                    message: if handled {
                        "action emitted to JSONL".to_owned()
                    } else {
                        "Linux CLI has no action handler".to_owned()
                    },
                }))
            }
            envelope::Body::Error(error) if error.fatal => {
                Err(ProducerError::Remote(error.message))
            }
            envelope::Body::Goodbye(_) => Err(ProducerError::Closed),
            _ => Ok(()),
        }
    }

    fn emit_event(&mut self, body: envelope::Body) -> Result<(), ProducerError> {
        if let Some(output) = self.event_output.as_mut() {
            let envelope = Envelope {
                session_id: self.session_id,
                message_id: self.last_received_id,
                body: Some(body),
            };
            write_event_jsonl(&envelope, output.as_mut())?;
        }
        Ok(())
    }

    fn send(&mut self, body: envelope::Body) -> Result<(), ProducerError> {
        self.send_measured(body).map(|_| ())
    }

    fn send_measured(&mut self, body: envelope::Body) -> Result<SendMeasurement, ProducerError> {
        let envelope = Envelope {
            session_id: self.session_id,
            message_id: self.next_message_id,
            body: Some(body),
        };
        self.next_message_id = self
            .next_message_id
            .checked_add(1)
            .ok_or(ProducerError::MessageOrder)?;
        let encode_started = Instant::now();
        let mut bytes = BytesMut::new();
        self.codec.encode(&envelope, &mut bytes)?;
        let wire_encode_us = elapsed_us(encode_started);
        let wire_bytes = bytes.len() as u64;
        let write_started = Instant::now();
        self.io.write_all(&bytes)?;
        self.io.flush()?;
        Ok(SendMeasurement {
            wire_encode_us,
            write_us: elapsed_us(write_started),
            wire_bytes,
        })
    }

    fn receive(&mut self) -> Result<Envelope, ProducerError> {
        let mut header = [0u8; HEADER_LEN];
        self.io.read_exact(&mut header)?;
        if header[..4] != MAGIC {
            return Err(ProducerError::Wire(WireError::InvalidMagic));
        }
        let payload_len = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
        if payload_len > self.codec.max_payload() {
            return Err(ProducerError::Wire(WireError::PayloadTooLarge {
                actual: payload_len,
                limit: self.codec.max_payload(),
            }));
        }
        let mut framed = BytesMut::with_capacity(HEADER_LEN + payload_len);
        framed.extend_from_slice(&header);
        framed.resize(HEADER_LEN + payload_len, 0);
        self.io.read_exact(&mut framed[HEADER_LEN..])?;
        let envelope = self
            .codec
            .decode(&mut framed)?
            .ok_or(ProducerError::Closed)?;
        if envelope.message_id <= self.last_received_id {
            return Err(ProducerError::MessageOrder);
        }
        if self.session_id != 0 && envelope.session_id != self.session_id {
            return Err(ProducerError::WrongSession);
        }
        self.last_received_id = envelope.message_id;
        Ok(envelope)
    }
}

fn validate_frame_pixels(
    surface: &Surface,
    pixels: &[u8],
    previous: Option<&[u8]>,
) -> Result<(), ProducerError> {
    let expected = (surface.width as usize)
        .checked_mul(surface.height as usize)
        .ok_or(ProducerError::FrameTooLarge {
            actual: usize::MAX,
            limit: surface.max_frame_bytes,
        })?;
    if pixels.len() != expected {
        return Err(ProducerError::BadPixelLength {
            actual: pixels.len(),
            expected,
        });
    }
    if pixels.len() > surface.max_frame_bytes {
        return Err(ProducerError::FrameTooLarge {
            actual: pixels.len(),
            limit: surface.max_frame_bytes,
        });
    }
    if let Some(previous) = previous {
        if previous.len() != expected {
            return Err(ProducerError::BadPixelLength {
                actual: previous.len(),
                expected,
            });
        }
    }
    Ok(())
}

fn encode_delta_regions(
    previous: &[u8],
    current: &[u8],
    width: u32,
    height: u32,
    tile: u32,
    max_regions: usize,
    allow_zstd: bool,
) -> Vec<rm_display_protocol::FrameRegion> {
    let tile = tile.max(1) as usize;
    let width = width as usize;
    let height = height as usize;
    let mut rects: Vec<Rect> = Vec::new();
    for y in (0..height).step_by(tile) {
        let rect_height = tile.min(height - y);
        let mut x = 0;
        while x < width {
            let changed = |tile_x: usize| {
                let rect_width = tile.min(width - tile_x);
                (0..rect_height).any(|row| {
                    let start = (y + row) * width + tile_x;
                    previous[start..start + rect_width] != current[start..start + rect_width]
                })
            };
            if !changed(x) {
                x += tile;
                continue;
            }
            let start = x;
            x += tile;
            while x < width && changed(x) {
                x += tile;
            }
            let run_width = x.min(width) - start;
            if let Some(rect) = rects.iter_mut().rev().find(|rect| {
                rect.x == start as u32
                    && rect.width == run_width as u32
                    && rect.y + rect.height == y as u32
            }) {
                rect.height += rect_height as u32;
            } else {
                rects.push(Rect {
                    x: start as u32,
                    y: y as u32,
                    width: run_width as u32,
                    height: rect_height as u32,
                });
            }
        }
    }
    if rects.len() > max_regions.max(1) {
        let left = rects.iter().map(|rect| rect.x).min().unwrap();
        let top = rects.iter().map(|rect| rect.y).min().unwrap();
        let right = rects.iter().map(|rect| rect.x + rect.width).max().unwrap();
        let bottom = rects.iter().map(|rect| rect.y + rect.height).max().unwrap();
        rects = vec![Rect {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        }];
    }
    if rects.is_empty() {
        rects.push(Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        });
    }
    rects
        .into_iter()
        .map(|rect| {
            let pixels = extract_region(current, width, &rect);
            encode_region(rect, &pixels, allow_zstd)
        })
        .collect()
}

fn extract_region(pixels: &[u8], surface_width: usize, rect: &Rect) -> Vec<u8> {
    let mut output = Vec::with_capacity(rect.width as usize * rect.height as usize);
    for row in rect.y as usize..(rect.y + rect.height) as usize {
        let start = row * surface_width + rect.x as usize;
        output.extend_from_slice(&pixels[start..start + rect.width as usize]);
    }
    output
}

fn encode_region(rect: Rect, pixels: &[u8], allow_zstd: bool) -> rm_display_protocol::FrameRegion {
    let mut region = raw_region(rect, pixels.to_vec());
    if allow_zstd {
        if let Ok(compressed) = zstd::stream::encode_all(pixels, 1) {
            if compressed.len() + 12 < pixels.len() {
                region.encoding = Encoding::Zstd as i32;
                region.data = compressed.into();
            }
        }
    }
    region
}

fn elapsed_us(started: Instant) -> u64 {
    duration_us(started.elapsed())
}

fn duration_us(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

fn validate_server_hello(server: &ServerHello) -> Result<(), ProducerError> {
    if server.selected_minor > MAX_MINOR {
        return Err(ProducerError::BadHello("unsupported minor version"));
    }
    if server.server_id.len() != 16 {
        return Err(ProducerError::BadHello("server id must be 16 bytes"));
    }
    let display = server
        .display
        .as_ref()
        .ok_or(ProducerError::BadHello("display info missing"))?;
    if display.width == 0
        || display.height == 0
        || !display.pixel_formats.contains(&(PixelFormat::Gray8 as i32))
        || !display.encodings.contains(&(Encoding::Raw as i32))
    {
        return Err(ProducerError::BadHello(
            "mandatory Gray8/Raw or geometry missing",
        ));
    }
    let limits = server
        .limits
        .as_ref()
        .ok_or(ProducerError::BadHello("limits missing"))?;
    if limits.max_payload == 0
        || limits.max_frame_bytes == 0
        || limits.max_regions == 0
        || limits.max_inflight == 0
        || (server.selected_minor >= 1 && limits.max_inflight_bytes == 0)
    {
        return Err(ProducerError::BadHello("invalid zero limit"));
    }
    if server.selected_minor >= 1
        && !server
            .features
            .contains(&(ProtocolFeature::ByteCredits as i32))
    {
        return Err(ProducerError::BadHello("v2.1 byte credits missing"));
    }
    if server.selected_minor >= 2
        && !server
            .features
            .contains(&(ProtocolFeature::EpaperCustomProfile as i32))
    {
        return Err(ProducerError::BadHello(
            "v2.2 custom profile capability missing",
        ));
    }
    for mandatory in [
        ProtocolFeature::AtomicMultiRegion,
        ProtocolFeature::ExactBaseDelta,
        ProtocolFeature::LatestSupersede,
        ProtocolFeature::SettledBarrier,
    ] {
        if !server.features.contains(&(mandatory as i32)) {
            return Err(ProducerError::BadHello("mandatory feature missing"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read, Write};
    use std::sync::{Arc, Mutex};

    use bytes::BytesMut;
    use rm_display_protocol::{DisplayInfo, FrameMetrics, Limits};

    use super::*;

    struct MockIo {
        input: Cursor<Vec<u8>>,
        output: Arc<Mutex<Vec<u8>>>,
    }

    impl Read for MockIo {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buffer)
        }
    }

    impl Write for MockIo {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.output.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn server_hello() -> ServerHello {
        ServerHello {
            selected_minor: 0,
            features: vec![
                ProtocolFeature::AtomicMultiRegion as i32,
                ProtocolFeature::ExactBaseDelta as i32,
                ProtocolFeature::LatestSupersede as i32,
                ProtocolFeature::SettledBarrier as i32,
            ],
            server_id: Bytes::from_static(&[7; 16]),
            display: Some(DisplayInfo {
                width: 2,
                height: 2,
                orientation: Orientation::Portrait as i32,
                pixel_formats: vec![PixelFormat::Gray8 as i32],
                encodings: vec![Encoding::Raw as i32],
                input_capabilities: vec![],
            }),
            limits: Some(limits()),
            name: "test receiver".to_owned(),
        }
    }

    #[test]
    fn zstd_region_is_used_only_when_negotiated_and_smaller() {
        let rect = Rect {
            x: 0,
            y: 0,
            width: 64,
            height: 64,
        };
        let pixels = vec![0xff; 64 * 64];
        let compressed = encode_region(rect.clone(), &pixels, true);
        assert_eq!(compressed.encoding, Encoding::Zstd as i32);
        assert!(compressed.data.len() + 12 < pixels.len());
        let raw = encode_region(rect, &pixels, false);
        assert_eq!(raw.encoding, Encoding::Raw as i32);
        assert_eq!(raw.data.as_ref(), pixels);
    }

    fn limits() -> Limits {
        Limits {
            max_payload: 1024 * 1024,
            max_frame_bytes: 1024,
            max_regions: 8,
            max_inflight: 1,
            max_fps_x100: 400,
            settled_deadline_ms: 500,
            max_inflight_bytes: 1024,
        }
    }

    fn ready() -> SurfaceReady {
        SurfaceReady {
            surface_id: 1,
            generation: 9,
            width: 2,
            height: 2,
            pixel_format: PixelFormat::Gray8 as i32,
            orientation: Orientation::Portrait as i32,
            source_kind: SourceKind::LinuxStream as i32,
            input_capabilities: vec![],
            action_capabilities: vec![],
            limits: Some(limits()),
        }
    }

    fn result(frame_id: u64, code: FrameResultCode, logical: u64) -> FrameResult {
        FrameResult {
            surface_id: 1,
            generation: 9,
            frame_id,
            result: code as i32,
            reason: if code == FrameResultCode::NeedKeyframe {
                rm_display_protocol::FrameResultReason::BadBase as i32
            } else {
                rm_display_protocol::FrameResultReason::None as i32
            },
            credits: 1,
            logical_frame_id: logical,
            presented_frame_id: logical,
            metrics: Some(FrameMetrics::default()),
            byte_credits: 1024,
        }
    }

    fn framed_responses(bodies: Vec<envelope::Body>) -> Vec<u8> {
        let codec = WireCodec::new(1024 * 1024);
        let mut bytes = BytesMut::new();
        for (index, body) in bodies.into_iter().enumerate() {
            codec
                .encode(
                    &Envelope {
                        session_id: 7,
                        message_id: index as u32 + 1,
                        body: Some(body),
                    },
                    &mut bytes,
                )
                .unwrap();
        }
        bytes.to_vec()
    }

    #[test]
    fn bad_base_result_retries_with_a_keyframe() {
        let responses = framed_responses(vec![
            envelope::Body::ServerHello(server_hello()),
            envelope::Body::SurfaceReady(ready()),
            envelope::Body::FrameResult(result(1, FrameResultCode::Presented, 1)),
            envelope::Body::FrameResult(result(2, FrameResultCode::NeedKeyframe, 0)),
            envelope::Body::FrameResult(result(3, FrameResultCode::Presented, 3)),
        ]);
        let output = Arc::new(Mutex::new(Vec::new()));
        let io = MockIo {
            input: Cursor::new(responses),
            output: output.clone(),
        };
        let mut client = ProducerClient::new(Box::new(io), [1; 16]);
        client.hello("test").unwrap();
        let surface = client
            .open_surface(0, 0, SourceKind::LinuxStream, false, "test")
            .unwrap();
        client
            .send_frame(
                &surface,
                &[0, 1, 2, 3],
                FrameIntent::Latest,
                ContentClass::TextUi,
            )
            .unwrap();
        client
            .send_delta_frame(
                &surface,
                &[0, 1, 2, 3],
                &[0, 1, 2, 7],
                1,
                FrameIntent::Settled,
                ContentClass::TextUi,
            )
            .unwrap();

        let mut encoded = BytesMut::from(output.lock().unwrap().as_slice());
        let codec = WireCodec::new(1024 * 1024);
        let mut frames = Vec::new();
        while let Some(envelope) = codec.decode(&mut encoded).unwrap() {
            if let Some(envelope::Body::Frame(frame)) = envelope.body {
                frames.push(frame);
            }
        }
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].base_frame_id, 0);
        assert_eq!(frames[1].base_frame_id, 1);
        assert_eq!(frames[1].regions.len(), 1);
        assert_eq!(
            frames[1].regions[0].rect,
            Some(Rect {
                x: 1,
                y: 1,
                width: 1,
                height: 1,
            })
        );
        assert_eq!(frames[1].regions[0].data.as_ref(), [7]);
        assert_eq!(frames[2].base_frame_id, 0);
        assert_eq!(frames[2].regions[0].data.as_ref(), [0, 1, 2, 7]);
        assert_eq!(frames[2].intent, FrameIntent::Settled as i32);
    }

    #[test]
    fn queued_latest_frames_wait_only_when_credit_is_exhausted() {
        let mut surface_ready = ready();
        surface_ready.limits.as_mut().unwrap().max_inflight = 2;
        let mut first_result = result(1, FrameResultCode::Presented, 1);
        first_result.credits = 2;
        let responses = framed_responses(vec![
            envelope::Body::ServerHello(server_hello()),
            envelope::Body::SurfaceReady(surface_ready),
            envelope::Body::FrameResult(first_result),
            envelope::Body::FrameResult(result(2, FrameResultCode::Superseded, 3)),
        ]);
        let mut client = ProducerClient::new(
            Box::new(MockIo {
                input: Cursor::new(responses),
                output: Arc::new(Mutex::new(Vec::new())),
            }),
            [1; 16],
        );
        client.hello("test").unwrap();
        let surface = client
            .open_surface(0, 0, SourceKind::LinuxStream, false, "test")
            .unwrap();
        client
            .send_frame_report(
                &surface,
                &[0, 1, 2, 3],
                FrameIntent::Latest,
                ContentClass::TextUi,
            )
            .unwrap();
        for (previous, current) in [([0, 1, 2, 3], [0, 1, 2, 4]), ([0, 1, 2, 4], [0, 1, 2, 5])] {
            assert!(client
                .queue_delta_frame_report(&surface, &previous, &current, 1, ContentClass::TextUi,)
                .unwrap()
                .is_none());
        }
        assert_eq!(client.pending_frame_count(), 2);
        let report = client.wait_for_frame_credit_report().unwrap().unwrap();
        assert_eq!(report.result.frame_id, 2);
        assert_eq!(client.pending_frame_count(), 1);
    }

    #[test]
    fn delta_regions_merge_tiles_and_respect_the_region_limit() {
        let previous = [0; 16];
        let current = [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2];
        let regions = encode_delta_regions(&previous, &current, 4, 4, 2, 8, false);
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].rect.as_ref().unwrap().x, 0);
        assert_eq!(regions[0].data.as_ref(), [1, 0, 0, 0]);
        assert_eq!(regions[1].rect.as_ref().unwrap().x, 2);
        assert_eq!(regions[1].data.as_ref(), [0, 0, 0, 2]);

        let bounded = encode_delta_regions(&previous, &current, 4, 4, 2, 1, false);
        assert_eq!(bounded.len(), 1);
        assert_eq!(bounded[0].decoded_len, 16);

        let unchanged = encode_delta_regions(&previous, &previous, 4, 4, 2, 8, false);
        assert_eq!(unchanged.len(), 1);
        assert_eq!(unchanged[0].decoded_len, 1);
    }

    #[test]
    fn negotiated_profile_request_is_correlated_and_returns_effective_state() {
        let mut hello = server_hello();
        hello
            .features
            .push(ProtocolFeature::EpaperProfileControl as i32);
        let responses = framed_responses(vec![
            envelope::Body::ServerHello(hello),
            envelope::Body::EpaperProfileResult(EpaperProfileResult {
                request_id: 1,
                operation: EpaperProfileOperation::Set as i32,
                requested_profile: EpaperProfile::Reading as i32,
                result: EpaperProfileResultCode::Applied as i32,
                active: Some(rm_display_protocol::EpaperProfileState {
                    profile: EpaperProfile::Reading as i32,
                    cleanup_after_updates: 45,
                    large_update_threshold_percent: 50,
                    damage_tile: 64,
                    clean_first_frame: true,
                    static_cleanup_after_fast_updates: 3,
                    effective: None,
                }),
                cleanup_performed: true,
                cleanup_pending: false,
                message: "applied".to_owned(),
            }),
        ]);
        let output = Arc::new(Mutex::new(Vec::new()));
        let io = MockIo {
            input: Cursor::new(responses),
            output: output.clone(),
        };
        let mut client = ProducerClient::new(Box::new(io), [1; 16]);
        client.hello("test").unwrap();
        let result = client
            .request_epaper_profile(EpaperProfile::Reading)
            .unwrap();
        assert_eq!(
            result.active.unwrap().profile,
            EpaperProfile::Reading as i32
        );

        let mut encoded = BytesMut::from(output.lock().unwrap().as_slice());
        let codec = WireCodec::new(1024 * 1024);
        let mut profile_request = None;
        while let Some(envelope) = codec.decode(&mut encoded).unwrap() {
            if let Some(envelope::Body::EpaperProfileRequest(request)) = envelope.body {
                profile_request = Some(request);
            }
        }
        let request = profile_request.expect("profile request");
        assert_eq!(request.request_id, 1);
        assert_eq!(request.operation, EpaperProfileOperation::Set as i32);
        assert_eq!(request.requested_profile, EpaperProfile::Reading as i32);
    }

    #[test]
    fn negotiated_refresh_update_preserves_optional_field_presence() {
        let mut hello = server_hello();
        hello
            .features
            .push(ProtocolFeature::EpaperRefreshControl as i32);
        let responses = framed_responses(vec![
            envelope::Body::ServerHello(hello),
            envelope::Body::EpaperRefreshResult(EpaperRefreshResult {
                request_id: 1,
                operation: EpaperRefreshOperation::Update as i32,
                result: EpaperRefreshResultCode::Applied as i32,
                active: Some(rm_display_protocol::EpaperRefreshState {
                    partial_refresh_enabled: false,
                    cleanup_after_updates: 0,
                    large_update_threshold_percent: 40,
                    presented_since_full_refresh: 3,
                    cleanup_pending: false,
                    static_cleanup_after_fast_updates: 3,
                    fast_updates_since_settled: 2,
                }),
                cleanup_performed: false,
                message: "applied".to_owned(),
            }),
        ]);
        let output = Arc::new(Mutex::new(Vec::new()));
        let io = MockIo {
            input: Cursor::new(responses),
            output: output.clone(),
        };
        let mut client = ProducerClient::new(Box::new(io), [1; 16]);
        client.hello("test").unwrap();
        let result = client
            .update_epaper_refresh(Some(false), Some(0), None, None)
            .unwrap();
        assert!(!result.active.unwrap().partial_refresh_enabled);

        let mut encoded = BytesMut::from(output.lock().unwrap().as_slice());
        let codec = WireCodec::new(1024 * 1024);
        let mut refresh_request = None;
        while let Some(envelope) = codec.decode(&mut encoded).unwrap() {
            if let Some(envelope::Body::EpaperRefreshRequest(request)) = envelope.body {
                refresh_request = Some(request);
            }
        }
        let request = refresh_request.expect("refresh request");
        assert_eq!(request.request_id, 1);
        assert_eq!(request.partial_refresh_enabled, Some(false));
        assert_eq!(request.cleanup_after_updates, Some(0));
        assert_eq!(request.large_update_threshold_percent, None);
    }
}
