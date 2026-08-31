//! Convergence properties of the CRDT replica.
//!
//! Every test here asserts the same underlying contract: peers that have seen
//! the same set of changes hold byte-identical tables, no matter what order,
//! how many times, or by which route those changes arrived.

use p2p_core::db::{ChangeKind, ChangeRecord, Db, SqlValue, VersionVector};
use p2p_core::hlc::Hlc;

fn replica(device: &str) -> Db {
    Db::open_in_memory(device).expect("open replica")
}

fn s(v: &str) -> SqlValue {
    SqlValue::Text(v.to_string())
}

/// One round of anti-entropy from `from` into `into`.
fn pull(into: &Db, from: &Db) -> usize {
    let mut total = 0;
    loop {
        let vv = into.version_vector().unwrap();
        let batch = from.changes_since(&vv, 128).unwrap();
        if batch.is_empty() {
            break;
        }
        let out = into.apply_remote(&batch).unwrap();
        total += out.applied;
        if out.applied == 0 {
            break; // nothing new; avoid spinning on duplicates
        }
    }
    total
}

fn sync(a: &Db, b: &Db) {
    a.flush_local().unwrap();
    b.flush_local().unwrap();
    pull(a, b);
    pull(b, a);
}

fn title_of(db: &Db, id: &str) -> Option<String> {
    let r = db.query("SELECT title FROM records WHERE id = ?1", &[s(id)]).unwrap();
    r.rows.first().map(|row| match &row[0] {
        SqlValue::Text(t) => t.clone(),
        other => format!("{other:?}"),
    })
}

fn row_count(db: &Db, table: &str) -> i64 {
    let r = db.query(&format!("SELECT COUNT(*) FROM {table}"), &[]).unwrap();
    match r.rows[0][0] {
        SqlValue::Int(i) => i,
        _ => -1,
    }
}

#[test]
fn a_local_write_becomes_replicable_changes() {
    let a = replica("device-a");
    let (n, changes) = a
        .execute(
            "INSERT INTO records (id, title, body) VALUES (?1, ?2, ?3)",
            &[s("r1"), s("Quarterly plan"), s("draft")],
        )
        .unwrap();
    assert_eq!(n, 1);
    // One change per replicated column that actually got a value.
    assert!(changes.iter().any(|c| c.col == "title"));
    assert!(changes.iter().all(|c| c.origin == "device-a"));
    assert!(changes.iter().all(|c| c.kind == ChangeKind::Cell));
}

#[test]
fn an_insert_replicates_to_a_second_device() {
    let (a, b) = (replica("device-a"), replica("device-b"));
    a.execute(
        "INSERT INTO records (id, title, body) VALUES (?1, ?2, ?3)",
        &[s("r1"), s("Quarterly plan"), s("draft")],
    )
    .unwrap();
    sync(&a, &b);
    assert_eq!(title_of(&b, "r1").as_deref(), Some("Quarterly plan"));
    assert_eq!(row_count(&b, "records"), 1);
}

#[test]
fn edits_to_different_columns_both_survive() {
    let (a, b) = (replica("device-a"), replica("device-b"));
    a.execute("INSERT INTO records (id, title, body) VALUES ('r1','t','b')", &[]).unwrap();
    sync(&a, &b);

    // Each device edits a different field while apart. Column-level merging
    // means neither edit should clobber the other.
    a.execute("UPDATE records SET title = 'from A' WHERE id = 'r1'", &[]).unwrap();
    b.execute("UPDATE records SET body = 'from B' WHERE id = 'r1'", &[]).unwrap();
    sync(&a, &b);

    for db in [&a, &b] {
        let r = db.query("SELECT title, body FROM records WHERE id='r1'", &[]).unwrap();
        assert_eq!(r.rows[0][0], s("from A"));
        assert_eq!(r.rows[0][1], s("from B"));
    }
}

