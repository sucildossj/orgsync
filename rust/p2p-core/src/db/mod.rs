//! CRDT-replicated SQLite.
//!
//! # How a write travels
//!
//! 1. Anything that writes a replicated table — the app through [`Db::execute`],
//!    or React Native through its own handle on the same file — fires a capture
//!    trigger that stages `(table, primary key)` in `_p2p_local_ops`.
//! 2. [`Db::flush_local`] drains that queue, diffs each touched row against the
//!    CRDT state, and turns every genuinely changed column into a
//!    [`ChangeRecord`] stamped with a fresh hybrid-logical-clock tick.
//! 3. The node ships those records to peers.
//! 4. A receiving device calls [`Db::apply_remote`], which stores each record,
//!    resolves last-writer-wins per column, and re-materialises the affected
//!    rows into the app's own tables.
//!
//! Merging is therefore commutative, associative and idempotent: peers can
//! exchange changes in any order, more than once, through any route, and end
//! up byte-identical.

pub mod change;
pub mod schema;
pub mod value;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, params_from_iter, Connection, OptionalExtension, TransactionBehavior};

pub use change::{incoming_wins, ChangeKind, ChangeRecord, VersionVector};
pub use value::SqlValue;

use crate::error::{Error, Result};
use crate::hlc::{now_ms, Hlc, HybridClock};
use crate::identity::DeviceIdentity;
use schema::quote_ident;

/// One cell of the CRDT state as SQLite hands it back:
/// column, type tag, raw value, stamp, author.
type RawCell = (String, i64, Option<Vec<u8>>, String, String);

/// A table taking part in replication.
#[derive(Debug, Clone)]
pub struct TableSpec {
    pub name: String,
    pub pk_col: String,
    /// Replicated columns, i.e. every column except the primary key.
    pub cols: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ApplyOutcome {
    /// Records stored for the first time.
    pub applied: usize,
    /// Records we already held; harmless, and expected during anti-entropy.
    pub duplicates: usize,
    /// Records whose table this device does not know about yet. They are kept
    /// so they still propagate, but nothing is materialised for them.
    pub deferred: usize,
    /// Records dropped because they were unsigned or not signed by the device
    /// they claim to come from.
    pub rejected: usize,
    pub tables_touched: BTreeSet<String>,
}

#[derive(Debug, Clone, Default)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<SqlValue>>,
}

#[derive(Debug, Clone, Default)]
pub struct DbStats {
    pub changes: u64,
    pub cells: u64,
    pub tombstones: u64,
    pub known_devices: u64,
    pub pending_local_ops: u64,
}

pub struct Db {
    conn: Mutex<Connection>,
    clock: HybridClock,
    device_id: String,
    tables: Mutex<HashMap<String, TableSpec>>,
    /// Present when this replica authors signed changes.
    signer: Option<DeviceIdentity>,
    /// When set, incoming changes must carry a valid author signature.
    require_signatures: bool,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db").field("device_id", &self.device_id).finish_non_exhaustive()
    }
}

