//! Enrolment: the one moment a device is admitted to the organisation.

use p2p_core::enroll::{build_enroll_request, EnrollResponse, Enrollment, OrgInfo};
use p2p_core::hlc::now_ms;
use p2p_core::identity::{DeviceIdentity, Role};

#[path = "../src/store.rs"]
#[allow(dead_code)] // the binary uses the rest of this module's surface
mod store;
use store::Store;

fn fresh() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("seed.db")).unwrap();
    store.init("Acme").unwrap();
    (dir, store)
}

fn accept(store: &Store, cert: p2p_core::DeviceCert, id: &DeviceIdentity) -> Enrollment {
    let org = store.org_keypair().unwrap();
    Enrollment::accept(
        EnrollResponse {
            cert,
            crl: store.crl().unwrap(),
            org: OrgInfo {
                org_id: org.org_id(),
                org_pub: hex::encode(org.public_bytes()),
                name: "Acme".into(),
                bootstrap: vec![],
            },
        },
        id,
    )
    .unwrap()
}

#[test]
fn a_device_enrols_and_gets_a_usable_certificate() {
    let (_dir, store) = fresh();
    let invite = store.create_invite("ada", "Ada", Role::Member, 3_600_000).unwrap();
    let device = DeviceIdentity::generate();

    // Exactly what the mobile app posts.
    let req = build_enroll_request(&device, &invite.token, "Ada's iPhone", "ios");
    let cert = store
        .redeem_invite(
            &req.invite_token,
            &req.device_pub,
            &req.device_name,
            &req.platform,
            req.at_ms,
            &req.proof,
            365 * 24 * 3_600_000,
        )
        .unwrap();

    let enrollment = accept(&store, cert, &device);
    enrollment.validate(&device).unwrap();
    assert_eq!(enrollment.cert.claims.user_id, "ada");
    assert_eq!(enrollment.cert.claims.role, Role::Member);
    assert_eq!(enrollment.cert.peer_id().unwrap(), device.peer_id());

    assert_eq!(store.list_devices().unwrap().len(), 1);
}

#[test]
fn the_grouped_code_a_person_actually_types_is_accepted() {
    // The CLI prints `4KP7M-9XQ2T-…`; the stored token has no dashes. If the
    // two forms did not normalise to the same thing, every hand-typed code
    // would be rejected.
    let (_dir, store) = fresh();
    let invite = store.create_invite("ada", "Ada", Role::Member, 3_600_000).unwrap();
    let typed = format!("  {}  ", invite.invite_code_display().to_lowercase());

    let device = DeviceIdentity::generate();
    let req = build_enroll_request(&device, &typed, "phone", "android");
    store
        .redeem_invite(
            &req.invite_token,
            &req.device_pub,
            &req.device_name,
            &req.platform,
            req.at_ms,
            &req.proof,
            1_000_000,
        )
        .expect("a grouped, lower-cased, padded code must still work");
}

#[test]
fn an_invite_cannot_be_used_twice() {
    let (_dir, store) = fresh();
    let invite = store.create_invite("ada", "Ada", Role::Member, 3_600_000).unwrap();

    for (attempt, expect_ok) in [(1, true), (2, false)] {
        let device = DeviceIdentity::generate();
        let req = build_enroll_request(&device, &invite.token, "phone", "ios");
        let result = store.redeem_invite(
            &req.invite_token,
            &req.device_pub,
            &req.device_name,
            &req.platform,
            req.at_ms,
            &req.proof,
            1_000_000,
        );
        assert_eq!(result.is_ok(), expect_ok, "attempt {attempt}");
        if let Err(e) = result {
            assert!(format!("{e}").contains("already been used"), "got {e}");
        }
    }
}

