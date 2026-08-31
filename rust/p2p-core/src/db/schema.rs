//! Replication bookkeeping tables and the triggers that capture local edits.

/// Everything the replicator needs, alongside whatever tables the app owns.
///
/// `_p2p_cell` holds the authoritative CRDT state; the app's own tables are a
/// materialised projection of it. Keeping both means a row deleted on one
/// device can still be resurrected by a later edit arriving from another,
/// because the surviving column values were never thrown away.
pub const MIGRATION_V1: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS _p2p_meta (
  k TEXT PRIMARY KEY,
  v TEXT NOT NULL
) WITHOUT ROWID;

-- While v = 1 the capture triggers stand down. Set only inside the write
-- transaction that materialises remote changes, so a replicated edit is never
-- mistaken for a local one and echoed back to the network.
CREATE TABLE IF NOT EXISTS _p2p_guard (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  v  INTEGER NOT NULL DEFAULT 0
);
INSERT OR IGNORE INTO _p2p_guard (id, v) VALUES (1, 0);

-- Triggers stage bare (table, pk) touches here; Rust drains the queue and
-- stamps each real change with a hybrid logical clock.
CREATE TABLE IF NOT EXISTS _p2p_local_ops (
  id  INTEGER PRIMARY KEY AUTOINCREMENT,
  tbl TEXT NOT NULL,
  pk  TEXT NOT NULL,
  op  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS _p2p_cell (
  tbl    TEXT NOT NULL,
  pk     TEXT NOT NULL,
  col    TEXT NOT NULL,
  val    BLOB,
  vtype  INTEGER NOT NULL,
  hlc    TEXT NOT NULL,
  origin TEXT NOT NULL,
  PRIMARY KEY (tbl, pk, col)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS _p2p_tombstone (
  tbl    TEXT NOT NULL,
  pk     TEXT NOT NULL,
  hlc    TEXT NOT NULL,
  origin TEXT NOT NULL,
  PRIMARY KEY (tbl, pk)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS _p2p_change (
  seq    INTEGER PRIMARY KEY AUTOINCREMENT,
  tbl    TEXT NOT NULL,
  pk     TEXT NOT NULL,
  col    TEXT NOT NULL,
  val    BLOB,
  vtype  INTEGER NOT NULL,
  hlc    TEXT NOT NULL,
  origin TEXT NOT NULL,
  kind   INTEGER NOT NULL,
  -- Ed25519 signature by `origin` over the record. Without it a member could
  -- forge another device's origin with a far-future stamp and permanently
  -- starve that device's real changes out of every version vector.
  sig    BLOB NOT NULL DEFAULT x''
);

-- Makes replays idempotent and backs the "everything after stamp S" scan.
CREATE UNIQUE INDEX IF NOT EXISTS _p2p_change_ident ON _p2p_change(origin, hlc);

CREATE TABLE IF NOT EXISTS _p2p_vv (
  origin TEXT PRIMARY KEY,
  hlc    TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS _p2p_synced_table (
  tbl    TEXT PRIMARY KEY,
  pk_col TEXT NOT NULL,
  cols   TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS _p2p_peer (
  peer_id      TEXT PRIMARY KEY,
  user_id      TEXT,
  display_name TEXT,
  role         TEXT,
  serial       INTEGER,
  last_seen_ms INTEGER NOT NULL DEFAULT 0,
  cert         TEXT
) WITHOUT ROWID;
"#;

/// Application tables that ship with the node.
///
/// Chat is not a separate subsystem: a message is just a row in a replicated
/// table. Direct delivery to a connected peer makes it feel instant, and the
/// change log makes it eventually arrive even if every device was offline at
/// a different time.
pub const APP_SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS messages (
  id         TEXT PRIMARY KEY,
  room       TEXT NOT NULL DEFAULT 'general',
  author     TEXT NOT NULL DEFAULT '',
  author_name TEXT NOT NULL DEFAULT '',
  body       TEXT NOT NULL DEFAULT '',
  sent_at_ms INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS messages_room_time ON messages(room, sent_at_ms);

-- A general-purpose org table so the sync engine is useful beyond chat.
CREATE TABLE IF NOT EXISTS records (
  id          TEXT PRIMARY KEY,
  collection  TEXT NOT NULL DEFAULT 'default',
  title       TEXT NOT NULL DEFAULT '',
  body        TEXT NOT NULL DEFAULT '',
  status      TEXT NOT NULL DEFAULT 'open',
  updated_by  TEXT NOT NULL DEFAULT '',
  updated_at_ms INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS records_collection ON records(collection);
"#;

fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn lit(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Builds the capture triggers for one replicated table.
///
/// The triggers record only *which* row was touched. Reading the new values
/// and diffing them happens in Rust during the flush, which keeps the trigger
/// bodies identical for every table regardless of its columns.
pub fn capture_triggers(tbl: &str, pk_col: &str) -> String {
    let t = ident(tbl);
    let p = ident(pk_col);
    let tl = lit(tbl);
    let prefix = format!("_p2p_cap_{}", tbl.replace(|c: char| !c.is_alphanumeric(), "_"));
    let unguarded = "(SELECT v FROM _p2p_guard WHERE id = 1) = 0";

    format!(
        r#"
DROP TRIGGER IF EXISTS {prefix}_ins;
DROP TRIGGER IF EXISTS {prefix}_upd;
DROP TRIGGER IF EXISTS {prefix}_pk;
DROP TRIGGER IF EXISTS {prefix}_del;

CREATE TRIGGER {prefix}_ins AFTER INSERT ON {t}
WHEN {unguarded}
BEGIN
  INSERT INTO _p2p_local_ops (tbl, pk, op)
  VALUES ({tl}, CAST(NEW.{p} AS TEXT), 'upsert');
END;

CREATE TRIGGER {prefix}_upd AFTER UPDATE ON {t}
WHEN {unguarded}
BEGIN
  INSERT INTO _p2p_local_ops (tbl, pk, op)
  VALUES ({tl}, CAST(NEW.{p} AS TEXT), 'upsert');
END;

-- Re-keying a row is replicated as "delete the old key, write the new one".
CREATE TRIGGER {prefix}_pk AFTER UPDATE OF {p} ON {t}
WHEN {unguarded} AND CAST(OLD.{p} AS TEXT) <> CAST(NEW.{p} AS TEXT)
BEGIN
  INSERT INTO _p2p_local_ops (tbl, pk, op)
  VALUES ({tl}, CAST(OLD.{p} AS TEXT), 'delete');
END;

CREATE TRIGGER {prefix}_del AFTER DELETE ON {t}
WHEN {unguarded}
BEGIN
  INSERT INTO _p2p_local_ops (tbl, pk, op)
  VALUES ({tl}, CAST(OLD.{p} AS TEXT), 'delete');
END;
"#
    )
}

pub fn quote_ident(name: &str) -> String {
    ident(name)
}
