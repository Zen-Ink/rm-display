use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rm_display_core::{GraySurface, MockPanel, RefreshPolicyConfig, RefreshProfile, Waveform};
use rm_display_protocol::envelope::Body;
use rm_display_protocol::semantic::raw_region;
use rm_display_protocol::{
    ClientHello, ContentClass, Encoding, Envelope, EpaperProfile, EpaperProfileConfiguration,
    EpaperProfileOperation, EpaperProfileRequest, EpaperProfileResultCode, EpaperRefreshOperation,
    EpaperRefreshRequest, EpaperRefreshResultCode, EpaperWaveform, Frame, FrameIntent,
    FrameResultCode, FrameResultReason, InputCapability, PixelFormat,
    PointerPhase as ProtocolPointerPhase, ProducerKind, ProtocolFeature, Rect, SourceKind,
    SurfaceClose, SurfaceOpen,
};
use rm_display_receiver::evdev::{PhysicalPointerEvent, PointerPhase};
use rm_display_receiver::{
    ReceiverConfig, ReceiverLimits, ReservedZeroToken, SecurityMode, Session,
};

fn config() -> ReceiverConfig {
    ReceiverConfig {
        listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        security: SecurityMode::Plaintext,
        token_verifier: Arc::new(ReservedZeroToken),
        server_id: [9; 16],
        name: "test receiver".into(),
        limits: ReceiverLimits {
            max_fps_x100: 400,
            ..ReceiverLimits::default()
        },
        refresh_policy: RefreshPolicyConfig::default(),
        input_device: None,
    }
}

fn envelope(session_id: u64, message_id: u32, body: Body) -> Envelope {
    Envelope {
        session_id,
        message_id,
        body: Some(body),
    }
}

fn hello() -> ClientHello {
    ClientHello {
        min_minor: 0,
        max_minor: 0,
        producer_kind: ProducerKind::LinuxCli as i32,
        features: [
            ProtocolFeature::AtomicMultiRegion,
            ProtocolFeature::ExactBaseDelta,
            ProtocolFeature::LatestSupersede,
            ProtocolFeature::SettledBarrier,
        ]
        .map(|feature| feature as i32)
        .to_vec(),
        pixel_formats: vec![PixelFormat::Gray8 as i32],
        encodings: vec![Encoding::Raw as i32],
        client_id: vec![1; 16].into(),
        token: vec![0; 32].into(),
        client_nonce: vec![3; 16].into(),
        name: "host test".into(),
    }
}

fn hello_v21_color() -> ClientHello {
    let mut hello = hello();
    hello.max_minor = 1;
    hello.features.extend([
        ProtocolFeature::ColorRgb565 as i32,
        ProtocolFeature::ByteCredits as i32,
    ]);
    hello.pixel_formats.push(PixelFormat::Rgb565Le as i32);
    hello
}

fn hello_v22_custom_profile() -> ClientHello {
    let mut hello = hello();
    hello.max_minor = 2;
    hello.features.extend([
        ProtocolFeature::ByteCredits as i32,
        ProtocolFeature::EpaperProfileControl as i32,
        ProtocolFeature::EpaperCustomProfile as i32,
    ]);
    hello
}

fn frame(surface: u32, generation: u32, id: u64, base: u64, value: u8) -> Frame {
    Frame {
        surface_id: surface,
        generation,
        frame_id: id,
        base_frame_id: base,
        intent: FrameIntent::Latest as i32,
        content_class: ContentClass::TextUi as i32,
        regions: vec![raw_region(
            Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 3,
            },
            vec![value; 12],
        )],
        source_timestamp_us: 0,
    }
}

fn color_frame(surface: u32, generation: u32, id: u64, base: u64, value: u16) -> Frame {
    let pixel = value.to_le_bytes();
    Frame {
        surface_id: surface,
        generation,
        frame_id: id,
        base_frame_id: base,
        intent: FrameIntent::Latest as i32,
        content_class: ContentClass::Mixed as i32,
        regions: vec![raw_region(
            Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 3,
            },
            pixel.repeat(12),
        )],
        source_timestamp_us: 0,
    }
}

#[test]
fn encodings_are_intersected_and_source_kind_is_advisory() {
    let mut client_hello = hello();
    client_hello.encodings.push(Encoding::Zstd as i32);
    let mut panel = MockPanel::new(4, 3);
    let mut session = Session::new(config(), &mut panel);
    let response = session
        .handle(
            envelope(0, 1, Body::ClientHello(client_hello)),
            Duration::ZERO,
        )
        .unwrap();
    let server = match &response[0].body {
        Some(Body::ServerHello(server)) => server,
        other => panic!("unexpected response: {other:?}"),
    };
    assert_eq!(
        server.display.as_ref().unwrap().encodings,
        vec![Encoding::Raw as i32, Encoding::Zstd as i32],
    );

    let session_id = session.session_id();
    let response = session
        .handle(
            envelope(
                session_id,
                2,
                Body::SurfaceOpen(SurfaceOpen {
                    surface_id: 19,
                    desired_width: 0,
                    desired_height: 0,
                    pixel_format: PixelFormat::Gray8 as i32,
                    orientation: 0,
                    source_kind: SourceKind::Unspecified as i32,
                    input_capabilities: Vec::new(),
                    action_capabilities: Vec::new(),
                    label: "generic".into(),
                }),
            ),
            Duration::ZERO,
        )
        .unwrap();
    let generation = match &response[0].body {
        Some(Body::SurfaceReady(ready)) => {
            assert_eq!(ready.source_kind, SourceKind::Unspecified as i32);
            ready.generation
        }
        other => panic!("unexpected response: {other:?}"),
    };

    let mut unnegotiated = frame(19, generation, 1, 0, 0xff);
    unnegotiated.regions[0].encoding = Encoding::Zlib as i32;
    let response = session
        .handle(
            envelope(session_id, 3, Body::Frame(unnegotiated)),
            Duration::ZERO,
        )
        .unwrap();
    assert!(matches!(
        response[0].body,
        Some(Body::FrameResult(ref result))
            if result.result == FrameResultCode::Rejected as i32
                && result.reason == FrameResultReason::Unsupported as i32
    ));
}

