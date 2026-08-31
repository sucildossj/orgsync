//! Durable state for the seed server: the org root key, invites and devices.
//!
//! This is the only place in the system that holds the org private key. Every
//! other participant only ever sees signatures made with it.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{anyhow, bail, Context, Result};
use p2p_core::enroll::normalize_invite_code;
use p2p_core::hlc::now_ms;
use p2p_core::identity::{
    verify_enrollment_proof, DeviceCert, DeviceIdentity, OrgKeypair, RevocationList, Role,
};
use rand::RngCore;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// How far an enrolment request's timestamp may be from ours.
const ENROLL_SKEW_MS: u64 = 5 * 60 * 1000;
/// Human-typable alphabet: no I, L, O, U, 0 or 1.
const TOKEN_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTVWXYZ";

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;

CREATE TABLE IF NOT EXISTS org (
  id            INTEGER PRIMARY KEY CHECK (id = 1),
  org_id        TEXT NOT NULL,
  org_key       BLOB NOT NULL,
  node_key      BLOB NOT NULL,
  name          TEXT NOT NULL,
  admin_token   TEXT NOT NULL,
  next_serial   INTEGER NOT NULL DEFAULT 1,
  crl_updated_at_ms INTEGER NOT NULL DEFAULT 0,
  created_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS invite (
  token         TEXT PRIMARY KEY,
  user_id       TEXT NOT NULL,
  display_name  TEXT NOT NULL,
  role          TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL,
  expires_at_ms INTEGER NOT NULL,
  used_at_ms    INTEGER,
  used_by       TEXT
);

CREATE TABLE IF NOT EXISTS device (
  serial         INTEGER PRIMARY KEY,
  peer_id        TEXT NOT NULL UNIQUE,
  device_pub     TEXT NOT NULL,
  user_id        TEXT NOT NULL,
  display_name   TEXT NOT NULL,
  role           TEXT NOT NULL,
  platform       TEXT NOT NULL,
  enrolled_at_ms INTEGER NOT NULL,
  expires_at_ms  INTEGER NOT NULL,
  revoked_at_ms  INTEGER
);
"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRow {
    pub serial: u64,
    pub peer_id: String,
    pub user_id: String,
    pub display_name: String,
    pub role: String,
    pub platform: String,
    pub enrolled_at_ms: u64,
    pub expires_at_ms: u64,
    pub revoked_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviteRow {
    pub token: String,
    pub user_id: String,
    pub display_name: String,
    pub role: String,
    pub expires_at_ms: u64,
    pub used_at_ms: Option<u64>,
    pub used_by: Option<String>,
}

impl InviteRow {
    /// Grouped into blocks of five, which is far easier to read aloud or type
    /// on a phone than one twenty-character run.
    pub fn invite_code_display(&self) -> String {
        self.token
            .as_bytes()
            .chunks(5)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect::<Vec<_>>()
            .join("-")
    }
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path).context("opening seed database")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_initialised(&self) -> Result<bool> {
        let n: i64 = self.lock().query_row("SELECT COUNT(*) FROM org", [], |r| r.get(0))?;
        Ok(n > 0)
    }

    /// Generates the org root key, this server's own peer identity, and an
    /// admin token. Refuses to run twice: regenerating the root key would
    /// invalidate every certificate already issued.
    pub fn init(&self, org_name: &str) -> Result<(String, String)> {
        if self.is_initialised()? {
            bail!("this data directory already holds an organisation; refusing to overwrite its root key");
        }
        let org = OrgKeypair::generate();
        let node = DeviceIdentity::generate();
        let admin_token = random_token(32);

        self.lock().execute(
            "INSERT INTO org (id, org_id, org_key, node_key, name, admin_token, next_serial, created_at_ms)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, 1, ?6)",
            params![
                org.org_id(),
                org.to_bytes().to_vec(),
                node.secret_bytes().to_vec(),
                org_name,
                &admin_token,
                now_ms() as i64
            ],
        )?;
        Ok((org.org_id(), admin_token))
    }

    pub fn org_keypair(&self) -> Result<OrgKeypair> {
        let raw: Vec<u8> = self
            .lock()
            .query_row("SELECT org_key FROM org WHERE id = 1", [], |r| r.get(0))
            .optional()?
            .ok_or_else(|| anyhow!("not initialised: run `seed-server init` first"))?;
        OrgKeypair::from_bytes(&raw).map_err(Into::into)
    }

    /// The server's own libp2p identity, stable across restarts so the
    /// bootstrap multiaddrs handed to devices never go stale.
    pub fn node_identity(&self) -> Result<DeviceIdentity> {
        let raw: Vec<u8> = self
            .lock()
            .query_row("SELECT node_key FROM org WHERE id = 1", [], |r| r.get(0))
            .optional()?
            .ok_or_else(|| anyhow!("not initialised: run `seed-server init` first"))?;
        DeviceIdentity::from_secret(&raw).map_err(Into::into)
    }

    pub fn org_name(&self) -> Result<String> {
        Ok(self.lock().query_row("SELECT name FROM org WHERE id = 1", [], |r| r.get(0))?)
    }

    pub fn admin_token(&self) -> Result<String> {
        Ok(self.lock().query_row("SELECT admin_token FROM org WHERE id = 1", [], |r| r.get(0))?)
    }

    pub fn create_invite(
        &self,
        user_id: &str,
        display_name: &str,
        role: Role,
        ttl_ms: u64,
    ) -> Result<InviteRow> {
        let token = random_token(20);
        let now = now_ms();
        let expires_at_ms = now.saturating_add(ttl_ms);
        self.lock().execute(
            "INSERT INTO invite (token, user_id, display_name, role, created_at_ms, expires_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![&token, user_id, display_name, role.as_str(), now as i64, expires_at_ms as i64],
        )?;
        Ok(InviteRow {
            token,
            user_id: user_id.into(),
            display_name: display_name.into(),
            role: role.as_str().into(),
            expires_at_ms,
            used_at_ms: None,
            used_by: None,
        })
    }

    /// Redeems an invite and mints the device certificate.
    ///
    /// Every check that matters happens here: the invite must exist, be
    /// unused and unexpired; the request must be recent; and the caller must
    /// prove possession of the device key it is asking us to certify — so a
    /// leaked invite token cannot be redeemed against somebody else's key.
    #[allow(clippy::too_many_arguments)]
    pub fn redeem_invite(
        &self,
        token: &str,
        device_pub_hex: &str,
        device_name: &str,
        platform: &str,
        at_ms: u64,
        proof_hex: &str,
        cert_ttl_ms: u64,
    ) -> Result<DeviceCert> {
        let token = &normalize_invite_code(token);
        let now = now_ms();
        if at_ms.abs_diff(now) > ENROLL_SKEW_MS {
            bail!("enrolment request timestamp is too far from server time");
        }

        let device_pub: [u8; 32] = hex::decode(device_pub_hex)
            .context("device_pub is not hex")?
            .try_into()
            .map_err(|_| anyhow!("device_pub must be 32 bytes"))?;
        let proof = hex::decode(proof_hex).context("proof is not hex")?;
        verify_enrollment_proof(&device_pub, token, device_name, at_ms, &proof)
            .context("enrolment proof rejected")?;

        let mut conn = self.lock();
        let tx = conn.transaction()?;

        let invite: Option<(String, String, String, i64, Option<i64>)> = tx
            .query_row(
                "SELECT user_id, display_name, role, expires_at_ms, used_at_ms
                 FROM invite WHERE token = ?1",
                params![token],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let Some((user_id, display_name, role, expires_at_ms, used_at_ms)) = invite else {
            bail!("unknown invite code");
        };
        if used_at_ms.is_some() {
            bail!("this invite code has already been used");
        }
        if (expires_at_ms as u64) < now {
            bail!("this invite code has expired");
        }

        let serial: i64 = tx.query_row("SELECT next_serial FROM org WHERE id = 1", [], |r| r.get(0))?;
        tx.execute("UPDATE org SET next_serial = next_serial + 1 WHERE id = 1", [])?;

        let org = {
            let raw: Vec<u8> = tx.query_row("SELECT org_key FROM org WHERE id = 1", [], |r| r.get(0))?;
            OrgKeypair::from_bytes(&raw)?
        };
        let role_enum = Role::from_str_lossy(&role);
        let name = if device_name.is_empty() { display_name.clone() } else { device_name.to_string() };
        let cert = org.issue_cert(
            device_pub,
            &user_id,
            &name,
            role_enum,
            serial as u64,
            cert_ttl_ms,
        );
        let peer_id = cert.peer_id()?.to_string();

        tx.execute(
            "UPDATE invite SET used_at_ms = ?2, used_by = ?3 WHERE token = ?1",
            params![token, now as i64, &peer_id],
        )?;
        tx.execute(
            "INSERT INTO device
               (serial, peer_id, device_pub, user_id, display_name, role, platform,
                enrolled_at_ms, expires_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(peer_id) DO UPDATE SET
               serial = excluded.serial, display_name = excluded.display_name,
               role = excluded.role, platform = excluded.platform,
               enrolled_at_ms = excluded.enrolled_at_ms,
               expires_at_ms = excluded.expires_at_ms, revoked_at_ms = NULL",
            params![
                serial,
                &peer_id,
                device_pub_hex,
                &user_id,
                &name,
                &role,
                platform,
                now as i64,
                cert.claims.expires_at_ms as i64
            ],
        )?;
        tx.commit()?;
        Ok(cert)
    }

    pub fn list_devices(&self) -> Result<Vec<DeviceRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT serial, peer_id, user_id, display_name, role, platform,
                    enrolled_at_ms, expires_at_ms, revoked_at_ms
             FROM device ORDER BY serial",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(DeviceRow {
                    serial: r.get::<_, i64>(0)? as u64,
                    peer_id: r.get(1)?,
                    user_id: r.get(2)?,
                    display_name: r.get(3)?,
                    role: r.get(4)?,
                    platform: r.get(5)?,
                    enrolled_at_ms: r.get::<_, i64>(6)? as u64,
                    expires_at_ms: r.get::<_, i64>(7)? as u64,
                    revoked_at_ms: r.get::<_, Option<i64>>(8)?.map(|v| v as u64),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn list_invites(&self) -> Result<Vec<InviteRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT token, user_id, display_name, role, expires_at_ms, used_at_ms, used_by
             FROM invite ORDER BY created_at_ms DESC LIMIT 200",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(InviteRow {
                    token: r.get(0)?,
                    user_id: r.get(1)?,
                    display_name: r.get(2)?,
                    role: r.get(3)?,
                    expires_at_ms: r.get::<_, i64>(4)? as u64,
                    used_at_ms: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
                    used_by: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn revoke(&self, serial: u64) -> Result<()> {
        let n = self.lock().execute(
            "UPDATE device SET revoked_at_ms = ?2 WHERE serial = ?1 AND revoked_at_ms IS NULL",
            params![serial as i64, now_ms() as i64],
        )?;
        if n == 0 {
            bail!("no active device with serial {serial}");
        }
        self.lock().execute(
            "UPDATE org SET crl_updated_at_ms = ?1 WHERE id = 1",
            params![now_ms() as i64],
        )?;
        Ok(())
    }

    /// Builds and signs the current revocation list.
    ///
    /// Devices gossip this to each other, so a revoked phone is locked out of
    /// the LAN even while this server is unreachable.
    pub fn crl(&self) -> Result<RevocationList> {
        let org = self.org_keypair()?;
        let (revoked, updated_at_ms) = {
            let conn = self.lock();
            let mut stmt =
                conn.prepare("SELECT serial FROM device WHERE revoked_at_ms IS NOT NULL ORDER BY serial")?;
            let serials = stmt
                .query_map([], |r| r.get::<_, i64>(0).map(|v| v as u64))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let updated: i64 =
                conn.query_row("SELECT crl_updated_at_ms FROM org WHERE id = 1", [], |r| r.get(0))?;
            (serials, updated as u64)
        };
        if revoked.is_empty() {
            return Ok(RevocationList::empty(org.org_id()));
        }
        Ok(org.sign_crl(revoked, updated_at_ms))
    }
}

fn random_token(len: usize) -> String {
    let mut raw = vec![0u8; len];
    rand::thread_rng().fill_bytes(&mut raw);
    raw.iter().map(|b| TOKEN_ALPHABET[*b as usize % TOKEN_ALPHABET.len()] as char).collect()
}
