//! Organisation identity: the org root key, device certificates, revocation.
//!
//! The trust model is deliberately small:
//!
//! * The seed server holds an Ed25519 **org root key**. Its public key hashes
//!   to the `OrgId` that every device pins at enrolment time.
//! * Enrolling a device mints a **[`DeviceCert`]**: the org root signs a
//!   statement binding a device's public key to a user, a role and an expiry.
//! * A device's Ed25519 key *is* its libp2p identity, so the PeerId that
//!   libp2p's Noise handshake authenticates is the same key named in the
//!   certificate. Verifying `cert.device_pub == remote_peer_id` is therefore
//!   enough to bind the certificate to the live connection — no extra
//!   challenge/response round trip is needed.
//! * Because verification is a plain signature check against a pinned org key,
//!   **two phones on an office LAN can authenticate each other with the seed
//!   server switched off.**

use libp2p::identity::{ed25519, Keypair, PublicKey};
use libp2p::PeerId;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::hlc::now_ms;

const CERT_DOMAIN: &[u8] = b"p2p-org-devcert-v1";
const CRL_DOMAIN: &[u8] = b"p2p-org-crl-v1";
const ENROLL_DOMAIN: &[u8] = b"p2p-org-enroll-request-v1";

/// Identifies an organisation: `blake3(org_public_key)`, hex encoded.
pub type OrgId = String;

pub fn org_id_from_public(org_pub: &[u8; 32]) -> OrgId {
    hex::encode(blake3::hash(org_pub).as_bytes())
}

/// What a device is allowed to do inside the org.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// May enrol and revoke other devices, and write any synced table.
    Admin,
    /// May read and write synced tables.
    Member,
    /// Receives changes but its writes are rejected by peers.
    ReadOnly,
}

impl Role {
    pub fn as_u8(self) -> u8 {
        match self {
            Role::Admin => 0,
            Role::Member => 1,
            Role::ReadOnly => 2,
        }
    }
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Role::Admin),
            1 => Some(Role::Member),
            2 => Some(Role::ReadOnly),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Member => "member",
            Role::ReadOnly => "readonly",
        }
    }
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "admin" => Role::Admin,
            "readonly" => Role::ReadOnly,
            _ => Role::Member,
        }
    }
    /// Whether a peer holding this role is permitted to author changes.
    pub fn may_write(self) -> bool {
        matches!(self, Role::Admin | Role::Member)
    }
}

/// The signed statement itself, without the signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertClaims {
    pub org_id: OrgId,
    /// Raw Ed25519 public key of the device. Also determines its PeerId.
    #[serde(with = "hex_bytes32")]
    pub device_pub: [u8; 32],
    pub user_id: String,
    pub display_name: String,
    pub role: Role,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    /// Monotonic per-org serial; what a revocation list names.
    pub serial: u64,
}

impl CertClaims {
    /// Deterministic pre-image for the signature.
    ///
    /// Built by hand with explicit length prefixes rather than by reusing a
    /// serde encoding: the bytes a signature covers must never shift because a
    /// field was reordered or a serialiser was swapped out.
    fn signing_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(160 + self.user_id.len() + self.display_name.len());
        b.extend_from_slice(CERT_DOMAIN);
        put_str(&mut b, &self.org_id);
        b.extend_from_slice(&self.device_pub);
        put_str(&mut b, &self.user_id);
        put_str(&mut b, &self.display_name);
        b.push(self.role.as_u8());
        b.extend_from_slice(&self.issued_at_ms.to_le_bytes());
        b.extend_from_slice(&self.expires_at_ms.to_le_bytes());
        b.extend_from_slice(&self.serial.to_le_bytes());
        b
    }
}

/// A device certificate: claims plus the org root's signature over them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCert {
    pub claims: CertClaims,
    /// The org root public key, so a verifier needs only the pinned `OrgId`.
    #[serde(with = "hex_bytes32")]
    pub org_pub: [u8; 32],
    #[serde(with = "hex_bytes")]
    pub signature: Vec<u8>,
}

impl DeviceCert {
    pub fn peer_id(&self) -> Result<PeerId> {
        device_pub_to_peer_id(&self.claims.device_pub)
    }

    pub fn is_expired_at(&self, now: u64) -> bool {
        now >= self.claims.expires_at_ms
    }