#[test]
fn v21_color_surface_and_byte_credits_are_negotiated_and_enforced() {
    let mut config = config();
    config.limits.max_inflight_bytes = 32;
    let mut panel = MockPanel::new(4, 3).with_rgb565();
    let mut session = Session::new(config, &mut panel);
    let response = session
        .handle(
            envelope(0, 1, Body::ClientHello(hello_v21_color())),
            Duration::ZERO,
        )
        .unwrap();
    let server = match &response[0].body {
        Some(Body::ServerHello(server)) => server,
        other => panic!("unexpected response: {other:?}"),
    };
    assert_eq!(server.selected_minor, 1);
    assert!(server
        .features
        .contains(&(ProtocolFeature::ColorRgb565 as i32)));
    assert!(server
        .features
        .contains(&(ProtocolFeature::ByteCredits as i32)));
    assert_eq!(server.limits.as_ref().unwrap().max_inflight_bytes, 32);

    let session_id = session.session_id();
    let response = session
        .handle(
            envelope(
                session_id,
                2,
                Body::SurfaceOpen(SurfaceOpen {
                    surface_id: 17,
                    desired_width: 0,
                    desired_height: 0,
                    pixel_format: PixelFormat::Rgb565Le as i32,
                    orientation: 0,
                    source_kind: SourceKind::ScreenMirror as i32,
                    input_capabilities: Vec::new(),
                    action_capabilities: Vec::new(),
                    label: "color".into(),
                }),
            ),
            Duration::ZERO,
        )
        .unwrap();
    let generation = match &response[0].body {
        Some(Body::SurfaceReady(ready)) => {
            assert_eq!(ready.pixel_format, PixelFormat::Rgb565Le as i32);
            ready.generation
        }
        other => panic!("unexpected response: {other:?}"),
    };

    let first = session
        .handle(
            envelope(
                session_id,
                3,
                Body::Frame(color_frame(17, generation, 1, 0, 0xf800)),
            ),
            Duration::ZERO,
        )
        .unwrap();
    assert!(first.iter().any(|response| matches!(
        response.body,
        Some(Body::FrameResult(ref result))
            if result.frame_id == 1 && result.byte_credits == 32
    )));

    assert!(session
        .handle(
            envelope(
                session_id,
                4,
                Body::Frame(color_frame(17, generation, 2, 1, 0x07e0)),
            ),
            Duration::from_millis(10),
        )
        .unwrap()
        .is_empty());
    let rejected = session
        .handle(
            envelope(
                session_id,
                5,
                Body::Frame(color_frame(17, generation, 3, 2, 0x001f)),
            ),
            Duration::from_millis(20),
        )
        .unwrap();
    assert!(rejected.iter().any(|response| matches!(
        response.body,
        Some(Body::FrameResult(ref result))
            if result.frame_id == 3
                && result.reason == FrameResultReason::NoCredit as i32
                && result.byte_credits == 8
    )));

    drop(session);
    assert_eq!(panel.submissions()[0].pixel_format, PixelFormat::Rgb565Le);
    assert_eq!(
        &panel.submissions()[0].pixels[..2],
        &0xf800_u16.to_le_bytes()
    );
}

#[test]
fn host_mock_handshake_surface_latest_supersede_and_present() {
    let mut panel = MockPanel::new(4, 3);
    let mut session = Session::new(config(), &mut panel);
    let server_hello = session
        .handle(envelope(0, 1, Body::ClientHello(hello())), Duration::ZERO)
        .unwrap();
    assert!(matches!(server_hello[0].body, Some(Body::ServerHello(_))));
    let session_id = session.session_id();

    let ready = session
        .handle(
            envelope(
                session_id,
                2,
                Body::SurfaceOpen(SurfaceOpen {
                    surface_id: 7,
                    desired_width: 0,
                    desired_height: 0,
                    pixel_format: PixelFormat::Gray8 as i32,
                    orientation: 0,
                    source_kind: SourceKind::LinuxStream as i32,
                    input_capabilities: Vec::new(),
                    action_capabilities: Vec::new(),
                    label: "test".into(),
                }),
            ),
            Duration::ZERO,
        )
        .unwrap();
    let generation = match &ready[0].body {
        Some(Body::SurfaceReady(ready)) => ready.generation,
        other => panic!("unexpected response: {other:?}"),
    };

    let first = session
        .handle(
            envelope(session_id, 3, Body::Frame(frame(7, generation, 1, 0, 10))),
            Duration::ZERO,
        )
        .unwrap();
    assert!(first.iter().any(|response| matches!(
        response.body,
        Some(Body::FrameResult(ref result)) if result.frame_id == 1 && result.presented_frame_id == 1
    )));
    let metrics = first
        .iter()
        .find_map(|response| match &response.body {
            Some(Body::FrameResult(result)) if result.frame_id == 1 => result.metrics.as_ref(),
            _ => None,
        })
        .expect("presented result metrics");
    assert_eq!(metrics.damage_pixels, 12);
    assert_eq!(metrics.damage_regions, 1);
    assert_eq!(metrics.waveform, 3);
    assert!(metrics.complete_refresh);

    let second = session
        .handle(
            envelope(session_id, 4, Body::Frame(frame(7, generation, 2, 1, 20))),
            Duration::from_millis(10),
        )
        .unwrap();
    assert!(second.is_empty());
    let third = session
        .handle(
            envelope(session_id, 5, Body::Frame(frame(7, generation, 3, 2, 30))),
            Duration::from_millis(20),
        )
        .unwrap();
    assert!(third.iter().any(|response| matches!(
        response.body,
        Some(Body::FrameResult(ref result)) if result.frame_id == 2
    )));
    let terminal = session.poll(Duration::from_millis(250)).unwrap();
    assert!(terminal.iter().any(|response| matches!(
        response.body,
        Some(Body::FrameResult(ref result)) if result.frame_id == 3 && result.presented_frame_id == 3
    )));
    drop(session);
    assert_eq!(panel.submissions().len(), 2);
    assert_eq!(panel.submissions()[1].pixels, vec![30; 12]);
}

