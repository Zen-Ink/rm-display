# Overlay and pen (protocol v2.3)

Negotiation adds `REMOTE_OVERLAY` (14) and `LOCAL_INK` (15). v2.3 also retains
v2.1 byte credits and v2.2 custom profile support. Older peers negotiate their
previous minor version. Pen uses the existing `POINTER_INPUT` feature and
`InputCapability::Pen`; only a successfully opened digitizer is advertised.
`LOCAL_INK` additionally requires `REMOTE_OVERLAY` and a working digitizer.

`Envelope.overlay_update` (100) / `overlay_result` (101) never reuse reserved
field numbers. The update addresses a surface and generation, and carries a
nonzero sequence strictly increasing across the connection. Invalid requests
receive `applied=false` without changing pixels. Even rejected requests consume
a newer sequence. Surface replacement and disconnect discard all planes.

Each update optionally replaces a raw Gray8 luma + alpha rectangle (one byte
per pixel per plane). Bounds and exact byte lengths are validated before any
mutation; combined decoded bytes cannot exceed `Limits.max_frame_bytes`, and
normal envelope `max_payload` applies. Large planes can be sent in row strips;
only each individual command is atomic. Alpha 0 is transparent, 255 opaque.
`clear=true` clears the peer plane and local ink; an optional rectangle is then
applied. Composition order is browser/base, peer overlay, local ink, receiver
menu. Peer updates never replace the remote frame delta base or receiver menu.

`local_ink=true` arms local ink. First pen DOWN after an actually presented
frame freezes that exact underlying base and cancels pending newer work.
`InputBatch.presented_frame_id` (32) identifies that base;
`InputBatch.ink_frozen` (33) tells the producer to stop sending frames. The pen
batch precedes any cancelled-frame result. While frozen, frames receive
`REJECTED / INK_FROZEN` (reason 10); this is recoverable, the connection stays
open. The sender client returns this as a frame report rather than an error.
The producer must retain snapshots by frame ID and must not advance its own
screenshot/delta baseline on a rejected report.

`clear=true` preserves the freeze. `local_ink=false` clears ink and releases
the base; a new keyframe is required. `ProducerClient::update_overlay` waits
for acknowledgement and marks the next frame as a keyframe after release.
There is no browser or Agent state in the receiver. Snapshot persistence and
feedback submission gestures belong to the host bridge.

Ink is drawn locally, without a network round trip: each drained pen batch
rasterizes into a separate plane, then updates dirty bounds through reusable
pixel buffers. Pen presentation uses an independent partial Fastest waveform
(RM2 native mode 0), bypassing webpage FPS and quality policy. Its exact dirty
rectangle is not expanded to webpage damage tiles. Generic idle cleanup is
paused while a physical touch remains down, the pen remains in proximity, or
for five seconds after physical input. Each refresh profile keeps its existing
damage and panel-idle thresholds; explicit receiver cleanup/menu actions remain
available. `RM_DISPLAY_INK_WAVEFORM=fastest|fast|quality`
selects a hardware calibration override (default `fastest`). A bounded reader
captures timestamped evdev reports independently of panel and network stalls;
the connection loop drains them before network processing and handles buffered
TLS data without waiting for another socket wakeup.
Pen width is
currently 5 physical pixels and eraser diameter 25. Pressure is forwarded but
does not currently change local pen width. Menu entry sends CANCEL for previously forwarded touch contacts and the last
pen contact/proximity, then clears the current ink segment. Menu interactions
suppress pen forwarding. After menu exit, stale MOVE/UP is suppressed until a
fresh DOWN; hover remains observable. Physical touch releases consumed by the
menu still update gesture state. Changing surfaces discards local ink.

Pointer coordinates are physical panel pixels in unsigned 16.16. Pressure is
0..65535, tilt is -90..90 degrees (0 when absent). Flags bit 0 denotes eraser;
buttons bit 0 is tip, bit 1 primary side button, bit 2 secondary side button.
Contact IDs must be interpreted together with device type. The digitizer emits
DOWN, MOVE, UP, HOVER and CANCEL, including cancellation/resync for dropped
kernel events and cancellation on device failure. A failed device is disabled
for subsequent handshakes; reconnecting the hardware requires restarting the
receiver. Existing active-session feature negotiation is immutable.

RM2 Wacom defaults to portrait `x=raw_y`, `y=max_raw_x-raw_x`, normalized using
queried axis bounds. Calibration overrides:

