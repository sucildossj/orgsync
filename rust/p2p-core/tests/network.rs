//! Two real libp2p nodes on the loopback: handshake, trust, and replication.

use std::sync::Arc;
use std::time::Duration;

use p2p_core::db::Db;
use p2p_core::enroll::Enrollment;
use p2p_core::hlc::now_ms;
use p2p_core::identity::{DeviceIdentity, OrgKeypair, RevocationList, Role};
use p2p_core::net::node::{Node, NodeEvent, NodeHandle};
use p2p_core::net::NodeConfig;
use tokio::sync::broadcast::Receiver;

const YEAR: u64 = 365 * 24 * 3_600_000;
const PATIENCE: Duration = Duration::from_secs(20);

fn trace() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

struct Device {
    handle: NodeHandle,
    db: Arc<Db>,
    events: Receiver<NodeEvent>,
}

fn config() -> NodeConfig {
    NodeConfig {
        // Loopback only, and no mDNS: these tests must not depend on — or
        // disturb — whatever else is on the developer's network.
        listen: vec![
            "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
            "/ip4/127.0.0.1/tcp/0".parse().unwrap(),
        ],
        bootstrap: vec![],
        enable_mdns: false,
        enable_relay_client: false,
        enable_relay_server: false,
        kad_server: false,
        replicate: true,
        sync_interval: Duration::from_secs(2),
        flush_interval: Duration::from_millis(200),
        idle_connection_timeout: Duration::from_secs(30),
    }
}

fn spawn_device(org: &OrgKeypair, user: &str, serial: u64, crl: RevocationList) -> Device {
    spawn_full(org, user, serial, crl, config(), Role::Member)
}

fn spawn_with(
    org: &OrgKeypair,
    user: &str,
    serial: u64,
    crl: RevocationList,
    cfg: NodeConfig,
) -> Device {
    spawn_full(org, user, serial, crl, cfg, Role::Member)
}

fn spawn_full(
    org: &OrgKeypair,
    user: &str,
    serial: u64,
    crl: RevocationList,
    cfg: NodeConfig,
    role: Role,
) -> Device {
    let identity = DeviceIdentity::generate();
    let cert = org.issue_cert(identity.public_bytes(), user, user, role, serial, YEAR);
    let enrollment = Enrollment {
        org_id: org.org_id(),
        org_name: "Test Org".into(),
        cert,
        crl,
        bootstrap: vec![],
    };
    let db = Arc::new(Db::open_authenticated_in_memory(&identity).unwrap());
    let handle = Node::spawn(cfg, identity, enrollment, db.clone()).unwrap();
    let events = handle.subscribe();
    Device { handle, db, events }
}

/// Waits for the first event matching `f`, or fails the test.
async fn wait_for<T>(
    rx: &mut Receiver<NodeEvent>,
    what: &str,
    mut f: impl FnMut(&NodeEvent) -> Option<T>,
) -> T {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for {what}");
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(ev)) => {
                if let Some(v) = f(&ev) {
                    return v;
                }
            }
            Ok(Err(e)) => panic!("event stream ended while waiting for {what}: {e}"),
            Err(_) => panic!("timed out waiting for {what}"),
        }
    }
}

async fn first_listen_addr(rx: &mut Receiver<NodeEvent>) -> String {
    wait_for(rx, "a listen address", |ev| match ev {
        NodeEvent::Listening { addr } => Some(addr.clone()),
        _ => None,
    })
    .await
}

