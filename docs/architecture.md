# rm-display receiver architecture

The architecture implements the protocol in
[`protocol-v2.md`](protocol-v2.md). Application-specific state remains on a
producer; display-hardware state remains on the receiver.

## Components

```text
protocol/rm_display/v2/       authoritative Protobuf schema + descriptor
crates/rm-display-protocol/   generated Rust types, framing, validators
crates/rm-display-core/       surfaces, scheduler, refresh policy, panel trait
crates/rm-display-transport/  optional TLS 1.3 external-PSK transport
crates/rm-display-receiver/   TCP/session server, Quill/mock panels, evdev input
crates/rm-display-cli/        Linux producer and diagnostics
```

The separately maintained Android producer consumes the schema as an external
input. No Android source or Gradle build is part of this repository.

## Receiver pipeline

```text
TCP or PSK connection
  -> validate framed Protobuf messages
  -> atomically advance the negotiated Gray8/RGB565 surface
  -> replace pending LATEST work or install a SETTLED barrier
  -> panel actor presents at the device FPS
  -> FrameResult returns terminal state and absolute frame/byte credits

Type-B evdev input
  -> normalize physical contacts
  -> map through the current surface generation
  -> send InputBatch over the same full-duplex connection

blocking KEY_POWER evdev source
  -> show the receiver-owned top overlay
  -> select an e-paper profile, request cleanup, or close the app
```

The receiver does not use QtFB or `qtfb-clients`. `rm-display-core` owns remote
base pixels, local overlay pixels, frame IDs, damage, replacement, final-frame
timing, and semantic refresh policy. Hardware-specific work is isolated behind
`PanelBackend`.

The host `MockPanel` records submissions for tests. `QuillPanel` is built only
for AArch64 Linux with the `quill` feature and is confined to one panel thread.
Its unsafe scope contains the Quill C ABI, native QImage pixel writes, swap,
and event pumping.

The Quill backend validates native width, height, stride, and format. It accepts
protocol Gray8 and RGB565, then converts as required by the vendor-owned
framebuffer. Qt format 4 is `QImage::Format_RGB32`, not a protocol format. Color
is advertised only when the native layout and a known Paper Pro machine
identity agree; unknown hardware remains Gray8.

## Refresh control

The receiver provides `realtime`, `animate`, `balanced`, `reading`, and
`quality` presets plus the negotiated v2.2 custom semantic policy. Profiles
control update waveform, settled waveform, periodic cleanup, large-damage
cleanup, and static fast-waveform debt. Native Quill enum values never cross
the wire.

`EPAPER_REFRESH_CONTROL` can change connection-scoped partial-update permission,
cleanup cadence, large-damage threshold, static-debt threshold, or request one
immediate cleanup. The receiver remains authoritative for the final physical
decision. Periodic debt counts successful physical partial submissions rather
than repeated zero-damage logical frames.

A five-finger chord is receiver-local: it cancels contacts already forwarded,
consumes the remaining gesture, and requests a complete cleanup. The power-key
menu uses blocking evdev input. While disconnected, the TCP listener, power
eventfd, and touchscreen are multiplexed by blocking `poll(2)`, so opening and
using the menu requires neither a producer connection nor periodic polling.

## Pairing and session security

Before accepting a connection, the receiver displays a server-authored
`rm-display://pair/v2` offer containing its live endpoint, required transport
mode, `ServerHello` identity, and (in managed PSK mode) a fresh 32-byte
credential. Producers accept that exact offer or abort; they cannot negotiate a
plaintext downgrade. Transport establishment precedes Protobuf because TLS
record mode cannot safely be selected by an unauthenticated in-band message.
`ClientHello`/`ServerHello` then confirms identity and negotiates protocol
capabilities. The PSK and server identity survive reconnects and receiver
restarts. The local `NEW PAIR` action disconnects the current producer and
atomically rotates the persisted PSK. See
[`pairing.md`](pairing.md).

Plain TCP is explicitly selected with `--plaintext` and is unauthenticated and
unencrypted. The default managed-PSK mode uses
TLS 1.3 external PSK with AES-128-GCM and pure `psk_ke`; certificates, 0-RTT,
tickets, and automatic downgrade are disabled.

## Linux producer

The CLI can inspect capabilities, show an image, stream Gray8, emit input/actions
as JSONL, run diagnostics, report producer/receiver timing, and control semantic
refresh settings. FFmpeg and GStreamer may provide capture frames without
changing the protocol.

## Build and lifecycle

Host builds never link the ARM Quill library. Cross builds require an SDK
environment that provides the compiler and sysroot plus `RMPP_QUILL_LIB_DIR`.
No absolute toolchain path is stored in Cargo configuration or the Makefile.

Takeover is supervised so systemd `ExecStopPost` restores xochitl after normal
exit or receiver failure. `CLOSE APP` clears its overlay and exits normally,
releasing input grabs before takeover cleanup runs.

Repository-configured build data lives under `.cache/`; project caches must not
be placed under `/tmp`.