impl Db {
    /// Opens (creating if needed) the replica at `path` for `device_id`,
    /// which must be this device's PeerId.
    pub fn open(path: impl AsRef<Path>, device_id: impl Into<String>) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::from_connection(conn, device_id)
    }

    pub fn open_in_memory(device_id: impl Into<String>) -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?, device_id)
    }

    /// Opens a replica that signs everything it authors and refuses anything a
    /// peer cannot prove authorship of. This is what a real node uses.
    pub fn open_authenticated(path: impl AsRef<Path>, id: &DeviceIdentity) -> Result<Self> {
        let mut db = Self::from_connection(Connection::open(path)?, id.peer_id().to_string())?;
        db.signer = Some(id.clone());
        db.require_signatures = true;
        Ok(db)
    }

    #[doc(hidden)]
    pub fn open_authenticated_in_memory(id: &DeviceIdentity) -> Result<Self> {
        let mut db = Self::from_connection(Connection::open_in_memory()?, id.peer_id().to_string())?;
        db.signer = Some(id.clone());
        db.require_signatures = true;
        Ok(db)
    }

    fn from_connection(conn: Connection, device_id: impl Into<String>) -> Result<Self> {
        let device_id = device_id.into();
        conn.execute_batch("PRAGMA busy_timeout = 5000;")?;
        conn.execute_batch(schema::MIGRATION_V1)?;
        conn.execute_batch(schema::APP_SCHEMA_V1)?;

        // Resume the clock strictly from our own past writes. Adopting a
        // remote maximum here would let one device with a broken clock drag
        // every replica into the future permanently.
        let resume: Option<String> = conn
            .query_row(
                "SELECT MAX(hlc) FROM _p2p_change WHERE origin = ?1",
                params![&device_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let clock = match resume.as_deref().and_then(Hlc::from_hex) {
            Some(h) => HybridClock::resuming_from(h),
            None => HybridClock::new(),
        };

        let db = Self {
            conn: Mutex::new(conn),
            clock,
            device_id,
            tables: Mutex::new(HashMap::new()),
            signer: None,
            require_signatures: false,
        };
        db.reload_table_specs()?;
        for (tbl, pk) in [("messages", "id"), ("records", "id")] {
            db.register_table(tbl, pk)?;
        }
        Ok(db)
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn clock(&self) -> &HybridClock {
        &self.clock
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn specs(&self) -> HashMap<String, TableSpec> {
        self.tables.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn reload_table_specs(&self) -> Result<()> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT tbl, pk_col, cols FROM _p2p_synced_table")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        drop(conn);

        let mut map = HashMap::new();
        for (name, pk_col, cols_json) in rows {
            let cols: Vec<String> = serde_json::from_str(&cols_json).unwrap_or_default();
            map.insert(name.clone(), TableSpec { name, pk_col, cols });
        }
        *self.tables.lock().unwrap_or_else(|e| e.into_inner()) = map;
        Ok(())
    }

    /// Brings a table into replication: records its shape, installs the
    /// capture triggers, and stages any pre-existing rows so they are shared
    /// rather than silently ignored.
    pub fn register_table(&self, tbl: &str, pk_col: &str) -> Result<TableSpec> {
        let (cols, found_pk) = {
            let conn = self.lock();
            let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", quote_ident(tbl)))?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(5)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if rows.is_empty() {
                return Err(Error::Schema(format!("table `{tbl}` does not exist")));
            }
            let pk_cols: Vec<&String> = rows.iter().filter(|(_, pk)| *pk > 0).map(|(n, _)| n).collect();
            if pk_cols.len() != 1 {
                return Err(Error::Schema(format!(
                    "table `{tbl}` must have exactly one primary key column to be replicated (found {})",
                    pk_cols.len()
                )));
            }
            let found_pk = pk_cols[0].clone();
            let cols: Vec<String> =
                rows.iter().map(|(n, _)| n.clone()).filter(|n| n != &found_pk).collect();
            (cols, found_pk)
        };

        if found_pk != pk_col {
            return Err(Error::Schema(format!(
                "table `{tbl}` has primary key `{found_pk}`, not `{pk_col}`"
            )));
        }

        let spec = TableSpec { name: tbl.to_string(), pk_col: found_pk, cols };
        {
            let conn = self.lock();
            conn.execute(
                "INSERT INTO _p2p_synced_table (tbl, pk_col, cols) VALUES (?1, ?2, ?3)
                 ON CONFLICT(tbl) DO UPDATE SET pk_col = excluded.pk_col, cols = excluded.cols",
                params![&spec.name, &spec.pk_col, serde_json::to_string(&spec.cols)?],
            )?;
            conn.execute_batch(&schema::capture_triggers(&spec.name, &spec.pk_col))?;

            // Adopt rows that predate replication. Idempotent: rows that
            // already have CRDT state are skipped.
            conn.execute(
                &format!(
                    "INSERT INTO _p2p_local_ops (tbl, pk, op)
                     SELECT ?1, CAST(t.{pk} AS TEXT), 'upsert' FROM {t} AS t
                     WHERE NOT EXISTS (
                       SELECT 1 FROM _p2p_cell c WHERE c.tbl = ?1 AND c.pk = CAST(t.{pk} AS TEXT)
                     )",
                    pk = quote_ident(&spec.pk_col),
                    t = quote_ident(&spec.name)
                ),
                params![&spec.name],
            )?;
        }
        self.tables
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(spec.name.clone(), spec.clone());
        Ok(spec)
    }

    pub fn synced_tables(&self) -> Vec<TableSpec> {
        self.specs().into_values().collect()
    }

    // ---------------------------------------------------------------- local

    /// Turns staged local edits into replicable changes.
    ///
    /// Returns the changes authored, which the caller broadcasts. An unchanged
    /// column produces nothing, so a no-op `UPDATE` costs no network traffic.
    pub fn flush_local(&self) -> Result<Vec<ChangeRecord>> {
        let specs = self.specs();
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let staged: Vec<(i64, String, String, String)> = {
            let mut stmt = tx.prepare("SELECT id, tbl, pk, op FROM _p2p_local_ops ORDER BY id")?;
            let v = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            v
        };
        if staged.is_empty() {
            return Ok(Vec::new());
        }
        let max_id = staged.iter().map(|s| s.0).max().unwrap_or(0);

        // Collapse to the final intent per row, keeping first-seen order so
        // the emitted changes stay deterministic.
        let mut order: Vec<(String, String)> = Vec::new();
        let mut final_op: HashMap<(String, String), String> = HashMap::new();
        for (_, tbl, pk, op) in &staged {
            let key = (tbl.clone(), pk.clone());
            if !final_op.contains_key(&key) {
                order.push(key.clone());
            }
            final_op.insert(key, op.clone());
        }

        let mut out: Vec<ChangeRecord> = Vec::new();
        for key in order {
            let (tbl, pk) = (&key.0, &key.1);
            let Some(spec) = specs.get(tbl.as_str()) else { continue };
            let op = final_op.get(&key).map(String::as_str).unwrap_or("upsert");

            let row = read_row(&tx, spec, pk)?;
            match (op, row) {
                ("delete", None) => {
                    let hlc = self.clock.tick();
                    let mut rec = ChangeRecord {
                        tbl: tbl.clone(),
                        pk: pk.clone(),
                        col: String::new(),
                        value: SqlValue::Null,
                        hlc,
                        origin: self.device_id.clone(),
                        kind: ChangeKind::Delete,
                        sig: Vec::new(),
                    };
                    if let Some(id) = &self.signer {
                        rec.sign_with(id)?;
                    }
                    store_change(&tx, &rec)?;
                    upsert_tombstone(&tx, &rec)?;
                    out.push(rec);
                }
                // Staged as a delete but the row is back: the re-insert is the
                // truth, so fall through and replicate its current values.
                (_, Some(values)) => {
                    for col in &spec.cols {
                        let Some(v) = values.get(col) else { continue };
                        let (vtype, raw) = v.to_storage();
                        if cell_matches(&tx, tbl, pk, col, vtype, raw.as_deref())? {
                            continue;
                        }
                        let hlc = self.clock.tick();
                        let mut rec = ChangeRecord {
                            tbl: tbl.clone(),
                            pk: pk.clone(),
                            col: col.clone(),
                            value: v.clone(),
                            hlc,
                            origin: self.device_id.clone(),
                            kind: ChangeKind::Cell,
                            sig: Vec::new(),
                        };
                        if let Some(id) = &self.signer {
                            rec.sign_with(id)?;
                        }
                        store_change(&tx, &rec)?;
                        upsert_cell(&tx, &rec)?;
                        out.push(rec);
                    }
                }
                // Staged as an upsert but the row is gone: a later delete in
                // the same batch already won, nothing to replicate.
                ("upsert", None) => {}
                _ => {}
            }
        }

        tx.execute("DELETE FROM _p2p_local_ops WHERE id <= ?1", params![max_id])?;
        if let Some(max) = out.iter().map(|c| c.hlc).max() {
            bump_vv(&tx, &self.device_id, max)?;
        }
        tx.commit()?;
        Ok(out)
    }

    // --------------------------------------------------------------- remote

    /// Stores and merges changes received from a peer.
    pub fn apply_remote(&self, changes: &[ChangeRecord]) -> Result<ApplyOutcome> {
        let mut outcome = ApplyOutcome::default();
        if changes.is_empty() {
            return Ok(outcome);
        }
        // Authorship is checked before the write lock is taken. Verification is
        // pure CPU work, and a batch of 500 signatures should not hold every
        // other writer out of the database while it runs.
        let mut sorted: Vec<&ChangeRecord> = Vec::with_capacity(changes.len());
        for rec in changes {
            if rec.origin.is_empty() || rec.tbl.is_empty() {
                continue;
            }
            if self.require_signatures {
                if let Err(e) = rec.verify_author() {
                    tracing::warn!(origin = %rec.origin, error = %e, "dropping change");
                    outcome.rejected += 1;
                    continue;
                }
            }
            sorted.push(rec);
        }
        if sorted.is_empty() {
            return Ok(outcome);
        }
        sorted.sort_by(|a, b| a.order_key().cmp(&b.order_key()));

        let specs = self.specs();
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        // Stand the capture triggers down for the duration: everything written
        // below is replication, not a local edit. `BEGIN IMMEDIATE` holds the
        // single writer lock, so no other connection can observe the guard set.
        tx.execute("UPDATE _p2p_guard SET v = 1 WHERE id = 1", [])?;

        let mut dirty: BTreeSet<(String, String)> = BTreeSet::new();
        let mut vv_max: BTreeMap<String, Hlc> = BTreeMap::new();

        for rec in sorted {
            if store_change(&tx, rec)? == 0 {
                outcome.duplicates += 1;
                continue;
            }
            outcome.applied += 1;
            self.clock.observe(rec.hlc);
            let e = vv_max.entry(rec.origin.clone()).or_insert(Hlc::ZERO);
            if rec.hlc > *e {
                *e = rec.hlc;
            }

            match rec.kind {
                ChangeKind::Delete => {
                    upsert_tombstone(&tx, rec)?;
                }
                ChangeKind::Cell => {
                    upsert_cell(&tx, rec)?;
                }
            }

            if specs.contains_key(&rec.tbl) {
                outcome.tables_touched.insert(rec.tbl.clone());
                dirty.insert((rec.tbl.clone(), rec.pk.clone()));
            } else {
                // A table this build does not know yet. The change is stored
                // and will still be forwarded, so an older device never
                // becomes a hole in the replication mesh.
                outcome.deferred += 1;
            }
        }

        for (tbl, pk) in &dirty {
            let Some(spec) = specs.get(tbl.as_str()) else { continue };
            if let Err(e) = materialize(&tx, spec, pk) {
                // One unmaterialisable row must not abort a whole batch: the
                // CRDT state is already stored and stays authoritative.
                tracing::warn!(table = %tbl, pk = %pk, error = %e, "could not materialise row");
            }
        }

        for (origin, hlc) in vv_max {
            bump_vv(&tx, &origin, hlc)?;
        }
        tx.execute("UPDATE _p2p_guard SET v = 0 WHERE id = 1", [])?;
        tx.commit()?;
        Ok(outcome)
    }

    // ----------------------------------------------------------- anti-entropy

    pub fn version_vector(&self) -> Result<VersionVector> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT origin, hlc FROM _p2p_vv")?;
        let mut vv = VersionVector::new();
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (origin, hlc) = row?;
            if let Some(h) = Hlc::from_hex(&hlc) {
                vv.observe(&origin, h);
            }
        }
        Ok(vv)
    }

    /// Everything we hold that `peer_vv` does not, oldest first.
    ///
    /// Ordering by stamp means any prefix is a valid batch: the peer applies
    /// it, advances its own vector, and asks again for the rest.
    pub fn changes_since(&self, peer_vv: &VersionVector, limit: usize) -> Result<Vec<ChangeRecord>> {
        let conn = self.lock();
        conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _p2p_ask (origin TEXT PRIMARY KEY, hlc TEXT NOT NULL);
             DELETE FROM _p2p_ask;",
        )?;
        {
            let mut ins = conn.prepare("INSERT OR REPLACE INTO _p2p_ask (origin, hlc) VALUES (?1, ?2)")?;
            for (origin, hlc) in &peer_vv.0 {
                ins.execute(params![origin, hlc.to_hex()])?;
            }
        }
        let mut stmt = conn.prepare(
            "SELECT c.tbl, c.pk, c.col, c.val, c.vtype, c.hlc, c.origin, c.kind, c.sig
             FROM _p2p_change c
             LEFT JOIN _p2p_ask a ON a.origin = c.origin
             WHERE c.hlc > COALESCE(a.hlc, '')
             ORDER BY c.hlc ASC, c.origin ASC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<Vec<u8>>>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, Vec<u8>>(8)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (tbl, pk, col, val, vtype, hlc, origin, kind, sig) = row?;
            out.push(ChangeRecord {
                tbl,
                pk,
                col,
                value: SqlValue::from_storage(vtype, val)?,
                hlc: Hlc::from_hex(&hlc).unwrap_or(Hlc::ZERO),
                origin,
                kind: ChangeKind::from_i64(kind),
                sig,
            });
        }
        Ok(out)
    }

    // ------------------------------------------------------------- app access

    /// Runs a write and immediately turns it into replicable changes.
    pub fn execute(&self, sql: &str, args: &[SqlValue]) -> Result<(usize, Vec<ChangeRecord>)> {
        let affected = {
            let conn = self.lock();
            let values: Vec<rusqlite::types::Value> = args.iter().map(Into::into).collect();
            conn.execute(sql, params_from_iter(values.iter()))?
        };
        let changes = self.flush_local()?;
        Ok((affected, changes))
    }

    pub fn query(&self, sql: &str, args: &[SqlValue]) -> Result<QueryResult> {
        let conn = self.lock();
        let mut stmt = conn.prepare(sql)?;
        let columns: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
        let width = columns.len();
        let values: Vec<rusqlite::types::Value> = args.iter().map(Into::into).collect();
        let mut rows = stmt.query(params_from_iter(values.iter()))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let mut r = Vec::with_capacity(width);
            for i in 0..width {
                r.push(SqlValue::from(row.get_ref(i)?));
            }
            out.push(r);
        }
        Ok(QueryResult { columns, rows: out })
    }

    /// Changes this device authored that have not been broadcast yet.
    ///
    /// Broadcasting is tracked separately from flushing on purpose. Whoever
    /// calls [`Db::execute`] gets the changes back, but the node still needs to
    /// send them — and if it relied on its own flush finding them, an edit made
    /// through `execute` would be drained before the node ever saw it and would
    /// only reach peers on the next anti-entropy pass. Tracking a high-water
    /// mark over the log instead makes the live path independent of who
    /// happened to flush.
    pub fn unbroadcast_local(&self, limit: usize) -> Result<(Vec<ChangeRecord>, i64)> {
        let conn = self.lock();
        let watermark: i64 = conn
            .query_row("SELECT v FROM _p2p_meta WHERE k = 'broadcast_seq'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let mut stmt = conn.prepare(
            "SELECT seq, tbl, pk, col, val, vtype, hlc, origin, kind, sig
             FROM _p2p_change
             WHERE origin = ?1 AND seq > ?2
             ORDER BY seq ASC LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![&self.device_id, watermark, limit as i64],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<Vec<u8>>>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, Vec<u8>>(9)?,
                ))
            },
        )?;

        let mut out = Vec::new();
        let mut high = watermark;
        for row in rows {
            let (seq, tbl, pk, col, val, vtype, hlc, origin, kind, sig) = row?;
            high = high.max(seq);
            out.push(ChangeRecord {
                tbl,
                pk,
                col,
                value: SqlValue::from_storage(vtype, val)?,
                hlc: Hlc::from_hex(&hlc).unwrap_or(Hlc::ZERO),
                origin,
                kind: ChangeKind::from_i64(kind),
                sig,
            });
        }
        Ok((out, high))
    }

    /// Advances the broadcast high-water mark. Call only after the changes
    /// have actually gone out, so a failure means they are retried.
    pub fn mark_broadcast(&self, seq: i64) -> Result<()> {
        self.set_meta("broadcast_seq", &seq.to_string())
    }

    pub fn stats(&self) -> Result<DbStats> {
        let conn = self.lock();
        let one = |sql: &str| -> Result<u64> {
            Ok(conn.query_row(sql, [], |r| r.get::<_, i64>(0))? as u64)
        };
        Ok(DbStats {
            changes: one("SELECT COUNT(*) FROM _p2p_change")?,
            cells: one("SELECT COUNT(*) FROM _p2p_cell")?,
            tombstones: one("SELECT COUNT(*) FROM _p2p_tombstone")?,
            known_devices: one("SELECT COUNT(*) FROM _p2p_vv")?,
            pending_local_ops: one("SELECT COUNT(*) FROM _p2p_local_ops")?,
        })
    }

    pub fn set_meta(&self, k: &str, v: &str) -> Result<()> {
        self.lock().execute(
            "INSERT INTO _p2p_meta (k, v) VALUES (?1, ?2)
             ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![k, v],
        )?;
        Ok(())
    }

    pub fn get_meta(&self, k: &str) -> Result<Option<String>> {
        Ok(self
            .lock()
            .query_row("SELECT v FROM _p2p_meta WHERE k = ?1", params![k], |r| r.get(0))
            .optional()?)
    }

    pub fn record_peer(
        &self,
        peer_id: &str,
        user_id: &str,
        display_name: &str,
        role: &str,
        serial: i64,
        cert_json: &str,
    ) -> Result<()> {
        self.lock().execute(
            "INSERT INTO _p2p_peer (peer_id, user_id, display_name, role, serial, last_seen_ms, cert)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(peer_id) DO UPDATE SET
               user_id = excluded.user_id, display_name = excluded.display_name,
               role = excluded.role, serial = excluded.serial,
               last_seen_ms = excluded.last_seen_ms, cert = excluded.cert",
            params![peer_id, user_id, display_name, role, serial, now_ms() as i64, cert_json],
        )?;
        Ok(())
    }

    /// Peer ids we have authenticated at some point that hold the given role.
    ///
    /// Survives restarts, so a device does not forget that a peer is
    /// read-only just because it has not met it yet this session.
    pub fn devices_with_role(&self, role: &str) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT peer_id FROM _p2p_peer WHERE role = ?1")?;
        let rows = stmt
            .query_map(params![role], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn touch_peer(&self, peer_id: &str) -> Result<()> {
        self.lock().execute(
            "UPDATE _p2p_peer SET last_seen_ms = ?2 WHERE peer_id = ?1",
            params![peer_id, now_ms() as i64],
        )?;
        Ok(())
    }
}