#[test]
fn plaintext_may_bind_non_loopback() {
    let mut config = config();
    config.listen = "0.0.0.0:7420".parse().unwrap();
    assert!(config.validate().is_ok());
}

#[test]
fn reserved_hello_token_must_be_zero() {
    let mut panel = MockPanel::new(4, 3);
    let mut session = Session::new(config(), &mut panel);
    let mut invalid = hello();
    invalid.token = vec![1; 32].into();
    let error = session
        .handle(envelope(0, 1, Body::ClientHello(invalid)), Duration::ZERO)
        .unwrap_err();
    assert!(matches!(
        error,
        rm_display_receiver::SessionError::Authentication
    ));
}

#[test]
fn physical_report_uses_surface_generation_sequence_and_fixed_point() {
    let mut config = config();
    config.input_device = Some(PathBuf::from("/dev/input/test"));
    let mut panel = MockPanel::new(4, 3);
    let mut session = Session::new(config, &mut panel);
    session
        .handle(envelope(0, 1, Body::ClientHello(hello())), Duration::ZERO)
        .unwrap();
    let session_id = session.session_id();
    let ready = session
        .handle(
            envelope(
                session_id,
                2,
                Body::SurfaceOpen(SurfaceOpen {
                    surface_id: 8,
                    desired_width: 0,
                    desired_height: 0,
                    pixel_format: PixelFormat::Gray8 as i32,
                    orientation: 0,
                    source_kind: SourceKind::Browser as i32,
                    input_capabilities: vec![InputCapability::Touch as i32],
                    action_capabilities: Vec::new(),
                    label: "input".into(),
                }),
            ),
            Duration::ZERO,
        )
        .unwrap();
    let generation = match &ready[0].body {
        Some(Body::SurfaceReady(ready)) => ready.generation,
        other => panic!("unexpected response: {other:?}"),
    };
    let batches = session
        .input_reports(
            vec![vec![PhysicalPointerEvent {
                phase: PointerPhase::Down,
                contact_id: 42,
                x: 2,
                y: 1,
            }]],
            Duration::from_micros(1234),
        )
        .unwrap();
    match &batches[0].body {
        Some(Body::InputBatch(batch)) => {
            assert_eq!(batch.surface_id, 8);
            assert_eq!(batch.generation, generation);
            assert_eq!(batch.sequence, 1);
            assert_eq!(batch.monotonic_us, 1234);
            assert_eq!(batch.records[0].contact_id, 42);
            assert_eq!(batch.records[0].phase, ProtocolPointerPhase::Down as i32);
            assert_eq!(batch.records[0].x_16_16, 2 << 16);
            assert_eq!(batch.records[0].y_16_16, 1 << 16);
        }
        other => panic!("unexpected input response: {other:?}"),
    }
}

#[test]
fn closing_surface_completes_its_pending_frame() {
    let mut panel = MockPanel::new(4, 3);
    let mut session = Session::new(config(), &mut panel);
    session
        .handle(envelope(0, 1, Body::ClientHello(hello())), Duration::ZERO)
        .unwrap();
    let session_id = session.session_id();
    let ready = session
        .handle(
            envelope(
                session_id,
                2,
                Body::SurfaceOpen(SurfaceOpen {
                    surface_id: 9,
                    desired_width: 0,
                    desired_height: 0,
                    pixel_format: PixelFormat::Gray8 as i32,
                    orientation: 0,
                    source_kind: SourceKind::LinuxStream as i32,
                    input_capabilities: Vec::new(),
                    action_capabilities: Vec::new(),
                    label: "close test".into(),
                }),
            ),
            Duration::ZERO,
        )
        .unwrap();
    let generation = match &ready[0].body {
        Some(Body::SurfaceReady(ready)) => ready.generation,
        other => panic!("unexpected response: {other:?}"),
    };
    session
        .handle(
            envelope(session_id, 3, Body::Frame(frame(9, generation, 1, 0, 10))),
            Duration::ZERO,
        )
        .unwrap();
    session
        .handle(
            envelope(session_id, 4, Body::Frame(frame(9, generation, 2, 1, 20))),
            Duration::from_millis(10),
        )
        .unwrap();
    let closed = session
        .handle(
            envelope(
                session_id,
                5,
                Body::SurfaceClose(SurfaceClose {
                    surface_id: 9,
                    generation,
                    reason: 0,
                }),
            ),
            Duration::from_millis(20),
        )
        .unwrap();
    assert!(closed.iter().any(|response| matches!(
        response.body,
        Some(Body::FrameResult(ref result))
            if result.surface_id == 9
                && result.generation == generation
                && result.frame_id == 2
                && result.result == FrameResultCode::Rejected as i32
                && result.reason == FrameResultReason::ProtocolState as i32
    )));
}

