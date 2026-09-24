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
pixel buffers. Pen presentation uses the receiver's LATEST text policy; the
receiver still owns cleanup and native waveform selection. Pen width is
currently 5 physical pixels and eraser diameter 25. Pressure is forwarded but
does not currently change local pen width. Menu interactions suppress pen
forwarding; changing surfaces discards local ink.

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
