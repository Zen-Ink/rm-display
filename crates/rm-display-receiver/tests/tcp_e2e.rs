use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;

use bytes::BytesMut;
use rm_display_core::{MockPanel, RefreshPolicyConfig};
use rm_display_protocol::envelope::Body;
use rm_display_protocol::wire::WireCodec;
use rm_display_protocol::{
    ClientHello, Encoding, Envelope, EpaperProfile, EpaperProfileOperation, EpaperProfileRequest,
    EpaperProfileResultCode, EpaperRefreshOperation, EpaperRefreshRequest, EpaperRefreshResultCode,
    Goodbye, PixelFormat, ProducerKind, ProtocolFeature, SourceKind, SurfaceOpen,
};
use rm_display_receiver::{
    ReceiverConfig, ReceiverLimits, ReceiverServer, ReservedZeroToken, SecurityMode,
};
use rm_display_transport::{Psk, PskClientConfig};

const TEST_PSK: [u8; 32] = [0x5a; 32];

#[test]
fn plaintext_loopback_server_exchanges_framed_hello_and_surface() {
    let config = ReceiverConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        security: SecurityMode::Plaintext,
        token_verifier: Arc::new(ReservedZeroToken),
        server_id: [7; 16],
        name: "tcp e2e".into(),
        limits: ReceiverLimits::default(),
        refresh_policy: RefreshPolicyConfig::default(),
        input_device: None,
    };
    let mut server = ReceiverServer::bind(config, Box::new(MockPanel::new(4, 3))).unwrap();
    assert!(server.input_status().contains("touch input disabled"));
    let address = server.local_addr().unwrap();

    let client = thread::spawn(move || {
        let stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        exchange(stream);
    });

    server.run_one().unwrap();
    client.join().unwrap();
}

#[test]
fn psk_server_exchanges_protocol_over_tls13_aes128_gcm() {
    let config = ReceiverConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        security: SecurityMode::Psk(Psk::from_bytes(TEST_PSK)),
        token_verifier: Arc::new(ReservedZeroToken),
        server_id: [8; 16],
        name: "psk tcp e2e".into(),
        limits: ReceiverLimits::default(),
        refresh_policy: RefreshPolicyConfig::default(),
        input_device: None,
    };
    let mut server = ReceiverServer::bind(config, Box::new(MockPanel::new(4, 3))).unwrap();
    let address = server.local_addr().unwrap();

    let client = thread::spawn(move || {
        let tcp = TcpStream::connect(address).unwrap();
        tcp.set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        let client = PskClientConfig::new(Psk::from_bytes(TEST_PSK)).unwrap();
        exchange(client.connect(tcp).unwrap());
    });

    server.run_one().unwrap();
    client.join().unwrap();
}

fn exchange<T: Read + Write>(mut stream: T) {
    let mut codec = WireCodec::pre_handshake();
    send(
        &mut stream,
        &codec,
        Envelope {
            session_id: 0,
            message_id: 1,
            body: Some(Body::ClientHello(hello())),
        },
    );
    let server_hello = receive(&mut stream, &codec);
    let session_id = server_hello.session_id;
    assert!(session_id != 0);
    assert!(matches!(
        server_hello.body,
        Some(Body::ServerHello(ref hello))
            if hello.features.contains(&(ProtocolFeature::EpaperProfileControl as i32))
                && hello.features.contains(&(ProtocolFeature::EpaperRefreshControl as i32))
    ));
    codec.set_max_payload(8 * 1024 * 1024);

    send(
        &mut stream,
        &codec,
        Envelope {
            session_id,
            message_id: 2,
            body: Some(Body::EpaperProfileRequest(EpaperProfileRequest {
                request_id: 1,
                operation: EpaperProfileOperation::Set as i32,
                requested_profile: EpaperProfile::Reading as i32,
                custom: None,
            })),
        },
    );
    let profile = receive(&mut stream, &codec);
    assert!(matches!(
        profile.body,
        Some(Body::EpaperProfileResult(ref result))
            if result.result == EpaperProfileResultCode::Applied as i32
                && result.cleanup_pending
                && result.active.as_ref().is_some_and(|state| {
                    state.profile == EpaperProfile::Reading as i32
                        && state.cleanup_after_updates == 45
                        && state.large_update_threshold_percent == 50
                })
    ));

    send(
        &mut stream,
        &codec,
        Envelope {
            session_id,
            message_id: 3,
            body: Some(Body::EpaperRefreshRequest(EpaperRefreshRequest {
                request_id: 1,
                operation: EpaperRefreshOperation::Update as i32,
                partial_refresh_enabled: Some(false),
                cleanup_after_updates: Some(0),
                large_update_threshold_percent: Some(25),
                static_cleanup_after_fast_updates: Some(2),
            })),
        },
    );
    let refresh = receive(&mut stream, &codec);
    assert!(matches!(
        refresh.body,
        Some(Body::EpaperRefreshResult(ref result))
            if result.result == EpaperRefreshResultCode::Applied as i32
                && result.active.as_ref().is_some_and(|state|
                    !state.partial_refresh_enabled
                        && state.cleanup_after_updates == 0
                        && state.large_update_threshold_percent == 25)
    ));

    send(
        &mut stream,
        &codec,
        Envelope {
            session_id,
            message_id: 4,
            body: Some(Body::SurfaceOpen(SurfaceOpen {
                surface_id: 11,
                desired_width: 0,
                desired_height: 0,
                pixel_format: PixelFormat::Gray8 as i32,
                orientation: 0,
                source_kind: SourceKind::TestPattern as i32,
                input_capabilities: Vec::new(),
                action_capabilities: Vec::new(),
                label: "tcp".into(),
            })),
        },
    );
    let ready = receive(&mut stream, &codec);
    assert!(matches!(
        ready.body,
        Some(Body::SurfaceReady(ref ready)) if ready.surface_id == 11 && ready.width == 4 && ready.height == 3
    ));

    send(
        &mut stream,
        &codec,
        Envelope {
            session_id,
            message_id: 5,
            body: Some(Body::Goodbye(Goodbye {
                reason: 0,
                message: "done".into(),
            })),
        },
    );
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
            ProtocolFeature::EpaperProfileControl,
            ProtocolFeature::EpaperRefreshControl,
        ]
        .map(|feature| feature as i32)
        .to_vec(),
        pixel_formats: vec![PixelFormat::Gray8 as i32],
        encodings: vec![Encoding::Raw as i32],
        client_id: vec![1; 16].into(),
        token: vec![0; 32].into(),
        client_nonce: vec![3; 16].into(),
        name: "tcp test".into(),
    }
}

fn send<T: Write>(stream: &mut T, codec: &WireCodec, envelope: Envelope) {
    let mut output = BytesMut::new();
    codec.encode(&envelope, &mut output).unwrap();
    stream.write_all(&output).unwrap();
}

fn receive<T: Read>(stream: &mut T, codec: &WireCodec) -> Envelope {
    let mut input = BytesMut::new();
    let mut buffer = [0_u8; 4096];
    loop {
        if let Some(envelope) = codec.decode(&mut input).unwrap() {
            return envelope;
        }
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0, "receiver closed before sending an envelope");
        input.extend_from_slice(&buffer[..count]);
    }
}
