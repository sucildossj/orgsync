# OrgSync — mobile client

A React Native app over a peer-to-peer SQLite replica. The app only ever talks
to the local database; getting rows to and from other devices is the Rust
core's job, and it happens whether or not any server is reachable.

## Layout

```
App.tsx                       enrolment gate, then three tabs
src/P2pProvider.tsx           owns the node lifecycle and the event stream
src/screens/                  enrol · chat · shared records · network status
modules/p2p-native/           the native module (a local package, autolinked)
  ├── src/                    the typed API the app imports
  ├── ios/                    Swift bridge + generated bindings + xcframework
  └── android/                Kotlin bridge + generated bindings + .so files
```

Everything under `modules/p2p-native/ios/generated`,
`modules/p2p-native/ios/P2PMobileFFI.xcframework`,
`modules/p2p-native/android/src/main/jniLibs` and
`.../java/uniffi` is a **build output**. The scripts regenerate them; they are
git-ignored.

## Building

The Rust core has to be cross-compiled before the app will link.

```bash
../scripts/build-ios.sh        # xcframework for device + simulator, Swift bindings
npm run ios                    # the CLI runs pod install for you

../scripts/build-android.sh    # arm64-v8a, armeabi-v7a, x86_64, Kotlin bindings
npm run android
```

Re-run the matching script after any change under `rust/`. JavaScript changes
need only a Metro reload.

## Using the API

```ts
import * as P2p from 'p2p-native';

await P2p.initialize({});
await P2p.enroll({ seedUrl, inviteCode, deviceName: 'My phone' });
await P2p.start();

const stop = P2p.addListener(event => {
  if (event.type === 'synced') refresh();   // rows are already merged
});

// Ordinary SQL. The write is stamped and pushed to peers before this resolves.
await P2p.execute('INSERT INTO records (id, title) VALUES (?1, ?2)', [id, title]);
const rows = await P2p.records();
```

Bring your own tables into replication with
`P2p.registerTable('invoices', 'id')`. The table must already exist and have a
single-column primary key — use an opaque id, because the primary key is what
identifies a row across devices.

## Tests

```bash
npx tsc --noEmit
npx jest
```

`__mocks__/p2p-native.js` stands in for the native module, so the tests
exercise the real provider and screens — enrolling, starting, handling sync
events — with no native code involved.

## Notes

* The bridge is a legacy React Native module on purpose: it works unchanged
  under both the old bridge and the new architecture's interop layer, and needs
  no codegen step.
* Rows and events cross the boundary as JSON strings. That keeps the FFI
  surface at about fourteen methods that do not move when a table gains a
  column, so a schema change never means regenerating and re-linking native
  bindings on two platforms.
* The device key lives in a `0600` file in the app's private directory, and the
  iOS data directory is excluded from iCloud backup — restoring it onto a
  second device would give two phones the same identity. Moving the key into
  the Keychain or Keystore is the natural hardening step.
