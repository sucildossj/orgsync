# Invites, devices and revocation

A device joins an organisation exactly once, by redeeming a single-use invite
code. What it gets back is a certificate binding its public key to a user, a
role and an expiry. From then on it authenticates to other devices directly,
with the server unreachable.

## Minting an invite

Either the CLI, on the machine holding `--data-dir`:

```bash
seed-server --data-dir ./seed-data invite \
  --user ravi --name "Ravi — deliveries" --role member --hours 48
```

or the admin API, from anywhere:

```bash
./scripts/invite.sh <url> <admin-token> ravi member
```

```
  Invite code for ravi (member):

      W66ZY-QZFES-EA3KY-C93E2

  Single use, valid 24h.
```

| Option | Default | |
|---|---|---|
| `--user` | required | who this is for; appears on the device list |
| `--name` | `--user` | display name |
| `--role` | `member` | `admin`, `member` or `readonly` |
| `--hours` | `24` | how long the code stays redeemable |

`invite.sh` takes `<url> <admin-token> <user> [role]` and always uses a 24-hour
TTL. Use the CLI when you need a longer window — handing a code to someone in
another city who will not install today is exactly that case.

## Roles

| Role | Can |
|---|---|
| `readonly` | receive and read data; its writes are dropped by every peer |
| `member` | read and write |
| `admin` | read and write, plus mint invites and revoke devices |

Roles are carried in the certificate and enforced by **every peer**, not only by
the server. A read-only device that tries to author changes has them rejected
during sync, so the rule holds on a LAN with the server switched off.

## What the code is

20 characters drawn from a 30-character alphabet that omits every confusable
glyph:

```
23456789ABCDEFGHJKMNPQRSTVWXYZ
```

No `0`/`O`, no `1`/`I`/`L`. Codes get read aloud over the phone and typed on a
keypad, so this matters more than it looks. That is about 2⁹⁸ of entropy.

The dashes are presentation only. Lookup normalises the code first, so
`w66zy qzfes ea3ky c93e2` and `W66ZYQZFESEA3KYC93E2` are the same code.

## Redemption

Every check happens in one transaction: the code must exist, be unused, be
unexpired, and — the important one — it is **redeemable only by a device that
proves it holds the key being certified**. A leaked code cannot be used to mint
a certificate for somebody else's key.

Then the code is marked spent and the certificate is issued.

**Codes are single-use.** This trips people up when testing: mint one per
device, up front. A second phone using the first phone's code gets
`this invite code has already been used`, which is the system working correctly.

Enrolment itself is an ordinary HTTPS call made by the *app*, not by Rust — the
device generates its key locally, sends only the public half, and the server
signs it. The private key never leaves the device.

## Listing devices

```bash
seed-server --data-dir ./seed-data devices
```

```bash
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  https://seed.example.com/v1/admin/devices
```

Each device has a **serial**, assigned in order. Serial 0 is the server's own
certificate; devices start at 1. You need the serial to revoke.

## Revoking

```bash
seed-server --data-dir ./seed-data revoke --serial 4
```

Revocation is a **signed list that peers gossip to each other**, not a server
lookup. A revoked phone is locked out of the office LAN too, not merely of the
server — which is the point, since the LAN is where it would otherwise still be
trusted.

Propagation is as fast as devices meet each other. A device that never comes
online again is simply never told, which does not matter: it cannot connect to
anyone without being rejected.

Revoke when a phone is lost or someone leaves. Certificates also expire on their
own after `--cert-days` (365 by default).

## Troubleshooting

| Message | Cause |
|---|---|
| `unknown invite code` | typo, or minted against a different organisation |
| `this invite code has already been used` | single-use; mint another |
| `this invite code has expired` | past `--hours`; mint another |

If enrolment fails with a network or DNS error rather than one of these, the
problem is reaching the server at all — see
[seed-server.md](seed-server.md#troubleshooting).
