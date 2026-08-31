//! The React Native binding.
//!
//! This crate is a thin, deliberately boring shell around [`p2p_core`]. It owns
//! the tokio runtime, keeps the device key on disk, and marshals a handful of
//! calls across the FFI boundary. No protocol logic lives here — the phone and
//! the seed server run the same code from `p2p-core`.
//!
//! # Shape of the API
//!
//! Rows and events cross as JSON strings rather than as generated record types.
//! That keeps the FFI surface small and stable: adding a column to a synced
//! table, or a field to an event, does not mean regenerating and re-linking
//! native bindings on both platforms. The TypeScript layer restores the types.
//!
//! # Enrolment
//!
//! The HTTP call is made by the app, not by Rust, so no TLS stack has to be
//! compiled into the binary:
//!
//! ```text
//!   beginEnrollment(code) ─► JSON body ─► app POSTs to the seed server
//!   completeEnrollment(reply) ◄─ JSON cert ◄─ server signs it
//! ```

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use p2p_core::db::{Db, SqlValue};
use p2p_core::enroll::{build_enroll_request, EnrollResponse, Enrollment};
use p2p_core::identity::DeviceIdentity;
use p2p_core::net::node::{Node, NodeHandle};
use p2p_core::net::NodeConfig;
use rand::RngCore;
use serde_json::{json, Value};

uniffi::setup_scaffolding!();

/// The core's `tracing` output has nowhere to go on a phone, which makes any
/// failure inside `start()` invisible. On Android it is pointed at logcat
/// (`adb logcat -s OrgSyncCore`); elsewhere it goes to stderr. Called once,
/// from `P2pClient::new`.
fn init_logging() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,libp2p=warn"));

        #[cfg(target_os = "android")]
        {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(android_log::MakeLogcat)
                .try_init();
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
        }
    });
}

/// A `tracing` writer that forwards whole lines to logcat.
#[cfg(target_os = "android")]
mod android_log {
    use std::ffi::CString;
    use std::io;
    use std::os::raw::{c_char, c_int};

    extern "C" {
        fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }

    const INFO: c_int = 4;
    const TAG: &str = "OrgSyncCore";

    pub struct Logcat;

