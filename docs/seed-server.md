# The seed server

The seed server is the organisation's certificate authority. It signs a
certificate for each device, helps devices find each other, and relays for those
that cannot connect directly. It is deliberately **not in the data path** — once
two devices know about each other, rows flow between them, and on a shared
network they keep syncing with the server switched off.

It runs the same `p2p-core` code as the phones. It differs only in
configuration.

## Creating the organisation

Run this once. It generates the org root key.

```bash
cargo run --release -p seed-server -- \
  --data-dir ./seed-data init --org-name "Kumar Poultry"
```

```
Organisation created.

  name         Kumar Poultry
  org id       ff68752d4f71…
  server peer  12D3KooWFAki…
  admin token  K7Q2M9XTBW4R
```

> **Back up `./seed-data` before you do anything else.** It holds the org root
> key. Lose it and every certificate ever issued becomes unverifiable — there is
> no recovery, and every device must re-enrol into a new organisation. See
> [backup.md](backup.md).

The **admin token** authenticates the admin HTTP API. Reprint it any time with
`seed-server --data-dir ./seed-data token`. Treat it like a password: anyone
holding it can mint invites and revoke devices.

## Running it

```bash
cargo run --release -p seed-server -- --data-dir ./seed-data run \
  --announce seed.example.com \
  --replica
```

| Flag | Default | What it does |
|---|---|---|
| `--http-port` | `8080` | enrolment and admin API |
| `--http-bind` | `0.0.0.0` | HTTP bind address |
| `--tcp-port` | `4001` | libp2p over TCP |
| `--quic-port` | `4001` | libp2p over QUIC |
| `--announce` | — | public hostname or IP devices should dial. **Repeatable** |
| `--replica` | off | keep a full copy of the org's data |
| `--cert-days` | `365` | lifetime of newly issued device certificates |

`--data-dir` is global and also reads `SEED_DATA_DIR`.

### `--announce` is not optional in practice

Without it the server can only advertise addresses it discovers locally, which
are usually private LAN addresses no phone can reach. Pass the public hostname
or IP. A bare host expands to both transports:

```
--announce seed.example.com
  → /dns4/seed.example.com/udp/4001/quic-v1/p2p/<peer id>
  → /dns4/seed.example.com/tcp/4001/p2p/<peer id>
```

A full multiaddr is taken as-is, and the `/p2p/<peer id>` suffix is appended if
you leave it off. Devices receive these addresses in their enrolment and dial
them automatically.

### `--replica` decides whether offline users can reach each other

Without it the server stores nothing. Two people who are **never online at the
same time never exchange data** — there is nothing holding the changes in
between. With it, the server is an always-on member of the org and hands over
whatever a device missed when it next connects.

For anything beyond a same-room demo, run with `--replica`.

## Exposing it to the internet

A phone needs two different things from the server, and most tunnel products
carry only the first:

1. **Enrolment** — ordinary HTTPS. Any HTTP tunnel works.
2. **Rendezvous and relay** — raw TCP carrying libp2p. Needs a TCP tunnel.

`scripts/tunnel.sh` sets up whatever your installed tools support and tells you
plainly which of the two you got:

```bash
./scripts/tunnel.sh                       # ORG_NAME=… to name a new org
./scripts/tunnel.sh --http-port 9000 --tcp-port 4002
```

```
  Enrolment URL   https://<random>.trycloudflare.com
  Admin token     K7Q2M9XTBW4R
  Cross-network peer-to-peer: yes
```

If that last line says **no**, you have HTTP only: devices can enrol from
anywhere and any two on the same network will find each other by mDNS and sync
directly, but two phones on *different* networks cannot reach each other.
Install `bore` (`cargo install bore-cli`) or `ngrok` and re-run.

Routing through someone else's tunnel does not expose your data. libp2p
authenticates the remote peer with a Noise handshake and encrypts end to end, so
the tunnel operator carries ciphertext it cannot read or forge.

Quick tunnels are for development. The URL changes every restart, which
invalidates anything you have configured to point at it.

### In production

Give the server a real hostname, open `8080/tcp` (behind TLS) and `4001` on both
TCP and UDP, and run it under a service manager. Then `--announce your.host`
needs no tunnel at all.

## Day-to-day administration

Both a CLI and an HTTP API are available. The CLI needs filesystem access to
`--data-dir`; the API needs the admin token.

```bash
seed-server --data-dir ./seed-data invite --user ravi --role member --hours 48
seed-server --data-dir ./seed-data devices
seed-server --data-dir ./seed-data revoke --serial 4
seed-server --data-dir ./seed-data token
```

See [invites.md](invites.md) for roles, enrolment and revocation.

## Health and logs

```bash
curl http://127.0.0.1:8080/health          # {"status":"ok"}
curl http://127.0.0.1:8080/v1/org          # org id, public key, bootstrap addrs
```

`/v1/org` is the quickest way to confirm what addresses devices are being told
to dial. If `bootstrap` contains only private addresses, your `--announce` is
missing or wrong.

Logging is `tracing`, controlled by `RUST_LOG`:

```bash
RUST_LOG=seed_server=debug,p2p_core=debug cargo run -p seed-server -- …
```

`scripts/tunnel.sh` writes `server.log`, `cloudflared.log` and `bore.log` into
`<data-dir>/logs/`.

## Troubleshooting

**A device enrols but never connects.** Check `/v1/org` — if `bootstrap` has no
public address, add `--announce`. If cross-network P2P is off, the device can
only reach peers on its own network.

**Enrolment fails with a DNS error.** Some ISPs block tunnel domains. Airtel in
India, for example, returns NXDOMAIN for `*.trycloudflare.com` and redirects
`bore.pub` to a block page. Set the phone's Private DNS to `one.one.one.one`
(Settings → Network → Private DNS) and the machine's resolver to `1.1.1.1`.

**`the seed server exited`** from `tunnel.sh` — read
`<data-dir>/logs/server.log`. The usual cause is a port already in use.
