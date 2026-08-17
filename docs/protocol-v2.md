# rm-display protocol v2.0 / v2.1 / v2.2

Status: **frozen base semantics with negotiated, wire-compatible optional
extensions** for the receiver, Android producer, and Linux CLI.

The normative message schema is
[`protocol/rm_display/v2/rm_display.proto`](../protocol/rm_display/v2/rm_display.proto).
Rust and Android code must be generated from that file; handwritten duplicate
message classes are not permitted.

This document defines transport and semantic rules that proto3 cannot express.

## 1. Boundary

The protocol transports rendered pixels and generic input:

- a producer owns application state and renders pixels;
- the receiver owns the remote base surface, local overlays, input collection,
  e-paper waveform selection, damage calculation, and ghost cleanup;
- browser URL/tab state, Android activities, Quill framebuffer formats, native
  Quill modes, and force-refresh commands never cross the protocol boundary;
- one connection contains one authenticated producer session and one active
  surface in v2.0.

Android may switch between its screen-mirror and browser producers by closing
the old surface and opening a new one. The browser address bar is part of the
browser producer's pixels; a receiver overlay may invoke a declared action or
send text but never stores the URL.

## 2. Transport

Transport uses one full-duplex TCP connection on default port `7420`. Control,
frame, result, input, text, and action messages all share that connection.
There are no auxiliary video ports and no UDP hole punching.

The connection mode is configured out of band and is one of:

- `plain`: the RMD2 stream is carried directly over TCP;
- `psk-aes-128-gcm`: TCP is wrapped in TLS 1.3 external-PSK records using only
  `TLS_AES_128_GCM_SHA256` and the `psk_ke` key exchange mode.

There is no in-band mode detection and no fallback from PSK to plaintext. A
mode mismatch fails the connection. The transport format does not prescribe a
deployment default. The reference receiver defaults to a managed PSK delivered
by its pairing-v2 QR; `--plaintext` is an explicit operator choice.

Each protobuf envelope is framed as:

```text
magic          4 bytes   ASCII "RMD2"
protobuf_len   u32be
envelope       protobuf_len bytes, rm_display.v2.Envelope
```

The outer frame is deliberately the only handwritten wire structure.

The receiver validates magic and length before allocation. Before successful
`ClientHello`, the payload limit is 64 KiB. Afterwards, the smaller of the
negotiated and hard implementation limits applies. The initial hard cap is
32 MiB.

`Envelope.message_id` begins at 1 and strictly increases independently in each
direction. `ClientHello` uses `session_id = 0`; `ServerHello` assigns a fresh,
random, nonzero session ID that every later envelope must match. Reconnect
always creates a new session, invalidates all surfaces and delta bases, and
requires a keyframe. v2.0 has no resume.

Unknown protobuf fields are ignored according to proto3 rules. An unknown
`Envelope.oneof` body, an illegal message direction, a missing body, or invalid
session/order is a protocol error. Removed field numbers are reserved forever.

Every protobuf enum except a true `NONE` result/reason reserves numeric zero as
`UNSPECIFIED`. Proto3 uses the first enum value when a scalar field is absent,
so this sentinel lets validation distinguish an omitted required semantic value
from a deliberately selected value while keeping an absent field safely
decodable. `UNSPECIFIED` is not an advertised capability. Required fields reject
it; advisory fields such as `SourceKind` may accept it.

## 3. Security

PSK mode uses one uniformly random 32-byte pre-shared key provisioned before
the transport handshake. The reference receiver normally generates it,
persists it in a mode-0600 state file, and delivers it out of band in a local
pairing-v2 QR. An explicit key path is also supported for unattended
deployments. The TLS external identity is the
ASCII string `rm-display-v2`. Both peers
negotiate only TLS 1.3, `TLS_AES_128_GCM_SHA256`, and `psk_ke`. A successful
session must report no negotiated DH/ECDH group. It uses no certificate or
asymmetric secret in traffic-key derivation, and disables TLS 0-RTT, session
tickets, and renegotiation. This keeps AES-GCM record nonces, ordering,
transcript authentication, and Finished verification in a standard protocol
without requiring a public-key infrastructure.