#[test]
fn a_conflicting_column_converges_to_one_winner() {
    let (a, b) = (replica("device-a"), replica("device-b"));
    a.execute("INSERT INTO records (id, title) VALUES ('r1','start')", &[]).unwrap();
    sync(&a, &b);

    a.execute("UPDATE records SET title = 'A wins?' WHERE id='r1'", &[]).unwrap();
    b.execute("UPDATE records SET title = 'B wins?' WHERE id='r1'", &[]).unwrap();
    sync(&a, &b);

    let ta = title_of(&a, "r1").unwrap();
    let tb = title_of(&b, "r1").unwrap();
    assert_eq!(ta, tb, "both devices must agree on the winner");
    assert!(ta == "A wins?" || ta == "B wins?");
}

#[test]
fn an_exact_timestamp_tie_is_broken_identically_on_both_sides() {
    // Two devices can legitimately produce the same wall/counter pair. The
    // origin id breaks the tie, and it must break the same way everywhere,
    // regardless of the order the two changes arrive in.
    let stamp = Hlc::new(1_700_000_000_000, 0);
    let from_a = ChangeRecord {
        tbl: "records".into(),
        pk: "r1".into(),
        col: "title".into(),
        value: s("written by A"),
        hlc: stamp,
        origin: "device-aaa".into(),
        kind: ChangeKind::Cell,
        sig: Vec::new(),
    };
    let from_b = ChangeRecord { origin: "device-bbb".into(), value: s("written by B"), ..from_a.clone() };

    let first = replica("obs-1");
    first.apply_remote(&[from_a.clone(), from_b.clone()]).unwrap();

    let second = replica("obs-2");
    second.apply_remote(std::slice::from_ref(&from_b)).unwrap();
    second.apply_remote(std::slice::from_ref(&from_a)).unwrap();

    assert_eq!(title_of(&first, "r1"), title_of(&second, "r1"));
    assert_eq!(title_of(&first, "r1").as_deref(), Some("written by B"));
}

#[test]
fn applying_the_same_batch_twice_changes_nothing() {
    let (a, b) = (replica("device-a"), replica("device-b"));
    a.execute("INSERT INTO records (id, title) VALUES ('r1','once')", &[]).unwrap();
    let batch = a.changes_since(&VersionVector::new(), 512).unwrap();

    let first = b.apply_remote(&batch).unwrap();
    let second = b.apply_remote(&batch).unwrap();

    assert!(first.applied > 0);
    assert_eq!(second.applied, 0, "a replay must apply nothing");
    assert_eq!(second.duplicates, batch.len());
    assert_eq!(row_count(&b, "records"), 1);
}

#[test]
fn changes_delivered_in_reverse_order_still_converge() {
    let a = replica("device-a");
    a.execute("INSERT INTO records (id, title) VALUES ('r1','v1')", &[]).unwrap();
    a.execute("UPDATE records SET title='v2' WHERE id='r1'", &[]).unwrap();
    a.execute("UPDATE records SET title='v3' WHERE id='r1'", &[]).unwrap();
    let mut batch = a.changes_since(&VersionVector::new(), 512).unwrap();

    let forward = replica("f");
    forward.apply_remote(&batch).unwrap();

    batch.reverse();
    let backward = replica("r");
    backward.apply_remote(&batch).unwrap();

    // And one that receives them one at a time, backwards.
    let dribble = replica("d");
    for c in batch.iter() {
        dribble.apply_remote(std::slice::from_ref(c)).unwrap();
    }

    assert_eq!(title_of(&forward, "r1").as_deref(), Some("v3"));
    assert_eq!(title_of(&backward, "r1"), title_of(&forward, "r1"));
    assert_eq!(title_of(&dribble, "r1"), title_of(&forward, "r1"));
}

#[test]
fn a_delete_replicates() {
    let (a, b) = (replica("device-a"), replica("device-b"));
    a.execute("INSERT INTO records (id, title) VALUES ('r1','doomed')", &[]).unwrap();
    sync(&a, &b);
    assert_eq!(row_count(&b, "records"), 1);

    a.execute("DELETE FROM records WHERE id='r1'", &[]).unwrap();
    sync(&a, &b);
    assert_eq!(row_count(&b, "records"), 0, "the delete must propagate");
}

