use bytes::BytesMut;
use flate2::{write::ZlibEncoder, Compression};
use prost::Message;
use rm_display_protocol::envelope;
use rm_display_protocol::semantic::{
    apply_validated_frame, raw_region, validate_and_decode_frame, SemanticError, SurfaceState,
};
use rm_display_protocol::wire::{WireCodec, WireError, HEADER_LEN, MAGIC};
use rm_display_protocol::{ContentClass, Envelope, Frame, FrameIntent, Ping, PixelFormat, Rect};
use std::io::Write;

fn ping_envelope() -> Envelope {
    Envelope {
        session_id: 0x0102_0304_0506_0708,
        message_id: 42,
        body: Some(envelope::Body::Ping(Ping {
            cookie: 0x1122_3344_5566_7788,
        })),
    }
}

fn decode_hex_fixture(value: &str) -> Vec<u8> {
    let compact = value.trim();
    assert_eq!(compact.len() % 2, 0);
    compact
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).unwrap();
            u8::from_str_radix(text, 16).unwrap()
        })
        .collect()
}

#[test]
fn epaper_profile_values_keep_original_wire_numbers_and_append_new_profiles() {
    use rm_display_protocol::EpaperProfile;

    assert_eq!(EpaperProfile::Animate as i32, 1);
    assert_eq!(EpaperProfile::Balanced as i32, 2);
    assert_eq!(EpaperProfile::Quality as i32, 3);
    assert_eq!(EpaperProfile::Realtime as i32, 4);
    assert_eq!(EpaperProfile::Reading as i32, 5);
    assert_eq!(EpaperProfile::Custom as i32, 6);
}

#[test]
fn custom_waveforms_are_backend_independent_and_exclude_full_refresh() {
    use rm_display_protocol::EpaperWaveform;

    assert_eq!(EpaperWaveform::Fastest as i32, 1);
    assert_eq!(EpaperWaveform::Fast as i32, 2);
    assert_eq!(EpaperWaveform::Quality as i32, 3);
    assert!(EpaperWaveform::try_from(4).is_err());
}

fn keyframe(frame_id: u64, pixels: Vec<u8>) -> Frame {
    Frame {
        surface_id: 1,
        generation: 1,
        frame_id,
        base_frame_id: 0,
        intent: FrameIntent::Settled as i32,
        content_class: ContentClass::TextUi as i32,
        regions: vec![raw_region(
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            pixels,
        )],
        source_timestamp_us: 0,
    }
}

fn surface(logical_frame_id: u64) -> SurfaceState {
    SurfaceState {
        surface_id: 1,
        generation: 1,
        width: 2,
        height: 2,
        pixel_format: PixelFormat::Gray8,
        logical_frame_id,
        max_regions: 8,
        max_frame_bytes: 1024,
    }
}

#[test]
fn ping_round_trip_and_every_split_point() {
    let codec = WireCodec::pre_handshake();
    let mut encoded = BytesMut::new();
    codec.encode(&ping_envelope(), &mut encoded).unwrap();
    assert_eq!(&encoded[..4], &MAGIC);
    assert_eq!(encoded.len(), HEADER_LEN + ping_envelope().encoded_len());

    for split in 0..=encoded.len() {
        let mut input = BytesMut::new();
        input.extend_from_slice(&encoded[..split]);
        let first = codec.decode(&mut input).unwrap();
        if split < encoded.len() {
            assert!(first.is_none(), "split {split} decoded prematurely");
            input.extend_from_slice(&encoded[split..]);
        }
        let decoded = first.or_else(|| codec.decode(&mut input).unwrap()).unwrap();
        assert_eq!(decoded, ping_envelope(), "split {split}");
        assert!(input.is_empty());
    }
}

#[test]
fn shared_rust_kotlin_ping_fixture_decodes() {
    let expected = ping_envelope();
    let fixture = decode_hex_fixture(include_str!("../../../protocol/fixtures/ping.rmd2.hex"));
    let mut input = BytesMut::from(fixture.as_slice());
    assert_eq!(
        WireCodec::pre_handshake().decode(&mut input).unwrap(),
        Some(expected)
    );
    assert!(input.is_empty());
}

#[test]
fn coalesced_messages_remain_separate() {
    let codec = WireCodec::pre_handshake();
    let mut bytes = BytesMut::new();
    codec.encode(&ping_envelope(), &mut bytes).unwrap();
    codec.encode(&ping_envelope(), &mut bytes).unwrap();
    assert!(codec.decode(&mut bytes).unwrap().is_some());
    assert!(codec.decode(&mut bytes).unwrap().is_some());
    assert!(bytes.is_empty());
}

#[test]
fn malformed_framing_is_rejected_before_allocation() {
    let codec = WireCodec::new(16);
    let mut wrong_magic = BytesMut::from(&b"MMIR\0\0\0\0"[..]);
    assert!(matches!(
        codec.decode(&mut wrong_magic),
        Err(WireError::InvalidMagic)
    ));

    let mut too_large = BytesMut::from(&b"RMD2\0\0\0\x11"[..]);
    assert!(matches!(
        codec.decode(&mut too_large),
        Err(WireError::PayloadTooLarge {
            actual: 17,
            limit: 16
        })
    ));
}