The Android producer advertises only `psk_ke` and emits no key share. OpenSSL
versions before 3.5 cannot suppress an unused client key-share extension
through their public API. The Rust profile therefore configures client and
server with disjoint group lists and rejects the connection after the handshake
unless OpenSSL reports negotiated group zero. Such an extension, if emitted,
does not participate in key derivation.

In the cipher-suite name, `SHA256` is the TLS handshake HKDF/transcript hash;
it is not used to encrypt frame payloads. Bulk records use AES-128-GCM with its
integrated authentication tag.

A fixed PSK file is 64 hexadecimal characters (optionally followed by a
newline) and must be mode `0600` or stricter on Unix. A PSK is not a user
password: weak or reused text is forbidden. It must never be logged or stored
in ordinary Android preferences. The Android reference producer keeps a
scanned managed key only in process memory. Compromise of a PSK allows
decryption of previously recorded sessions because pure `psk_ke` deliberately
provides no forward secrecy; use the receiver's local `NEW PAIR` action after
suspected disclosure.

`ClientHello.token` remains exactly 32 bytes for v2 wire compatibility, but is
all zero and has no authentication meaning. Authentication in PSK mode happens
during the TLS handshake. Plain mode is explicitly unauthenticated and
unencrypted; applications should communicate that fact to the user.

## 4. Negotiation

The producer sends `ClientHello` first. Byte-string sizes are mandatory:

- `client_id`: 16 stable random bytes;
- `token`: 32 zero bytes (reserved for v2 wire compatibility);
- `client_nonce`: 16 fresh random bytes per connection.

The receiver selects the highest mutually supported minor version and replies
with `ServerHello`, including actual panel geometry, formats, encodings, input
capabilities, and resource limits. No common version or missing mandatory
capability is fatal.

Mandatory v2.0 capabilities are:

- atomic multi-region frames;
- exact-base delta validation;
- latest-frame superseding;
- settled presentation barrier;
- `PIXEL_FORMAT_GRAY8` and `ENCODING_RAW`.

`ENCODING_ZSTD` and `ENCODING_ZLIB` are optional and negotiated by intersection.
ZSTD carries one standard Zstandard frame without a shared dictionary; the
compression level is an encoder choice and has no wire meaning. ZLIB carries an
RFC 1950 wrapped DEFLATE stream. Producers prefer ZSTD when both are available,
but use RAW whenever compression does not reduce the framed region. Gray8 is
tightly packed one byte per pixel: `00` black, `ff` white. JPEG, WebP, PNG,
Android Bitmap formats, and Quill native formats are not protocol pixel
encodings.

Qt/QImage formats reported by a receiver backend are implementation details,
not aliases for protocol formats.

Protocol v2.1 requires `PROTOCOL_FEATURE_BYTE_CREDITS`. Its optional negotiated
color capability is `PROTOCOL_FEATURE_COLOR_RGB565` together with
`PIXEL_FORMAT_RGB565_LE`. RGB565 is tightly packed at two bytes per pixel in
little-endian order; the 16-bit value is `rrrrrggg gggbbbbb`. Rectangles remain
pixel coordinates, while `decoded_len` and CRC cover the packed bytes. Color is
advertised only when the physical panel/backend can preserve it. Gray8 remains
mandatory and is the fallback for v2.0 or monochrome receivers. A producer must
never send RGBA bytes or RGB565 bytes while declaring Gray8.

Protocol v2.2 requires both `PROTOCOL_FEATURE_BYTE_CREDITS` and
`PROTOCOL_FEATURE_EPAPER_CUSTOM_PROFILE`. It adds an atomic, connection-scoped
custom policy to the existing profile control. Its four `EpaperWaveform`
fields are portable semantic choices (`FASTEST`, `FAST`, or `QUALITY`) for
LATEST text/mixed, LATEST photo, LATEST video, and SETTLED presentations. They
are not native Quill constants. `FULL_QUALITY` is intentionally absent, and a
producer still cannot request a partial or complete refresh directly.

Optional capabilities are negotiated by intersection: the producer includes a
feature in `ClientHello`, and the receiver echoes it in `ServerHello` only when
that session may use it. `PROTOCOL_FEATURE_EPAPER_PROFILE_CONTROL` and
`PROTOCOL_FEATURE_EPAPER_REFRESH_CONTROL` enable the session-scoped controls
described in section 9. Sending either request without its echoed feature
receives a nonfatal `UNSUPPORTED` result and changes no state.

