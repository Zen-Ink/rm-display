# rm-display Linux CLI

`rm-display-cli` is a protocol-v2 producer and diagnostic client. It supports
receiver probing, negotiated capability inspection, image display, fixed-size
raw Gray8 streaming, and generic input/action JSONL output.

The CLI negotiates v2.2 Custom policy control and v2.1 decoded-byte credits
while continuing to emit Gray8; `info` reports the selected minor plus frame
and byte limits. The current raw-stream CLI adapter emits Gray8 rather than
RGB565.

Connections use plain TCP by default. Encryption is enabled explicitly with a
mode-`0600` PSK file containing exactly 64 hexadecimal digits (32 bytes):

```text
8d7993...64 hexadecimal digits total...be41
```

For example, create one in the current directory with:

```sh
umask 077
openssl rand -hex 32 > receiver.psk
```

PSK mode uses TLS 1.3 external PSK with the fixed
`TLS_AES_128_GCM_SHA256` suite and pure `psk_ke`; it uses no certificate or
asymmetric key. Plain and encrypted modes are selected locally and are never
negotiated or automatically downgraded.

Examples:

```sh
rm-display-cli --host rm-display.local probe
rm-display-cli --host rm-display.local --psk-file receiver.psk info
rm-display-cli --host rm-display.local --psk-file receiver.psk show page.png
producer | rm-display-cli --host rm-display.local --psk-file receiver.psk \
  stream --width 960 --height 1696
rm-display-cli --host rm-display.local --psk-file receiver.psk doctor
rm-display-cli --host rm-display.local --psk-file receiver.psk events
rm-display-cli --host rm-display.local --epaper-profile quality show page.png
rm-display-cli --host rm-display.local --epaper-profile reading profile
rm-display-cli --host rm-display.local --epaper-profile custom \
  --latest-text-waveform fast --latest-photo-waveform quality \
  --latest-video-waveform fastest --settled-waveform fast \
  --partial-refresh-enabled true --cleanup-after-updates 120 \
  --clean-first-frame true --large-update-threshold-percent 0 \
  --static-cleanup-after-fast-updates 8 --damage-tile 64 profile
rm-display-cli --host rm-display.local refresh
rm-display-cli --host rm-display.local --partial-refresh-enabled false \
  --cleanup-after-updates 0 --large-update-threshold-percent 0 doctor
rm-display-cli --host rm-display.local --cleanup-now refresh
```

Existing FFmpeg, GStreamer, and wlroots capture commands can feed `stream`
directly; see [`docs/linux-producers.md`](../../docs/linux-producers.md).

The default host is the reMarkable USB address `10.11.99.1`. Connection options
are global, so both `--host 192.168.1.20 info` and
`info --host 192.168.1.20` are accepted.

`--epaper-profile realtime|animate|balanced|reading|quality|custom` negotiates the
optional profile control feature and requests that receiver-owned policy for
this connection. `profile` prints the receiver's authoritative effective
profile/configuration as JSON.
It can accompany any subcommand and is applied before a surface is opened.
The receiver prints the effective cleanup values and remains authoritative;
the option neither persists after disconnect nor exposes Quill waveform or
force-refresh controls.

Custom requires protocol v2.2 plus `EPAPER_CUSTOM_PROFILE`. The four global
waveform flags select portable Fastest/Fast/Quality semantics for LATEST
text/mixed, photo, video, and SETTLED; they never carry native Quill integers.
`--clean-first-frame` and `--damage-tile` are also Custom-only. The partial,
periodic, large-area, and static-fast-debt flags work with named profiles as
online overrides and are included in the single atomic Custom request when
Custom is selected. Damage tile must be a power of two from 8 through 512;
large area is 0..100. FullQuality and the complete-refresh flag have no CLI
option because the receiver alone owns full-panel cleanup.

`refresh` queries the receiver's effective connection-scoped refresh state.
Global `--partial-refresh-enabled BOOL`, `--cleanup-after-updates N`, and
`--large-update-threshold-percent 0..100` update only fields explicitly
present on the command line; `false` and zero are real values, not omission.
`--cleanup-now` requests one full cleanup. These options apply after
`--epaper-profile`, so explicit parameter values are final. The periodic count
tracks physical partial panel submissions; identical zero-damage frames do not
advance it.

`stream` reads tightly packed Gray8 bytes. With no `--width`/`--height`, it
learns the surface dimensions from the receiver and treats each stdin frame as
exactly that size. For a differently sized source, specify both dimensions;
they are used to find frame boundaries and scale into the negotiated surface.
Each complete source frame is sent as `LATEST`. Clean EOF repeats the newest
pixels as `SETTLED` and waits for `PRESENTED`; partial frames are fatal. If the
receiver reports `NEED_KEYFRAME`, the CLI retries the current pixels as a new
full keyframe while preserving the original intent.

SETTLED is always an unsupersedable terminal barrier. Its waveform follows the
effective policy: Realtime uses Fastest, Animate uses Fast, and the other named
presets use Quality; Custom uses its explicit settled waveform. A fast SETTLED
does not itself request ghost cleanup. Periodic/static/manual and other
receiver-owned rules decide when a complete refresh occurs.

If `--psk-file` is present, any PSK/TLS failure is fatal; the CLI never retries
that connection as plaintext.

Use global `--stats` to print per-frame timing plus a rolling-last-10,000-frame
p50/p95/max summary to stderr. `--stats-jsonl PATH` writes the same measurements as structured JSONL
(`-` selects stdout). Metrics distinguish source wait/preparation, protobuf
construction, outer framing, socket write, result wait, receiver decode/queue,
composition, native framebuffer conversion, and backend submission. The byte
count is the RMD2 framed plaintext size; TLS record overhead is not included.
Quill submission return is not a measurement of physical ink settling, which
remains explicitly unavailable.