- `RM_DISPLAY_PEN_DEVICE=/dev/input/eventN`: explicit digitizer; otherwise
  capability discovery is enabled on reMarkable Quill builds.
- `RM_DISPLAY_PEN_TRANSFORM=rm2|identity`: coordinate transform.
- `RM_DISPLAY_PEN_BOUNDS=xmin,xmax,ymin,ymax`: raw axis calibration.

Run `cargo run -p rm-display-receiver --example overlay-smoke` for a standalone
TCP loopback replay covering negotiation, patch/clear, stale sequence rejection,
synthetic pen snapshot binding, frozen-frame rejection and release/keyframe.
This is an operational example, not a unit-test suite. Real input timing,
physical placement and e-paper quality still require device acceptance.

## Embedded application APIs

`rm-display-receiver` exposes receiver-local capabilities through `Session`.
The embedding application owns snapshot delivery, file saving, submission UI,
and the choice of when to freeze, pause, clear, or clean the panel. These Rust
APIs do not add wire messages or a deferred stroke-history buffer.

- `ink_snapshot()` copies only the local black ink plane, excluding the remote
  background, producer overlay and receiver menu. It returns surface ID,
  generation, presented frame ID, dimensions, frozen state and a packed mask.
  Bits are continuous row-major MSB-first, without row padding; unused tail
  bits are zero. A fully erased plane returns a full-length zero mask. Reading
  a snapshot does not freeze, pause, clear, or redraw anything.
- `set_ink_paused(paused, now)` controls local raster mutation. Physical input
  and producer input delivery continue. Resuming requires a fresh DOWN and
  rejects input captured before the change. Pause and snapshot on the same
  display thread when retries must retain an unchanged mask.
- `clear_ink(now)` clears only local ink, preserving peer overlays, the frozen
  background and pause state. Save successfully before calling it when the
  application must retain unsaved work.
- `set_frame_frozen(frozen, now)` explicitly freezes the presented background
  or releases it without clearing ink. Freezing before the first presentation
  is rejected. Pending frames are cancelled with terminal responses; resume
  requires a fresh keyframe. Returned input metadata reports the new state.
- `set_freeze_on_pen_down(false)` permits local ink over a live background.
  The default remains first-DOWN freeze for existing v2.3 clients. Applications
  wanting that exact boundary should use the receiver-side first-DOWN policy,
  rather than waiting for a host round trip. Explicit freeze and pause are
  independent, and a frozen background still allows drawing unless paused.
- `set_automatic_idle_cleanup(false)` disables profile-based idle maintenance.
  `interaction_state()`, `last_partial_at()` and `current_refresh_state()` let
  the application choose its idle rule. `request_cleanup(now)` returns the
  cleanup report plus producer responses. Explicit cleanup and configured
  frame refresh thresholds still work; no fixed 30-update/5-second rule is
  imposed by this API. Cleanup preserves the current composite and local ink.

`ReceiverServer::set_session_hook` calls the embedding application's callback
on the display thread at connection initialization and on each loop iteration,
after physical input and before scheduled presentation. The server forwards
all envelopes returned by the hook. A direct `Session` user must forward all
returned envelopes itself, including cancelled-frame terminals. The hook
persists across reconnects, while `now` is a monotonic duration relative to the
new connection. Use `session_id()` to distinguish connections; surface IDs and
generations belong to that session. The hook covers connected sessions, not
pre-connection pairing/menu UI.

For example, an application can take ownership of idle maintenance while
using a channel in the callback to receive snapshot or cleanup requests:

```rust,ignore
server.set_session_hook(move |session, now| {
    session.set_automatic_idle_cleanup(false);
    // Drain application commands here without blocking. For a snapshot:
    // session.set_ink_paused(true, now)?;
    // snapshot_tx.send(session.ink_snapshot()?) ...
    // For a cleanup request, return session.request_cleanup(now)?.1.
    Ok(Vec::new())
});
```

Keep the hook short: move file and network I/O to the application worker after
copying a snapshot. Local ink, pause and freeze state are surface/session
memory; snapshot ownership is transferred by copying, not device persistence.
A host wanting bitmap delivery must arrange its own application transport.

Run `cargo run -p rm-display-receiver --example application-api --offline` for
live-background drawing, pure snapshot reads, non-byte-aligned masks, pause,
freeze/resume, keyframe recovery, explicit cleanup, stale input suppression and
an actual server hook/channel exchange without device hardware.