    /// Full offline validation against a pinned org id.
    ///
    /// `expected_peer` binds the certificate to the identity libp2p already
    /// authenticated during the Noise handshake.
    pub fn verify(
        &self,
        expected_org: &str,
        expected_peer: Option<&PeerId>,
        crl: Option<&RevocationList>,
        now_ms_: u64,
    ) -> Result<()> {
        if org_id_from_public(&self.org_pub) != expected_org {
            return Err(Error::Rejected("certificate is from a different organisation".into()));
        }
        if self.claims.org_id != expected_org {
            return Err(Error::Rejected("certificate org id does not match its signing key".into()));
        }
        let org_key = ed25519_public(&self.org_pub)?;
        if !org_key.verify(&self.claims.signing_bytes(), &self.signature) {
            return Err(Error::Rejected("certificate signature is not valid".into()));
        }
        if now_ms_ >= self.claims.expires_at_ms {
            return Err(Error::Rejected("certificate has expired".into()));
        }
        if now_ms_ + crate::hlc::MAX_CLOCK_DRIFT_MS < self.claims.issued_at_ms {
            return Err(Error::Rejected("certificate is not valid yet".into()));
        }
        if let Some(peer) = expected_peer {
            let cert_peer = self.peer_id()?;
            if cert_peer != *peer {
                return Err(Error::Rejected(
                    "certificate belongs to a different device than the one connected".into(),
                ));
            }
        }
        if let Some(crl) = crl {
            if crl.is_revoked(self.claims.serial) {
                return Err(Error::Rejected("certificate has been revoked".into()));
            }
        }
        Ok(())
    }
}

/// The org root key. Only the seed server ever holds the private half.
#[derive(Clone)]
pub struct OrgKeypair {
    inner: ed25519::Keypair,
}

impl std::fmt::Debug for OrgKeypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrgKeypair").field("org_id", &self.org_id()).finish_non_exhaustive()
    }
}

impl OrgKeypair {
    pub fn generate() -> Self {
        Self { inner: ed25519::Keypair::generate() }
    }

    /// Restores from the 64-byte libp2p Ed25519 keypair encoding.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let mut buf = bytes.to_vec();
        let inner = ed25519::Keypair::try_from_bytes(&mut buf)
            .map_err(|e| Error::Crypto(format!("bad org key: {e}")))?;
        Ok(Self { inner })
    }

    pub fn to_bytes(&self) -> [u8; 64] {
        self.inner.to_bytes()
    }

    pub fn public_bytes(&self) -> [u8; 32] {
        self.inner.public().to_bytes()
    }

    pub fn org_id(&self) -> OrgId {
        org_id_from_public(&self.public_bytes())
    }

    /// Mints a certificate for a device.
    pub fn issue_cert(
        &self,
        device_pub: [u8; 32],
        user_id: impl Into<String>,
        display_name: impl Into<String>,
        role: Role,
        serial: u64,
        valid_for_ms: u64,
    ) -> DeviceCert {
        let issued_at_ms = now_ms();
        let claims = CertClaims {
            org_id: self.org_id(),
            device_pub,
            user_id: user_id.into(),
            display_name: display_name.into(),
            role,
            issued_at_ms,
            expires_at_ms: issued_at_ms.saturating_add(valid_for_ms),
            serial,
        };
        let signature = self.inner.sign(&claims.signing_bytes());
        DeviceCert { claims, org_pub: self.public_bytes(), signature }
    }

    pub fn sign_crl(&self, mut revoked: Vec<u64>, updated_at_ms: u64) -> RevocationList {
        revoked.sort_unstable();
        revoked.dedup();
        let mut crl = RevocationList {
            org_id: self.org_id(),
            updated_at_ms,
            revoked,
            org_pub: self.public_bytes(),
            signature: Vec::new(),
        };
        crl.signature = self.inner.sign(&crl.signing_bytes());
        crl
    }
}

/// A signed list of revoked certificate serials.
///
/// Peers gossip this to each other, so a revoked phone gets locked out of the
/// LAN even when nobody can reach the seed server. Newest `updated_at_ms` wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevocationList {
    pub org_id: OrgId,
    pub updated_at_ms: u64,
    pub revoked: Vec<u64>,
    #[serde(with = "hex_bytes32")]
    pub org_pub: [u8; 32],
    #[serde(with = "hex_bytes")]
    pub signature: Vec<u8>,
}

impl RevocationList {
    pub fn empty(org_id: OrgId) -> Self {
        Self {
            org_id,
            updated_at_ms: 0,
            revoked: Vec::new(),
            org_pub: [0u8; 32],
            signature: Vec::new(),
        }
    }

    fn signing_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(64 + self.revoked.len() * 8);
        b.extend_from_slice(CRL_DOMAIN);
        put_str(&mut b, &self.org_id);
        b.extend_from_slice(&self.updated_at_ms.to_le_bytes());
        b.extend_from_slice(&(self.revoked.len() as u32).to_le_bytes());
        for s in &self.revoked {
            b.extend_from_slice(&s.to_le_bytes());
        }
        b
    }

    pub fn is_revoked(&self, serial: u64) -> bool {
        self.revoked.binary_search(&serial).is_ok()
    }

    pub fn verify(&self, expected_org: &str) -> Result<()> {
        if self.updated_at_ms == 0 && self.revoked.is_empty() {
            return Ok(()); // the empty bootstrap list carries no authority
        }
        if self.org_id != expected_org || org_id_from_public(&self.org_pub) != expected_org {
            return Err(Error::Rejected("revocation list is for a different organisation".into()));
        }
        if !self.revoked.windows(2).all(|w| w[0] < w[1]) {
            return Err(Error::Rejected("revocation list is not sorted and deduplicated".into()));
        }
        let key = ed25519_public(&self.org_pub)?;
        if !key.verify(&self.signing_bytes(), &self.signature) {
            return Err(Error::Rejected("revocation list signature is not valid".into()));
        }
        Ok(())
    }
}

