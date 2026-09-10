#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
if [ -n "${WAGA_TOOLCHAIN_DIR:-}" ]; then
    export RUSTUP_HOME="$WAGA_TOOLCHAIN_DIR/rustup"
    export CARGO_HOME="$WAGA_TOOLCHAIN_DIR/cargo"
fi
cd "$project_dir"
cargo fmt --all -- --check
cargo test --locked
cargo test --manifest-path vendor/str0m-0.23.1/Cargo.toml --locked --features _internal_test_exports --lib --test bwe
cargo clippy --locked --all-targets -- -D warnings
./scripts/build-xcframework.sh
xcodebuild -scheme WagaWebRTC \
    -destination 'generic/platform=iOS Simulator' \
    -derivedDataPath .build/verify-simulator \
    build CODE_SIGNING_ALLOWED=NO
xcodebuild -scheme WagaWebRTC \
    -destination 'generic/platform=iOS' \
    -derivedDataPath .build/verify-device \
    build CODE_SIGNING_ALLOWED=NO