// ------------------------------------------------------------------ helpers

fn read_row(
    tx: &rusqlite::Transaction<'_>,
    spec: &TableSpec,
    pk: &str,
) -> Result<Option<BTreeMap<String, SqlValue>>> {
    let sql = format!(
        "SELECT * FROM {t} WHERE CAST({p} AS TEXT) = ?1 LIMIT 1",
        t = quote_ident(&spec.name),
        p = quote_ident(&spec.pk_col)
    );
    let mut stmt = tx.prepare(&sql)?;
    let names: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
    let mut rows = stmt.query(params![pk])?;
    let Some(row) = rows.next()? else { return Ok(None) };
    let mut map = BTreeMap::new();
    for (i, name) in names.iter().enumerate() {
        map.insert(name.clone(), SqlValue::from(row.get_ref(i)?));
    }
    Ok(Some(map))
}

/// Inserts a change unless we already hold it. Returns rows inserted, so `0`
/// means "already seen" — which is how replays stay idempotent.
fn store_change(tx: &rusqlite::Transaction<'_>, rec: &ChangeRecord) -> Result<usize> {
    let (vtype, raw) = rec.value.to_storage();
    Ok(tx.execute(
        "INSERT INTO _p2p_change (tbl, pk, col, val, vtype, hlc, origin, kind, sig)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(origin, hlc) DO NOTHING",
        params![
            &rec.tbl,
            &rec.pk,
            &rec.col,
            raw,
            vtype,
            rec.hlc.to_hex(),
            &rec.origin,
            rec.kind.as_i64(),
            &rec.sig
        ],
    )?)
}

