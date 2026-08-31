//! The unit of replication and the summary a peer sends to ask for changes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::value::SqlValue;
use crate::error::{Error, Result};
use crate::hlc::Hlc;
use crate::identity::{peer_public_key_from_str, DeviceIdentity};

const CHANGE_DOMAIN: &[u8] = b"p2p-change-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    /// One column of one row was set to a value.
    Cell,
    /// A whole row was deleted.
    Delete,
}

impl ChangeKind {
    pub fn as_i64(self) -> i64 {
        match self {
            ChangeKind::Cell => 0,
            ChangeKind::Delete => 1,
        }
    }
    pub fn from_i64(v: i64) -> Self {
        if v == 1 { ChangeKind::Delete } else { ChangeKind::Cell }
    }
}

/// A single replicated fact.
///
/// `(origin, hlc)` is globally unique: a device stamps every change it authors
/// with a fresh tick of its own clock. That is what makes the version-vector
/// protocol exact — "everything from device X above stamp S" is a precise,
/// index-backed range query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeRecord {
    pub tbl: String,
    pub pk: String,
    /// Empty for [`ChangeKind::Delete`].
    pub col: String,
    pub value: SqlValue,
    pub hlc: Hlc,
    /// PeerId of the device that authored this change.
    pub origin: String,
    pub kind: ChangeKind,
    /// `origin`'s Ed25519 signature over [`ChangeRecord::signing_bytes`].
    /// Empty only in unauthenticated (local tooling / test) replicas.
    #[serde(default, with = "serde_bytes_vec")]
    pub sig: Vec<u8>,
}

impl ChangeRecord {
    /// Total order across the org. The origin breaks ties, because two devices
    /// can independently produce the same wall/counter pair.
    pub fn order_key(&self) -> (String, &str) {
        (self.hlc.to_hex(), self.origin.as_str())
    }

    /// Deterministic pre-image for the author's signature.
    ///
    /// Built by hand with explicit length prefixes so the bytes a signature
    /// covers can never shift because a serialiser changed.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let (vtype, raw) = self.value.to_storage();
        let raw = raw.unwrap_or_default();
        let mut b = Vec::with_capacity(96 + raw.len());
        b.extend_from_slice(CHANGE_DOMAIN);
        put_str(&mut b, &self.tbl);
        put_str(&mut b, &self.pk);
        put_str(&mut b, &self.col);
        b.push(self.kind.as_i64() as u8);
        b.extend_from_slice(&vtype.to_le_bytes());
        b.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        b.extend_from_slice(&raw);
        b.extend_from_slice(&self.hlc.wall_ms.to_le_bytes());
        b.extend_from_slice(&self.hlc.counter.to_le_bytes());
        put_str(&mut b, &self.origin);
        b
    }

    pub fn sign_with(&mut self, id: &DeviceIdentity) -> Result<()> {
        let bytes = self.signing_bytes();
        self.sig = id.libp2p_keypair().sign(&bytes)?;
        Ok(())
    }

    /// Checks the record really was written by the device it names.
    pub fn verify_author(&self) -> Result<()> {
        if self.sig.is_empty() {
            return Err(Error::Rejected(format!("change from `{}` is unsigned", self.origin)));
        }
        let key = peer_public_key_from_str(&self.origin)?;
        if key.verify(&self.signing_bytes(), &self.sig) {
            Ok(())
        } else {
            Err(Error::Rejected(format!(
                "change claiming to come from `{}` is not signed by that device",
                self.origin
            )))
        }
    }
}

fn put_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}

/// Symmetric byte-vector codec.
///
/// `serialize_bytes` emits a CBOR byte string, but a plain `Vec<u8>` reader
/// expects a CBOR array — the two do not meet, and the mismatch shows up only
/// once records cross the wire. Accepting either shape keeps the compact
/// encoding on both postcard (gossip) and CBOR (request/response).
mod serde_bytes_vec {
    use std::fmt;

    use serde::de::{SeqAccess, Visitor};
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(v)
    }

    struct BytesVisitor;

    impl<'de> Visitor<'de> for BytesVisitor {
        type Value = Vec<u8>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a byte string or a sequence of bytes")
        }

        fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
            Ok(v.to_vec())
        }

        fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
            Ok(v)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(64));
            while let Some(b) = seq.next_element::<u8>()? {
                out.push(b);
            }
            Ok(out)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        d.deserialize_bytes(BytesVisitor)
    }
}

/// Highest stamp held from each device, i.e. "what I already know".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionVector(pub BTreeMap<String, Hlc>);

impl VersionVector {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    pub fn get(&self, origin: &str) -> Hlc {
        self.0.get(origin).copied().unwrap_or(Hlc::ZERO)
    }

    pub fn observe(&mut self, origin: &str, hlc: Hlc) {
        let e = self.0.entry(origin.to_string()).or_insert(Hlc::ZERO);
        if hlc > *e {
            *e = hlc;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// Decides last-writer-wins between an incoming change and what we already
/// hold for the same cell.
pub fn incoming_wins(new_hlc: &str, new_origin: &str, old_hlc: &str, old_origin: &str) -> bool {
    (new_hlc, new_origin) > (old_hlc, old_origin)
}
