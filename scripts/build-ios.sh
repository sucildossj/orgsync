#!/usr/bin/env bash
#
# Builds the Rust core for iOS and drops it into the React Native module.
#
# Produces two things inside mobile/modules/p2p-native/ios:
#   * P2PMobileFFI.xcframework — the static library for device and simulator,
#     with the C header and a module map so Swift can import it.
#   * generated/p2p_mobile.swift — the uniffi bindings.
#
# Both are build outputs. Re-run this after changing anything in rust/.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODULE="$ROOT/mobile/modules/p2p-native"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

DEVICE="aarch64-apple-ios"
SIM_ARM="aarch64-apple-ios-sim"
SIM_X86="x86_64-apple-ios"

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }

command -v xcodebuild >/dev/null || { echo "xcodebuild not found; install Xcode." >&2; exit 1; }

say "Adding Rust targets"
rustup target add "$DEVICE" "$SIM_ARM" >/dev/null

say "Building the core for iOS device"
cargo build -p p2p-mobile --release --target "$DEVICE"

say "Building the core for the simulator"
cargo build -p p2p-mobile --release --target "$SIM_ARM"

# An Intel Mac (or an Intel simulator slice) needs the x86_64 build too. It is
# optional: fold it in only when that target is actually installed.
SIM_LIB="$ROOT/target/$SIM_ARM/release/libp2p_mobile.a"
if rustup target list --installed | grep -qx "$SIM_X86"; then
  say "Also building x86_64 simulator slice"
  cargo build -p p2p-mobile --release --target "$SIM_X86"
  SIM_LIB="$STAGE/libp2p_mobile_sim.a"
  lipo -create \
    "$ROOT/target/$SIM_ARM/release/libp2p_mobile.a" \
    "$ROOT/target/$SIM_X86/release/libp2p_mobile.a" \
    -output "$SIM_LIB"
fi

say "Generating Swift bindings"
# uniffi reads the metadata out of a built library; a host build is the
# cheapest one to point it at.
cargo build -q -p p2p-mobile
cargo run -q -p p2p-mobile --bin uniffi-bindgen -- generate \
  --library "$ROOT/target/debug/libp2p_mobile.dylib" \
  --language swift --out-dir "$STAGE/swift" --no-format

# Each xcframework slice needs its own headers directory, and the module map
# has to be called module.modulemap for `import p2p_mobileFFI` to resolve.
for slice in device sim; do
  mkdir -p "$STAGE/$slice/Headers"
  cp "$STAGE/swift/p2p_mobileFFI.h" "$STAGE/$slice/Headers/"
  cp "$STAGE/swift/p2p_mobileFFI.modulemap" "$STAGE/$slice/Headers/module.modulemap"
done

say "Assembling P2PMobileFFI.xcframework"
rm -rf "$MODULE/ios/P2PMobileFFI.xcframework"
xcodebuild -create-xcframework \
  -library "$ROOT/target/$DEVICE/release/libp2p_mobile.a" -headers "$STAGE/device/Headers" \
  -library "$SIM_LIB" -headers "$STAGE/sim/Headers" \
  -output "$MODULE/ios/P2PMobileFFI.xcframework" >/dev/null

mkdir -p "$MODULE/ios/generated"
cp "$STAGE/swift/p2p_mobile.swift" "$MODULE/ios/generated/p2p_mobile.swift"

say "Done."
echo
echo "  Next:  cd mobile && npm run ios"
echo
echo "  (The React Native CLI runs pod install itself. To do it by hand:"
echo "   cd mobile/ios && bundle exec pod install)"
