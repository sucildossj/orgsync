# How it fits together

## One protocol, three deployments

`p2p-core` contains everything: transport, handshake, certificate checking and
replication. The seed server, the desktop CLI and the phone all run it, and
differ only in `NodeConfig`:

| | mDNS | relay client | relay server | DHT mode | stores data |
|---|---|---|---|---|---|
| phone / CLI | yes | yes | no | client | yes |
| seed server | no | no | yes | server | optional (`--replica`) |

That is why "the seed server is just a well-known peer" is literally true
rather than a figure of speech.

## The path a write takes

```
app calls execute()                    ← ordinary SQL
        │
        ▼
capture trigger stages (table, pk)     ← fires for any writer of the file
        │
        ▼
flush: diff row against CRDT state     ← unchanged columns produce nothing
        │
        ▼
one signed ChangeRecord per changed column
        │
        ├──► direct push to each connected peer   (fast, acknowledged)
        └──► gossipsub broadcast                  (reaches indirect peers)
                    │
                    ▼
        peer verifies signature, stores record,
        resolves last-writer-wins, re-materialises the row
```

Duplicates are free: `(origin, stamp)` is unique, so applying a record twice is
a no-op. That is what lets both delivery routes run at once without
coordination.

## Why the app's tables are a projection

The authoritative state is `_p2p_cell` — one row per (table, primary key,
column) holding the winning value and the stamp that won. Your tables are
rebuilt from it.

Keeping both costs storage but buys the behaviour people expect: when a row is
deleted on one device and edited on another, the edit (if it is newer) brings
the row back **with the columns nobody touched still populated**. Had the cells
been dropped along with the row, resurrection would produce a half-empty
record.

## Catching up

A peer sends its version vector — "the highest stamp I hold from each device".
The other returns everything above those marks, oldest first, capped per page.
Because records are emitted in stamp order, any prefix is a valid batch: apply
it, advance the vector, ask again.

Changes reach devices that never met the author. If A syncs with B, and B later
syncs with C, C ends up with A's writes. An organisation converges as long as
its devices overlap in pairs over time — they never all need to be online at
once.

A device that receives a change for a table it does not know about **stores and
forwards it anyway**, so an out-of-date build never becomes a hole in the mesh.

## Finding each other

1. **mDNS** on the local network. No server involved; this is the path that
   keeps working when the internet is down.
2. **The seed server** as a rendezvous point with a stable address, plus a
   Kademlia DHT on a private protocol name (never the public IPFS DHT).
3. **Relay + DCUtR** when both devices are behind NAT: the seed server lends
   its address, then the two attempt a hole punch to get direct.

## Why enrolment is HTTP and not libp2p

A device that has not joined yet has no certificate, so it has nothing to
authenticate a p2p connection with. Enrolment is therefore plain HTTP JSON —
and keeping it there means the mobile binary needs no TLS stack, because React
Native's own `fetch` makes the call:

```
Rust  beginEnrollment()  ─► signed JSON ─►  JS fetch ─► seed server
Rust  completeEnrollment() ◄─ certificate ◄─────────────┘
```

The certificate is verified against the pinned org key inside Rust before
anything is written, so a hostile server cannot enrol the device into an
organisation it does not control.

## The FFI boundary

Rows and events cross as JSON strings rather than as generated record types.
The surface is about fourteen methods and it does not move when a table gains a
column or an event gains a field — no regenerating and re-linking native
bindings on two platforms for a schema change. TypeScript restores the types
on the other side.