#[test]
fn an_edit_that_outranks_a_delete_resurrects_the_row_intact() {
    // A deletes the row; B, not yet knowing that, edits one field. B's edit is
    // newer, so the row comes back — and it must come back with the columns B
    // never touched still populated, not blanked out.
    let (a, b) = (replica("device-a"), replica("device-b"));
    a.execute(
        "INSERT INTO records (id, title, body) VALUES ('r1','Original title','Original body')",
        &[],
    )
    .unwrap();
    sync(&a, &b);

    a.execute("DELETE FROM records WHERE id='r1'", &[]).unwrap();
    a.flush_local().unwrap();

    std::thread::sleep(std::time::Duration::from_millis(3));
    b.execute("UPDATE records SET body='Revived body' WHERE id='r1'", &[]).unwrap();
    sync(&a, &b);

    for (name, db) in [("a", &a), ("b", &b)] {
        let r = db.query("SELECT title, body FROM records WHERE id='r1'", &[]).unwrap();
        assert_eq!(r.rows.len(), 1, "row should be resurrected on {name}");
        assert_eq!(r.rows[0][0], s("Original title"), "untouched column preserved on {name}");
        assert_eq!(r.rows[0][1], s("Revived body"));
    }
}

#[test]
fn a_delete_that_outranks_an_edit_keeps_the_row_deleted() {
    let (a, b) = (replica("device-a"), replica("device-b"));
    a.execute("INSERT INTO records (id, title, body) VALUES ('r1','t','b')", &[]).unwrap();
    sync(&a, &b);

    b.execute("UPDATE records SET body='late edit' WHERE id='r1'", &[]).unwrap();
    b.flush_local().unwrap();

    std::thread::sleep(std::time::Duration::from_millis(3));
    a.execute("DELETE FROM records WHERE id='r1'", &[]).unwrap();
    sync(&a, &b);

    assert_eq!(row_count(&a, "records"), 0);
    assert_eq!(row_count(&b, "records"), 0);
}

#[test]
fn changes_reach_a_device_that_never_met_the_author() {
    // A and C are never connected. B relays. This is what keeps an org in sync
    // when two phones are never on the same network at the same time.
    let (a, b, c) = (replica("device-a"), replica("device-b"), replica("device-c"));
    a.execute("INSERT INTO records (id, title) VALUES ('r1','from A')", &[]).unwrap();

    sync(&a, &b);
    sync(&b, &c);

    assert_eq!(title_of(&c, "r1").as_deref(), Some("from A"));
    assert!(c.version_vector().unwrap().get("device-a") > Hlc::ZERO);
}

#[test]
fn a_rewrite_with_identical_values_produces_no_traffic() {
    let a = replica("device-a");
    a.execute("INSERT INTO records (id, title) VALUES ('r1','same')", &[]).unwrap();
    let (_, changes) = a.execute("UPDATE records SET title='same' WHERE id='r1'", &[]).unwrap();
    assert!(changes.is_empty(), "an update that changes nothing must emit nothing");
}

#[test]
fn a_row_that_predates_replication_is_adopted() {
    let a = replica("device-a");
    // Simulate a table populated before the table was registered for sync.
    a.query("SELECT 1", &[]).unwrap();
    a.execute("INSERT INTO records (id, title) VALUES ('legacy','was here first')", &[]).unwrap();
    a.register_table("records", "id").unwrap();
    a.flush_local().unwrap();

    let b = replica("device-b");
    sync(&a, &b);
    assert_eq!(title_of(&b, "legacy").as_deref(), Some("was here first"));
}

