//! How a node listens, who it dials, and how often it reconciles.

use std::time::Duration;

use libp2p::Multiaddr;

#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// Addresses to listen on. Port 0 means "pick one", which is what a phone
    /// wants; a seed server pins real ports instead.
    pub listen: Vec<Multiaddr>,
    /// Seed servers, as multiaddrs ending in `/p2p/<peer id>`.
    pub bootstrap: Vec<Multiaddr>,
    /// Find peers on the local network with no server involved at all. This is
    /// what makes an office LAN keep working when the internet is down.
    pub enable_mdns: bool,
    /// Ask a seed server to relay for us when we are behind a NAT, then try to
    /// upgrade to a direct connection with DCUtR hole punching.
    pub enable_relay_client: bool,
    /// Act as a relay for other org devices. Seed servers do; phones do not.
    pub enable_relay_server: bool,
    /// Serve DHT queries rather than only issuing them.
    pub kad_server: bool,
    /// Whether this node stores and serves org data at all.
    ///
    /// A seed server can be pure infrastructure — discovery, relay and
    /// rendezvous — without ever holding a copy of the organisation's rows.
    /// Turn it on to make the server an always-on replica instead, so devices
    /// that are never online at the same time still converge.
    pub replicate: bool,
    /// How often to run anti-entropy with each authenticated peer.
    pub sync_interval: Duration,
    /// How often to sweep for local edits made outside `Db::execute`, e.g. by
    /// the app writing the same file through its own SQLite handle.
    pub flush_interval: Duration,
    pub idle_connection_timeout: Duration,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            listen: vec![
                "/ip4/0.0.0.0/udp/0/quic-v1".parse().expect("valid multiaddr"),
                "/ip4/0.0.0.0/tcp/0".parse().expect("valid multiaddr"),
            ],
            bootstrap: Vec::new(),
            enable_mdns: true,
            enable_relay_client: true,
            enable_relay_server: false,
            kad_server: false,
            replicate: true,
            sync_interval: Duration::from_secs(20),
            flush_interval: Duration::from_millis(750),
            idle_connection_timeout: Duration::from_secs(60),
        }
    }
}

impl NodeConfig {
    /// Preset for a seed server: fixed ports, relays for others, serves the
    /// DHT, and does not bother with mDNS.
    pub fn seed_server(quic_port: u16, tcp_port: u16) -> Self {
        Self {
            listen: vec![
                format!("/ip4/0.0.0.0/udp/{quic_port}/quic-v1").parse().expect("valid multiaddr"),
                format!("/ip4/0.0.0.0/tcp/{tcp_port}").parse().expect("valid multiaddr"),
            ],
            enable_mdns: false,
            enable_relay_client: false,
            enable_relay_server: true,
            kad_server: true,
            replicate: false,
            idle_connection_timeout: std::time::Duration::from_secs(600),
            ..Default::default()
        }
    }

    pub fn with_bootstrap(mut self, addrs: impl IntoIterator<Item = Multiaddr>) -> Self {
        self.bootstrap = addrs.into_iter().collect();
        self
    }
}
