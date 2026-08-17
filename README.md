# rm-display receiver

`rm-display` is a general-purpose remote-display receiver for reMarkable
devices. Producers own application state and render pixels; the receiver owns
the panel, input forwarding, local overlays, and e-paper refresh policy.

This public repository contains:

- `rm-display-receiver`: the TCP/session server, host mock panel, Type-B evdev
  input, and optional AArch64 Quill backend. It does not use QtFB or
  `qtfb-clients`.
- `rm-display-cli`: Linux diagnostics, still-image display, raw-frame streaming,
  and generic input/action output.
- `rm-display-protocol`, `rm-display-transport`, and `rm-display-core`: the
  generated Protobuf model, optional PSK transport, and hardware-independent
  display state.

The Android producer is developed and distributed separately. It consumes the
same authoritative schema but is not part of this repository or the receiver's
GPL license scope.

## Repository layout

```text
.
├── protocol/rm_display/v2/       authoritative Protobuf schema + descriptor
├── crates/
│   ├── rm-display-protocol/       generated Rust types and validation
│   ├── rm-display-transport/      optional TLS 1.3 external-PSK transport
│   ├── rm-display-core/           surfaces, scheduler, and refresh policy
│   ├── rm-display-receiver/       Quill/mock receiver
│   └── rm-display-cli/            Linux producer and diagnostics
├── docs/                          protocol, architecture, and operation notes
├── packaging/                     AppLoad takeover manifest
├── quill/                         Zen-Ink Quill Git submodule
├── scripts/takeover.sh            device runtime takeover entry point
└── Makefile                       build, verification, and packaging targets
```

The wire contract is generated from
[`rm_display.proto`](protocol/rm_display/v2/rm_display.proto). See
[`docs/protocol-v2.md`](docs/protocol-v2.md) for semantic rules,
[`docs/architecture.md`](docs/architecture.md) for component boundaries, and
[`docs/refresh-strategy.md`](docs/refresh-strategy.md) for refresh behavior.

## Build and test

For host development:

```sh
make check
make run-receiver
make run-cli ARGS=doctor
```

Cross-building deliberately has no machine-specific SDK path in version
control. Pass the environment setup file and Quill checkout explicitly:

```sh
SDK_ENV=/path/to/environment-setup-cortexa53-crypto-remarkable-linux \
QUILL_DIR=quill \
  make receiver-aarch64
```

Clone with `--recurse-submodules`, or run `git submodule update --init` after
an ordinary clone. Quill's proprietary vendor dependency is not distributed;
place the matching device `libqsgepaper.so` at
`quill/vendor/libqsgepaper.so` before cross-building. The Makefile builds only
the embeddable Quill shared library before linking the receiver.

The SDK environment must put `aarch64-remarkable-linux-gcc` on `PATH` and
provide its sysroot variables. The Cargo configuration names only that linker;
it does not embed a local toolchain or sysroot path.

Run `make receiver-takeover` to produce
`dist/rm-display-receiver-aarch64.tar.gz`. The archive includes the AppLoad
manifest, takeover entry point, receiver, Quill libraries, and the receiver's
GPL-2.0-only license. The executable uses an `$ORIGIN` RPATH so colocated
libraries are found when it is launched directly.

## Transport and pairing

The receiver creates a receiver-generated 32-byte PSK on first startup and
reuses it until `NEW PAIR`. It puts that credential in its local pairing QR and
uses TLS 1.3 external-PSK records
fixed to AES-128-GCM, without certificates or asymmetric keys. Explicit
`--plaintext` selects unauthenticated plain TCP; `--psk-file` selects an
operator-chosen persistent key path for unattended installations. The modes
never fall back to one another.

Pairing is server-authoritative. The receiver publishes its currently bound
endpoint, required security mode, and server identity in a QR offer. A producer
may accept the offer or abort; it cannot change or downgrade those parameters.
After the selected transport is established, `ServerHello` confirms identity
and negotiates protocol capabilities. The managed PSK and receiver identity
remain valid across reconnects and receiver restarts. Selecting `NEW PAIR`
from the receiver's power-key menu disconnects the current producer, rotates
the PSK, and displays a replacement QR. See
[`docs/pairing.md`](docs/pairing.md).

Ready-to-use FFmpeg, GStreamer, and wlroots producer pipelines are documented
in [`docs/linux-producers.md`](docs/linux-producers.md).

## Workspace policy

Project caches must not be placed under `/tmp`. Repository-configured Cargo
outputs live under `.cache/`. The `Makefile` does not override `CARGO_HOME`,
`CARGO_TARGET_DIR`, or `TMPDIR`, and build orchestration belongs in the
`Makefile` rather than helper scripts.
