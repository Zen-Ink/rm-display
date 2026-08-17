SHELL := /usr/bin/bash

.DEFAULT_GOAL := help

SDK_ENV ?=
QUILL_DIR ?= quill

.PHONY: help clean cargo fmt-check test clippy check quill-aarch64 receiver-aarch64 \
	receiver-takeover run-receiver run-cli

help:
	@printf '%s\n' \
		'make check                 Format, test, and lint the Rust workspace' \
		'make clean                 Remove receiver workspace build outputs' \
		'make quill-aarch64         Cross-build the embedded Quill library' \
		'make receiver-aarch64      Cross-build the Quill receiver' \
		'make receiver-takeover     Build the installable AArch64 takeover archive' \
		'make run-receiver          Run the loopback mock receiver' \
		'make run-cli               Run CLI ARGS, for example ARGS="doctor"' \
		'make cargo ARGS="..."      Run an arbitrary Cargo command'

clean:
	cargo clean
	rm -rf dist/rm-display-receiver-aarch64 dist/rm-display-receiver-aarch64.tar.gz

cargo:
	@test -n "$(ARGS)" || { printf '%s\n' 'ARGS is required'; exit 2; }
	cargo $(ARGS)

fmt-check:
	cargo fmt --all -- --check

test:
	cargo test --workspace --offline

clippy:
	cargo clippy --workspace --all-targets --offline -- -D warnings

check: fmt-check test clippy

quill-aarch64:
	@test -n "$(SDK_ENV)" || { printf '%s\n' 'SDK_ENV is required'; exit 2; }
	@test -f "$(SDK_ENV)" || { printf 'missing SDK environment: %s\n' "$(SDK_ENV)"; exit 2; }
	@test -x "$(QUILL_DIR)/build.sh" || { printf 'missing Quill submodule; run git submodule update --init\n'; exit 2; }
	@test -f "$(QUILL_DIR)/vendor/libqsgepaper.so" || { printf 'missing qsgepaper library\n'; exit 2; }
	@SDK_ENV="$(abspath $(SDK_ENV))" QUILL_LIBRARY_ONLY=1 "$(QUILL_DIR)/build.sh"

receiver-aarch64: quill-aarch64
	@unset LD_LIBRARY_PATH; source "$(SDK_ENV)"; \
	test -n "$${SDKTARGETSYSROOT:-}" || { printf 'SDK does not export SDKTARGETSYSROOT\n'; exit 2; }; \
	RMPP_QUILL_LIB_DIR="$(abspath $(QUILL_DIR))/build" \
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=--sysroot=$$SDKTARGETSYSROOT -C link-arg=-mcpu=cortex-a53+crc+crypto -C link-arg=-mbranch-protection=standard" \
		cargo build --release --target aarch64-unknown-linux-gnu \
		-p rm-display-receiver --features quill
	@output="$$(cargo metadata --no-deps --format-version=1 2>/dev/null | \
		sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/aarch64-unknown-linux-gnu/release"; \
	install -m 0755 "$(QUILL_DIR)/build/libquill.so" "$$output/libquill.so"; \
	install -m 0755 "$(QUILL_DIR)/vendor/libqsgepaper.so" "$$output/libqsgepaper.so"; \
	printf 'output: %s\n' "$$output"

receiver-takeover: receiver-aarch64
	@output="$$(cargo metadata --no-deps --format-version=1 2>/dev/null | \
		sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/aarch64-unknown-linux-gnu/release"; \
	stage="dist/rm-display-receiver-aarch64"; \
	archive="dist/rm-display-receiver-aarch64.tar.gz"; \
	rm -rf "$$stage" "$$archive"; \
	install -d "$$stage/scripts"; \
	install -m 0755 "$$output/rm-display-receiver" "$$stage/rm-display-receiver"; \
	install -m 0755 "$$output/libquill.so" "$$stage/libquill.so"; \
	install -m 0755 "$$output/libqsgepaper.so" "$$stage/libqsgepaper.so"; \
	install -m 0755 scripts/takeover.sh "$$stage/scripts/takeover.sh"; \
	install -m 0644 packaging/receiver-takeover/external.manifest.json \
		"$$stage/external.manifest.json"; \
	install -m 0644 crates/rm-display-receiver/LICENSE-GPL-2.0-only \
		"$$stage/LICENSE-GPL-2.0-only"; \
	version="$$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)"; \
	manifest_version="$$(sed -n 's/.*"version": "\([^"]*\)".*/\1/p' \
		packaging/receiver-takeover/external.manifest.json)"; \
	test -n "$$version" && test "$$version" = "$$manifest_version" || { \
		printf 'manifest version %s does not match Cargo version %s\n' \
			"$$manifest_version" "$$version"; exit 2; }; \
	tar --sort=name --mtime='UTC 1970-01-01' --owner=0 --group=0 --numeric-owner \
		-C "$$stage" -czf "$$archive" \
		external.manifest.json LICENSE-GPL-2.0-only libqsgepaper.so libquill.so \
		rm-display-receiver scripts/takeover.sh; \
	printf 'package: %s\n' "$$archive"

run-receiver:
	cargo run -p rm-display-receiver -- \
		--listen=127.0.0.1:7420 --mock=960x1696 $(RECEIVER_ARGS)

run-cli:
	cargo run -p rm-display-cli -- \
		--host 127.0.0.1 $(ARGS)
