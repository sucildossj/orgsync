# Backup, restore and retention

Two very different things are worth protecting, and confusing them is the usual
mistake:

| | Loss means |
|---|---|
| **The org root key** | unrecoverable. Every certificate ever issued becomes unverifiable and every device must re-enrol into a new organisation. |
| **The replicated data** | recoverable from any other device, as long as one still holds it. |

The first is small, static, and irreplaceable. The second is large, changing,
and already replicated.

## Backing up the organisation

Everything that matters lives in the seed server's `--data-dir`:

```
seed-data/
  seed.db          org root key, admin token, invites, device registry
  replica.db       the org's data — only with --replica
  logs/
```

`seed.db` is the one that cannot be reconstructed. Back it up **the moment you
run `init`**, before a single device enrols.

SQLite files must not be copied while being written. Use the online backup API:

```bash
sqlite3 seed-data/seed.db ".backup '/secure/backup/seed-$(date +%F).db'"
```

Encrypt it — it holds the root private key and the admin token — and store it
somewhere separate from the server. A copy in a password manager or an encrypted
bucket with versioning is appropriate. This file is a few dozen KB; there is no
excuse for having only one copy.

```bash
# restore
systemctl stop orgsync-seed
cp /secure/backup/seed-2026-08-31.db seed-data/seed.db
systemctl start orgsync-seed
```

Devices reconnect and continue. Their certificates are still valid because the
key that signed them is the same one.

### Rotating the admin token

Reprint it with `seed-server --data-dir ./seed-data token`. It is stored in
`seed.db`; treat it like a password and keep it out of shell history and CI
logs.

## Backing up the data

With `--replica` the seed server holds a full copy, which makes it the natural
backup target:

```bash
sqlite3 seed-data/replica.db ".backup '/secure/backup/replica-$(date +%F).db'"
```

Without `--replica` the server stores nothing, and your only copies are on
phones. That is a legitimate configuration for a small team who are always in
the same place — but it means a snapshot exists only where somebody's phone is.
For anything you would be upset to lose, run with `--replica`.

A `replica.db` restored from backup rejoins as an ordinary peer: it announces
what it has via a version vector, and the mesh sends it the difference. Restoring
a stale copy is safe — it catches up rather than overwriting anyone.

### Exporting for other tools

The replica is a normal SQLite database. Read it with anything:

```bash
sqlite3 seed-data/replica.db \
  ".headers on" ".mode csv" \
  "SELECT * FROM invoices WHERE issued_at_ms > strftime('%s','now','-30 days')*1000;"
```

Read freely. **Do not write to it directly** — writes must go through the
replication engine, or they are captured incorrectly and never reach anyone.
Use `p2p-cli sql` for a guaranteed read-only query.

## Retention: keeping the replica small

Every device holds the whole organisation's data, so replica size is a product
constraint, not just a server one. Two facts drive it:

- Each changed **column** is stored as a signed change record of roughly 205
  bytes, of which about 18 bytes is your value. The rest is the origin peer id,
  an Ed25519 signature, the hybrid logical clock and the key.
- Nothing prunes that log. A row edited ten times keeps ten records forever.

A six-column invoice therefore costs about 1.2 KB of change log on top of ~150
bytes of row. At a thousand documents a day that is roughly 500 MB a year, per
device, before updates.

### Do not archive with deletes

The obvious approach — upload old rows, then `DELETE` them — makes things worse.
`_p2p_tombstone` is keyed `(tbl, pk)` and keeps one row per deleted record
permanently, because a tombstone must outrank every surviving cell for the
delete to converge; and each delete appends another signed change record. You
would trade rows you can compress and offload for tombstones you can never
remove.

### Prune by policy instead

Archival is a **change of scope**, not a delete: the set of rows the org
replicates shrinks. Express that as a small replicated table, and every peer
reaches the same conclusion independently — no per-row messages, no tombstones.

```sql
CREATE TABLE retention (
  tbl                TEXT PRIMARY KEY,
  date_col           TEXT    NOT NULL,
  horizon_days       INTEGER NOT NULL,
  archived_before_ms INTEGER NOT NULL   -- the watermark; only moves forward
);
```

Each peer then evicts locally, inside the guard the replication engine already
uses when applying remote changes, so the capture triggers never fire:

```sql
UPDATE _p2p_guard SET v = 1 WHERE id = 1;
DELETE FROM invoices WHERE issued_at_ms < :watermark;
DELETE FROM _p2p_change
 WHERE tbl = 'invoices' AND pk NOT IN (SELECT id FROM invoices);
UPDATE _p2p_guard SET v = 0 WHERE id = 1;
```

Nothing is broadcast and the version vector is untouched, because it is keyed by
`(origin, hlc)` rather than by row.

### Which tables

Retention is per table: a table with a `retention` row ages out at its own
horizon, one without is kept forever.

| Tables | Policy |
|---|---|
| customers, suppliers, vehicles, drivers, products, price lists | keep forever — bounded, and needed offline constantly |
| trips | archive, short horizon |
| orders, order_lines | archive |
| invoices, payments | archive, longer horizon for accounting |

Three rules that are not preferences:

- **Never archive a table that archived rows point at.** If old invoices are in
  cold storage and `customers` is archived too, those objects reference
  customers that exist nowhere.
- **Age children on the parent's date.** `order_lines` must follow the order's
  date, or one order is split across the horizon.
- **Denormalise on the way out.** Snapshot customer name, address and unit price
  into the archived invoice, so re-rendering it years later does not show
  today's values.

The archiver must run on one authority — the `--replica` server — in strict
order: export, verify the readback, publish an index, then advance the
watermark. Never move the watermark before the export is verified, and never
move it backwards.

> Archive is not deletion. Statutory record-retention periods apply to the data
> wherever it lives; check what your accountant needs before choosing a horizon.
> The horizon only decides what sits on a phone.
