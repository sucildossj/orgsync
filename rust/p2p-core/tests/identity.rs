//! Trust decisions a device makes about its peers, all of them offline.

use p2p_core::hlc::now_ms;
use p2p_core::identity::{
    verify_enrollment_proof, DeviceIdentity, OrgKeypair, RevocationList, Role,
};

const YEAR: u64 = 365 * 24 * 3_600_000;

fn enrolled() -> (OrgKeypair, DeviceIdentity, p2p_core::identity::DeviceCert) {
    let org = OrgKeypair::generate();
    let dev = DeviceIdentity::generate();
    let cert = org.issue_cert(dev.public_bytes(), "ada", "Ada's phone", Role::Member, 1, YEAR);
    (org, dev, cert)
}

#[test]
fn a_freshly_issued_certificate_verifies_against_the_pinned_org() {
    let (org, dev, cert) = enrolled();
    cert.verify(&org.org_id(), Some(&dev.peer_id()), None, now_ms())
        .expect("cert should verify");
}

#[test]
fn the_certificate_key_is_the_peer_id_libp2p_authenticates() {
    // This is the whole binding: Noise proves the peer holds the key behind
    // its PeerId, and the certificate names that same key. No extra challenge
    // round trip is needed.
    let (_, dev, cert) = enrolled();
    assert_eq!(cert.peer_id().unwrap(), dev.peer_id());
}

#[test]
fn a_certificate_from_another_org_is_rejected() {
    let (_, dev, cert) = enrolled();
    let stranger = OrgKeypair::generate();
    let err = cert.verify(&stranger.org_id(), Some(&dev.peer_id()), None, now_ms()).unwrap_err();
    assert!(matches!(err, p2p_core::Error::Rejected(_)), "got {err:?}");
}

#[test]
fn a_certificate_cannot_be_replayed_by_a_different_device() {
    // An attacker who copies Ada's certificate still cannot present it, because
    // it will not match the key their connection was authenticated with.
    let (org, _, cert) = enrolled();
    let attacker = DeviceIdentity::generate();
    let err = cert.verify(&org.org_id(), Some(&attacker.peer_id()), None, now_ms()).unwrap_err();
    assert!(matches!(err, p2p_core::Error::Rejected(_)));
}

#[test]
fn tampering_with_any_claim_invalidates_the_signature() {
    let (org, dev, cert) = enrolled();
    for mutate in [
        (|c: &mut p2p_core::identity::DeviceCert| c.claims.role = Role::Admin) as fn(&mut _),
        |c| c.claims.expires_at_ms += YEAR,
        |c| c.claims.user_id = "root".to_string(),
        |c| c.claims.serial = 999,
    ] {
        let mut forged = cert.clone();
        mutate(&mut forged);
        assert!(
            forged.verify(&org.org_id(), Some(&dev.peer_id()), None, now_ms()).is_err(),
            "a mutated claim must break verification"
        );
    }
}

#[test]
fn an_expired_certificate_is_rejected() {
    let org = OrgKeypair::generate();
    let dev = DeviceIdentity::generate();
    let cert = org.issue_cert(dev.public_bytes(), "ada", "phone", Role::Member, 1, 1_000);
    let err = cert
        .verify(&org.org_id(), Some(&dev.peer_id()), None, now_ms() + 60_000)
        .unwrap_err();
    assert!(format!("{err}").contains("expired"), "got {err}");
}

#[test]
fn revocation_locks_a_device_out_with_no_server_reachable() {
    let (org, dev, cert) = enrolled();
    let crl = org.sign_crl(vec![1], now_ms());
    crl.verify(&org.org_id()).expect("crl should verify");

    let err = cert
        .verify(&org.org_id(), Some(&dev.peer_id()), Some(&crl), now_ms())
        .unwrap_err();
    assert!(format!("{err}").contains("revoked"), "got {err}");

    // A different device's serial is unaffected.
    let other = org.issue_cert(DeviceIdentity::generate().public_bytes(), "bob", "b", Role::Member, 2, YEAR);
    assert!(!crl.is_revoked(other.claims.serial));
}

#[test]
fn a_revocation_list_cannot_be_forged_or_reordered() {
    let org = OrgKeypair::generate();
    let real = org.sign_crl(vec![1, 2, 3], now_ms());

    let mut extra = real.clone();
    extra.revoked.push(4);
    assert!(extra.verify(&org.org_id()).is_err(), "adding a serial must break the signature");

    let mut unsorted = real.clone();
    unsorted.revoked = vec![3, 1, 2];
    assert!(unsorted.verify(&org.org_id()).is_err(), "an unsorted list must be rejected");

    let attacker = OrgKeypair::generate();
    let fake = attacker.sign_crl(vec![1], now_ms());
    assert!(fake.verify(&org.org_id()).is_err(), "another key must not sign our CRL");
}

#[test]
fn the_empty_bootstrap_list_carries_no_authority() {
    let org = OrgKeypair::generate();
    let empty = RevocationList::empty(org.org_id());
    assert!(empty.verify(&org.org_id()).is_ok());
    assert!(!empty.is_revoked(1));
}

#[test]
fn enrolment_requires_proof_the_device_holds_its_key() {
    // Stops a stolen invite token from being used to enrol an attacker's key.
    let dev = DeviceIdentity::generate();
    let at = now_ms();
    let sig = dev.sign_enrollment("invite-abc", "Ada's phone", at);
    verify_enrollment_proof(&dev.public_bytes(), "invite-abc", "Ada's phone", at, &sig).unwrap();

    let attacker = DeviceIdentity::generate();
    assert!(
        verify_enrollment_proof(&attacker.public_bytes(), "invite-abc", "Ada's phone", at, &sig).is_err(),
        "the proof must not transfer to another key"
    );
    assert!(
        verify_enrollment_proof(&dev.public_bytes(), "different-invite", "Ada's phone", at, &sig).is_err(),
        "the proof must be bound to the invite it was made for"
    );
}

#[test]
fn keys_survive_a_round_trip_through_storage() {
    let org = OrgKeypair::generate();
    let restored = OrgKeypair::from_bytes(&org.to_bytes()).unwrap();
    assert_eq!(org.org_id(), restored.org_id());

    let dev = DeviceIdentity::generate();
    let dev2 = DeviceIdentity::from_secret(&dev.secret_bytes()).unwrap();
    assert_eq!(dev.peer_id(), dev2.peer_id());

    // And a certificate issued before the restart still verifies after it.
    let cert = org.issue_cert(dev.public_bytes(), "ada", "phone", Role::Member, 7, YEAR);
    cert.verify(&restored.org_id(), Some(&dev2.peer_id()), None, now_ms()).unwrap();
}

#[test]
fn a_readonly_device_is_marked_as_unable_to_write() {
    assert!(!Role::ReadOnly.may_write());
    assert!(Role::Member.may_write() && Role::Admin.may_write());
}
