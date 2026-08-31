# Trust model

## What identity means here

Every device generates an Ed25519 keypair on first run. That key is three
things at once:

* its **libp2p PeerId**, which the Noise handshake proves possession of on
  every connection;
* the subject of its **org certificate**;
* the signer of every **change record** it authors.

Collapsing these into one key is what makes the rest simple. When a peer
presents a certificate, checking `certificate.device_pub == remote_peer_id` is
enough to bind that certificate to the live, already-authenticated connection.
There is no challenge/response step to get wrong, and a copied certificate is
inert without the private key.

## The chain

```
org root key  (Ed25519, only on the seed server)
      │  signs
      ├── device certificate  →  device public key + user + role + expiry + serial
      └── revocation list     →  sorted serials + timestamp
```

`org_id` is `blake3(org_public_key)`. A device pins it at enrolment, so a
server cannot later claim to be a different organisation, and a certificate
from another org fails immediately.

**Verification is entirely offline.** Two phones on an office LAN with the
internet down authenticate each other from the pinned org id alone.

## Enrolment

An invite code is single-use, expiring, and bound to a user and role. To redeem
it the device must sign a statement covering the code, the device name, its own
public key and a timestamp. So:

* a leaked code cannot be redeemed against an attacker's key — the proof does
  not transfer;
* a captured request cannot be replayed later — the timestamp must be within
  five minutes of the server's;
* a code cannot be used twice.

## Why change records are signed

Each record carries `origin` (the authoring device) and a hybrid logical clock
stamp, and peers track "highest stamp seen per origin" as a version vector.

Without a signature, any member could publish a record claiming
`origin = someone-else` stamped years in the future. Every receiver's vector
for that device would jump past everything it will ever legitimately write, and
its real changes would never be requested again — a permanent, silent
partition of one device, caused by an ordinary member.

So every record is signed by its author, and the author's public key is
recovered from `origin` itself (libp2p inlines Ed25519 keys into the PeerId).
No key directory is needed, and it works for records relayed by a device that
has never met the author.

## What is *not* defended against

Being clear about this matters more than the list above.

* **A malicious member.** Roles are coarse: admin, member, read-only. A
  read-only device is genuinely held to it — see below — but a *member* may
  write or delete any row in any replicated table. There is no per-table or
  per-row authorisation. Anyone you enrol as a member can corrupt shared data.
* **Seed server compromise.** It holds the org root key; whoever has that can
  mint a certificate for any key and join as anyone. Protect the data
  directory, back it up, and treat it as the crown jewels. Losing it invalidates
  every certificate ever issued.
* **Data at rest.** The replica is an ordinary SQLite file inside the app
  sandbox, and the device key is a `0600` file beside it. Platform disk
  encryption is what protects them. Hardening: move the key into the iOS
  Keychain or Android Keystore, and switch rusqlite to `bundled-sqlcipher`.
* **Revocation latency.** Revocation is eventual. A revoked device stays usable
  against peers that have not yet seen the new list. Peers gossip it on every
  handshake, so it spreads quickly, but it is not instant.
* **Denial of service by a member.** No rate limiting on how many changes a
  device may author.
* **Clock manipulation within the drift window.** A member whose clock is fast
  by less than a minute wins conflicts it should not. Beyond that window the
  timestamp is honoured for ordering but not absorbed into other clocks.
* **Metadata.** Authenticated peers learn each other's peer ids, display names,
  user ids and roles.
* **The admin token** is a bearer token over HTTP. Put TLS in front of the seed
  server, and rate-limit `/v1/enroll` if it is exposed to the internet.

## How read-only is enforced

Changes are filtered on the **device that authored them**, not on whichever
peer handed them over. That distinction is the whole thing:

* Checking the sender would be too weak. Anti-entropy *pulls* — refusing a
  read-only device's pushes achieves nothing when a peer turns around and asks
  it for changes, and it would happily serve its own.
* Checking the sender would also be too strong. A read-only device is a full
  replica, and relaying everyone else's writes is exactly what it is for.

So every peer drops records authored by a device it knows to be read-only,
whether they arrive by direct push, by broadcast, or by anti-entropy pull, and
keeps relaying that device's copies of everybody else's records. Peers learn the
role from the certificate at handshake and remember it across restarts.

The limit: a peer that has never authenticated the read-only device cannot know
its role, so a record reaching such a peer only through third parties is
accepted. Every device that has actually met it will drop it, so the writes do
not survive, but convergence is not instantaneous in that corner.

## What the network cannot see

libp2p's Noise handshake authenticates the peer's static key and derives
session keys from ephemeral ones, so traffic is encrypted with forward secrecy.
A relay, a tunnel provider, or anyone on the path carries ciphertext addressed
to a peer identity they cannot impersonate.