#[test]
fn keyframe_validates_and_applies_atomically() {
    let frame = keyframe(1, vec![0, 64, 128, 255]);
    let validated = validate_and_decode_frame(&frame, &surface(0)).unwrap();
    let mut pixels = vec![255; 4];
    apply_validated_frame(&mut pixels, 2, &validated);
    assert_eq!(pixels, [0, 64, 128, 255]);
}

#[test]
fn rgb565_keyframe_uses_two_bytes_per_pixel() {
    let pixels = vec![0x00, 0xf8, 0xe0, 0x07, 0x1f, 0x00, 0xff, 0xff];
    let frame = keyframe(1, pixels.clone());
    let mut rgb_surface = surface(0);
    rgb_surface.pixel_format = PixelFormat::Rgb565Le;
    let validated = validate_and_decode_frame(&frame, &rgb_surface).unwrap();
    assert_eq!(validated.decoded_bytes, 8);
    assert_eq!(validated.pixel_format, PixelFormat::Rgb565Le);
    let mut output = vec![0; 8];
    apply_validated_frame(&mut output, 2, &validated);
    assert_eq!(output, pixels);

    let short = keyframe(2, vec![0; 4]);
    assert_eq!(
        validate_and_decode_frame(&short, &rgb_surface),
        Err(SemanticError::BadDecodedLength)
    );
}

#[test]
fn bad_crc_and_bad_base_do_not_modify_surface() {
    let mut frame = keyframe(1, vec![0, 64, 128, 255]);
    frame.regions[0].decoded_crc32 ^= 1;
    assert_eq!(
        validate_and_decode_frame(&frame, &surface(0)),
        Err(SemanticError::BadCrc)
    );

    let mut delta = keyframe(2, vec![1, 2, 3, 4]);
    delta.base_frame_id = 7;
    assert_eq!(
        validate_and_decode_frame(&delta, &surface(1)),
        Err(SemanticError::BadBase {
            expected: 1,
            actual: 7
        })
    );
}

#[test]
fn keyframe_must_cover_the_surface_once() {
    let mut frame = keyframe(1, vec![0, 1, 2, 3]);
    frame.regions[0].rect.as_mut().unwrap().width = 1;
    frame.regions[0].decoded_len = 2;
    frame.regions[0].data = vec![0, 1].into();
    frame.regions[0].decoded_crc32 = crc32fast::hash(&[0, 1]);
    assert_eq!(
        validate_and_decode_frame(&frame, &surface(0)),
        Err(SemanticError::BadKeyframe)
    );
}

#[test]
fn zlib_keyframe_uses_the_same_crc_semantics() {
    let pixels = vec![0, 64, 128, 255];
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&pixels).unwrap();
    let compressed = encoder.finish().unwrap();
    let mut frame = keyframe(1, pixels.clone());
    frame.regions[0].encoding = rm_display_protocol::Encoding::Zlib as i32;
    frame.regions[0].data = compressed.into();

    let validated = validate_and_decode_frame(&frame, &surface(0)).unwrap();
    assert_eq!(validated.regions[0].pixels, pixels);
}

#[test]
fn zstd_keyframe_uses_the_same_crc_semantics() {
    let pixels = vec![0, 64, 128, 255];
    let compressed = zstd::stream::encode_all(pixels.as_slice(), 1).unwrap();
    let mut frame = keyframe(1, pixels.clone());
    frame.regions[0].encoding = rm_display_protocol::Encoding::Zstd as i32;
    frame.regions[0].data = compressed.into();

    let validated = validate_and_decode_frame(&frame, &surface(0)).unwrap();
    assert_eq!(validated.regions[0].pixels, pixels);
}

#[test]
fn overlapping_delta_regions_are_rejected() {
    let mut frame = keyframe(2, vec![0; 4]);
    frame.base_frame_id = 1;
    frame.regions = vec![
        raw_region(
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            },
            vec![1, 2],
        ),
        raw_region(
            Rect {
                x: 1,
                y: 0,
                width: 1,
                height: 2,
            },
            vec![3, 4],
        ),
    ];
    assert_eq!(
        validate_and_decode_frame(&frame, &surface(1)),
        Err(SemanticError::RegionOverlap)
    );
}

#[test]
fn out_of_bounds_region_is_rejected() {
    let mut frame = keyframe(2, vec![0; 4]);
    frame.base_frame_id = 1;
    frame.regions = vec![raw_region(
        Rect {
            x: 2,
            y: 0,
            width: 1,
            height: 1,
        },
        vec![7],
    )];
    assert_eq!(
        validate_and_decode_frame(&frame, &surface(1)),
        Err(SemanticError::BadRegion)
    );
}