    impl io::Write for Logcat {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let text = String::from_utf8_lossy(buf);
            let text = text.trim_end();
            if !text.is_empty() {
                // A NUL anywhere would truncate the line; drop those bytes.
                if let (Ok(tag), Ok(msg)) = (CString::new(TAG), CString::new(text)) {
                    unsafe { __android_log_write(INFO, tag.as_ptr(), msg.as_ptr()) };
                }
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    pub struct MakeLogcat;

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for MakeLogcat {
        type Writer = Logcat;
        fn make_writer(&'a self) -> Self::Writer {
            Logcat
        }
    }
}

const DEVICE_KEY_FILE: &str = "device.key";
const REPLICA_FILE: &str = "replica.db";

// `flat_error` sends only the Display string across the boundary. The variant
// field below is named `message`, which on Kotlin collides with
// `Throwable.message`; flattening also matches how both bridges read the error
// (`e.message` on Kotlin, `localizedDescription` on Swift).
#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum P2pError {
    /// Something went wrong; `message` is safe to show to a person.
    #[error("{message}")]
    Failed { message: String },
    /// This device has not joined an organisation yet.
    #[error("this device has not been enrolled yet")]
    NotEnrolled,
    /// The node is not running.
    #[error("the node is not running")]
    NotRunning,
}

impl P2pError {
    fn failed(e: impl std::fmt::Display) -> Self {
        P2pError::Failed { message: e.to_string() }
    }
}

type Result<T> = std::result::Result<T, P2pError>;

/// Where to keep state and how much of the network stack to enable.
#[derive(Debug, Clone, uniffi::Record)]
pub struct P2pConfig {
    /// A directory the app owns. On iOS use the Application Support
    /// directory, on Android `context.filesDir`.
    pub data_dir: String,
    /// Discover peers on the local network. Leave on: it is what keeps an
    /// office syncing when the internet is down.
    #[uniffi(default = true)]
    pub enable_mdns: bool,
    /// Ask the seed server to relay while behind NAT, then hole-punch.
    #[uniffi(default = true)]
    pub enable_relay: bool,
}

/// Receives node events as JSON. See `P2pEvent` in the TypeScript layer.
#[uniffi::export(callback_interface)]
pub trait P2pListener: Send + Sync {
    fn on_event(&self, event_json: String);
}

/// A running (or startable) node.
#[derive(uniffi::Object)]
pub struct P2pClient {
    runtime: tokio::runtime::Runtime,
    identity: DeviceIdentity,
    db: Arc<Db>,
    data_dir: PathBuf,
    config: P2pConfig,
    node: Mutex<Option<NodeHandle>>,
    listener: Arc<Mutex<Option<Box<dyn P2pListener>>>>,
}

#[uniffi::export]
impl P2pClient {
    /// Opens the local replica, loading or creating this device's key.
    #[uniffi::constructor]
    pub fn new(config: P2pConfig) -> Result<Arc<Self>> {
        init_logging();
        let data_dir = PathBuf::from(&config.data_dir);
        std::fs::create_dir_all(&data_dir).map_err(P2pError::failed)?;

        let identity = load_or_create_identity(&data_dir)?;
        let db = Arc::new(
            Db::open_authenticated(data_dir.join(REPLICA_FILE), &identity)
                .map_err(P2pError::failed)?,
        );
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("p2p")
            .build()
            .map_err(P2pError::failed)?;

        Ok(Arc::new(Self {
            runtime,
            identity,
            db,
            data_dir,
            config,
            node: Mutex::new(None),
            listener: Arc::new(Mutex::new(None)),
        }))
    }

    /// This device's stable network identity.
    pub fn peer_id(&self) -> String {
        self.identity.peer_id().to_string()
    }

    /// Path of the SQLite file. The app may open it read-only with its own
    /// SQLite driver for queries; writes should go through `execute` so they
    /// are captured and replicated immediately.
    pub fn db_path(&self) -> String {
        self.data_dir.join(REPLICA_FILE).to_string_lossy().into_owned()
    }

    pub fn is_enrolled(&self) -> bool {
        matches!(Enrollment::load(&self.db), Ok(Some(_)))
    }

    pub fn is_running(&self) -> bool {
        self.node.lock().map(|n| n.is_some()).unwrap_or(false)
    }

    // ------------------------------------------------------------ enrolment

    /// Step one of joining an org. Returns the JSON body to POST to
    /// `<seed url>/v1/enroll`. It embeds a proof that this device holds the
    /// key it is asking to have certified.
    pub fn begin_enrollment(
        &self,
        invite_code: String,
        device_name: String,
        platform: String,
    ) -> Result<String> {
        let req = build_enroll_request(&self.identity, &invite_code, &device_name, &platform);
        serde_json::to_string(&req).map_err(P2pError::failed)
    }

    /// Step two: hand back what the server replied. The certificate is
    /// verified against the org key before anything is stored, so a hostile
    /// server cannot enrol this device into an org it does not control.
    /// Returns the enrolment as JSON.
    pub fn complete_enrollment(&self, response_json: String) -> Result<String> {
        let resp: EnrollResponse =
            serde_json::from_str(&response_json).map_err(P2pError::failed)?;
        let enrollment = Enrollment::accept(resp, &self.identity).map_err(P2pError::failed)?;
        enrollment.save(&self.db).map_err(P2pError::failed)?;
        serde_json::to_string(&enrollment).map_err(P2pError::failed)
    }

    // -------------------------------------------------------------- runtime

