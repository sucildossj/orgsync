# OrgSync

An organisation's SQLite database, replicated peer-to-peer between phones.

Devices talk **directly** to each other. A small Rust seed server hands out
identity and helps devices find each other, but it is not in the data path:
once two devices know about each other, messages and rows flow between them,
and on a shared network they keep working with the server switched off
entirely.

```
┌──────────────────────────────┐
│  seed server  (Rust)         │   certificate authority
│  · signs device certificates │   rendezvous
│  · rendezvous + relay        │   relay
└──────────────┬───────────────┘   (optionally an always-on replica)
               │  enrol once, then discovery only
   ┌───────────┴───────────┐
   ▼                       ▼
┌────────┐  QUIC / TCP  ┌────────┐
│ phone  │◄────────────►│ phone  │   encrypted, authenticated, direct
│ SQLite │   CRDT sync  │ SQLite │
└────────┘              └────────┘
```

Built for teams whose work does not stop when the network does — field staff,
delivery routes, sites with bad signal. The app reads and writes its local
replica and converges with everyone else whenever it can.

## What's in here

| Path | What it is |
|---|---|
| `rust/p2p-core` | The whole protocol: libp2p transport, device certificates, CRDT replication. Shared by every participant. |
| `rust/seed-server` | Certificate authority, rendezvous, relay, and optional always-on replica. |
| `rust/p2p-mobile` | The uniffi FFI layer the phone app calls. |
| `rust/p2p-cli` | A desktop peer. Runs the same code as a phone — the fastest way to test. |
| `mobile/` | The React Native app. |
| `mobile/modules/p2p-native` | The native module: Swift and Kotlin bridges plus a typed TS API. |

The seed server and the phones run **the same protocol code**. They differ only
in configuration, so there is one implementation to reason about.

## Try it in five minutes

No phone required — `p2p-cli` is a full peer. You need [Rust](https://rustup.rs).

```bash
# 1. Start the seed server behind a public tunnel.
./scripts/tunnel.sh
#    → prints an enrolment URL and an admin token

# 2. In another terminal, mint two invite codes.
./scripts/invite.sh <url> <admin-token> ada
./scripts/invite.sh <url> <admin-token> bob

# 3. Join with two peers.
cargo run -p p2p-cli -- --data-dir ./ada  enroll --url <url> --code <ada's code>  --name ada
cargo run -p p2p-cli -- --data-dir ./bob  enroll --url <url> --code <bob's code>  --name bob

# 4. Watch one, post from the other.
cargo run -p p2p-cli -- --data-dir ./bob run --watch
cargo run -p p2p-cli -- --data-dir ./ada send "hello"
```

Bob prints the message. Nothing routed through the server to get there.

## Documentation

| | |
|---|---|
| [Installation](docs/install.md) | toolchains for Rust, Android and iOS |
| [The seed server](docs/seed-server.md) | creating an org, running it, exposing it, `--replica` |
| [Invites and devices](docs/invites.md) | invite codes, roles, enrolment, revocation |
| [The mobile app](docs/mobile-app.md) | building, configuring, adding your own tables |
| [Backup and retention](docs/backup.md) | protecting the root key, keeping replicas small |
| [Architecture](docs/architecture.md) | how the pieces fit |
| [Security](docs/security.md) | threat model, including what is *not* defended against |

## How replication works

Every replicated table gets triggers that record which rows were touched. A
flush turns each genuinely changed column into a **change record** stamped with
a hybrid logical clock and signed by the device that wrote it.

* **Merging is per column.** Two people editing different fields of the same
  row while apart both keep their edit — neither clobbers the other.
* **Conflicts on the same field** resolve last-writer-wins, with the device id
  breaking exact ties so every replica picks the same winner.
* **Deletes are versioned**, so a later edit from elsewhere resurrects the row
  with the columns nobody touched still intact.
* **Catch-up is a version vector.** A peer says what it already has; the other
  sends the difference. Changes reach devices that never met the author, by
  travelling through any third device.
* **Messages are just rows.** Chat is the `messages` table. There is no
  separate messaging system to keep in sync with the data one.

Register your own tables with `registerTable('invoices', 'id')` and write to
them with ordinary SQL.

## Security

* The seed server holds an Ed25519 **org root key** and signs a certificate per
  device binding its public key to a user, a role and an expiry.
* A device's key **is** its libp2p identity, so the certificate names exactly
  the key the Noise handshake authenticated. No extra challenge is needed, and
  a stolen certificate is useless without the matching private key.
* **Verification is offline.** Two phones on an office LAN authenticate each
  other with the server unreachable.
* **Revocation is a signed list** that peers gossip to each other, so a revoked
  phone is locked out of the LAN too — not only of the server.
* **Every change record is signed by its author.** Without this a member could
  forge another device's `origin` with a far-future timestamp and starve that
  device's real changes out of every version vector permanently.
* Invite codes are single-use, expiring, and redeemable only by a device that
  proves it holds the key being certified.

See [docs/security.md](docs/security.md) for the threat model, including what
is *not* defended against.

## Verifying it works

```bash
./scripts/verify.sh
```

Self-contained: it builds a throwaway organisation on its own ports, enrols two
devices, and checks that a row written on one — through a plain `sqlite3`
client, never through the server — turns up on the other. Then it checks the
parts that are supposed to refuse: an anonymous admin call, a spent invite
code, and a revoked certificate. It touches nothing you have running.

## Tests

```bash
cargo test --workspace          # 61 tests
cd mobile && npx tsc --noEmit && npx jest   # 6 app tests
```

The suite covers convergence (order independence, idempotency, delete and
resurrect, transitive propagation), certificate and revocation handling,
enrolment abuse, role enforcement, and end-to-end replication between real
libp2p nodes on the loopback.

## Status and limits

Working and tested end to end, and honest about where the edges are:

* **Every device holds the whole organisation's data.** There is no partial
  replication, so replica size is a product constraint. See
  [backup.md](docs/backup.md#retention-keeping-the-replica-small).
* **The change log is never compacted.** A row edited ten times keeps ten
  records. Fine for years of ordinary use; plan for it before it is not.
* **Cross-network sync needs a TCP tunnel or a public host.** An HTTPS-only
  tunnel carries enrolment but not libp2p.
* The release APK is signed with the checked-in debug keystore. Generate your
  own before distributing anything.

## Contributing

Issues and pull requests are welcome. Please run `cargo test --workspace` and
`./scripts/verify.sh` before opening one, and keep `cargo fmt` and `cargo
clippy` clean.

If you are touching the native bridge, read the note in
[mobile-app.md](docs/mobile-app.md#contributing-to-the-native-module) first —
every `@ReactMethod` must return `void`, and violating it crashes the app on
launch with an error that does not point at the cause.

## Licence

[Apache License 2.0](LICENSE).