## 5. Surface lifecycle and coordinates

`SurfaceOpen.surface_id` is producer-selected and nonzero. The receiver returns
a monotonically increasing nonzero `generation` in `SurfaceReady`. Opening a
new surface closes the previous one.

The surface coordinate space is its output pixel grid: top-left origin, x to
the right, y downward. The producer owns source-to-surface mapping, including
crop, letterbox, scale, and rotation. A change to the surface coordinate grid
or receiver-side transform requires reopening the surface and therefore a new
generation. A producer-local source transform may change while retaining the
same grid only if it atomically cancels its active contact state before using
the new inverse mapping; queued MOVE/UP records without a new DOWN are then
dropped. Every frame and input message carries the surface generation; stale
generations are rejected.

Input and action capabilities are producer-declared per surface:

- browser producer: pointer, text, keys, and navigation actions;
- screen mirror: display-only unless Android Accessibility permission is
  explicitly enabled, then the actually supported pointer/global actions;
- Linux CLI: normally display-only, optionally consumes input as JSONL.

`SourceKind` is optional advisory metadata for diagnostics and producer UI. It
does not grant a capability and must never select pixel conversion, waveform,
refresh, security, or flow-control behavior. Existing labels remain on the wire
for compatibility, but a new or composite producer may send `UNSPECIFIED`
instead of extending receiver policy with application-specific source types.

## 6. Atomic frames and delta bases

One protobuf `Frame` is one transaction. The receiver:

1. checks surface/generation, monotonically increasing nonzero frame ID, credit,
   and negotiated limits;
2. validates every nonempty rectangle is in bounds and rectangles do not
   overlap;
3. decodes every region into staging memory with a strict decoded-size limit;
4. checks `decoded_len == width * height * bytes_per_pixel` and validates IEEE
   CRC-32 of decoded bytes (`crc32fast`/zlib CRC semantics);
5. only then applies all regions and advances `logical_frame_id`.

Any failure leaves both logical and displayed surfaces unchanged.

`base_frame_id = 0` means keyframe. In v2 a keyframe has exactly one RAW, ZSTD,
or ZLIB region covering the complete surface. A nonzero base must exactly equal
the receiver's current logical frame ID. A mismatch returns
`FRAME_RESULT_CODE_NEED_KEYFRAME` and changes no state.

The receiver may accept a delta that builds on a logical frame whose physical
presentation was superseded; its staging buffer already contains that logical
base. `presented_frame_id` changes after a successful panel submission. It may
also advance when the receiver proves a frame has zero damage and is
already physically equivalent to the displayed surface. Backend failure
invalidates the delta base and requires a keyframe.

## 7. LATEST and SETTLED

The receiver retains at most one pending `FRAME_INTENT_LATEST` presentation. A
new valid frame may replace it, and the old frame receives exactly one terminal
`SUPERSEDED` result. There is no FIFO backlog of stale UI frames.

`FRAME_INTENT_SETTLED` is a presentation barrier:

- the producer sends it when its source becomes quiet, stops, or reaches EOF;
- it sends no later frame until the barrier receives a terminal result;
- the receiver never supersedes it and presents it before the advertised
  settled deadline unless the connection or backend fails.

Every syntactically received frame receives exactly one terminal `FrameResult`.
`credits` is an absolute current value, not an increment. A producer never
exceeds `max_inflight` and keeps its capture/render queue conflated: while the
transport is busy, only the latest unencoded source frame is retained. Stopping
a producer must still enqueue one final SETTLED frame.

In v2.1 flow control has a second dimension. `Limits.max_inflight_bytes` limits
the sum of decoded region lengths claimed by frames that have not received a
terminal result; `FrameResult.byte_credits` is the receiver's absolute remaining
value. Compressed wire size is not used for this accounting. A producer must
claim both a frame credit and decoded-byte credit atomically before writing.
The Android producer uses at most two frame credits: its second LATEST delta may
name the first in-flight frame as `base_frame_id`, because TCP preserves order
and the receiver advances its logical base before returning the first terminal
result. If either speculative frame is rejected, later dependent results are
drained and the producer resumes with a keyframe. A SETTLED frame remains a
barrier and prevents sending a later frame.