/// This device's long-lived keypair plus, once enrolled, its certificate.
#[derive(Clone)]
pub struct DeviceIdentity {
    keypair: Keypair,
    ed: ed25519::Keypair,
}

impl std::fmt::Debug for DeviceIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceIdentity").field("peer_id", &self.peer_id()).finish_non_exhaustive()
    }
}

impl DeviceIdentity {
    pub fn generate() -> Self {
        let ed = ed25519::Keypair::generate();
        Self { keypair: Keypair::from(ed.clone()), ed }
    }

    /// Restores from the 32-byte Ed25519 secret scalar.
    pub fn from_secret(secret: &[u8]) -> Result<Self> {
        if secret.len() != 32 {
            return Err(Error::Crypto("device secret key must be 32 bytes".into()));
        }
        let mut buf = [0u8; 32];
        buf.copy_from_slice(secret);
        let sk = ed25519::SecretKey::try_from_bytes(&mut buf)
            .map_err(|e| Error::Crypto(format!("bad device secret: {e}")))?;
        let ed = ed25519::Keypair::from(sk);
        Ok(Self { keypair: Keypair::from(ed.clone()), ed })
    }

    pub fn secret_bytes(&self) -> [u8; 32] {
        self.ed.secret().as_ref().try_into().expect("ed25519 secret is 32 bytes")
    }

    pub fn public_bytes(&self) -> [u8; 32] {
        self.ed.public().to_bytes()
    }

    pub fn peer_id(&self) -> PeerId {
        self.keypair.public().to_peer_id()
    }

    /// The libp2p keypair, used for the Noise handshake and gossipsub signing.
    pub fn libp2p_keypair(&self) -> Keypair {
        self.keypair.clone()
    }

    /// Proves possession of the device key to the seed server at enrolment,
    /// so an invite token alone cannot be used to enrol an attacker's key.
    pub fn sign_enrollment(&self, invite_token: &str, device_name: &str, at_ms: u64) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(ENROLL_DOMAIN);
        put_str(&mut b, invite_token);
        put_str(&mut b, device_name);
        b.extend_from_slice(&self.public_bytes());
        b.extend_from_slice(&at_ms.to_le_bytes());
        self.ed.sign(&b)
    }
}

/// Server-side counterpart of [`DeviceIdentity::sign_enrollment`].
pub fn verify_enrollment_proof(
    device_pub: &[u8; 32],
    invite_token: &str,
    device_name: &str,
    at_ms: u64,
    signature: &[u8],
) -> Result<()> {
    let mut b = Vec::new();
    b.extend_from_slice(ENROLL_DOMAIN);
    put_str(&mut b, invite_token);
    put_str(&mut b, device_name);
    b.extend_from_slice(device_pub);
    b.extend_from_slice(&at_ms.to_le_bytes());
    let key = ed25519_public(device_pub)?;
    if key.verify(&b, signature) {
        Ok(())
    } else {
        Err(Error::Rejected("enrolment proof-of-possession is not valid".into()))
    }
}

pub fn device_pub_to_peer_id(device_pub: &[u8; 32]) -> Result<PeerId> {
    Ok(PublicKey::from(ed25519_public(device_pub)?).to_peer_id())
}

fn ed25519_public(bytes: &[u8; 32]) -> Result<ed25519::PublicKey> {
    ed25519::PublicKey::try_from_bytes(bytes)
        .map_err(|e| Error::Crypto(format!("bad ed25519 public key: {e}")))
}

fn put_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}

mod hex_bytes32 {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(v))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        let raw = hex::decode(&s).map_err(serde::de::Error::custom)?;
        raw.try_into().map_err(|_| serde::de::Error::custom("expected 32 bytes"))
    }
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &Vec<u8>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(v))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        hex::decode(&s).map_err(serde::de::Error::custom)
    }
}

/// Recovers a device's Ed25519 public key from its PeerId.
///
/// libp2p inlines public keys of 42 bytes or fewer directly into the PeerId
/// multihash, and an Ed25519 key always is. That means the author of a change
/// can be verified from the `origin` field alone — no key directory, no extra
/// exchange, and it works for changes relayed by a device we have never met.
pub fn peer_public_key(peer: &PeerId) -> Result<PublicKey> {
    use libp2p::multihash::Multihash;
    let mh = Multihash::<64>::from_bytes(&peer.to_bytes())
        .map_err(|e| Error::Crypto(format!("bad peer id: {e}")))?;
    if mh.code() != 0 {
        return Err(Error::Crypto(
            "peer id hashes its key rather than inlining it; not an ed25519 identity".into(),
        ));
    }
    PublicKey::try_decode_protobuf(mh.digest()).map_err(Into::into)
}

pub fn peer_public_key_from_str(peer: &str) -> Result<PublicKey> {
    let pid: PeerId = peer.parse().map_err(|e| Error::Crypto(format!("bad peer id `{peer}`: {e}")))?;
    peer_public_key(&pid)
}
