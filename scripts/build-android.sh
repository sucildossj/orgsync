#!/usr/bin/env bash
#
# Builds the Rust core for Android and drops it into the React Native module.
#
# Produces:
#   * android/src/main/jniLibs/<abi>/libp2p_mobile.so — one per architecture
#   * android/src/main/java/uniffi/p2p_mobile/p2p_mobile.kt — the bindings
#
# Needs the Android NDK. Set ANDROID_NDK_HOME, or let this find the newest one
# under $ANDROID_HOME/ndk.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODULE="$ROOT/mobile/modules/p2p-native"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

# Matches minSdk in the module's build.gradle.
API="${ANDROID_API:-24}"

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }

SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
NDK="${ANDROID_NDK_HOME:-}"
if [[ -z "$NDK" ]]; then
  NDK="$(ls -d "$SDK"/ndk/* 2>/dev/null | sort -V | tail -1 || true)"
fi
[[ -n "$NDK" && -d "$NDK" ]] || {
  echo "Android NDK not found. Install one in Android Studio, or set ANDROID_NDK_HOME." >&2
  exit 1
}
say "Using NDK $NDK"

HOST_TAG="darwin-x86_64"
[[ "$(uname)" == "Linux" ]] && HOST_TAG="linux-x86_64"
TOOLCHAIN="$NDK/toolchains/llvm/prebuilt/$HOST_TAG"
[[ -d "$TOOLCHAIN" ]] || { echo "No prebuilt toolchain at $TOOLCHAIN" >&2; exit 1; }

# rust target | jni abi | clang prefix
TARGETS=(
  "aarch64-linux-android|arm64-v8a|aarch64-linux-android"
  "armv7-linux-androideabi|armeabi-v7a|armv7a-linux-androideabi"
  "x86_64-linux-android|x86_64|x86_64-linux-android"
)

say "Adding Rust targets"
for entry in "${TARGETS[@]}"; do
  rustup target add "${entry%%|*}" >/dev/null
done

export AR="$TOOLCHAIN/bin/llvm-ar"
export RANLIB="$TOOLCHAIN/bin/llvm-ranlib"

for entry in "${TARGETS[@]}"; do
  IFS='|' read -r target abi prefix <<<"$entry"
  CLANG="$TOOLCHAIN/bin/${prefix}${API}-clang"
  [[ -x "$CLANG" ]] || { echo "Missing compiler $CLANG" >&2; exit 1; }

  # Cargo reads the linker from a target-shaped variable name.
  upper="$(echo "$target" | tr 'a-z-' 'A-Z_')"
  under="$(echo "$target" | tr '-' '_')"
  export "CARGO_TARGET_${upper}_LINKER=$CLANG"
  export "CC_${under}=$CLANG"
  export "AR_${under}=$TOOLCHAIN/bin/llvm-ar"

  # Android 15 can use 16 KB memory pages; aligning here keeps the library
  # loadable on those devices.
  export "CARGO_TARGET_${upper}_RUSTFLAGS=-C link-arg=-Wl,-z,max-page-size=16384"

  say "Building for $abi"
  cargo build -p p2p-mobile --release --target "$target"

  mkdir -p "$MODULE/android/src/main/jniLibs/$abi"
  cp "$ROOT/target/$target/release/libp2p_mobile.so" \
     "$MODULE/android/src/main/jniLibs/$abi/libp2p_mobile.so"
done

say "Generating Kotlin bindings"
cargo build -q -p p2p-mobile
cargo run -q -p p2p-mobile --bin uniffi-bindgen -- generate \
  --library "$ROOT/target/debug/libp2p_mobile.dylib" \
  --language kotlin --out-dir "$STAGE/kotlin" --no-format

DEST="$MODULE/android/src/main/java"
mkdir -p "$DEST"
rm -rf "$DEST/uniffi"
cp -R "$STAGE/kotlin/uniffi" "$DEST/uniffi"

say "Done."
echo
echo "  Next:  cd mobile && npm run android"