`Frame.intent` and `content_class` are semantic hints only. Quill waveform,
partial/full update, FPS, and cleanup policy are receiver decisions.
`SETTLED` defines ordering and terminal presentation, not a fixed-quality
waveform: the named/custom active profile selects its semantic waveform.

Gesture detection and motion sampling are producer policy, not receiver pixel
heuristics. A producer may drop or sample intermediate capture frames before
encoding, sends any retained intermediate as `LATEST`, and must send the final
quiet result as `SETTLED`. Pointer DOWN/MOVE/UP remains reliable input and is
not redefined as a frame-transport control message.

## 8. Input, text, and actions

`InputBatch` represents one coherent physical report and may contain multiple
contacts. Coordinates use unsigned 16.16 fixed-point surface pixels. Contact
IDs are stable for DOWN..UP/CANCEL; implementations do not expose evdev slot
numbers across sessions. Pointer UP/CANCEL travels reliably over TCP and may
not be intentionally dropped.

`INPUT_CAPABILITY_KEY` means physical key transitions represented by USB HID
usage page/usage, not Linux or Android private keycodes.
`INPUT_CAPABILITY_TEXT` means Unicode IME operations; `TextInput` distinguishes
commit, composition, and cancel. They are separate because a key does not imply
text and IME text often has no corresponding physical key. The current Quill
receiver advertises and emits only `TOUCH`; Android and the CLI retain KEY/TEXT
handlers for a future receiver keyboard or physical-key backend, so those two
capabilities are not currently negotiated end to end.

`ActionInvoke` is receiver-to-producer and only uses actions declared in
`SurfaceOpen`. Back, forward, reload, home, menu, previous-page, and next-page
remain remote application actions; `ActionResult` completes every invocation.

## 9. Receiver presentation rules and online policy control

- The first keyframe after startup/open is physically submitted even if all
  black or equal to an uninitialized software buffer.
- FPS limiting retains pending work and arms a timer; it never returns early
  and forgets the final frame.
- Local overlays are composed separately from the remote base surface.
- Named profiles map semantic content and intent to waveforms. In particular,
  Realtime uses Fastest for SETTLED, Animate uses Fast, and Balanced, Reading,
  and Quality use Quality.
- A fast SETTLED remains an unsupersedable terminal barrier and presents the
  final pixels, but it is not a ghost-cleanup promise. Full-panel cleanup is a
  separate receiver decision driven by first-frame, periodic, large-damage,
  static-fast-debt, explicit/profile, partial-disabled, or recovery policy.
- Quill/native state is owned by one panel actor. A successful submit advances
  `presented_frame_id`; a zero-damage frame may also advance it without a
  redundant panel operation.

The optional `EPAPER_PROFILE_CONTROL` feature changes receiver policy, never a
frame or native backend parameter. The presets, from fastest to highest
quality, are `REALTIME`, `ANIMATE`, `BALANCED`, `READING`, and `QUALITY`.
Original wire values remain `ANIMATE=1`, `BALANCED=2`, and `QUALITY=3`;
`REALTIME=4` and `READING=5` are appended, so numeric order has no quality
meaning. They select receiver-side waveform policy, cleanup cadence, and
large-damage behavior. `Frame` still exposes only `intent` and `content_class`;
there is no per-frame waveform, Quill mode, partial/full flag, or force-refresh
command. The normative preset matrix and override behavior are documented in
[`epaper-profile-control.md`](epaper-profile-control.md).

When v2.2 and `EPAPER_CUSTOM_PROFILE` are also negotiated, a SET may select
`CUSTOM` and must carry one complete `EpaperProfileConfiguration`. The receiver
validates all fields before changing any state, then installs together:

- semantic waveforms for LATEST text/mixed, photo, video, and SETTLED; each is
  exactly `FASTEST`, `FAST`, or `QUALITY`;
- partial-refresh permission, periodic cleanup interval, first-frame cleanup,
  large-damage threshold, and static-fast-debt threshold;
- a power-of-two damage tile from 8 through 512 pixels.

The large-damage threshold is 0..100, where zero disables it. Zero also
disables the corresponding periodic or static cleanup threshold. No custom
field can select `FULL_QUALITY` or require a complete refresh; on every complete
refresh, the receiver chooses its native full-panel mode and damage.