#[test]
fn a_stolen_invite_cannot_be_redeemed_against_another_key() {
    // The attacker has the code but must still present a proof made with the
    // key being certified, which they cannot forge.
    let (_dir, store) = fresh();
    let invite = store.create_invite("ada", "Ada", Role::Member, 3_600_000).unwrap();

    let ada = DeviceIdentity::generate();
    let mallory = DeviceIdentity::generate();
    let req = build_enroll_request(&ada, &invite.token, "phone", "ios");

    let err = store
        .redeem_invite(
            &req.invite_token,
            &hex::encode(mallory.public_bytes()), // swapped key, Ada's proof
            &req.device_name,
            &req.platform,
            req.at_ms,
            &req.proof,
            1_000_000,
        )
        .unwrap_err();
    assert!(format!("{err:#}").contains("proof"), "got {err:#}");
}

#[test]
fn an_expired_invite_is_refused() {
    let (_dir, store) = fresh();
    let invite = store.create_invite("ada", "Ada", Role::Member, 0).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));

    let device = DeviceIdentity::generate();
    let req = build_enroll_request(&device, &invite.token, "phone", "ios");
    let err = store
        .redeem_invite(
            &req.invite_token,
            &req.device_pub,
            &req.device_name,
            &req.platform,
            req.at_ms,
            &req.proof,
            1_000_000,
        )
        .unwrap_err();
    assert!(format!("{err}").contains("expired"), "got {err}");
}

#[test]
fn a_replayed_request_from_long_ago_is_refused() {
    let (_dir, store) = fresh();
    let invite = store.create_invite("ada", "Ada", Role::Member, 3_600_000).unwrap();
    let device = DeviceIdentity::generate();
    let stale = now_ms() - 30 * 60 * 1000;
    let proof = device.sign_enrollment(&invite.token, "phone", stale);

    let err = store
        .redeem_invite(
            &invite.token,
            &hex::encode(device.public_bytes()),
            "phone",
            "ios",
            stale,
            &hex::encode(proof),
            1_000_000,
        )
        .unwrap_err();
    assert!(format!("{err}").contains("too far from server time"), "got {err}");
}

#[test]
fn revoking_a_device_puts_it_on_a_signed_list_everyone_can_check() {
    let (_dir, store) = fresh();
    let invite = store.create_invite("ada", "Ada", Role::Member, 3_600_000).unwrap();
    let device = DeviceIdentity::generate();
    let req = build_enroll_request(&device, &invite.token, "phone", "ios");
    let cert = store
        .redeem_invite(
            &req.invite_token, &req.device_pub, &req.device_name,
            &req.platform, req.at_ms, &req.proof, 1_000_000,
        )
        .unwrap();

    assert!(store.crl().unwrap().revoked.is_empty());
    store.revoke(cert.claims.serial).unwrap();

    let crl = store.crl().unwrap();
    let org_id = store.org_keypair().unwrap().org_id();
    crl.verify(&org_id).expect("the list must be properly signed");
    assert!(crl.is_revoked(cert.claims.serial));

    // Which is exactly what makes any peer refuse the device, offline.
    let err = cert.verify(&org_id, Some(&device.peer_id()), Some(&crl), now_ms()).unwrap_err();
    assert!(format!("{err}").contains("revoked"));

    assert!(store.revoke(cert.claims.serial).is_err(), "revoking twice should be an error");
}

#[test]
fn the_root_key_cannot_be_regenerated_by_accident() {
    // Re-initialising would silently invalidate every certificate in the org.
    let (dir, store) = fresh();
    let first = store.org_keypair().unwrap().org_id();
    assert!(store.init("Acme Again").is_err());

    drop(store);
    let reopened = Store::open(dir.path().join("seed.db")).unwrap();
    assert_eq!(reopened.org_keypair().unwrap().org_id(), first);
    assert!(reopened.init("Nope").is_err());
}

#[test]
fn the_server_identity_is_stable_across_restarts() {
    // Bootstrap multiaddrs embed this peer id; if it moved, every enrolled
    // device would be left holding an address that no longer resolves.
    let (dir, store) = fresh();
    let before = store.node_identity().unwrap().peer_id();
    drop(store);
    let reopened = Store::open(dir.path().join("seed.db")).unwrap();
    assert_eq!(reopened.node_identity().unwrap().peer_id(), before);
}