#[test]
fn negotiated_profile_switch_flushes_pending_settled_and_reports_effective_policy() {
    let mut receiver_config = config();
    receiver_config.refresh_policy = RefreshPolicyConfig {
        clean_first_frame: false,
        ..RefreshPolicyConfig::for_profile(RefreshProfile::Animate)
    };
    let mut panel = MockPanel::new(4, 3);
    let mut session = Session::new(receiver_config, &mut panel);
    let mut client_hello = hello();
    client_hello
        .features
        .push(ProtocolFeature::EpaperProfileControl as i32);
    let hello_responses = session
        .handle(
            envelope(0, 1, Body::ClientHello(client_hello)),
            Duration::ZERO,
        )
        .unwrap();
    assert!(matches!(
        &hello_responses[0].body,
        Some(Body::ServerHello(hello))
            if hello.features.contains(&(ProtocolFeature::EpaperProfileControl as i32))
    ));
    let session_id = session.session_id();
    let ready = session
        .handle(
            envelope(
                session_id,
                2,
                Body::SurfaceOpen(SurfaceOpen {
                    surface_id: 10,
                    desired_width: 0,
                    desired_height: 0,
                    pixel_format: PixelFormat::Gray8 as i32,
                    orientation: 0,
                    source_kind: SourceKind::LinuxStream as i32,
                    input_capabilities: Vec::new(),
                    action_capabilities: Vec::new(),
                    label: "profile test".into(),
                }),
            ),
            Duration::ZERO,
        )
        .unwrap();
    let generation = match &ready[0].body {
        Some(Body::SurfaceReady(ready)) => ready.generation,
        other => panic!("unexpected response: {other:?}"),
    };
    session
        .handle(
            envelope(session_id, 3, Body::Frame(frame(10, generation, 1, 0, 1))),
            Duration::ZERO,
        )
        .unwrap();
    let mut settled = frame(10, generation, 2, 1, 2);
    settled.intent = FrameIntent::Settled as i32;
    assert!(session
        .handle(
            envelope(session_id, 4, Body::Frame(settled)),
            Duration::from_millis(10),
        )
        .unwrap()
        .is_empty());

    let responses = session
        .handle(
            envelope(
                session_id,
                5,
                Body::EpaperProfileRequest(EpaperProfileRequest {
                    request_id: 1,
                    operation: EpaperProfileOperation::Set as i32,
                    requested_profile: EpaperProfile::Quality as i32,
                    custom: None,
                }),
            ),
            Duration::from_millis(20),
        )
        .unwrap();
    assert!(responses.iter().any(|response| matches!(
        &response.body,
        Some(Body::FrameResult(result))
            if result.frame_id == 2 && result.result == FrameResultCode::Presented as i32
    )));
    let result = responses
        .iter()
        .find_map(|response| match &response.body {
            Some(Body::EpaperProfileResult(result)) => Some(result),
            _ => None,
        })
        .expect("profile result");
    assert_eq!(result.result, EpaperProfileResultCode::Applied as i32);
    assert_eq!(
        result.active.as_ref().unwrap().profile,
        EpaperProfile::Quality as i32
    );
    assert_eq!(result.active.as_ref().unwrap().cleanup_after_updates, 20);
    assert!(result.cleanup_performed);
    assert!(!result.cleanup_pending);
    drop(session);
    assert_eq!(panel.submissions().len(), 2);
    assert!(panel.submissions()[1].refresh.complete_refresh);
    assert_eq!(
        panel.submissions()[1].refresh.waveform,
        Waveform::FullQuality
    );
}

#[test]
fn unnegotiated_profile_control_is_nonfatal_and_unsupported() {
    let mut panel = MockPanel::new(4, 3);
    let mut session = Session::new(config(), &mut panel);
    session
        .handle(envelope(0, 1, Body::ClientHello(hello())), Duration::ZERO)
        .unwrap();
    let result = session
        .handle(
            envelope(
                session.session_id(),
                2,
                Body::EpaperProfileRequest(EpaperProfileRequest {
                    request_id: 1,
                    operation: EpaperProfileOperation::Query as i32,
                    requested_profile: EpaperProfile::Unspecified as i32,
                    custom: None,
                }),
            ),
            Duration::ZERO,
        )
        .unwrap();
    assert!(matches!(
        &result[0].body,
        Some(Body::EpaperProfileResult(result))
            if result.result == EpaperProfileResultCode::Unsupported as i32
    ));
}

#[test]
fn appended_realtime_and_reading_profiles_map_to_effective_presets() {
    let mut panel = MockPanel::new(4, 3);
    let mut session = Session::new(config(), &mut panel);
    let mut client_hello = hello();
    client_hello
        .features
        .push(ProtocolFeature::EpaperProfileControl as i32);
    session
        .handle(
            envelope(0, 1, Body::ClientHello(client_hello)),
            Duration::ZERO,
        )
        .unwrap();
    let session_id = session.session_id();

    let realtime = session
        .handle(
            envelope(
                session_id,
                2,
                Body::EpaperProfileRequest(EpaperProfileRequest {
                    request_id: 1,
                    operation: EpaperProfileOperation::Set as i32,
                    requested_profile: EpaperProfile::Realtime as i32,
                    custom: None,
                }),
            ),
            Duration::ZERO,
        )
        .unwrap();
    assert!(matches!(
        &realtime[0].body,
        Some(Body::EpaperProfileResult(result))
            if result.active.as_ref().is_some_and(|state|
                state.profile == EpaperProfile::Realtime as i32
                    && state.cleanup_after_updates == 360
                    && state.large_update_threshold_percent == 0)
    ));

    let reading = session
        .handle(
            envelope(
                session_id,
                3,
                Body::EpaperProfileRequest(EpaperProfileRequest {
                    request_id: 2,
                    operation: EpaperProfileOperation::Set as i32,
                    requested_profile: EpaperProfile::Reading as i32,
                    custom: None,
                }),
            ),
            Duration::ZERO,
        )
        .unwrap();
    assert!(matches!(
        &reading[0].body,
        Some(Body::EpaperProfileResult(result))
            if result.active.as_ref().is_some_and(|state|
                state.profile == EpaperProfile::Reading as i32
                    && state.cleanup_after_updates == 45
                    && state.large_update_threshold_percent == 50)
    ));
}

fn custom_profile() -> EpaperProfileConfiguration {
    EpaperProfileConfiguration {
        latest_text_waveform: EpaperWaveform::Fastest as i32,
        latest_photo_waveform: EpaperWaveform::Quality as i32,
        latest_video_waveform: EpaperWaveform::Fast as i32,
        settled_waveform: EpaperWaveform::Fast as i32,
        partial_refresh_enabled: true,
        cleanup_after_updates: 77,
        clean_first_frame: false,
        large_update_threshold_percent: 42,
        static_cleanup_after_fast_updates: 5,
        damage_tile: 32,
    }
}

