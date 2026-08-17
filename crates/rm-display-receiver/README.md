# rm-display receiver

This receiver crate and its takeover package are licensed under
`GPL-2.0-only`; see [`LICENSE-GPL-2.0-only`](LICENSE-GPL-2.0-only). This license
declaration is intentionally scoped to the receiver and does not license
separately distributed producers.

The receiver owns the negotiated Gray8/RGB565 surface, Quill framebuffer conversion, e-paper
refresh policy, and optional Type-B touchscreen forwarding. It does not use
QtFB or `qtfb-clients`.

Host mock mode binds loopback through the root Makefile:

```sh
make run-receiver
```

The receiver generates a persistent 32-byte PSK by default and listens on
`0.0.0.0:7420`:

```sh
./rm-display-receiver
```

By default the receiver displays a startup QR containing its active IPv4
addresses, port, selected `psk` mode, ephemeral server identity, and the
receiver-generated credential. A producer can scan it to accept and pin the
server-authored connection offer. The key and receiver identity remain stable
across reconnects and receiver restarts. By default they are stored under
`$XDG_STATE_HOME/rm-display`, or `$HOME/.local/state/rm-display` when the XDG
variable is unset; this state never uses `/tmp`. See
[`../../docs/pairing.md`](../../docs/pairing.md) for the wire-independent
descriptor contract and security boundary. With the default wildcard bind, QR
hosts are ordered as eligible `wlan0`, then `usb0`, then other active
interfaces; an explicit bind publishes only its exact address.

On a reMarkable AArch64 Quill build, the receiver verifies the machine identity
and auto-discovers one unambiguous Type-B multitouch device from its evdev name,
capability bits, and axis ranges. It never assumes a fixed `eventN`. Use
`--input=/dev/input/eventN` only as an explicit override. Startup output states
which device was grabbed or why touch input is disabled.

The receiver owns native Quill mapping and every complete-refresh decision.
From fastest to highest quality, presets are `realtime` (360 partial
updates, all LATEST content Fastest), `animate` (180), the default `balanced`
(90), `reading` (45 and a 50% large-damage trigger), and `quality` (20 and a
33% trigger). Reading favors Quality for text/photos while keeping video Fast;
quality uses Quality for every LATEST class. SETTLED follows the selected
profile: Realtime uses Fastest, Animate uses Fast, and the other presets use
Quality:

```sh
./rm-display-receiver --epaper-profile=quality
```

`--full-refresh-interval=N` overrides the profile cadence; zero disables only
the periodic trigger. `--damage-tile=PIXELS` controls receiver-side pixel
damage granularity and defaults to 64. A final `SETTLED` includes dirty area
accumulated by preceding fast updates, even when its pixels equal the last
`LATEST` frame; its configured semantic waveform determines whether that
repaint favors latency or clarity. Fast/Fastest SETTLED remains a final-frame
barrier but does not itself clean ghosting; periodic, static-fast-debt,
large-damage, first-frame, manual, and recovery policy decide full cleanup.

Producers may negotiate connection-scoped profile and refresh controls. The
refresh control can query/update partial-refresh permission, periodic physical
partial-update cadence, the 0..100 large-damage cleanup threshold, and the
static-fast-debt threshold, or request one immediate full cleanup. Protocol
v2.2 additionally permits an
atomic `CUSTOM` semantic policy with LATEST text/photo/video and SETTLED
choices limited to Fastest, Fast, or Quality, plus the existing refresh
parameters. It never carries native Quill integers or grants control of
complete refresh. The receiver returns the effective values; disconnect
restores the receiver command-line baseline for the next producer.
The v2.2 profile result's `effective` message contains all four semantic
waveforms and every refresh parameter for named presets as well as Custom.

On the Type-B touchscreen, reaching five simultaneous contacts triggers one
local full cleanup. Contacts already forwarded are cancelled, the rest of the
gesture is consumed until all fingers lift, and the action works even when the
producer did not request pointer input.

On production reMarkable Quill builds the receiver also discovers and grabs
the capability-advertised `KEY_POWER` evdev device. Pressing the power button
opens a receiver-owned menu at the top of the panel; no key-state polling or
hard-coded `eventN` path is used. The five preset receiver profiles are
separate buttons rather than a cycle action. The active profile is shown with
a black, inverse-text button and repeated in the menu header, so a touch both
selects an exact preset and immediately shows the effective preset. After a
producer has installed a valid CUSTOM policy, a sixth CUSTOM button appears;
it restores that last session-local policy and is inverted while active. Further
actions toggle partial refresh, request a full cleanup, select `NEW PAIR`,
close the menu, or select `CLOSE APP`. `NEW PAIR` disconnects the active
producer, rotates the managed PSK, and redraws the QR. The menu is a separate
local overlay: closing it restores the remote base pixels. `CLOSE APP` first attempts that same restoration, then
ends the session and makes `ReceiverServer::run` return `Ok(())` even if the
producer has already disconnected. Dropping the receiver releases its evdev
grabs; takeover/systemd cleanup can then restore xochitl.
The evdev worker blocks in the kernel until a key event or shutdown signal; it
does not periodically read key state. While no producer is connected, the
listener, power key, and touchscreen are multiplexed with blocking `poll(2)`.
The pairing frame supplies the menu's fallback base, so the menu remains usable
before the first connection and between reconnects.

An idle connection no longer calls the Quill/Qt event pump every network read
timeout. Scheduler ticks return before entering the backend unless work is due;
real frame, overlay, and cleanup submissions still pump synchronously. A short
CPU spike while diffing/copying a 960x1696 frame and submitting a waveform is
expected, but sustained high CPU with no pending frame is not.

Use explicit plaintext only on a trusted network where authentication and
confidentiality are not required:

```sh
./rm-display-receiver --plaintext
```

For an unattended deployment, create a random 32-byte PSK with mode `0600` and
select its persistent path explicitly. This also permits suppressing the QR
because the producer already has the key. Selecting `NEW PAIR` atomically
replaces this file before publishing the new credential:

```sh
umask 077
openssl rand -hex 32 > receiver.psk
./rm-display-receiver --psk-file=receiver.psk --no-pairing-qr
```

PSK mode uses TLS 1.3 external PSK, `TLS_AES_128_GCM_SHA256`, and pure
`psk_ke`. It has no certificate or asymmetric key. A failed PSK handshake is
never retried as plaintext. `ClientHello.token` is a reserved all-zero field;
it is not a second credential. `--no-pairing-qr` is rejected with a generated
PSK because it would leave no way to provision that credential.

Build the aarch64 Quill version with `make receiver-aarch64`. The target stages
`rm-display-receiver`, `libquill.so`, and `libqsgepaper.so` together. The binary
uses an inherited `$ORIGIN` RPATH so it also works when launched directly from
that directory. The target OS must provide `libssl.so.3` and `libcrypto.so.3`,
as represented by the selected reMarkable SDK. Run it through
`scripts/takeover.sh` so xochitl is restored by systemd after exit or failure.

Build the installable AppLoad takeover archive with `make receiver-takeover`.
It produces `dist/rm-display-receiver-aarch64.tar.gz` with a flat application
root containing `external.manifest.json`, `scripts/takeover.sh`, the receiver, both
bundled Quill libraries, and the receiver GPL license. The manifest records the
startup entry, AArch64 architecture, release version, SPDX license identifier,
bundled libraries, and the OpenSSL 3 libraries expected from the target OS.
