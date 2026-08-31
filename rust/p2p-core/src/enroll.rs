//! Joining an organisation.
//!
//! Enrolment is deliberately plain HTTP JSON rather than something the node
//! speaks over libp2p, so the mobile app can do the network call with its own
//! `fetch` and the Rust binary never has to carry a TLS stack. The split is:
//!
//! ```text
//!   Rust  build_enroll_request()  ──► JSON  ──►  app POSTs it to the seed server
//!   app   ◄── JSON cert ───────────────────────  seed server signs and replies
//!   Rust  Enrollment::accept()  verifies and pins the org
//! ```
//!
//! The request carries a proof of possession of the device key, so an invite
//! token that leaks cannot be redeemed against somebody else's key.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::hlc::now_ms;
use crate::identity::{
    org_id_from_public, DeviceCert, DeviceIdentity, OrgId, RevocationList,
};

/// What a device sends to `POST /v1/enroll`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollRequest {
    pub invite_token: String,
    pub device_name: String,
    pub platform: String,
    /// Hex-encoded Ed25519 public key; becomes the device's PeerId.
    pub device_pub: String,
    pub at_ms: u64,
    /// Hex signature proving the sender holds the matching secret key.
    pub proof: String,
}

/// Public description of an org, served unauthenticated at `GET /v1/org`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrgInfo {
    pub org_id: OrgId,
    pub org_pub: String,
    pub name: String,
    /// Multiaddrs of the seed server(s), each ending in `/p2p/<peer id>`.
    pub bootstrap: Vec<String>,
}

/// What the seed server returns from a successful enrolment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollResponse {
    pub cert: DeviceCert,
    pub crl: RevocationList,
    pub org: OrgInfo,
}

/// Everything a node needs to prove it belongs and to find its peers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Enrollment {
    pub org_id: OrgId,
    pub org_name: String,
    pub cert: DeviceCert,
    pub crl: RevocationList,
    pub bootstrap: Vec<String>,
}

pub const META_KEY: &str = "enrollment";

/// Canonical form of an invite code.
///
/// Codes are shown grouped (`4KP7M-9XQ2T-…`) because that is far easier to
/// read out or type, but the grouping is presentation only. Both the client's
/// proof-of-possession and the server's lookup run on this normalised form, so
/// spaces, dashes and case never matter.
pub fn normalize_invite_code(raw: &str) -> String {
    raw.chars().filter(|c| c.is_ascii_alphanumeric()).flat_map(|c| c.to_uppercase()).collect()
}

pub fn build_enroll_request(
    identity: &DeviceIdentity,
    invite_token: &str,
    device_name: &str,
    platform: &str,
) -> EnrollRequest {
    let invite_token = normalize_invite_code(invite_token);
    let at_ms = now_ms();
    let proof = identity.sign_enrollment(&invite_token, device_name, at_ms);
    EnrollRequest {
        invite_token,
        device_name: device_name.to_string(),
        platform: platform.to_string(),
        device_pub: hex::encode(identity.public_bytes()),
        at_ms,
        proof: hex::encode(proof),
    }
}

impl Enrollment {
    /// Validates a server's reply before trusting any of it.
    ///
    /// A hostile or misconfigured server cannot enrol us into an org whose id
    /// does not match its own key, nor hand us a certificate for a key we do
    /// not hold.
    pub fn accept(resp: EnrollResponse, identity: &DeviceIdentity) -> Result<Self> {
        let org_pub: [u8; 32] = hex::decode(&resp.org.org_pub)
            .map_err(|e| Error::Crypto(format!("bad org public key: {e}")))?
            .try_into()
            .map_err(|_| Error::Crypto("org public key must be 32 bytes".into()))?;

        if org_id_from_public(&org_pub) != resp.org.org_id {
            return Err(Error::Rejected("server's org id does not match its own key".into()));
        }
        if resp.cert.claims.device_pub != identity.public_bytes() {
            return Err(Error::Rejected(
                "server issued a certificate for a different device key".into(),
            ));
        }
        resp.cert.verify(&resp.org.org_id, Some(&identity.peer_id()), None, now_ms())?;
        resp.crl.verify(&resp.org.org_id)?;

        Ok(Enrollment {
            org_id: resp.org.org_id,
            org_name: resp.org.name,
            cert: resp.cert,
            crl: resp.crl,
            bootstrap: resp.org.bootstrap,
        })
    }

    /// Re-checks stored enrolment on load, so a device notices its own expiry
    /// or revocation rather than failing mysteriously at handshake time.
    pub fn validate(&self, identity: &DeviceIdentity) -> Result<()> {
        self.cert.verify(&self.org_id, Some(&identity.peer_id()), Some(&self.crl), now_ms())
    }

    /// Adopts a newer revocation list, ignoring stale or unsigned ones.
    pub fn merge_crl(&mut self, incoming: &RevocationList) -> bool {
        if incoming.updated_at_ms <= self.crl.updated_at_ms {
            return false;
        }
        if incoming.verify(&self.org_id).is_err() {
            return false;
        }
        self.crl = incoming.clone();
        true
    }

    pub fn save(&self, db: &crate::db::Db) -> Result<()> {
        db.set_meta(META_KEY, &serde_json::to_string(self)?)
    }

    pub fn load(db: &crate::db::Db) -> Result<Option<Self>> {
        match db.get_meta(META_KEY)? {
            Some(raw) => Ok(Some(serde_json::from_str(&raw)?)),
            None => Ok(None),
        }
    }
}