`EpaperProfileRequest` obeys these rules:

- `request_id` is producer-selected, nonzero, and strictly increases within a
  connection; it correlates exactly one `EpaperProfileResult`;
- `QUERY` requires `requested_profile = UNSPECIFIED` and changes no state;
- a named `SET` requires one known preset and no `custom` message;
- a `CUSTOM` SET requires v2.2 plus echoed `EPAPER_CUSTOM_PROFILE` and one
  complete valid `custom` message; CUSTOM without it, a custom message on a
  named preset, or any invalid field rejects the entire request without a
  partial policy update;
- malformed combinations and duplicate/stale request IDs receive nonfatal
  `REJECTED`; an unnegotiated request receives nonfatal `UNSUPPORTED`;
- the result reports the receiver's actual `EpaperProfileState`, including its
  complete `effective` configuration and preserved operator overrides, rather
  than assuming the requested preset/custom policy was accepted verbatim.

A changed profile is a presentation barrier. The receiver keeps all existing
fast-waveform/periodic-cleanup debt until a complete refresh succeeds. If a
LATEST or SETTLED frame is pending, it is immediately presented as a full
cleanup and still receives its one terminal `FrameResult` before the profile
result. Otherwise, the current displayed pixels are resubmitted as a full
maintenance refresh. Before the first frame, or after a backend failure,
`cleanup_pending` remains true and survives surface replacement until the next
valid presentation completes the cleanup. A backend cleanup failure does not
undo the selected policy; it invalidates the delta base and leaves cleanup
armed for recovery.

`EPAPER_REFRESH_CONTROL` adjusts four connection-scoped receiver parameters
without replacing the active high-level profile:

- `partial_refresh_enabled` permits or forbids partial panel updates;
- `cleanup_after_updates` schedules a complete cleanup after that many
  successful physical partial panel submissions; zero disables this trigger;
- `large_update_threshold_percent` schedules cleanup when bounding damage
  covers the configured panel percentage; zero disables it, and 1..100 are
  valid thresholds.
- `static_cleanup_after_fast_updates` waits for a `SETTLED` barrier and then
  schedules one cleanup after the configured number of successful Fast/Fastest
  submissions; zero disables it.

The scalar fields in `EpaperRefreshRequest` are explicitly `optional`, so
omission is distinct from setting `false` or zero. `QUERY` and `CLEANUP` carry
no parameter fields. `UPDATE` carries at least one field and leaves every
omitted parameter unchanged. Invalid presence combinations, a threshold above
100, and stale/duplicate/nonzero violations receive nonfatal `REJECTED`.
`request_id` is strictly increasing within the refresh-control request stream;
it is independent of profile-control request IDs.

`EpaperRefreshResult.active` is the receiver's authoritative state. Despite
its compact field name, `presented_since_full_refresh` counts only successful
physical partial panel submissions; zero-damage logical presentations do not
accrue ink-cleanup debt. `fast_updates_since_settled` reports motion-local fast
waveform debt and resets after a successful SETTLED presentation or cleanup.
A fast SETTLED ends the episode but does not itself perform a quality cleanup;
the static threshold causes a full cleanup at that barrier when due. Startup
options provide the baseline for each new
connection. Online changes, the physical partial-submission counter, and
pending cleanup state survive explicit SurfaceClose/Open and direct surface
replacement within that connection, then reset on reconnect.

`CLEANUP` requests one receiver-decided full-panel refresh. With a pending
LATEST or SETTLED frame, that frame is flushed and its terminal `FrameResult`
precedes `EpaperRefreshResult`. Otherwise the receiver resubmits the current
presented pixels. With no presented image it arms `cleanup_pending` for the
next valid frame. Backend failure returns `FAILED`, invalidates the delta base,
and leaves cleanup armed. No control message contains a native waveform.

The receiver also recognizes a local five-finger chord from synchronized
Type-B slots. Reaching five active contacts triggers CLEANUP once, emits CANCEL
for any first 1..4 contacts already forwarded, and consumes the gesture until
all contacts are up. This local action works even when PointerInput was not
enabled by the producer; without an active surface it is ignored. A surface
transition clears producer-forwarded ownership but preserves active/suppressed
physical contacts, so trailing MOVE/UP reports cannot leak into the new
surface.

