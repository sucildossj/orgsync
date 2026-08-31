# Installation

Everything here builds from source. There is one Rust workspace and one React
Native app; the phone and the server run the same protocol code, so the Rust
toolchain is required even if you only care about the app.

## Common

| Tool | Version | Why |
|---|---|---|
| Rust | stable, via [rustup](https://rustup.rs) | the whole protocol |
| Node.js | 20 or newer | the React Native app |
| Git | any | — |

```bash
git clone https://github.com/<you>/orgsync.git
cd orgsync
cargo build --workspace          # server, CLI peer and the FFI layer
```

That is enough to run a seed server and the desktop peer. The phone app needs
one more toolchain, below.

## Android

| Tool | Notes |
|---|---|
| Android Studio | for the SDK, or install the command-line tools |
| Android SDK | compile/target SDK 36 |
| Android NDK | set `ANDROID_NDK_HOME`, or the newest under `$ANDROID_HOME/ndk` is used |
| JDK | 17 |

`minSdk` is 24. The build script cross-compiles for `arm64-v8a`, `armeabi-v7a`
and `x86_64`, adding the Rust targets itself:

```bash
./scripts/build-android.sh       # native libs + Kotlin bindings
cd mobile && npm install && npm run android
```

If the SDK is not at `~/Library/Android/sdk`, set `ANDROID_HOME`.

> **32-bit x86 emulators are not supported.** The script builds `x86_64` but
> not `x86`, so an old 32-bit emulator image will fail to load the library.
> Use an `x86_64` image or a real device.

## iOS

| Tool | Notes |
|---|---|
| Xcode | with the iOS SDK and command-line tools |
| CocoaPods | `bundle install` in `mobile/` uses the checked-in `Gemfile` |

```bash
./scripts/build-ios.sh           # cross-compiles, builds the xcframework
cd mobile && npm install && npm run ios    # runs pod install for you
```

## Optional, for reaching devices over the internet

Neither is needed on a shared network, where devices find each other by mDNS.

| Tool | Gives you |
|---|---|
| [`cloudflared`](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/) | an HTTPS tunnel for the enrolment API — no account needed |
| [`bore`](https://github.com/ekzhang/bore) (`cargo install bore-cli`) or [`ngrok`](https://ngrok.com) | a raw TCP tunnel, which is what libp2p needs |

`scripts/tunnel.sh` uses whichever it finds and tells you which of the two you
got. See [seed-server.md](seed-server.md#exposing-it-to-the-internet).

`sqlite3` on your PATH is handy for inspecting a replica, and is used by
`scripts/verify.sh`.

## Checking the install

```bash
./scripts/verify.sh
```

Self-contained: it builds a throwaway organisation on its own ports, enrols two
devices, checks that a row written on one turns up on the other, and then checks
that the things which should be refused are refused. It touches nothing you have
running.

```bash
cargo test --workspace
cd mobile && npx tsc --noEmit && npx jest
```

## Troubleshooting

**`failed to find tool "aarch64-linux-android-clang"`** — a bare
`cargo build --target aarch64-linux-android` does not set up the NDK toolchain.
Use `./scripts/build-android.sh`, which exports the linker and compiler
variables Cargo needs.

**Gradle stalls downloading its distribution** — the wrapper's `networkTimeout`
is 10s, which is not always enough for a 128 MB download. Fetch it once by hand:

```bash
curl -fL -o ~/.gradle/wrapper/dists/gradle-9.0.0-bin/*/gradle-9.0.0-bin.zip \
  https://services.gradle.org/distributions/gradle-9.0.0-bin.zip
```

**The Android build succeeds but the app closes on launch** — you are probably
running a stale native library. Re-run `./scripts/build-android.sh` before
`assembleRelease`; the Kotlin bindings and the `.so` files must come from the
same build of the Rust core.