fn upsert_cell(tx: &rusqlite::Transaction<'_>, rec: &ChangeRecord) -> Result<bool> {
    let existing: Option<(String, String)> = tx
        .query_row(
            "SELECT hlc, origin FROM _p2p_cell WHERE tbl = ?1 AND pk = ?2 AND col = ?3",
            params![&rec.tbl, &rec.pk, &rec.col],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let hlc = rec.hlc.to_hex();
    if let Some((old_hlc, old_origin)) = &existing {
        if !incoming_wins(&hlc, &rec.origin, old_hlc, old_origin) {
            return Ok(false);
        }
    }
    let (vtype, raw) = rec.value.to_storage();
    tx.execute(
        "INSERT INTO _p2p_cell (tbl, pk, col, val, vtype, hlc, origin)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(tbl, pk, col) DO UPDATE SET
           val = excluded.val, vtype = excluded.vtype,
           hlc = excluded.hlc, origin = excluded.origin",
        params![&rec.tbl, &rec.pk, &rec.col, raw, vtype, hlc, &rec.origin],
    )?;
    Ok(true)
}

fn upsert_tombstone(tx: &rusqlite::Transaction<'_>, rec: &ChangeRecord) -> Result<bool> {
    let existing: Option<(String, String)> = tx
        .query_row(
            "SELECT hlc, origin FROM _p2p_tombstone WHERE tbl = ?1 AND pk = ?2",
            params![&rec.tbl, &rec.pk],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let hlc = rec.hlc.to_hex();
    if let Some((old_hlc, old_origin)) = &existing {
        if !incoming_wins(&hlc, &rec.origin, old_hlc, old_origin) {
            return Ok(false);
        }
    }
    tx.execute(
        "INSERT INTO _p2p_tombstone (tbl, pk, hlc, origin) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(tbl, pk) DO UPDATE SET hlc = excluded.hlc, origin = excluded.origin",
        params![&rec.tbl, &rec.pk, hlc, &rec.origin],
    )?;
    Ok(true)
}

fn cell_matches(
    tx: &rusqlite::Transaction<'_>,
    tbl: &str,
    pk: &str,
    col: &str,
    vtype: i64,
    raw: Option<&[u8]>,
) -> Result<bool> {
    let existing: Option<(i64, Option<Vec<u8>>)> = tx
        .query_row(
            "SELECT vtype, val FROM _p2p_cell WHERE tbl = ?1 AND pk = ?2 AND col = ?3",
            params![tbl, pk, col],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(match existing {
        Some((t, v)) => t == vtype && v.as_deref() == raw,
        None => false,
    })
}

fn bump_vv(tx: &rusqlite::Transaction<'_>, origin: &str, hlc: Hlc) -> Result<()> {
    tx.execute(
        "INSERT INTO _p2p_vv (origin, hlc) VALUES (?1, ?2)
         ON CONFLICT(origin) DO UPDATE SET hlc = MAX(hlc, excluded.hlc)",
        params![origin, hlc.to_hex()],
    )?;
    Ok(())
}

/// Rebuilds one app row from the CRDT state.
///
/// A row is deleted only while its tombstone outranks every surviving cell.
/// Because the cells are kept, a later edit from another device resurrects the
/// row with the correct merged contents instead of a blank one.
fn materialize(tx: &rusqlite::Transaction<'_>, spec: &TableSpec, pk: &str) -> Result<()> {
    let tomb: Option<(String, String)> = tx
        .query_row(
            "SELECT hlc, origin FROM _p2p_tombstone WHERE tbl = ?1 AND pk = ?2",
            params![&spec.name, pk],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;

    let cells: Vec<RawCell> = {
        let mut stmt = tx.prepare(
            "SELECT col, vtype, val, hlc, origin FROM _p2p_cell WHERE tbl = ?1 AND pk = ?2",
        )?;
        let v = stmt
            .query_map(params![&spec.name, pk], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        v
    };

    if cells.is_empty() && tomb.is_none() {
        return Ok(()); // nothing is known about this row; leave the table alone
    }
    let max_cell = cells.iter().map(|c| (c.3.clone(), c.4.clone())).max();
    let deleted = match (&tomb, &max_cell) {
        (Some((th, to)), Some((ch, co))) => (th, to) > (ch, co),
        (Some(_), None) => true,
        _ => false,
    };

    let table = quote_ident(&spec.name);
    let pk_ident = quote_ident(&spec.pk_col);

    if deleted {
        tx.execute(
            &format!("DELETE FROM {table} WHERE CAST({pk_ident} AS TEXT) = ?1"),
            params![pk],
        )?;
        return Ok(());
    }

    // Only columns this build actually has; anything else stays in the CRDT
    // state so a future schema version can pick it up.
    let known: BTreeSet<&str> = spec.cols.iter().map(String::as_str).collect();
    let mut cols: Vec<String> = Vec::new();
    let mut vals: Vec<rusqlite::types::Value> = Vec::new();
    for (col, vtype, raw, _, _) in &cells {
        if !known.contains(col.as_str()) {
            continue;
        }
        cols.push(col.clone());
        vals.push((&SqlValue::from_storage(*vtype, raw.clone())?).into());
    }

    let mut insert_cols = vec![spec.pk_col.clone()];
    insert_cols.extend(cols.iter().cloned());
    let mut binds: Vec<rusqlite::types::Value> = vec![rusqlite::types::Value::Text(pk.to_string())];
    binds.extend(vals);

    let col_list = insert_cols.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ");
    let placeholders = (1..=insert_cols.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", ");
    let assignments = if cols.is_empty() {
        format!("{pk_ident} = excluded.{pk_ident}")
    } else {
        cols.iter().map(|c| format!("{0} = excluded.{0}", quote_ident(c))).collect::<Vec<_>>().join(", ")
    };

    let sql = format!(
        "INSERT INTO {table} ({col_list}) VALUES ({placeholders})
         ON CONFLICT({pk_ident}) DO UPDATE SET {assignments}"
    );
    tx.execute(&sql, params_from_iter(binds.iter()))?;
    Ok(())
}