#[test]
fn v22_custom_profile_is_atomic_and_reports_complete_effective_state() {
    let mut panel = MockPanel::new(4, 3);
    let mut session = Session::new(config(), &mut panel);
    let hello_result = session
        .handle(
            envelope(0, 1, Body::ClientHello(hello_v22_custom_profile())),
            Duration::ZERO,
        )
        .unwrap();
    assert!(matches!(
        &hello_result[0].body,
        Some(Body::ServerHello(hello))
            if hello.selected_minor == 2
                && hello.features.contains(&(ProtocolFeature::EpaperCustomProfile as i32))
    ));
    let custom = custom_profile();
    let result = session
        .handle(
            envelope(
                session.session_id(),
                2,
                Body::EpaperProfileRequest(EpaperProfileRequest {
                    request_id: 1,
                    operation: EpaperProfileOperation::Set as i32,
                    requested_profile: EpaperProfile::Custom as i32,
                    custom: Some(custom.clone()),
                }),
            ),
            Duration::ZERO,
        )
        .unwrap();
    let state = match &result[0].body {
        Some(Body::EpaperProfileResult(result)) => {
            assert_eq!(result.result, EpaperProfileResultCode::Applied as i32);
            result.active.as_ref().unwrap()
        }
        _ => panic!("expected profile result"),
    };
    assert_eq!(state.profile, EpaperProfile::Custom as i32);
    assert_eq!(state.effective.as_ref(), Some(&custom));
    assert_eq!(state.cleanup_after_updates, custom.cleanup_after_updates);
    assert_eq!(state.damage_tile, custom.damage_tile);
}

#[test]
fn custom_profile_rejects_incomplete_waveforms_without_mutating_active_state() {
    let mut panel = MockPanel::new(4, 3);
    let mut session = Session::new(config(), &mut panel);
    session
        .handle(
            envelope(0, 1, Body::ClientHello(hello_v22_custom_profile())),
            Duration::ZERO,
        )
        .unwrap();
    let mut custom = custom_profile();
    custom.settled_waveform = EpaperWaveform::Unspecified as i32;
    let result = session
        .handle(
            envelope(
                session.session_id(),
                2,
                Body::EpaperProfileRequest(EpaperProfileRequest {
                    request_id: 1,
                    operation: EpaperProfileOperation::Set as i32,
                    requested_profile: EpaperProfile::Custom as i32,
                    custom: Some(custom),
                }),
            ),
            Duration::ZERO,
        )
        .unwrap();
    assert!(matches!(
        &result[0].body,
        Some(Body::EpaperProfileResult(result))
            if result.result == EpaperProfileResultCode::Rejected as i32
                && result.active.as_ref().is_some_and(|state|
                    state.profile == EpaperProfile::Balanced as i32)
    ));
}

#[test]
fn negotiated_refresh_parameters_query_update_and_cleanup_are_session_scoped() {
    let mut panel = MockPanel::new(4, 3);
    let mut session = Session::new(config(), &mut panel);
    let mut client_hello = hello();
    client_hello
        .features
        .push(ProtocolFeature::EpaperRefreshControl as i32);
    let hello_result = session
        .handle(
            envelope(0, 1, Body::ClientHello(client_hello)),
            Duration::ZERO,
        )
        .unwrap();
    assert!(matches!(
        &hello_result[0].body,
        Some(Body::ServerHello(hello))
            if hello.features.contains(&(ProtocolFeature::EpaperRefreshControl as i32))
    ));
    let session_id = session.session_id();

    let query = session
        .handle(
            envelope(
                session_id,
                2,
                Body::EpaperRefreshRequest(EpaperRefreshRequest {
                    request_id: 1,
                    operation: EpaperRefreshOperation::Query as i32,
                    partial_refresh_enabled: None,
                    cleanup_after_updates: None,
                    large_update_threshold_percent: None,
                    static_cleanup_after_fast_updates: None,
                }),
            ),
            Duration::ZERO,
        )
        .unwrap();
    assert!(matches!(
        &query[0].body,
        Some(Body::EpaperRefreshResult(result))
            if result.result == EpaperRefreshResultCode::Unchanged as i32
                && result.active.as_ref().is_some_and(|state| state.partial_refresh_enabled)
    ));

    let updated = session
        .handle(
            envelope(
                session_id,
                3,
                Body::EpaperRefreshRequest(EpaperRefreshRequest {
                    request_id: 2,
                    operation: EpaperRefreshOperation::Update as i32,
                    partial_refresh_enabled: Some(false),
                    cleanup_after_updates: Some(0),
                    large_update_threshold_percent: Some(75),
                    static_cleanup_after_fast_updates: Some(4),
                }),
            ),
            Duration::ZERO,
        )
        .unwrap();
    assert!(matches!(
        &updated[0].body,
        Some(Body::EpaperRefreshResult(result))
            if result.result == EpaperRefreshResultCode::Applied as i32
                && result.active.as_ref().is_some_and(|state|
                    !state.partial_refresh_enabled
                        && state.cleanup_after_updates == 0
                        && state.large_update_threshold_percent == 75)
    ));

    let ready = session
        .handle(
            envelope(
                session_id,
                4,
                Body::SurfaceOpen(SurfaceOpen {
                    surface_id: 12,
                    desired_width: 0,
                    desired_height: 0,
                    pixel_format: PixelFormat::Gray8 as i32,
                    orientation: 0,
                    source_kind: SourceKind::TestPattern as i32,
                    input_capabilities: Vec::new(),
                    action_capabilities: Vec::new(),
                    label: "refresh control".into(),
                }),
            ),
            Duration::ZERO,
        )
        .unwrap();
    let generation = match &ready[0].body {
        Some(Body::SurfaceReady(ready)) => ready.generation,
        other => panic!("unexpected response: {other:?}"),
    };
    session
        .handle(
            envelope(session_id, 5, Body::Frame(frame(12, generation, 1, 0, 10))),
            Duration::ZERO,
        )
        .unwrap();
    let cleanup = session
        .handle(
            envelope(
                session_id,
                6,
                Body::EpaperRefreshRequest(EpaperRefreshRequest {
                    request_id: 3,
                    operation: EpaperRefreshOperation::Cleanup as i32,
                    partial_refresh_enabled: None,
                    cleanup_after_updates: None,
                    large_update_threshold_percent: None,
                    static_cleanup_after_fast_updates: None,
                }),
            ),
            Duration::from_millis(10),
        )
        .unwrap();
    assert!(matches!(
        cleanup.last().and_then(|envelope| envelope.body.as_ref()),
        Some(Body::EpaperRefreshResult(result))
            if result.result == EpaperRefreshResultCode::Applied as i32
                && result.cleanup_performed
                && result.active.as_ref().is_some_and(|state| !state.cleanup_pending)
    ));
    drop(session);
    assert_eq!(panel.submissions().len(), 2);
    assert!(panel
        .submissions()
        .iter()
        .all(|submission| submission.refresh.complete_refresh));
}