#[test]
fn version_vector_paging_delivers_everything() {
    let a = replica("device-a");
    for i in 0..60 {
        a.execute(
            "INSERT INTO records (id, title, body) VALUES (?1, ?2, ?3)",
            &[s(&format!("r{i}")), s(&format!("title {i}")), s("body")],
        )
        .unwrap();
    }
    let b = replica("device-b");
    // Deliberately tiny pages, to exercise the resume-from-vector path.
    let mut rounds = 0;
    loop {
        let vv = b.version_vector().unwrap();
        let batch = a.changes_since(&vv, 7).unwrap();
        if batch.is_empty() {
            break;
        }
        b.apply_remote(&batch).unwrap();
        rounds += 1;
        assert!(rounds < 500, "paging failed to make progress");
    }
    assert!(rounds > 5, "expected many small pages, got {rounds}");
    assert_eq!(row_count(&b, "records"), 60);
    assert_eq!(title_of(&b, "r59").as_deref(), Some("title 59"));
}

#[test]
fn messages_are_just_replicated_rows() {
    let (a, b) = (replica("device-a"), replica("device-b"));
    a.execute(
        "INSERT INTO messages (id, room, author, author_name, body, sent_at_ms)
         VALUES (?1,?2,?3,?4,?5,?6)",
        &[s("m1"), s("general"), s("device-a"), s("Ada"), s("ship it"), SqlValue::Int(1_700_000_000)],
    )
    .unwrap();
    sync(&a, &b);
    let r = b
        .query("SELECT author_name, body FROM messages WHERE room='general' ORDER BY sent_at_ms", &[])
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], s("Ada"));
    assert_eq!(r.rows[0][1], s("ship it"));
}

#[test]
fn changes_for_an_unknown_table_are_stored_and_still_forwarded() {
    // An older build must not become a hole in the mesh: it keeps changes it
    // cannot materialise so it can still relay them onwards.
    let relay = replica("relay");
    let rec = ChangeRecord {
        tbl: "table_from_the_future".into(),
        pk: "x1".into(),
        col: "whatever".into(),
        value: s("payload"),
        hlc: Hlc::new(1_700_000_000_000, 1),
        origin: "device-z".into(),
        kind: ChangeKind::Cell,
        sig: Vec::new(),
    };
    let out = relay.apply_remote(std::slice::from_ref(&rec)).unwrap();
    assert_eq!(out.applied, 1);
    assert_eq!(out.deferred, 1);

    let forwarded = relay.changes_since(&VersionVector::new(), 32).unwrap();
    assert_eq!(forwarded, vec![rec], "the change must still be relayable");
}

// ---------------------------------------------------------------------------
// Authorship: a replica that requires signatures
// ---------------------------------------------------------------------------

use p2p_core::identity::DeviceIdentity;

fn signed_replica(id: &DeviceIdentity) -> Db {
    Db::open_authenticated_in_memory(id).expect("open signed replica")
}

#[test]
fn signed_replicas_replicate_normally() {
    let (ka, kb) = (DeviceIdentity::generate(), DeviceIdentity::generate());
    let (a, b) = (signed_replica(&ka), signed_replica(&kb));

    a.execute("INSERT INTO records (id, title) VALUES ('r1','signed')", &[]).unwrap();
    sync(&a, &b);
    assert_eq!(title_of(&b, "r1").as_deref(), Some("signed"));

    let batch = a.changes_since(&VersionVector::new(), 16).unwrap();
    assert!(batch.iter().all(|c| !c.sig.is_empty()), "every authored change must be signed");
    assert!(batch.iter().all(|c| c.verify_author().is_ok()));
}

#[test]
fn an_unsigned_change_is_refused() {
    let victim = signed_replica(&DeviceIdentity::generate());
    let naked = ChangeRecord {
        tbl: "records".into(),
        pk: "r1".into(),
        col: "title".into(),
        value: s("no signature"),
        hlc: Hlc::new(1_700_000_000_000, 0),
        origin: DeviceIdentity::generate().peer_id().to_string(),
        kind: ChangeKind::Cell,
        sig: Vec::new(),
    };
    let out = victim.apply_remote(&[naked]).unwrap();
    assert_eq!(out.applied, 0);
    assert_eq!(out.rejected, 1);
    assert_eq!(row_count(&victim, "records"), 0);
}