/// Polls the replica until `check` passes.
async fn eventually(db: &Db, what: &str, mut check: impl FnMut(&Db) -> bool) {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while tokio::time::Instant::now() < deadline {
        if check(db) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

fn has_title(db: &Db, id: &str, title: &str) -> bool {
    db.query("SELECT title FROM records WHERE id = ?1", &[p2p_core::SqlValue::Text(id.into())])
        .map(|r| {
            r.rows.first().is_some_and(|row| row[0] == p2p_core::SqlValue::Text(title.into()))
        })
        .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread")]
async fn two_devices_in_one_org_authenticate_and_replicate() {
    trace();
    let org = OrgKeypair::generate();
    let crl = RevocationList::empty(org.org_id());
    let mut ada = spawn_device(&org, "ada", 1, crl.clone());
    let mut bob = spawn_device(&org, "bob", 2, crl);

    let ada_addr = first_listen_addr(&mut ada.events).await;
    bob.handle.dial(ada_addr.parse().unwrap()).await.unwrap();

    // Bob must see Ada as a *verified* org member, not merely connected.
    let name = wait_for(&mut bob.events, "an authenticated peer", |ev| match ev {
        NodeEvent::PeerConnected { display_name, .. } => Some(display_name.clone()),
        _ => None,
    })
    .await;
    assert_eq!(name, "ada");

    // A write on Ada reaches Bob without anyone asking for a sync.
    ada.db
        .execute("INSERT INTO records (id, title) VALUES ('r1','written on ada')", &[])
        .unwrap();
    ada.handle.local_changed().await.unwrap();
    eventually(&bob.db, "Ada's row on Bob", |db| has_title(db, "r1", "written on ada")).await;

    // And the reverse direction, including an edit to an existing row.
    bob.db.execute("UPDATE records SET title='edited on bob' WHERE id='r1'", &[]).unwrap();
    bob.handle.local_changed().await.unwrap();
    eventually(&ada.db, "Bob's edit on Ada", |db| has_title(db, "r1", "edited on bob")).await;

    let status = bob.handle.status().await.unwrap();
    assert_eq!(status.peers.len(), 1);
    assert_eq!(status.org_id, org.org_id());

    ada.handle.shutdown().await.unwrap();
    bob.handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_from_another_org_is_refused() {
    let ours = OrgKeypair::generate();
    let theirs = OrgKeypair::generate();

    let mut insider = spawn_device(&ours, "ada", 1, RevocationList::empty(ours.org_id()));
    let outsider = spawn_device(&theirs, "mallory", 1, RevocationList::empty(theirs.org_id()));

    let addr = first_listen_addr(&mut insider.events).await;
    outsider.handle.dial(addr.parse().unwrap()).await.unwrap();

    let reason = wait_for(&mut insider.events, "a rejection", |ev| match ev {
        NodeEvent::PeerRejected { reason, .. } => Some(reason.clone()),
        _ => None,
    })
    .await;
    assert!(
        reason.contains("different organisation"),
        "expected an org mismatch, got: {reason}"
    );

    // Nothing this device writes may reach the other side.
    outsider
        .db
        .execute("INSERT INTO records (id, title) VALUES ('x','should never arrive')", &[])
        .unwrap();
    outsider.handle.local_changed().await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!has_title(&insider.db, "x", "should never arrive"));

    let status = insider.handle.status().await.unwrap();
    assert!(status.peers.is_empty(), "no peer should be authenticated");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_device_is_locked_out_with_no_server_involved() {
    // The revocation is published only to Ada. Nothing contacts a seed server
    // anywhere in this test: the certificate check is entirely local.
    let org = OrgKeypair::generate();
    let revoked_serial = 2;
    let crl = org.sign_crl(vec![revoked_serial], now_ms());

    let mut ada = spawn_device(&org, "ada", 1, crl);
    let stolen = spawn_device(
        &org,
        "stolen-phone",
        revoked_serial,
        RevocationList::empty(org.org_id()),
    );

    let addr = first_listen_addr(&mut ada.events).await;
    stolen.handle.dial(addr.parse().unwrap()).await.unwrap();

    let reason = wait_for(&mut ada.events, "a revocation rejection", |ev| match ev {
        NodeEvent::PeerRejected { reason, .. } => Some(reason.clone()),
        _ => None,
    })
    .await;
    assert!(reason.contains("revoked"), "expected revocation, got: {reason}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_third_device_catches_up_on_everything_it_missed() {
    // Carol joins after all the traffic has happened. Anti-entropy, not the
    // live broadcast, is what has to deliver the history.
    let org = OrgKeypair::generate();
    let crl = RevocationList::empty(org.org_id());
    let mut ada = spawn_device(&org, "ada", 1, crl.clone());
    let bob = spawn_device(&org, "bob", 2, crl.clone());

    let ada_addr = first_listen_addr(&mut ada.events).await;
    bob.handle.dial(ada_addr.parse().unwrap()).await.unwrap();

    for i in 0..25 {
        ada.db
            .execute(
                "INSERT INTO records (id, title) VALUES (?1, ?2)",
                &[
                    p2p_core::SqlValue::Text(format!("r{i}")),
                    p2p_core::SqlValue::Text(format!("row {i}")),
                ],
            )
            .unwrap();
    }
    ada.handle.local_changed().await.unwrap();
    eventually(&bob.db, "Bob to catch up", |db| has_title(db, "r24", "row 24")).await;

    let carol = spawn_device(&org, "carol", 3, crl);
    carol.handle.dial(ada_addr.parse().unwrap()).await.unwrap();
    eventually(&carol.db, "Carol to backfill", |db| {
        has_title(db, "r0", "row 0") && has_title(db, "r24", "row 24")
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_sent_on_one_phone_appears_on_the_other() {
    let org = OrgKeypair::generate();
    let crl = RevocationList::empty(org.org_id());
    let mut ada = spawn_device(&org, "ada", 1, crl.clone());
    let bob = spawn_device(&org, "bob", 2, crl);

    let addr = first_listen_addr(&mut ada.events).await;
    bob.handle.dial(addr.parse().unwrap()).await.unwrap();

    ada.db
        .execute(
            "INSERT INTO messages (id, room, author, author_name, body, sent_at_ms)
             VALUES ('m1','general','ada','Ada','are we synced?', 1700000000000)",
            &[],
        )
        .unwrap();
    ada.handle.local_changed().await.unwrap();

    eventually(&bob.db, "the message on Bob", |db| {
        db.query("SELECT body FROM messages WHERE id='m1'", &[])
            .map(|r| {
                r.rows
                    .first()
                    .is_some_and(|row| row[0] == p2p_core::SqlValue::Text("are we synced?".into()))
            })
            .unwrap_or(false)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_is_announced_exactly_once() {
    // Both sides greet each other, so the handshake completes twice from each
    // node's point of view. The app must still see one arrival, or every UI
    // built on these events shows duplicate peers.
    let org = OrgKeypair::generate();
    let crl = RevocationList::empty(org.org_id());
    let mut ada = spawn_device(&org, "ada", 1, crl.clone());
    let mut bob = spawn_device(&org, "bob", 2, crl);

    let addr = first_listen_addr(&mut ada.events).await;
    bob.handle.dial(addr.parse().unwrap()).await.unwrap();

    wait_for(&mut bob.events, "the first connection", |ev| match ev {
        NodeEvent::PeerConnected { .. } => Some(()),
        _ => None,
    })
    .await;

    // Give the reciprocal handshake ample time to produce a second event.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut extra = 0;
    while let Ok(ev) = bob.events.try_recv() {
        if matches!(ev, NodeEvent::PeerConnected { .. }) {
            extra += 1;
        }
    }
    assert_eq!(extra, 0, "the same peer was announced {} extra times", extra);
    assert_eq!(bob.handle.status().await.unwrap().peers.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_discovery_only_node_is_not_mistaken_for_a_hostile_one() {
    // A seed server that stores no org data still has to hold the connection
    // open: it is the rendezvous point and the relay. If it answered a sync
    // request with a refusal, the peer would disconnect, redial, be refused
    // again, and thrash forever instead of staying reachable.
    let org = OrgKeypair::generate();
    let crl = RevocationList::empty(org.org_id());

    let seed_cfg = NodeConfig { replicate: false, ..config() };
    let mut seed = spawn_with(&org, "seed-server", 0, crl.clone(), seed_cfg);
    let mut phone = spawn_device(&org, "ada", 1, crl);

    let addr = first_listen_addr(&mut seed.events).await;
    phone.handle.dial(addr.parse().unwrap()).await.unwrap();

    wait_for(&mut phone.events, "the seed server", |ev| match ev {
        NodeEvent::PeerConnected { .. } => Some(()),
        _ => None,
    })
    .await;

    // Write something, so the phone tries to push and to run anti-entropy.
    phone.db.execute("INSERT INTO records (id, title) VALUES ('r1','x')", &[]).unwrap();
    phone.handle.local_changed().await.unwrap();
    phone.handle.sync_now().await.unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;

    let mut rejections = Vec::new();
    while let Ok(ev) = phone.events.try_recv() {
        if let NodeEvent::PeerRejected { reason, .. } = ev {
            rejections.push(reason);
        }
    }
    assert!(rejections.is_empty(), "the seed server was treated as hostile: {rejections:?}");

    let status = phone.handle.status().await.unwrap();
    assert_eq!(status.peers.len(), 1, "the phone must still be connected to the seed server");

    // And the seed server genuinely stored nothing.
    assert_eq!(
        seed.db.query("SELECT COUNT(*) FROM records", &[]).unwrap().rows[0][0],
        p2p_core::SqlValue::Int(0)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_only_device_receives_everything_but_writes_nothing() {
    let org = OrgKeypair::generate();
    let crl = RevocationList::empty(org.org_id());
    let mut member = spawn_device(&org, "ada", 1, crl.clone());
    let viewer = spawn_full(&org, "kiosk", 2, crl, config(), Role::ReadOnly);

    let addr = first_listen_addr(&mut member.events).await;
    viewer.handle.dial(addr.parse().unwrap()).await.unwrap();
    eventually(&viewer.db, "the connection", |_| true).await;

    // The member's writes must reach the read-only device.
    member.db.execute("INSERT INTO records (id, title) VALUES ('r1','published')", &[]).unwrap();
    member.handle.local_changed().await.unwrap();
    eventually(&viewer.db, "the member's row", |db| has_title(db, "r1", "published")).await;

    // The read-only device's writes must not reach the member — by push, and
    // equally by broadcasting, which must not be a way around the check.
    viewer.db.execute("INSERT INTO records (id, title) VALUES ('x1','not allowed')", &[]).unwrap();
    viewer.handle.local_changed().await.unwrap();
    viewer.handle.sync_now().await.unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;

    assert!(!has_title(&member.db, "x1", "not allowed"), "a read-only device wrote to the org");

    // And it stays connected: being unable to write is not being unwelcome.
    let status = viewer.handle.status().await.unwrap();
    assert_eq!(status.peers.len(), 1, "the read-only device was disconnected");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_made_while_already_connected_is_pushed_live() {
    // Anti-entropy is pushed an hour out, so the only thing that can deliver
    // this is the live push. Without that, `Db::execute` drains the staged
    // edits as part of the write, the node's own flush then finds nothing to
    // send, and messages quietly wait for the next reconciliation instead of
    // arriving at once — which looks fine in any test patient enough to let
    // anti-entropy run.
    let slow = NodeConfig { sync_interval: Duration::from_secs(3600), ..config() };
    let org = OrgKeypair::generate();
    let crl = RevocationList::empty(org.org_id());
    let mut ada = spawn_with(&org, "ada", 1, crl.clone(), slow.clone());
    let mut bob = spawn_with(&org, "bob", 2, crl, slow);

    let addr = first_listen_addr(&mut ada.events).await;
    bob.handle.dial(addr.parse().unwrap()).await.unwrap();
    wait_for(&mut bob.events, "the connection", |ev| match ev {
        NodeEvent::PeerConnected { .. } => Some(()),
        _ => None,
    })
    .await;
    // Let the handshake's one-off catch-up finish, so it cannot be what
    // delivers the row written below.
    tokio::time::sleep(Duration::from_millis(500)).await;

    ada.db.execute("INSERT INTO records (id, title) VALUES ('live','pushed live')", &[]).unwrap();
    ada.handle.local_changed().await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if has_title(&bob.db, "live", "pushed live") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the write never arrived; the live push path is broken");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_only_device_still_relays_other_devices_writes() {
    // The other half of the rule. Ada and Bob never meet; a read-only kiosk
    // sits between them. Filtering on the author rather than the sender is
    // what lets it do its job as a full replica while still being unable to
    // contribute writes of its own.
    let org = OrgKeypair::generate();
    let crl = RevocationList::empty(org.org_id());
    let mut ada = spawn_device(&org, "ada", 1, crl.clone());
    let mut kiosk = spawn_full(&org, "kiosk", 2, crl.clone(), config(), Role::ReadOnly);
    let bob = spawn_device(&org, "bob", 3, crl);

    let ada_addr = first_listen_addr(&mut ada.events).await;
    kiosk.handle.dial(ada_addr.parse().unwrap()).await.unwrap();

    ada.db.execute("INSERT INTO records (id, title) VALUES ('r1','from ada')", &[]).unwrap();
    ada.handle.local_changed().await.unwrap();
    eventually(&kiosk.db, "the kiosk to receive", |db| has_title(db, "r1", "from ada")).await;

    let kiosk_addr = first_listen_addr(&mut kiosk.events).await;
    bob.handle.dial(kiosk_addr.parse().unwrap()).await.unwrap();

    eventually(&bob.db, "Ada's row relayed through the kiosk", |db| {
        has_title(db, "r1", "from ada")
    })
    .await;
}