#[test]
fn refresh_control_rejects_invalid_threshold_and_presence_without_closing_session() {
    let mut panel = MockPanel::new(4, 3);
    let mut session = Session::new(config(), &mut panel);
    let mut client_hello = hello();
    client_hello
        .features
        .push(ProtocolFeature::EpaperRefreshControl as i32);
    session
        .handle(
            envelope(0, 1, Body::ClientHello(client_hello)),
            Duration::ZERO,
        )
        .unwrap();
    let session_id = session.session_id();

    let invalid_threshold = session
        .handle(
            envelope(
                session_id,
                2,
                Body::EpaperRefreshRequest(EpaperRefreshRequest {
                    request_id: 1,
                    operation: EpaperRefreshOperation::Update as i32,
                    partial_refresh_enabled: None,
                    cleanup_after_updates: None,
                    large_update_threshold_percent: Some(101),
                    static_cleanup_after_fast_updates: None,
                }),
            ),
            Duration::ZERO,
        )
        .unwrap();
    assert!(matches!(
        &invalid_threshold[0].body,
        Some(Body::EpaperRefreshResult(result))
            if result.result == EpaperRefreshResultCode::Rejected as i32
    ));

    let invalid_query = session
        .handle(
            envelope(
                session_id,
                3,
                Body::EpaperRefreshRequest(EpaperRefreshRequest {
                    request_id: 2,
                    operation: EpaperRefreshOperation::Query as i32,
                    partial_refresh_enabled: Some(false),
                    cleanup_after_updates: None,
                    large_update_threshold_percent: None,
                    static_cleanup_after_fast_updates: None,
                }),
            ),
            Duration::ZERO,
        )
        .unwrap();
    assert!(matches!(
        &invalid_query[0].body,
        Some(Body::EpaperRefreshResult(result))
            if result.result == EpaperRefreshResultCode::Rejected as i32
    ));
    assert!(!session.is_closed());
}

#[test]
fn five_finger_cleanup_is_local_and_cancels_forwarded_contacts() {
    let mut receiver_config = config();
    receiver_config.input_device = Some(PathBuf::from("/dev/input/test"));
    let mut panel = MockPanel::new(4, 3);
    let mut session = Session::new(receiver_config, &mut panel);
    session
        .handle(envelope(0, 1, Body::ClientHello(hello())), Duration::ZERO)
        .unwrap();
    let session_id = session.session_id();
    let ready = session
        .handle(
            envelope(
                session_id,
                2,
                Body::SurfaceOpen(SurfaceOpen {
                    surface_id: 13,
                    desired_width: 0,
                    desired_height: 0,
                    pixel_format: PixelFormat::Gray8 as i32,
                    orientation: 0,
                    source_kind: SourceKind::TestPattern as i32,
                    input_capabilities: vec![InputCapability::Touch as i32],
                    action_capabilities: Vec::new(),
                    label: "five finger".into(),
                }),
            ),
            Duration::ZERO,
        )
        .unwrap();
    let generation = match &ready[0].body {
        Some(Body::SurfaceReady(ready)) => ready.generation,
        other => panic!("unexpected response: {other:?}"),
    };
    session
        .handle(
            envelope(session_id, 3, Body::Frame(frame(13, generation, 1, 0, 10))),
            Duration::ZERO,
        )
        .unwrap();

    let four = (1..=4)
        .map(|contact_id| PhysicalPointerEvent {
            phase: PointerPhase::Down,
            contact_id,
            x: contact_id,
            y: contact_id,
        })
        .collect();
    let forwarded = session
        .input_reports(vec![four], Duration::from_millis(1))
        .unwrap();
    assert!(matches!(
        &forwarded[0].body,
        Some(Body::InputBatch(batch)) if batch.records.len() == 4
    ));

    let triggered = session
        .input_reports(
            vec![vec![PhysicalPointerEvent {
                phase: PointerPhase::Down,
                contact_id: 5,
                x: 3,
                y: 2,
            }]],
            Duration::from_millis(2),
        )
        .unwrap();
    assert!(matches!(
        &triggered[0].body,
        Some(Body::InputBatch(batch))
            if batch.records.len() == 4
                && batch.records.iter().all(|record| record.phase == ProtocolPointerPhase::Cancel as i32)
    ));
    drop(session);
    assert_eq!(panel.submissions().len(), 2);
    assert!(panel.submissions()[1].refresh.complete_refresh);
}

#[test]
fn five_finger_cleanup_does_not_require_pointer_input_negotiation() {
    let mut receiver_config = config();
    receiver_config.input_device = Some(PathBuf::from("/dev/input/test"));
    let mut panel = MockPanel::new(4, 3);
    let mut session = Session::new(receiver_config, &mut panel);
    session
        .handle(envelope(0, 1, Body::ClientHello(hello())), Duration::ZERO)
        .unwrap();
    let session_id = session.session_id();
    let ready = session
        .handle(
            envelope(
                session_id,
                2,
                Body::SurfaceOpen(SurfaceOpen {
                    surface_id: 14,
                    desired_width: 0,
                    desired_height: 0,
                    pixel_format: PixelFormat::Gray8 as i32,
                    orientation: 0,
                    source_kind: SourceKind::TestPattern as i32,
                    input_capabilities: Vec::new(),
                    action_capabilities: Vec::new(),
                    label: "local gesture".into(),
                }),
            ),
            Duration::ZERO,
        )
        .unwrap();
    let generation = match &ready[0].body {
        Some(Body::SurfaceReady(ready)) => ready.generation,
        other => panic!("unexpected response: {other:?}"),
    };
    session
        .handle(
            envelope(session_id, 3, Body::Frame(frame(14, generation, 1, 0, 10))),
            Duration::ZERO,
        )
        .unwrap();
    let five = (1..=5)
        .map(|contact_id| PhysicalPointerEvent {
            phase: PointerPhase::Down,
            contact_id,
            x: contact_id,
            y: contact_id,
        })
        .collect();
    assert!(session
        .input_reports(vec![five], Duration::from_millis(1))
        .unwrap()
        .is_empty());
    drop(session);
    assert_eq!(panel.submissions().len(), 2);
    assert!(panel.submissions()[1].refresh.complete_refresh);
}