`FrameResult.metrics` describes receiver-observed CPU and scheduling stages:

- `decode_us`: validation, decompression, CRC, and atomic logical-base update;
- `queue_us`: acceptance until presentation work starts, including FPS wait;
- `compose_us`: overlay composition, damage calculation, and refresh choice;
- `convert_us`: negotiated pixel conversion/copy into the native framebuffer;
- `submit_us`: backend refresh submission and event pumping;
- `present_us`: total compose-through-backend-return interval;
- damage area/count, native waveform value, and full-refresh flag describe the
  actual receiver decision.

When `complete_refresh` is true, `full_refresh_reason` distinguishes disabled
partial refresh, an explicit/profile/recovery force, first-frame cleanup,
periodic cleanup, large-damage cleanup, and static-fast-debt cleanup. A zero
large-damage threshold only
disables that one trigger; setting both cleanup interval and area threshold to
zero disables those two triggers, but static-fast-debt may still be enabled.
Setting all three automatic thresholds to zero while leaving partial refresh
enabled disables periodic, large-damage, and static cleanup. First-frame,
explicit five-finger/control, profile-transition, and recovery safety refreshes
remain possible.

These values are diagnostic, not protocol deadlines. `PRESENTED` means that
the synchronous panel backend accepted/returned, or that the receiver proved
the frame had zero damage and needed no panel operation. In either case the
software presented state advances. `present_us` does not claim that
electrophoretic ink has physically settled; measuring that requires a vendor
completion primitive or external optical instrumentation.

## 10. Generated code and validation

Rust generation lives in `crates/rm-display-protocol/build.rs` using `prost`.
Android uses protobuf-lite against the same root schema. Large `bytes` fields
are configured as `bytes::Bytes` on Rust. Neither implementation defines a
parallel enum or message layout by hand.

Proto3 does not express all invariants, so both languages implement semantic
validators and share fixtures. Required tests include:

- valid Raw and Zlib keyframes/deltas;
- Gray8 and negotiated RGB565 decoded-length/CRC behavior, plus frame and byte
  credit exhaustion;
- every TCP split point and adjacent coalesced messages;
- bad magic, length cap, session, message order, byte-string size, enum, and
  direction;
- non-full keyframe, bad base, decoded length, CRC, overlapping/out-of-bounds
  regions, decompression bomb, and no-credit behavior;
- atomic failure (surface bytes unchanged);
- LATEST superseding and mandatory SETTLED presentation;
- reconnect requiring a keyframe;
- negotiated/unnegotiated profile control, QUERY/SET validation, pending
  SETTLED flushing, effective-policy reporting, atomic v2.2 CUSTOM validation,
  and cleanup-debt persistence;
- optional refresh-parameter presence, QUERY/UPDATE/CLEANUP validation,
  physical-update counting, and five-finger cancellation/suppression;
- Rust/Kotlin decoding of the same checked-in protobuf fixtures.

Protobuf serialization is not treated as a canonical signing format. Golden
tests compare decoded messages and semantic behavior; exact bytes are asserted
only for the 8-byte outer framing and selected fixtures without maps or unknown
fields.

## 11. Versioning

The outer magic fixes major version 2. Minor versions are negotiated in Hello.
New protobuf fields receive new numbers; removed numbers and enum values are
reserved. Existing Gray8, CRC, base-frame, terminal-result, and SETTLED
semantics cannot change in a minor version.

v2.1 appends RGB565 and byte credits without changing v2.0 fields. A v2.0 peer
continues to see zero for the appended fields and uses only frame credits.

v2.2 appends `EPAPER_CUSTOM_PROFILE`, `EPAPER_PROFILE_CUSTOM`, the stable
`EpaperWaveform` enum, `EpaperProfileRequest.custom`, and
`EpaperProfileState.effective`. A v2.0/v2.1 peer never negotiates CUSTOM and
continues to use the named-profile state fields. v2.2 does not weaken SETTLED:
it remains a mandatory terminal barrier even when its configured waveform is
Fast or Fastest.

Assigning a previously reserved meaning without negotiation, changing an
existing field type/number, or weakening atomic/base semantics requires a new
major version and new outer magic.
