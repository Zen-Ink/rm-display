//! Operational loopback protocol replay with synthetic digitizer input (no hardware).
use bytes::BytesMut;
use rm_display_core::{MockPanel, RefreshPolicyConfig};
use rm_display_protocol::{envelope::Body, semantic::raw_region, wire::WireCodec, *};
use rm_display_receiver::{
    ReceiverConfig, ReceiverLimits, ReservedZeroToken, SecurityMode, Session,
};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::Arc,
    time::Duration,
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
fn send(stream: &mut TcpStream, codec: &WireCodec, message: Envelope) -> Result<()> {
    let mut bytes = BytesMut::new();
    codec.encode(&message, &mut bytes)?;
    stream.write_all(&bytes)?;
    Ok(())
}
fn receive(stream: &mut TcpStream, codec: &WireCodec) -> Result<Envelope> {
    let mut bytes = BytesMut::new();
    loop {
        if let Some(message) = codec.decode(&mut bytes)? {
            return Ok(message);
        }
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        bytes.extend_from_slice(&byte);
    }
}
fn exchange(
    stream: &mut TcpStream,
    codec: &WireCodec,
    id: &mut u32,
    session_id: u64,
    body: Body,
) -> Result<Envelope> {
    *id += 1;
    send(
        stream,
        codec,
        Envelope {
            session_id,
            message_id: *id,
            body: Some(body),
        },
    )?;
    receive(stream, codec)
}
fn main() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let client = std::thread::spawn(move || -> std::result::Result<(), String> {
        client(address).map_err(|e| e.to_string())
    });
    let (mut stream, _) = listener.accept()?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let codec = WireCodec::pre_handshake();
    let config = ReceiverConfig {
        listen: address,
        security: SecurityMode::Plaintext,
        token_verifier: Arc::new(ReservedZeroToken),
        server_id: [1; 16],
        name: "overlay smoke".into(),
        limits: ReceiverLimits::default(),
        refresh_policy: RefreshPolicyConfig::default(),
        input_device: Some("synthetic-touch".into()),
    };
    let mut panel = MockPanel::new(32, 32);
    {
        let mut session = Session::new(config, &mut panel);
        session.set_pen_available(true); // Synthetic input driver for this executable only.
        let mut now = Duration::ZERO;
        loop {
            now += Duration::from_secs(2);
            let request = receive(&mut stream, &codec)?;
            let inject =
                matches!(request.body,Some(Body::OverlayUpdate(ref u)) if u.local_ink==Some(true));
            for response in session.handle(request, now)? {
                send(&mut stream, &codec, response)?;
            }
            for response in session.poll(now + Duration::from_secs(1))? {
                send(&mut stream, &codec, response)?;
            }
            if inject {
                let pen = PointerRecord {
                    device: PointerDevice::Pen as i32,
                    phase: PointerPhase::Down as i32,
                    x_16_16: 16 << 16,
                    y_16_16: 16 << 16,
                    pressure: 32000,
                    buttons: 1,
                    ..Default::default()
                };
                for response in
                    session.pen_reports(vec![vec![pen.clone()]], now + Duration::from_secs(1))?
                {
                    send(&mut stream, &codec, response)?;
                }
                use rm_display_receiver::evdev::{
                    PhysicalPointerEvent, PointerPhase as TouchPhase,
                };
                let touch = PhysicalPointerEvent {
                    phase: TouchPhase::Down,
                    contact_id: 8,
                    x: 20,
                    y: 20,
                };
                for response in session.input_reports(vec![vec![touch]], now)? {
                    send(&mut stream, &codec, response)?;
                }
                for response in session.power_key_pressed(now)? {
                    send(&mut stream, &codec, response)?;
                }
                // Physical releases consumed by the menu must not leave stale
                // touch suppression or leak a pen continuation when it closes.
                session.input_reports(
                    vec![vec![PhysicalPointerEvent {
                        phase: TouchPhase::Cancel,
                        ..touch
                    }]],
                    now,
                )?;
                let mut stale = pen.clone();
                stale.phase = PointerPhase::Move as i32;
                if !session
                    .pen_reports(vec![vec![stale.clone()]], now)?
                    .is_empty()
                {
                    return Err("menu leaked pen input".into());
                }
                session.power_key_pressed(now)?;
                if !session.pen_reports(vec![vec![stale]], now)?.is_empty() {
                    return Err("menu close leaked stale pen MOVE".into());
                }
                for response in session.pen_reports(vec![vec![pen]], now)? {
                    send(&mut stream, &codec, response)?;
                }
                for response in session.input_reports(vec![vec![touch]], now)? {
                    send(&mut stream, &codec, response)?;
                }
            }
            if session.is_closed() {
                break;
            }
        }
    }
    client
        .join()
        .map_err(|_| "client panicked")?
        .map_err(|e| format!("client: {e}"))?;
    if !panel
        .submissions()
        .iter()
        .any(|s| s.pixels[16 * 32 + 16] == 0)
    {
        return Err("local ink did not reach panel".into());
    }
    if panel
        .submissions()
        .last()
        .ok_or("no display submissions")?
        .pixels
        .iter()
        .any(|p| *p != 210)
    {
        return Err("clear did not restore the new base".into());
    }
    println!("overlay/pen loopback: negotiated v2.3, patch, stale rejection, frozen snapshot, frame rejection, release/keyframe, independent clear, menu cancellation/restart OK ({} panel submissions)",panel.submissions().len());
    Ok(())
}
fn client(address: std::net::SocketAddr) -> Result<()> {
    let mut stream = TcpStream::connect(address)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let codec = WireCodec::pre_handshake();
    let mut id = 0;
    let hello = exchange(
        &mut stream,
        &codec,
        &mut id,
        0,
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
            name: "smoke".into(),
        }),
    )?;
    let session = hello.session_id;
    if !matches!(hello.body,Some(Body::ServerHello(ref h)) if h.selected_minor==3 && h.features.contains(&15))
    {
        return Err("negotiation failed".into());
    }
    exchange(
        &mut stream,
        &codec,
        &mut id,
        session,
        Body::SurfaceOpen(SurfaceOpen {
            surface_id: 1,
            pixel_format: PixelFormat::Gray8 as i32,
            input_capabilities: vec![InputCapability::Pen as i32, InputCapability::Touch as i32],
            ..Default::default()
        }),
    )?;
    let frame = |id, luma| Frame {
        surface_id: 1,
        generation: 1,
        frame_id: id,
        intent: FrameIntent::Settled as i32,
        content_class: ContentClass::TextUi as i32,
        regions: vec![raw_region(
            Rect {
                x: 0,
                y: 0,
                width: 32,
                height: 32,
            },
            vec![luma; 1024],
        )],
        ..Default::default()
    };
    exchange(
        &mut stream,
        &codec,
        &mut id,
        session,
        Body::Frame(frame(1, 240)),
    )?;
    let overlay = |sequence| OverlayUpdate {
        surface_id: 1,
        generation: 1,
        sequence,
        ..Default::default()
    };
    let mut patch = overlay(1);
    patch.rect = Some(Rect {
        x: 0,
        y: 0,
        width: 1,
        height: 1,
    });
    patch.luma = vec![0].into();
    patch.alpha = vec![255].into();
    if !matches!(exchange(&mut stream, &codec, &mut id, session, Body::OverlayUpdate(patch.clone()))?.body,Some(Body::OverlayResult(ref r)) if r.applied)
    {
        return Err("patch failed".into());
    }
    if !matches!(exchange(&mut stream, &codec, &mut id, session, Body::OverlayUpdate(patch))?.body,Some(Body::OverlayResult(ref r)) if !r.applied)
    {
        return Err("stale overlay accepted".into());
    }
    let mut arm = overlay(2);
    arm.local_ink = Some(true);
    exchange(
        &mut stream,
        &codec,
        &mut id,
        session,
        Body::OverlayUpdate(arm),
    )?;
    // A server-originated pen batch precedes the next command response.
    let pen = receive(&mut stream, &codec)?;
    if !matches!(pen.body,Some(Body::InputBatch(ref b)) if b.presented_frame_id==1 && b.ink_frozen && b.records[0].device==2)
    {
        return Err("pen snapshot binding failed".into());
    }
    for (device, phase) in [
        (PointerDevice::Touch, PointerPhase::Down),
        (PointerDevice::Touch, PointerPhase::Cancel),
        (PointerDevice::Pen, PointerPhase::Cancel),
        (PointerDevice::Pen, PointerPhase::Down),
        (PointerDevice::Touch, PointerPhase::Down),
    ] {
        if !matches!(receive(&mut stream,&codec)?.body, Some(Body::InputBatch(ref batch)) if batch.records.len()==1 && batch.records[0].device==device as i32 && batch.records[0].phase==phase as i32)
        {
            return Err("menu pointer cancellation/restart failed".into());
        }
    }
    if !matches!(exchange(&mut stream, &codec, &mut id, session, Body::Frame(frame(2,100)))?.body,Some(Body::FrameResult(ref r)) if r.reason==FrameResultReason::InkFrozen as i32)
    {
        return Err("frozen frame accepted".into());
    }
    let mut release = overlay(3);
    release.local_ink = Some(false);
    release.clear = true;
    exchange(
        &mut stream,
        &codec,
        &mut id,
        session,
        Body::OverlayUpdate(release),
    )?;
    exchange(
        &mut stream,
        &codec,
        &mut id,
        session,
        Body::Frame(frame(3, 210)),
    )?;
    id += 1;
    send(
        &mut stream,
        &codec,
        Envelope {
            session_id: session,
            message_id: id,
            body: Some(Body::Goodbye(Goodbye::default())),
        },
    )?;
    Ok(())
}
