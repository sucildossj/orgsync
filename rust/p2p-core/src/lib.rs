//! Org-authenticated peer-to-peer SQLite replication.
//!
//! The same crate powers the seed server and every phone; they run identical
//! protocol code and differ only in configuration.

pub mod db;
pub mod enroll;
pub mod error;
pub mod hlc;
pub mod identity;
pub mod net;

pub use db::{ChangeRecord, Db, SqlValue, VersionVector};
pub use enroll::Enrollment;
pub use error::{Error, Result};
pub use identity::{DeviceCert, DeviceIdentity, OrgKeypair, RevocationList, Role};
pub use net::{NodeConfig, NodeEvent, NodeHandle, NodeStatus};
pub use net::node::Node;
