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
export CARGO_ENCODED_RUSTFLAGS="$(python3 - <<'PY'
import os, shlex
from pathlib import Path
flags = os.environ.get('CARGO_ENCODED_RUSTFLAGS')
flags = flags.split('\x1f') if flags else shlex.split(os.environ.get('RUSTFLAGS', ''))
for path, replacement in [(Path.home(), '/build/home'), (Path.cwd(), '/src/wagawebrtc'),
                          (Path(os.environ.get('CARGO_HOME', Path.home() / '.cargo')), '/build/cargo'),
                          (Path(os.environ.get('RUSTUP_HOME', Path.home() / '.rustup')), '/build/rustup')]:
    for prefix in dict.fromkeys([str(path.absolute()), str(path.resolve())]):
        flags.append('--remap-path-prefix=' + prefix + '=' + replacement)
print('\x1f'.join(flags))
PY
)"
rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
for target in aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios; do
    cargo build --locked --release --no-default-features --features apple-crypto --target "$target"
done

staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT
python3 scripts/license-notices.py > "$staging/THIRD_PARTY_NOTICES.txt"
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
    cp LICENSE "$framework/LICENSE"
    cp "$staging/THIRD_PARTY_NOTICES.txt" "$framework/THIRD_PARTY_NOTICES.txt"
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
python3 - "$artifact" <<'PY'
import os, sys
from pathlib import Path
prefixes = [str(Path.home()), str(Path.cwd()), os.environ.get('CARGO_HOME', ''), os.environ.get('RUSTUP_HOME', '')]
for path in Path(sys.argv[1]).rglob('*'):
    if path.is_file():
        data = path.read_bytes()
        if any(prefix and prefix.encode() in data for prefix in prefixes):
            sys.exit('Release rejected: local build path in ' + str(path))
PY
echo "Built $artifact"