#[test]
fn physical_partial_debt_survives_surface_replace_close_and_reopen() {
    let mut receiver_config = config();
    receiver_config.refresh_policy = RefreshPolicyConfig {
        cleanup_after_updates: 2,
        clean_first_frame: false,
        ..RefreshPolicyConfig::default()
    };
    let mut panel = MockPanel::new(4, 3);
    let mut session = Session::new(receiver_config, &mut panel);
    let mut client_hello = hello();
    client_hello
        .features
        .push(ProtocolFeature::EpaperRefreshControl as i32);
    session
        .handle(
            envelope(0, 1, Body::ClientHello(client_hello)),
            Duration::ZERO,
        )
        .unwrap();
    let session_id = session.session_id();

    let open = |surface_id| SurfaceOpen {
        surface_id,
        desired_width: 0,
        desired_height: 0,
        pixel_format: PixelFormat::Gray8 as i32,
        orientation: 0,
        source_kind: SourceKind::TestPattern as i32,
        input_capabilities: Vec::new(),
        action_capabilities: Vec::new(),
        label: "debt carry".into(),
    };
    let first_ready = session
        .handle(
            envelope(session_id, 2, Body::SurfaceOpen(open(20))),
            Duration::ZERO,
        )
        .unwrap();
    let first_generation = match &first_ready[0].body {
        Some(Body::SurfaceReady(ready)) => ready.generation,
        other => panic!("unexpected response: {other:?}"),
    };
    session
        .handle(
            envelope(
                session_id,
                3,
                Body::Frame(frame(20, first_generation, 1, 0, 10)),
            ),
            Duration::ZERO,
        )
        .unwrap();

    let replacement = session
        .handle(
            envelope(session_id, 4, Body::SurfaceOpen(open(21))),
            Duration::from_millis(10),
        )
        .unwrap();
    let replacement_generation = replacement
        .iter()
        .find_map(|response| match &response.body {
            Some(Body::SurfaceReady(ready)) => Some(ready.generation),
            _ => None,
        })
        .expect("replacement SurfaceReady");
    let after_replace = session
        .handle(
            envelope(
                session_id,
                5,
                Body::EpaperRefreshRequest(EpaperRefreshRequest {
                    request_id: 1,
                    operation: EpaperRefreshOperation::Query as i32,
                    partial_refresh_enabled: None,
                    cleanup_after_updates: None,
                    large_update_threshold_percent: None,
                    static_cleanup_after_fast_updates: None,
                }),
            ),
            Duration::from_millis(20),
        )
        .unwrap();
    assert!(matches!(
        &after_replace[0].body,
        Some(Body::EpaperRefreshResult(result))
            if result.active.as_ref().is_some_and(|state| state.presented_since_full_refresh == 1)
    ));
    session
        .handle(
            envelope(
                session_id,
                6,
                Body::Frame(frame(21, replacement_generation, 1, 0, 20)),
            ),
            Duration::from_millis(250),
        )
        .unwrap();
    session
        .handle(
            envelope(
                session_id,
                7,
                Body::SurfaceClose(SurfaceClose {
                    surface_id: 21,
                    generation: replacement_generation,
                    reason: 0,
                }),
            ),
            Duration::from_millis(260),
        )
        .unwrap();
    let while_closed = session
        .handle(
            envelope(
                session_id,
                8,
                Body::EpaperRefreshRequest(EpaperRefreshRequest {
                    request_id: 2,
                    operation: EpaperRefreshOperation::Query as i32,
                    partial_refresh_enabled: None,
                    cleanup_after_updates: None,
                    large_update_threshold_percent: None,
                    static_cleanup_after_fast_updates: None,
                }),
            ),
            Duration::from_millis(270),
        )
        .unwrap();
    assert!(matches!(
        &while_closed[0].body,
        Some(Body::EpaperRefreshResult(result))
            if result.active.as_ref().is_some_and(|state| state.presented_since_full_refresh == 2)
    ));

    let reopened = session
        .handle(
            envelope(session_id, 9, Body::SurfaceOpen(open(22))),
            Duration::from_millis(280),
        )
        .unwrap();
    let reopened_generation = reopened
        .iter()
        .find_map(|response| match &response.body {
            Some(Body::SurfaceReady(ready)) => Some(ready.generation),
            _ => None,
        })
        .expect("reopened SurfaceReady");
    session
        .handle(
            envelope(
                session_id,
                10,
                Body::Frame(frame(22, reopened_generation, 1, 0, 30)),
            ),
            Duration::from_millis(500),
        )
        .unwrap();
    drop(session);
    assert_eq!(panel.submissions().len(), 3);
    assert!(!panel.submissions()[0].refresh.complete_refresh);
    assert!(!panel.submissions()[1].refresh.complete_refresh);
    assert!(panel.submissions()[2].refresh.complete_refresh);
}