#[test]
fn a_forged_origin_cannot_poison_a_version_vector() {
    // The attack: a member fabricates a change claiming to come from Ada's
    // phone stamped years in the future. If it were accepted, the receiver's
    // version vector for Ada would jump past everything she actually writes,
    // and her real changes would never be requested again.
    let ada = DeviceIdentity::generate();
    let attacker = DeviceIdentity::generate();
    let bob = signed_replica(&DeviceIdentity::generate());
    let ada_db = signed_replica(&ada);

    let far_future = Hlc::new(p2p_core::hlc::now_ms() + 10 * 365 * 24 * 3_600_000, 0);
    let mut forged = ChangeRecord {
        tbl: "records".into(),
        pk: "r1".into(),
        col: "title".into(),
        value: s("forged"),
        hlc: far_future,
        origin: ada.peer_id().to_string(), // claims to be Ada
        kind: ChangeKind::Cell,
        sig: Vec::new(),
    };
    // Signed with the attacker's own key, which is the best they can do.
    forged.sign_with(&attacker).unwrap();

    let out = bob.apply_remote(&[forged]).unwrap();
    assert_eq!(out.rejected, 1, "the forgery must be refused");
    assert_eq!(out.applied, 0);
    assert_eq!(
        bob.version_vector().unwrap().get(&ada.peer_id().to_string()),
        Hlc::ZERO,
        "a refused change must not move the version vector"
    );

    // Ada's genuine writes still get through afterwards.
    ada_db.execute("INSERT INTO records (id, title) VALUES ('r1','the real thing')", &[]).unwrap();
    sync(&ada_db, &bob);
    assert_eq!(title_of(&bob, "r1").as_deref(), Some("the real thing"));
}

#[test]
fn a_change_cannot_be_edited_in_flight() {
    let ada = DeviceIdentity::generate();
    let ada_db = signed_replica(&ada);
    let bob = signed_replica(&DeviceIdentity::generate());

    ada_db.execute("INSERT INTO records (id, title) VALUES ('r1','approved')", &[]).unwrap();
    let mut batch = ada_db.changes_since(&VersionVector::new(), 16).unwrap();

    // A relaying peer rewrites the payload but keeps Ada's signature.
    for c in batch.iter_mut() {
        if c.col == "title" {
            c.value = s("tampered");
        }
    }
    let out = bob.apply_remote(&batch).unwrap();
    assert!(out.rejected >= 1, "the tampered record must be refused");
    assert_ne!(title_of(&bob, "r1").as_deref(), Some("tampered"));
}

#[test]
fn a_change_survives_both_wire_formats() {
    // Regression guard. `sig` is serialised with `serialize_bytes`, which CBOR
    // writes as a byte string; a naive `Vec<u8>` reader expects an array and
    // fails to decode. The mismatch is invisible in unit tests and silently
    // stops all replication on the wire, so both encodings are pinned here.
    let id = DeviceIdentity::generate();
    let mut rec = ChangeRecord {
        tbl: "records".into(),
        pk: "r1".into(),
        col: "title".into(),
        value: s("round trip"),
        hlc: Hlc::new(1_700_000_000_000, 3),
        origin: id.peer_id().to_string(),
        kind: ChangeKind::Cell,
        sig: Vec::new(),
    };
    rec.sign_with(&id).unwrap();

    // postcard: the gossipsub broadcast path.
    let bytes = postcard::to_stdvec(&rec).unwrap();
    let back: ChangeRecord = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(back, rec);
    back.verify_author().expect("signature must survive postcard");

    // CBOR: the request/response path.
    let cbor = cbor4ii::serde::to_vec(Vec::new(), &rec).unwrap();
    let back: ChangeRecord = cbor4ii::serde::from_slice(&cbor).unwrap();
    assert_eq!(back, rec);
    back.verify_author().expect("signature must survive CBOR");
}
