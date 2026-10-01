//! Runnable application API replay; no device required.
use bytes::BytesMut;
use rm_display_core::{LocalOverlay, MockPanel, RefreshPolicyConfig, Waveform};
use rm_display_protocol::{envelope::Body, semantic::raw_region, wire::WireCodec, *};
use rm_display_receiver::{
    ReceiverConfig, ReceiverLimits, ReceiverServer, ReservedZeroToken, SecurityMode, Session,
};
use std::{
    io::Write,
    net::TcpStream,
    sync::{mpsc, Arc},
    time::Duration,
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
fn config() -> ReceiverConfig {
    ReceiverConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        security: SecurityMode::Plaintext,
        token_verifier: Arc::new(ReservedZeroToken),
        server_id: [1; 16],
        name: "application API replay".into(),
        limits: ReceiverLimits::default(),
        refresh_policy: RefreshPolicyConfig::default(),
        ink_waveform: Waveform::Fastest,
        input_device: None,
    }
}
fn hello() -> Body {
    Body::ClientHello(ClientHello {
        min_minor: 3,
        max_minor: 3,
        producer_kind: ProducerKind::LinuxCli as i32,
        features: vec![1, 2, 3, 4, 5, 9, 12, 13, 14, 15],
        pixel_formats: vec![PixelFormat::Gray8 as i32],
        encodings: vec![Encoding::Raw as i32],
        client_id: vec![1; 16].into(),
        token: vec![0; 32].into(),
        client_nonce: vec![2; 16].into(),
        name: "API replay".into(),
    })
}
fn open() -> Body {
    Body::SurfaceOpen(SurfaceOpen {
        surface_id: 1,
        pixel_format: PixelFormat::Gray8 as i32,
        input_capabilities: vec![InputCapability::Pen as i32],
        ..Default::default()
    })
}
fn frame(id: u64, base: u64) -> Body {
    Body::Frame(Frame {
        surface_id: 1,
        generation: 1,
        frame_id: id,
        base_frame_id: base,
        intent: FrameIntent::Settled as i32,
        regions: vec![raw_region(
            Rect {
                x: 0,
                y: 0,
                width: 33,
                height: 33,
            },
            vec![255; 1089],
        )],
        ..Default::default()
    })
}
fn send(session: &mut Session<'_>, id: &mut u32, body: Body, second: u64) -> Result<Vec<Envelope>> {
    *id += 1;
    Ok(session.handle(
        Envelope {
            session_id: if *id == 1 { 0 } else { session.session_id() },
            message_id: *id,
            body: Some(body),
        },
        Duration::from_secs(second),
    )?)
}
fn main() -> Result<()> {
    // Coverage uses continuous rows, including non-byte-aligned row boundaries.
    let mut overlay = LocalOverlay::transparent(3, 3)?;
    overlay.patch(
        3,
        &Rect {
            x: 2,
            y: 0,
            width: 1,
            height: 3,
        },
        &[0; 3],
        &[255; 3],
    )?;
    assert_eq!(overlay.coverage_bitmap(), vec![0x24, 0x80]);
    let mut panel = MockPanel::new(33, 33);
    let mut session = Session::new(config(), &mut panel);
    session.set_pen_available(true);
    session.set_freeze_on_pen_down(false);
    session.set_automatic_idle_cleanup(false);
    assert!(session.ink_snapshot().is_err());
    assert!(session.set_frame_frozen(true, Duration::ZERO).is_err());
    let mut id = 0;
    send(&mut session, &mut id, hello(), 0)?;
    send(&mut session, &mut id, open(), 0)?;
    assert!(session.set_frame_frozen(true, Duration::ZERO).is_err());
    send(&mut session, &mut id, frame(1, 0), 1)?;
    session.poll(Duration::from_secs(2))?;
    send(
        &mut session,
        &mut id,
        Body::OverlayUpdate(OverlayUpdate {
            surface_id: 1,
            generation: 1,
            sequence: 1,
            local_ink: Some(true),
            ..Default::default()
        }),
        2,
    )?;
    let pen = PointerRecord {
        device: PointerDevice::Pen as i32,
        phase: PointerPhase::Down as i32,
        x_16_16: 16 << 16,
        y_16_16: 16 << 16,
        buttons: 1,
        pressure: 1000,
        ..Default::default()
    };
    session.pen_reports(vec![vec![pen.clone()]], Duration::from_secs(3))?;
    let first = session.ink_snapshot()?;
    assert!(!first.frozen && first.presented_frame_id == 1);
    assert_eq!(first.bitmap.len(), 137);
    assert!(first.bitmap.iter().any(|b| *b != 0));
    session.pen_reports(
        vec![vec![PointerRecord {
            phase: PointerPhase::Move as i32,
            x_16_16: 30 << 16,
            y_16_16: 30 << 16,
            ..pen.clone()
        }]],
        Duration::from_secs(4),
    )?;
    assert_ne!(first.bitmap, session.ink_snapshot()?.bitmap); // Export did not seal.
    session.set_ink_paused(true, Duration::from_secs(5))?;
    let paused = session.ink_snapshot()?;
    session.pen_reports(
        vec![vec![PointerRecord {
            flags: 1,
            ..pen.clone()
        }]],
        Duration::from_secs(6),
    )?;
    assert_eq!(paused, session.ink_snapshot()?);
    session.set_frame_frozen(true, Duration::from_secs(7))?;
    let frozen = session.ink_snapshot()?;
    assert!(frozen.frozen);
    let rejected = send(&mut session, &mut id, frame(2, 1), 8)?;
    assert!(rejected.iter().any(|e| matches!(&e.body, Some(Body::FrameResult(r)) if r.reason == FrameResultReason::InkFrozen as i32)));
    let debt = session.current_refresh_state().presented_since_full_refresh;
    assert!(debt > 0 && session.last_partial_at().is_some());
    session.poll(Duration::from_secs(100))?;
    assert_eq!(
        session.current_refresh_state().presented_since_full_refresh,
        debt
    );
    let (report, _) = session.request_cleanup(Duration::from_secs(101))?;
    assert!(report.cleanup_performed && !report.backend_failed);
    assert_eq!(
        session.current_refresh_state().presented_since_full_refresh,
        0
    );
    assert_eq!(frozen, session.ink_snapshot()?); // Cleanup preserves the draft.
    session.set_ink_paused(false, Duration::from_secs(102))?;
    session.pen_reports(vec![vec![pen.clone()]], Duration::from_secs(6))?;
    session.pen_reports(
        vec![vec![PointerRecord {
            phase: PointerPhase::Move as i32,
            flags: 1,
            ..pen.clone()
        }]],
        Duration::from_secs(103),
    )?;
    assert_eq!(frozen, session.ink_snapshot()?); // Stale DOWN/continuation suppressed.
    session.pen_reports(
        vec![vec![PointerRecord { flags: 1, ..pen }]],
        Duration::from_secs(104),
    )?;
    assert_ne!(frozen.bitmap, session.ink_snapshot()?.bitmap); // Fresh DOWN resumes drawing.
    session.clear_ink(Duration::from_secs(105))?;
    assert!(session.ink_snapshot()?.bitmap.iter().all(|b| *b == 0));
    assert!(session.ink_snapshot()?.frozen);
    session.set_frame_frozen(false, Duration::from_secs(106))?;
    let delta = send(&mut session, &mut id, frame(3, 1), 107)?;
    assert!(delta.iter().any(|e| matches!(&e.body, Some(Body::FrameResult(r)) if r.result == FrameResultCode::NeedKeyframe as i32)));
    send(&mut session, &mut id, frame(4, 0), 108)?;
    assert_eq!(session.ink_snapshot()?.presented_frame_id, 4);
    let Body::Frame(mut pending) = frame(5, 4) else {
        unreachable!()
    };
    pending.intent = FrameIntent::Latest as i32;
    pending.regions[0] = raw_region(pending.regions[0].rect.clone().unwrap(), vec![180; 1089]);
    send(&mut session, &mut id, Body::Frame(pending), 108)?;
    let cancelled = session.set_frame_frozen(true, Duration::from_secs(108))?;
    assert!(cancelled.iter().any(|e| matches!(&e.body, Some(Body::FrameResult(r)) if r.frame_id == 5 && r.reason == FrameResultReason::InkFrozen as i32)));
    assert!(cancelled.iter().any(|e| matches!(&e.body, Some(Body::InputBatch(b)) if b.ink_frozen && b.presented_frame_id == 4)));
    session.poll(Duration::from_secs(109))?;
    assert_eq!(session.ink_snapshot()?.presented_frame_id, 4);
    drop(session);
    // Actual server hook: initialize policy, receive an application request via
    // a channel, and export a snapshot without touching the panel externally.
    let mut server = ReceiverServer::bind(config(), Box::new(MockPanel::new(33, 33)))?;
    let address = server.local_addr()?;
    let (request_tx, request_rx) = mpsc::channel();
    let (snapshot_tx, snapshot_rx) = mpsc::channel();
    let mut pending = false;
    server.set_session_hook(move |session, _now| {
        session.set_freeze_on_pen_down(false);
        session.set_automatic_idle_cleanup(false);
        pending |= request_rx.try_recv().is_ok();
        if pending {
            if let Ok(snapshot) = session.ink_snapshot() {
                snapshot_tx.send(snapshot).unwrap();
                pending = false;
            }
        }
        Ok(Vec::new())
    });
    let client = std::thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let codec = WireCodec::pre_handshake();
        let mut encoded = BytesMut::new();
        codec
            .encode(
                &Envelope {
                    session_id: 0,
                    message_id: 1,
                    body: Some(hello()),
                },
                &mut encoded,
            )
            .unwrap();
        stream.write_all(&encoded).unwrap();
        use std::io::Read;
        let mut input = BytesMut::new();
        let hello = loop {
            if let Some(envelope) = codec.decode(&mut input).unwrap() {
                break envelope;
            }
            let mut buffer = [0; 4096];
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0);
            input.extend_from_slice(&buffer[..count]);
        };
        encoded.clear();
        codec
            .encode(
                &Envelope {
                    session_id: hello.session_id,
                    message_id: 2,
                    body: Some(open()),
                },
                &mut encoded,
            )
            .unwrap();
        stream.write_all(&encoded).unwrap();
        request_tx.send(()).unwrap();
        let snapshot = snapshot_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!((snapshot.width, snapshot.height), (33, 33));
        assert!(!snapshot.frozen && snapshot.bitmap.iter().all(|b| *b == 0));
        // Consume SurfaceReady before shutting down, avoiding a TCP reset with
        // unread server responses.
        loop {
            if codec.decode(&mut input).unwrap().is_some() {
                break;
            }
            let mut buffer = [0; 4096];
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0);
            input.extend_from_slice(&buffer[..count]);
        }
        stream.shutdown(std::net::Shutdown::Write).unwrap();
    });
    server.run_one()?;
    client.join().unwrap();
    println!("application API: live ink, pure snapshot, pause, freeze/resume, keyframe, explicit cleanup, stale input, server hook OK");
    Ok(())
}
