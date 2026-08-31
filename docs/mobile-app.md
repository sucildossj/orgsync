# The mobile app

`mobile/` is a React Native app. `mobile/modules/p2p-native` is the native
module: a Swift bridge, a Kotlin bridge, and a typed TypeScript API over the
uniffi bindings generated from `rust/p2p-mobile`.

The app talks only to its **local** replica. Getting rows to and from other
devices is the Rust core's job, and it happens whether or not any server is
reachable.

## Building

The Rust core must be cross-compiled first. The gradle and Xcode builds consume
its output; they do not produce it.

```bash
./scripts/build-android.sh      # arm64-v8a, armeabi-v7a, x86_64 + Kotlin bindings
cd mobile && npm install
npm run android

./scripts/build-ios.sh          # xcframework + Swift bindings
npm run ios                     # runs pod install for you
```

Re-run the build script whenever anything under `rust/` changes. The generated
bindings and the compiled library must come from the same build, or the app will
fail at runtime in confusing ways.

### A standalone APK

`npm run android` produces a debug build that loads JavaScript from Metro, so it
needs your development machine. To hand someone an APK that runs on its own:

```bash
./scripts/build-android.sh
cd mobile/android && ./gradlew assembleRelease
# → app/build/outputs/apk/release/app-release.apk
```

The release variant compiles the JS into the APK with Hermes, so no Metro is
needed. It is signed with the checked-in **debug keystore** — fine for testing,
not for distribution. Generate your own before shipping anywhere real; see
[the React Native signing guide](https://reactnative.dev/docs/signed-apk-android).

## Configuring a device

There is nothing to configure at build time. The app opens on an enrolment
screen and asks for:

| Field | |
|---|---|
| **Server address** | the seed server's base URL, e.g. `https://seed.example.com` |
| **Invite code** | single-use, from an admin — see [invites.md](invites.md) |
| **Device name** | shown to other members in the peer list |

On Join, the device generates its key, sends only the public half to be signed,
stores the certificate, and starts its node. Everything after that is
peer-to-peer.

If you are testing repeatedly and want the server address prefilled, set the
`useState` default in `mobile/src/screens/EnrollScreen.tsx`. Do not prefill an
invite code: they are single-use, so a baked-in code works exactly once and then
every later install fails with `already been used`.

After enrolment the app is three tabs — messages, a shared list, and network
status. The **Network** tab is the diagnostic surface: connected peers,
replicated change count, this device's listen addresses, and a **Sync now**
button.

## Reading the Network tab

| Shows | Means |
|---|---|
| `Starting…`, `offline` | the node failed to start — an error banner explains why |
| `alone` | running, no peers reachable yet |
| `Connected 1 of 1` | connected to the seed server, or to another device |
| `No external address yet` | nobody outside this network can dial you; normal without a relay |

## Adding your own tables

Chat is not a separate system — it is the `messages` table. Register any table
with a single-column primary key and write to it with ordinary SQL:

```ts
await P2p.registerTable('invoices', 'id');
await P2p.execute(
  `INSERT INTO invoices (id, customer, amount, issued_at_ms)
   VALUES (?1, ?2, ?3, ?4)`,
  [id, customer, amount, Date.now()],
);
```

Writes must go through `execute` rather than a separate SQLite driver, so they
are captured and replicated. Reads can go anywhere; `dbPath()` returns the file
if you want to open it read-only yourself.

Every replicated table needs **exactly one** primary key column. Registration is
per device, so register the same set everywhere.

## Contributing to the native module

One rule that is easy to violate and produces a crash with no obvious cause:

> Every `@ReactMethod` must return `void`.

With the New Architecture the TurboModule interop layer parses these
annotations at startup and rejects the whole module if any method returns a
value — a Kotlin expression body like

```kotlin
@ReactMethod
fun initialize(options: ReadableMap, promise: Promise) = scope.launch { … }
```

returns `Job`, not `Unit`, and the app dies on launch with
`TurboModule system assumes returnType == void iff the method is synchronous`.
Use a block body. `javap -p` on the compiled class is the quickest way to check.

## Troubleshooting

**The app closes immediately on launch.** Get the actual reason:

```bash
adb logcat -b crash -d          # native and JS crashes
adb logcat -s OrgSyncNative     # failed calls into the Rust core
adb logcat -s OrgSyncCore       # the Rust core's own tracing
```

`OrgSyncCore` is quiet by default; raise it with `RUST_LOG` semantics compiled
in at `info,libp2p=warn`.

**Enrolment fails with a DNS error.** Some ISPs block tunnel domains — Airtel in
India returns NXDOMAIN for `*.trycloudflare.com`. Set Private DNS to
`one.one.one.one` under Settings → Network.

**The keyboard covers the message box.** Android 15 runs apps edge to edge, so
the window is no longer resized for the keyboard and `KeyboardAvoidingView`
alone does nothing. The app measures the keyboard directly and pads the root;
if you add another screen with a text input at the bottom, do the same rather
than reaching for `adjustResize`.

**Text renders mirrored in a list.** Do not use `inverted` on a `FlatList` with
counter-`transform` on rows — the cancellation is unreliable on Android. Render
oldest-first and scroll to the end.
