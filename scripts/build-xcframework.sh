#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
if [ -n "${WAGA_TOOLCHAIN_DIR:-}" ]; then
    export RUSTUP_HOME="$WAGA_TOOLCHAIN_DIR/rustup"
    export CARGO_HOME="$WAGA_TOOLCHAIN_DIR/cargo"
fi
export IPHONEOS_DEPLOYMENT_TARGET=15.0

if ! xcodebuild -version >/dev/null 2>&1; then
    echo "Full Xcode is required. Select it with: sudo xcode-select -s /Applications/Xcode.app" >&2
    exit 1
fi

cd "$project_dir"
rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
for target in aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios; do
    cargo build --locked --release --no-default-features --features apple-crypto --target "$target"
done

staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT
lipo -create \
    target/aarch64-apple-ios-sim/release/libwaga_webrtc_core.a \
    target/x86_64-apple-ios/release/libwaga_webrtc_core.a \
    -output "$staging/libwaga_webrtc_sim.a"

make_framework() {
    framework="$1/CWagaWebRTC.framework"
    library="$2"
    platform="$3"
    mkdir -p "$framework/Headers" "$framework/Modules"
    cp "$library" "$framework/CWagaWebRTC"
    cp include/waga_webrtc.h "$framework/Headers/"
    cp include/framework.modulemap "$framework/Modules/module.modulemap"
    plutil -create xml1 "$framework/Info.plist"
    plutil -insert CFBundleExecutable -string CWagaWebRTC "$framework/Info.plist"
    plutil -insert CFBundleIdentifier -string com.wagastrim.CWagaWebRTC "$framework/Info.plist"
    plutil -insert CFBundleInfoDictionaryVersion -string 6.0 "$framework/Info.plist"
    plutil -insert CFBundleName -string CWagaWebRTC "$framework/Info.plist"
    plutil -insert CFBundlePackageType -string FMWK "$framework/Info.plist"
    plutil -insert CFBundleShortVersionString -string 1.0 "$framework/Info.plist"
    plutil -insert CFBundleVersion -string 1 "$framework/Info.plist"
    plutil -insert MinimumOSVersion -string 15.0 "$framework/Info.plist"
    plutil -insert CFBundleSupportedPlatforms -array "$framework/Info.plist"
    plutil -insert CFBundleSupportedPlatforms.0 -string "$platform" "$framework/Info.plist"
}

make_framework "$staging/device" \
    target/aarch64-apple-ios/release/libwaga_webrtc_core.a iPhoneOS
make_framework "$staging/simulator" \
    "$staging/libwaga_webrtc_sim.a" iPhoneSimulator

artifact="$project_dir/Artifacts/CWagaWebRTC.xcframework"
rm -rf "$artifact"
xcodebuild -create-xcframework \
    -framework "$staging/device/CWagaWebRTC.framework" \
    -framework "$staging/simulator/CWagaWebRTC.framework" \
    -output "$artifact"
echo "Built $artifact"