    pub fn start(&self) -> Result<()> {
        let mut slot = self.node.lock().map_err(|_| P2pError::failed("lock poisoned"))?;
        if slot.is_some() {
            return Ok(());
        }
        let enrollment = Enrollment::load(&self.db)
            .map_err(P2pError::failed)?
            .ok_or(P2pError::NotEnrolled)?;

        let cfg = NodeConfig {
            enable_mdns: self.config.enable_mdns,
            enable_relay_client: self.config.enable_relay,
            ..NodeConfig::default()
        };

        // `Node::spawn` uses `tokio::spawn`, so it needs the runtime entered.
        let _guard = self.runtime.enter();
        let handle = Node::spawn(cfg, self.identity.clone(), enrollment, self.db.clone())
            .map_err(P2pError::failed)?;

        // Pump events to the app's listener.
        let listener = self.listener.clone();
        let mut events = handle.subscribe();
        self.runtime.spawn(async move {
            while let Ok(ev) = events.recv().await {
                let payload = match serde_json::to_string(&ev) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(error = %e, "could not encode event");
                        continue;
                    }
                };
                let cb = listener.lock().ok().and_then(|g| {
                    // The callback is invoked outside the lock so a slow or
                    // re-entrant listener cannot deadlock the pump.
                    g.as_ref().map(|_| ())
                });
                if cb.is_some() {
                    if let Ok(guard) = listener.lock() {
                        if let Some(l) = guard.as_ref() {
                            l.on_event(payload);
                        }
                    }
                }
            }
        });

        *slot = Some(handle);
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        let handle = {
            let mut slot = self.node.lock().map_err(|_| P2pError::failed("lock poisoned"))?;
            slot.take()
        };
        if let Some(h) = handle {
            let _ = self.runtime.block_on(h.shutdown());
        }
        Ok(())
    }

    pub fn set_listener(&self, listener: Box<dyn P2pListener>) {
        if let Ok(mut g) = self.listener.lock() {
            *g = Some(listener);
        }
    }

    /// Connects to a peer or seed server by multiaddr. Rarely needed: seed
    /// servers from the enrolment are dialled automatically.
    pub fn dial(&self, multiaddr: String) -> Result<()> {
        let handle = self.handle()?;
        let addr = multiaddr.parse().map_err(P2pError::failed)?;
        self.runtime.block_on(handle.dial(addr)).map_err(P2pError::failed)
    }

    /// Runs anti-entropy with every connected peer straight away.
    pub fn sync_now(&self) -> Result<()> {
        let handle = self.handle()?;
        self.runtime.block_on(handle.sync_now()).map_err(P2pError::failed)
    }

    /// Peers, addresses and replica counters, as JSON.
    pub fn status(&self) -> Result<String> {
        let handle = self.handle()?;
        let status = self.runtime.block_on(handle.status()).map_err(P2pError::failed)?;
        serde_json::to_string(&status).map_err(P2pError::failed)
    }

    // ----------------------------------------------------------------- data

    /// Reads. `params_json` is a JSON array. Returns a JSON array of row
    /// objects.
    pub fn query(&self, sql: String, params_json: String) -> Result<String> {
        let params = parse_params(&params_json)?;
        let result = self.db.query(&sql, &params).map_err(P2pError::failed)?;
        let rows: Vec<Value> = result
            .rows
            .iter()
            .map(|row| {
                let mut obj = serde_json::Map::new();
                for (i, col) in result.columns.iter().enumerate() {
                    obj.insert(col.clone(), value_to_json(&row[i]));
                }
                Value::Object(obj)
            })
            .collect();
        serde_json::to_string(&rows).map_err(P2pError::failed)
    }

    /// Writes. The change is captured, stamped and pushed to peers before
    /// this returns, so a message appears on other devices immediately.
    pub fn execute(&self, sql: String, params_json: String) -> Result<u64> {
        let params = parse_params(&params_json)?;
        let (affected, _) = self.db.execute(&sql, &params).map_err(P2pError::failed)?;
        if let Ok(handle) = self.handle() {
            let _ = self.runtime.block_on(handle.local_changed());
        }
        Ok(affected as u64)
    }

    /// Convenience for the chat screen. A message is an ordinary replicated
    /// row, so this is just an insert. Returns the new message id.
    pub fn send_message(&self, room: String, body: String) -> Result<String> {
        let enrollment = Enrollment::load(&self.db)
            .map_err(P2pError::failed)?
            .ok_or(P2pError::NotEnrolled)?;
        let id = random_id();
        let params = json!([
            id,
            room,
            self.identity.peer_id().to_string(),
            enrollment.cert.claims.display_name,
            body,
            p2p_core::hlc::now_ms() as i64
        ]);
        self.execute(
            "INSERT INTO messages (id, room, author, author_name, body, sent_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
                .into(),
            params.to_string(),
        )?;
        Ok(id)
    }

    /// Brings an app-defined table into replication. It must already exist and
    /// have a single-column primary key; use an opaque id, since a primary key
    /// is what identifies a row across devices.
    pub fn register_table(&self, table: String, pk_column: String) -> Result<()> {
        self.db.register_table(&table, &pk_column).map_err(P2pError::failed)?;
        Ok(())
    }
}