#[test]
fn surface_close_open_suppresses_old_contacts_until_release() {
    let mut receiver_config = config();
    receiver_config.input_device = Some(PathBuf::from("/dev/input/test"));
    let mut panel = MockPanel::new(4, 3);
    let mut session = Session::new(receiver_config, &mut panel);
    session
        .handle(envelope(0, 1, Body::ClientHello(hello())), Duration::ZERO)
        .unwrap();
    let session_id = session.session_id();
    let open = |surface_id| SurfaceOpen {
        surface_id,
        desired_width: 0,
        desired_height: 0,
        pixel_format: PixelFormat::Gray8 as i32,
        orientation: 0,
        source_kind: SourceKind::TestPattern as i32,
        input_capabilities: vec![InputCapability::Touch as i32],
        action_capabilities: Vec::new(),
        label: "gesture transition".into(),
    };
    let first = session
        .handle(
            envelope(session_id, 2, Body::SurfaceOpen(open(30))),
            Duration::ZERO,
        )
        .unwrap();
    let first_generation = match &first[0].body {
        Some(Body::SurfaceReady(ready)) => ready.generation,
        other => panic!("unexpected response: {other:?}"),
    };
    let downs = (1..=4)
        .map(|contact_id| PhysicalPointerEvent {
            phase: PointerPhase::Down,
            contact_id,
            x: contact_id,
            y: contact_id,
        })
        .collect();
    assert_eq!(
        session
            .input_reports(vec![downs], Duration::from_millis(1))
            .unwrap()
            .len(),
        1
    );
    session
        .handle(
            envelope(
                session_id,
                3,
                Body::SurfaceClose(SurfaceClose {
                    surface_id: 30,
                    generation: first_generation,
                    reason: 0,
                }),
            ),
            Duration::from_millis(2),
        )
        .unwrap();
    let second = session
        .handle(
            envelope(session_id, 4, Body::SurfaceOpen(open(31))),
            Duration::from_millis(3),
        )
        .unwrap();
    let second_generation = match &second[0].body {
        Some(Body::SurfaceReady(ready)) => ready.generation,
        other => panic!("unexpected response: {other:?}"),
    };
    assert!(session
        .input_reports(
            vec![vec![PhysicalPointerEvent {
                phase: PointerPhase::Move,
                contact_id: 1,
                x: 2,
                y: 2,
            }]],
            Duration::from_millis(4),
        )
        .unwrap()
        .is_empty());
    let ups = (1..=4)
        .map(|contact_id| PhysicalPointerEvent {
            phase: PointerPhase::Up,
            contact_id,
            x: contact_id,
            y: contact_id,
        })
        .collect();
    assert!(session
        .input_reports(vec![ups], Duration::from_millis(5))
        .unwrap()
        .is_empty());
    let fresh = session
        .input_reports(
            vec![vec![PhysicalPointerEvent {
                phase: PointerPhase::Down,
                contact_id: 9,
                x: 1,
                y: 1,
            }]],
            Duration::from_millis(6),
        )
        .unwrap();
    assert!(matches!(
        &fresh[0].body,
        Some(Body::InputBatch(batch))
            if batch.surface_id == 31
                && batch.generation == second_generation
                && batch.records.len() == 1
                && batch.records[0].phase == ProtocolPointerPhase::Down as i32
    ));
}

#[test]
fn power_key_menu_restores_base_and_exit_hit_closes_receiver() {
    let mut receiver_config = config();
    receiver_config.input_device = Some(PathBuf::from("/dev/input/test"));
    let mut panel = MockPanel::new(100, 300);
    {
        let mut session = Session::new(receiver_config.clone(), &mut panel);
        session
            .handle(envelope(0, 1, Body::ClientHello(hello())), Duration::ZERO)
            .unwrap();
        let session_id = session.session_id();
        session
            .handle(
                envelope(
                    session_id,
                    2,
                    Body::SurfaceOpen(SurfaceOpen {
                        surface_id: 40,
                        desired_width: 0,
                        desired_height: 0,
                        pixel_format: PixelFormat::Gray8 as i32,
                        orientation: 0,
                        source_kind: SourceKind::TestPattern as i32,
                        input_capabilities: vec![InputCapability::Touch as i32],
                        action_capabilities: Vec::new(),
                        label: "local menu".into(),
                    }),
                ),
                Duration::ZERO,
            )
            .unwrap();
        session.power_key_pressed(Duration::from_millis(1)).unwrap();
        session.power_key_pressed(Duration::from_millis(2)).unwrap();
        assert!(!session.is_closed());
    }
    assert_eq!(panel.submissions().len(), 2);
    assert!(panel.submissions()[0]
        .pixels
        .iter()
        .any(|pixel| *pixel != 255));
    assert!(panel.submissions()[1]
        .pixels
        .iter()
        .all(|pixel| *pixel == 255));

    let mut panel = MockPanel::new(100, 300);
    let mut session = Session::new(receiver_config, &mut panel);
    session
        .handle(envelope(0, 1, Body::ClientHello(hello())), Duration::ZERO)
        .unwrap();
    let session_id = session.session_id();
    session
        .handle(
            envelope(
                session_id,
                2,
                Body::SurfaceOpen(SurfaceOpen {
                    surface_id: 41,
                    desired_width: 0,
                    desired_height: 0,
                    pixel_format: PixelFormat::Gray8 as i32,
                    orientation: 0,
                    source_kind: SourceKind::TestPattern as i32,
                    input_capabilities: vec![InputCapability::Touch as i32],
                    action_capabilities: Vec::new(),
                    label: "local exit".into(),
                }),
            ),
            Duration::ZERO,
        )
        .unwrap();
    session.power_key_pressed(Duration::from_millis(1)).unwrap();
    session
        .input_reports(
            vec![vec![PhysicalPointerEvent {
                phase: PointerPhase::Up,
                contact_id: 1,
                x: 50,
                y: 245,
            }]],
            Duration::from_millis(2),
        )
        .unwrap();
    assert!(session.is_closed());
    assert!(session.receiver_exit_requested());
    drop(session);
    assert!(panel
        .submissions()
        .last()
        .expect("close app restores the base frame")
        .pixels
        .iter()
        .all(|pixel| *pixel == 255));
}

#[test]
fn power_key_menu_without_remote_surface_can_request_new_pair() {
    let mut panel = MockPanel::new(100, 300);
    let fallback = GraySurface::new(100, 300, 240).unwrap();
    let mut session = Session::new_with_fallback(config(), &mut panel, Some(fallback));

    session.power_key_pressed(Duration::from_millis(1)).unwrap();
    session
        .input_reports(
            vec![vec![PhysicalPointerEvent {
                phase: PointerPhase::Up,
                contact_id: 1,
                x: 50,
                y: 230,
            }]],
            Duration::from_millis(2),
        )
        .unwrap();

    assert!(session.is_closed());
    assert!(session.pairing_reset_requested());
    assert!(!session.receiver_exit_requested());
    drop(session);
    assert_eq!(panel.submissions().len(), 2);
    assert!(panel.submissions()[0]
        .pixels
        .iter()
        .any(|pixel| *pixel < 240));
    assert!(panel
        .submissions()
        .last()
        .unwrap()
        .pixels
        .iter()
        .all(|pixel| *pixel == 240));
}
