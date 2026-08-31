//! The application protocol two org devices speak over libp2p.
//!
//! Everything rides one request/response protocol, CBOR framed. libp2p's Noise
//! handshake has already authenticated the peer's key by the time any of this
//! is exchanged; [`Req::Hello`] is what proves that key belongs to the org.

use serde::{Deserialize, Serialize};

use crate::db::{ChangeRecord, VersionVector};
use crate::identity::{DeviceCert, RevocationList};

pub const PROTOCOL_VERSION: u32 = 1;
pub const SYNC_PROTOCOL: &str = "/orgsync/1.0.0";
pub const KAD_PROTOCOL: &str = "/orgsync/kad/1.0.0";
pub const IDENTIFY_PROTOCOL: &str = "/orgsync/id/1.0.0";

/// CBOR request frames cap at 1 MiB, so pushes stay well under it.
pub const MAX_PUSH_CHANGES: usize = 200;
/// Responses may be 10 MiB, so a catch-up page can be larger.
pub const MAX_SYNC_CHANGES: usize = 500;

pub fn changes_topic(org_id: &str) -> String {
    format!("orgsync/{org_id}/changes")
}

// `Hello` is much larger than the other variants because it carries a
// certificate and a revocation list. Boxing it would save stack on every
// frame, but these are constructed once per connection and immediately
// serialised, so the simpler shape is worth more than the bytes.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Req {
    /// Presents our certificate and revocation list. Sent immediately on every
    /// new connection; nothing else is honoured until it succeeds.
    ///
    /// `replicates` says whether this node stores org data at all. A seed
    /// server running purely as rendezvous and relay says `false`, and peers
    /// then know not to waste round trips asking it to sync.
    Hello { proto: u32, cert: DeviceCert, crl: RevocationList, replicates: bool },
    /// Anti-entropy: "here is everything I already know, send me the rest".
    Sync { vv: VersionVector, limit: u32 },
    /// Live push of freshly authored changes.
    Push { changes: Vec<ChangeRecord> },
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Resp {
    Hello { proto: u32, cert: DeviceCert, crl: RevocationList, replicates: bool },
    Sync { changes: Vec<ChangeRecord>, has_more: bool },
    Push { applied: u32, rejected: u32 },
    /// The peer refused us outright. The connection is dropped straight after.
    ///
    /// Reserved for genuine rejection — a bad certificate, a wrong org, a
    /// revoked device. It must never mean "I cannot serve this particular
    /// request", or a node that simply stores no data would look like a
    /// hostile one and get disconnected and redialled forever.
    Denied { reason: String },
}

/// Broadcast payload on the gossipsub topic: the same records, multi-hop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeBroadcast {
    pub changes: Vec<ChangeRecord>,
}