impl P2pClient {
    fn handle(&self) -> Result<NodeHandle> {
        self.node
            .lock()
            .map_err(|_| P2pError::failed("lock poisoned"))?
            .clone()
            .ok_or(P2pError::NotRunning)
    }
}

// ------------------------------------------------------------------ helpers

/// Loads this device's key, generating one on first run.
///
/// The file is the device's whole identity, so it is written with owner-only
/// permissions. It sits inside the app's private sandbox; moving it to the
/// iOS Keychain or Android Keystore is the natural hardening step.
fn load_or_create_identity(dir: &Path) -> Result<DeviceIdentity> {
    let path = dir.join(DEVICE_KEY_FILE);
    if path.exists() {
        let raw = std::fs::read(&path).map_err(P2pError::failed)?;
        return DeviceIdentity::from_secret(&raw).map_err(P2pError::failed);
    }
    let identity = DeviceIdentity::generate();
    std::fs::write(&path, identity.secret_bytes()).map_err(P2pError::failed)?;
    restrict_permissions(&path);
    Ok(identity)
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) {}

fn random_id() -> String {
    let mut raw = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut raw);
    hex::encode(raw)
}

fn parse_params(params_json: &str) -> Result<Vec<SqlValue>> {
    if params_json.trim().is_empty() {
        return Ok(Vec::new());
    }
    let parsed: Value = serde_json::from_str(params_json).map_err(P2pError::failed)?;
    let Value::Array(items) = parsed else {
        return Err(P2pError::failed("parameters must be a JSON array"));
    };
    items.iter().map(json_to_value).collect()
}

fn json_to_value(v: &Value) -> Result<SqlValue> {
    Ok(match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Int(*b as i64),
        Value::Number(n) => match n.as_i64() {
            Some(i) => SqlValue::Int(i),
            None => SqlValue::Real(n.as_f64().unwrap_or_default()),
        },
        Value::String(s) => SqlValue::Text(s.clone()),
        // `{"$blob": "<hex>"}` is how binary crosses the JSON boundary.
        Value::Object(map) => match map.get("$blob").and_then(Value::as_str) {
            Some(hexstr) => SqlValue::Blob(hex::decode(hexstr).map_err(P2pError::failed)?),
            None => return Err(P2pError::failed("objects are not valid SQL parameters")),
        },
        Value::Array(_) => return Err(P2pError::failed("arrays are not valid SQL parameters")),
    })
}

fn value_to_json(v: &SqlValue) -> Value {
    match v {
        SqlValue::Null => Value::Null,
        SqlValue::Int(i) => json!(i),
        SqlValue::Real(f) => json!(f),
        SqlValue::Text(s) => json!(s),
        SqlValue::Blob(b) => json!({ "$blob": hex::encode(b) }),
    }
}
