//! The node runtime: one task owning the swarm, the replica and the peer table.
//!
//! Nothing outside this module touches the swarm. Callers get a [`NodeHandle`]
//! that sends commands in and reads [`NodeEvent`]s out, which keeps the whole
//! libp2p surface off the FFI boundary and off the app's threads.
//!
//! # Trust gate
//!
//! A TCP/QUIC connection means nothing on its own. On every new connection the
//! node sends [`Req::Hello`] and refuses to serve or accept data until the peer
//! has produced a certificate that (a) is signed by our pinned org key, (b)
//! names the very key libp2p's Noise handshake authenticated, and (c) is not
//! expired or revoked. Unauthenticated peers are disconnected.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use libp2p::{
    gossipsub, identify, kad, mdns, multiaddr::Protocol,
    request_response::{self, Message as RrMessage, ResponseChannel},
    swarm::SwarmEvent,
    Multiaddr, PeerId, Swarm,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};

use super::behaviour::{self, NodeBehaviour, NodeBehaviourEvent};
use super::config::NodeConfig;
use super::proto::{
    changes_topic, ChangeBroadcast, Req, Resp, MAX_PUSH_CHANGES, MAX_SYNC_CHANGES,
    PROTOCOL_VERSION,
};
use crate::db::{ChangeRecord, Db, DbStats};
use crate::enroll::Enrollment;
use crate::error::{Error, Result};
use crate::hlc::now_ms;
use crate::identity::{DeviceIdentity, Role};

#[derive(Debug, Clone, Serialize, Deserialize)]
// `rename_all_fields` matters as much as `rename_all` here: without it the
// variant tags would be camelCase but the payload keys would still be
// snake_case, which is a miserable shape to consume from TypeScript.
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum NodeEvent {
    Started { peer_id: String, org_id: String },
    Listening { addr: String },
    /// We have a publicly reachable address via a relay; other devices can now
    /// reach us from outside the LAN.
    RelayReserved { addr: String },
    PeerConnected { peer: String, user_id: String, display_name: String, role: String },
    PeerDisconnected { peer: String },
    PeerRejected { peer: String, reason: String },
    /// Remote changes landed and the local tables were updated.
    Synced { peer: String, applied: u32, tables: Vec<String> },
    /// This device authored changes and pushed them out.
    LocalChanges { count: u32 },
    Stopped,
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerSummary {
    pub peer_id: String,
    pub user_id: String,
    pub display_name: String,
    pub role: String,
    pub since_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeStatus {
    pub peer_id: String,
    pub org_id: String,
    pub org_name: String,
    pub display_name: String,
    pub listen_addrs: Vec<String>,
    pub external_addrs: Vec<String>,
    pub connections: u32,
    pub peers: Vec<PeerSummary>,
    pub changes: u64,
    pub known_devices: u64,
    pub cert_expires_at_ms: u64,
}

#[derive(Debug)]
pub enum Command {
    /// The app wrote to the database; pick the edits up and share them now.
    LocalChanged,
    SyncNow,
    Dial(Multiaddr),
    Status(oneshot::Sender<NodeStatus>),
    Shutdown,
}

#[derive(Clone)]
pub struct NodeHandle {
    peer_id: PeerId,
    cmd: mpsc::Sender<Command>,
    events: broadcast::Sender<NodeEvent>,
}

impl NodeHandle {
    pub fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    pub fn subscribe(&self) -> broadcast::Receiver<NodeEvent> {
        self.events.subscribe()
    }

    /// Tells the node to flush local edits and push them. Safe to call after
    /// every write; the flush is a no-op when nothing actually changed.
    pub async fn local_changed(&self) -> Result<()> {
        self.send(Command::LocalChanged).await
    }

    pub async fn sync_now(&self) -> Result<()> {
        self.send(Command::SyncNow).await
    }

    pub async fn dial(&self, addr: Multiaddr) -> Result<()> {
        self.send(Command::Dial(addr)).await
    }

    pub async fn status(&self) -> Result<NodeStatus> {
        let (tx, rx) = oneshot::channel();
        self.send(Command::Status(tx)).await?;
        rx.await.map_err(|_| Error::NotRunning)
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.send(Command::Shutdown).await
    }

    async fn send(&self, c: Command) -> Result<()> {
        self.cmd.send(c).await.map_err(|_| Error::NotRunning)
    }
}

struct AuthedPeer {
    user_id: String,
    display_name: String,
    role: Role,
    since_ms: u64,
    /// Whether this peer holds org data. False for a discovery-only seed
    /// server, which we stay connected to but never ask for changes.
    replicates: bool,
}

pub struct Node;

impl Node {
    /// Starts the node on the current tokio runtime and returns its handle.
    pub fn spawn(
        cfg: NodeConfig,
        identity: DeviceIdentity,
        enrollment: Enrollment,
        db: Arc<Db>,
    ) -> Result<NodeHandle> {
        enrollment.validate(&identity)?;

        let swarm = build_swarm(&identity, &cfg)?;
        let peer_id = *swarm.local_peer_id();
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let (ev_tx, _) = broadcast::channel(256);

        let runtime = Runtime {
            swarm,
            db: db.clone(),
            enrollment,
            cfg,
            authed: HashMap::new(),
            greeted: HashSet::new(),
            readonly_origins: db_readonly_origins(&db),
            deny: HashMap::new(),
            listen_addrs: Vec::new(),
            external_addrs: Vec::new(),
            events: ev_tx.clone(),
        };

        tokio::spawn(runtime.run(cmd_rx));
        Ok(NodeHandle { peer_id, cmd: cmd_tx, events: ev_tx })
    }
}

fn build_swarm(identity: &DeviceIdentity, cfg: &NodeConfig) -> Result<Swarm<NodeBehaviour>> {
    use libp2p::{noise, tcp, yamux};
    let netcfg = cfg.clone();
    let builder = libp2p::SwarmBuilder::with_existing_identity(identity.libp2p_keypair())
        .with_tokio()
        .with_tcp(tcp::Config::default().nodelay(true), noise::Config::new, yamux::Config::default)
        .map_err(|e| Error::Network(format!("tcp transport: {e}")))?
        .with_quic();

    // `with_dns` builds its resolver from the host's `/etc/resolv.conf`. Android
    // has no such file, so that call fails with ENOENT and takes the whole node
    // down before it ever listens. There the resolver is configured explicitly
    // instead — which also keeps name resolution working on networks whose ISP
    // resolver refuses to answer for the seed server's hostname.
    #[cfg(target_os = "android")]
    let builder = builder.with_dns_config(
        libp2p::dns::ResolverConfig::cloudflare(),
        libp2p::dns::ResolverOpts::default(),
    );
    #[cfg(not(target_os = "android"))]
    let builder = builder.with_dns().map_err(|e| Error::Network(format!("dns: {e}")))?;

    builder
        .with_relay_client(noise::Config::new, yamux::Config::default)
        .map_err(|e| Error::Network(format!("relay client: {e}")))?
        .with_behaviour(|key, relay_client| {
            behaviour::build(key, relay_client, &netcfg)
                .map_err(|e| Box::<dyn std::error::Error + Send + Sync>::from(e.to_string()))
        })
        .map_err(|e| Error::Network(format!("behaviour: {e}")))?
        .with_swarm_config(|c| c.with_idle_connection_timeout(cfg.idle_connection_timeout))
        .build()
        .pipe_ok()
}

/// Small helper so the builder chain above reads as one expression.
trait PipeOk: Sized {
    fn pipe_ok(self) -> Result<Self> {
        Ok(self)
    }
}
impl PipeOk for Swarm<NodeBehaviour> {}

struct Runtime {
    swarm: Swarm<NodeBehaviour>,
    db: Arc<Db>,
    enrollment: Enrollment,
    cfg: NodeConfig,
    authed: HashMap<PeerId, AuthedPeer>,
    /// Peers we have already sent a Hello to on this connection.
    greeted: HashSet<PeerId>,
    /// Devices enrolled read-only. Anything they author is discarded whatever
    /// route it arrives by.
    readonly_origins: HashSet<String>,
    /// Peers to disconnect once their Denied response has been flushed.
    deny: HashMap<PeerId, String>,
    listen_addrs: Vec<Multiaddr>,
    external_addrs: Vec<Multiaddr>,
    events: broadcast::Sender<NodeEvent>,
}

impl Runtime {
    fn emit(&self, ev: NodeEvent) {
        // A send error only means nobody is listening yet; that is fine.
        let _ = self.events.send(ev);
    }

    async fn run(mut self, mut cmd_rx: mpsc::Receiver<Command>) {
        if let Err(e) = self.start_listening() {
            self.emit(NodeEvent::Error { message: e.to_string() });
            return;
        }

        self.emit(NodeEvent::Started {
            peer_id: self.swarm.local_peer_id().to_string(),
            org_id: self.enrollment.org_id.clone(),
        });

        let mut sync_tick = tokio::time::interval(self.cfg.sync_interval);
        let mut flush_tick = tokio::time::interval(self.cfg.flush_interval);
        sync_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                event = self.swarm.select_next_some() => self.on_swarm_event(event),
                cmd = cmd_rx.recv() => match cmd {
                    Some(Command::Shutdown) | None => break,
                    Some(c) => self.on_command(c),
                },
                _ = flush_tick.tick() => self.flush_and_push(),
                _ = sync_tick.tick() => self.request_sync_from_all(),
            }
        }
        self.emit(NodeEvent::Stopped);
    }

    fn start_listening(&mut self) -> Result<()> {
        for addr in self.cfg.listen.clone() {
            self.swarm
                .listen_on(addr.clone())
                .map_err(|e| Error::Network(format!("listen on {addr}: {e}")))?;
        }

        let topic = gossipsub::IdentTopic::new(changes_topic(&self.enrollment.org_id));
        self.swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&topic)
            .map_err(|e| Error::Network(format!("subscribe: {e}")))?;

        for addr in self.bootstrap_addrs() {
            self.dial_bootstrap(addr);
        }
        Ok(())
    }

    fn bootstrap_addrs(&self) -> Vec<Multiaddr> {
        let mut addrs = self.cfg.bootstrap.clone();
        for raw in &self.enrollment.bootstrap {
            if let Ok(a) = raw.parse::<Multiaddr>() {
                if !addrs.contains(&a) {
                    addrs.push(a);
                }
            }
        }
        addrs
    }

    fn dial_bootstrap(&mut self, addr: Multiaddr) {
        let Some((peer, base)) = split_p2p(&addr) else {
            tracing::warn!(%addr, "bootstrap address has no /p2p/<peer id> suffix; ignoring");
            return;
        };
        self.swarm.behaviour_mut().kad.add_address(&peer, base);
        if let Err(e) = self.swarm.dial(addr.clone()) {
            tracing::warn!(%addr, error = %e, "could not dial seed server");
        }
        // Ask the seed server to relay for us. Without a reservation a phone
        // behind carrier NAT has no address other devices can reach.
        if self.cfg.enable_relay_client {
            let circuit = addr.with(Protocol::P2pCircuit);
            if let Err(e) = self.swarm.listen_on(circuit.clone()) {
                tracing::debug!(%circuit, error = %e, "relay reservation not started");
            }
        }
    }

    // ------------------------------------------------------------- commands

    fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::LocalChanged => self.flush_and_push(),
            Command::SyncNow => self.request_sync_from_all(),
            Command::Dial(addr) => {
                if let Err(e) = self.swarm.dial(addr.clone()) {
                    self.emit(NodeEvent::Error { message: format!("dial {addr}: {e}") });
                }
            }
            Command::Status(reply) => {
                let status = self.status();
                let _ = reply.send(status);
            }
            Command::Shutdown => {}
        }
    }

    fn status(&self) -> NodeStatus {
        let stats: DbStats = self.db.stats().unwrap_or_default();
        NodeStatus {
            peer_id: self.swarm.local_peer_id().to_string(),
            org_id: self.enrollment.org_id.clone(),
            org_name: self.enrollment.org_name.clone(),
            display_name: self.enrollment.cert.claims.display_name.clone(),
            listen_addrs: self.listen_addrs.iter().map(ToString::to_string).collect(),
            external_addrs: self.external_addrs.iter().map(ToString::to_string).collect(),
            connections: self.swarm.connected_peers().count() as u32,
            peers: self
                .authed
                .iter()
                .map(|(p, i)| PeerSummary {
                    peer_id: p.to_string(),
                    user_id: i.user_id.clone(),
                    display_name: i.display_name.clone(),
                    role: i.role.as_str().to_string(),
                    since_ms: i.since_ms,
                })
                .collect(),
            changes: stats.changes,
            known_devices: stats.known_devices,
            cert_expires_at_ms: self.enrollment.cert.claims.expires_at_ms,
        }
    }

    // ---------------------------------------------------------- replication

    /// Picks up local edits and gets them to everyone, by both routes:
    /// a direct push to each connected peer (fast and acknowledged) and a
    /// gossipsub broadcast (reaches peers we are not directly connected to).
    /// Duplicates cost nothing — applying a change twice is a no-op.
    fn flush_and_push(&mut self) {
        if !self.cfg.replicate {
            return;
        }
        // Stamp anything staged by a trigger. This may legitimately find
        // nothing: `Db::execute` flushes as part of the write.
        if let Err(e) = self.db.flush_local() {
            self.emit(NodeEvent::Error { message: format!("flush: {e}") });
            return;
        }
        // Then send whatever we have authored but not yet sent, regardless of
        // which code path stamped it.
        let (changes, high) = match self.db.unbroadcast_local(MAX_SYNC_CHANGES) {
            Ok(v) => v,
            Err(e) => {
                self.emit(NodeEvent::Error { message: format!("read outbox: {e}") });
                return;
            }
        };
        if changes.is_empty() {
            return;
        }
        self.emit(NodeEvent::LocalChanges { count: changes.len() as u32 });
        self.broadcast(&changes);
        if let Err(e) = self.db.mark_broadcast(high) {
            tracing::warn!(error = %e, "could not record the broadcast watermark");
        }
    }

    fn broadcast(&mut self, changes: &[ChangeRecord]) {
        let peers: Vec<PeerId> =
            self.authed.iter().filter(|(_, i)| i.replicates).map(|(p, _)| *p).collect();
        for chunk in changes.chunks(MAX_PUSH_CHANGES) {
            for peer in &peers {
                self.swarm
                    .behaviour_mut()
                    .rr
                    .send_request(peer, Req::Push { changes: chunk.to_vec() });
            }
            match postcard::to_stdvec(&ChangeBroadcast { changes: chunk.to_vec() }) {
                Ok(bytes) => {
                    let topic = gossipsub::IdentTopic::new(changes_topic(&self.enrollment.org_id));
                    // Fails harmlessly when no peer has joined the mesh yet.
                    if let Err(e) = self.swarm.behaviour_mut().gossipsub.publish(topic, bytes) {
                        tracing::debug!(error = %e, "gossip publish skipped");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "could not encode broadcast"),
            }
        }
    }

    fn request_sync_from_all(&mut self) {
        if !self.cfg.replicate {
            return;
        }
        let peers: Vec<PeerId> = self.authed.keys().copied().collect();
        for peer in peers {
            self.request_sync(peer);
        }
    }

    fn request_sync(&mut self, peer: PeerId) {
        if !self.cfg.replicate {
            return;
        }
        if !self.authed.get(&peer).map(|p| p.replicates).unwrap_or(false) {
            return;
        }
        match self.db.version_vector() {
            Ok(vv) => {
                self.swarm
                    .behaviour_mut()
                    .rr
                    .send_request(&peer, Req::Sync { vv, limit: MAX_SYNC_CHANGES as u32 });
            }
            Err(e) => self.emit(NodeEvent::Error { message: format!("version vector: {e}") }),
        }
    }

    /// Applies changes received by any route, after dropping anything a
    /// read-only device authored.
    ///
    /// The filter is on the **author**, not on whoever handed us the records.
    /// Checking the sender would be both too weak and too strong: too weak
    /// because anti-entropy *pulls* — refusing a read-only device's pushes
    /// achieves nothing when a peer turns around and asks it for changes — and
    /// too strong because a read-only device is still a full replica whose job
    /// includes relaying everyone else's writes.
    fn apply(&mut self, peer: PeerId, changes: &[ChangeRecord]) -> (u32, u32) {
        let mut unauthorised = 0;
        let filtered: Option<Vec<ChangeRecord>> =
            if changes.iter().any(|c| self.readonly_origins.contains(&c.origin)) {
                let kept: Vec<ChangeRecord> = changes
                    .iter()
                    .filter(|c| !self.readonly_origins.contains(&c.origin))
                    .cloned()
                    .collect();
                unauthorised = changes.len() - kept.len();
                Some(kept)
            } else {
                None
            };
        if unauthorised > 0 {
            tracing::warn!(%peer, dropped = unauthorised, "discarded writes authored read-only");
        }
        let changes: &[ChangeRecord] = filtered.as_deref().unwrap_or(changes);

        match self.db.apply_remote(changes) {
            Ok(out) => {
                if out.applied > 0 {
                    self.emit(NodeEvent::Synced {
                        peer: peer.to_string(),
                        applied: out.applied as u32,
                        tables: out.tables_touched.into_iter().collect(),
                    });
                }
                if out.rejected > 0 {
                    tracing::warn!(%peer, rejected = out.rejected, "dropped unverifiable changes");
                }
                (out.applied as u32, out.rejected as u32)
            }
            Err(e) => {
                self.emit(NodeEvent::Error { message: format!("apply from {peer}: {e}") });
                (0, 0)
            }
        }
    }

    // --------------------------------------------------------------- events

    fn on_swarm_event(&mut self, event: SwarmEvent<NodeBehaviourEvent>) {
        match event {
            SwarmEvent::NewListenAddr { address, .. } => {
                let full = address.clone().with(Protocol::P2p(*self.swarm.local_peer_id()));
                if address.iter().any(|p| matches!(p, Protocol::P2pCircuit)) {
                    self.emit(NodeEvent::RelayReserved { addr: full.to_string() });
                } else {
                    self.emit(NodeEvent::Listening { addr: full.to_string() });
                }
                if !self.listen_addrs.contains(&address) {
                    self.listen_addrs.push(address);
                }
            }
            SwarmEvent::ExternalAddrConfirmed { address } => {
                if !self.external_addrs.contains(&address) {
                    self.external_addrs.push(address);
                }
            }
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                // Nothing is trusted yet: introduce ourselves and wait.
                if self.greeted.insert(peer_id) {
                    let hello = Req::Hello {
                        proto: PROTOCOL_VERSION,
                        cert: self.enrollment.cert.clone(),
                        crl: self.enrollment.crl.clone(),
                        replicates: self.cfg.replicate,
                    };
                    self.swarm.behaviour_mut().rr.send_request(&peer_id, hello);
                }
            }
            SwarmEvent::ConnectionClosed { peer_id, num_established, .. } => {
                if num_established == 0 {
                    self.greeted.remove(&peer_id);
                    if self.authed.remove(&peer_id).is_some() {
                        self.emit(NodeEvent::PeerDisconnected { peer: peer_id.to_string() });
                    }
                }
            }
            SwarmEvent::Behaviour(ev) => self.on_behaviour_event(ev),
            _ => {}
        }
    }

    fn on_behaviour_event(&mut self, event: NodeBehaviourEvent) {
        match event {
            NodeBehaviourEvent::Rr(e) => self.on_rr_event(e),
            NodeBehaviourEvent::Gossipsub(gossipsub::Event::Message {
                propagation_source,
                message,
                ..
            }) => self.on_gossip(propagation_source, message),
            NodeBehaviourEvent::Mdns(mdns::Event::Discovered(list)) => {
                for (peer, addr) in list {
                    self.swarm.behaviour_mut().kad.add_address(&peer, addr.clone());
                    if !self.swarm.is_connected(&peer) {
                        let _ = self.swarm.dial(addr);
                    }
                }
            }
            NodeBehaviourEvent::Identify(identify::Event::Received { peer_id, info, .. }) => {
                // Feed the DHT what this peer says it listens on, so other
                // devices can find it later without the seed server.
                for addr in info.listen_addrs {
                    self.swarm.behaviour_mut().kad.add_address(&peer_id, addr);
                }
            }
            NodeBehaviourEvent::Kad(kad::Event::OutboundQueryProgressed { .. }) => {}
            _ => {}
        }
    }

    fn on_gossip(&mut self, source: PeerId, message: gossipsub::Message) {
        // Broadcasts are only honoured from peers that have proved org
        // membership; every record inside is signature-checked besides.
        if !self.authed.contains_key(&source) {
            tracing::debug!(%source, "ignoring gossip from an unauthenticated peer");
            return;
        }
        if !self.cfg.replicate {
            return;
        }
        match postcard::from_bytes::<ChangeBroadcast>(&message.data) {
            Ok(b) => {
                self.apply(source, &b.changes);
            }
            Err(e) => tracing::warn!(%source, error = %e, "undecodable broadcast"),
        }
    }

    fn on_rr_event(&mut self, event: request_response::Event<Req, Resp>) {
        match event {
            request_response::Event::Message { peer, message, .. } => match message {
                RrMessage::Request { request, channel, .. } => {
                    self.on_request(peer, request, channel)
                }
                RrMessage::Response { response, .. } => self.on_response(peer, response),
            },
            request_response::Event::ResponseSent { peer, .. } => {
                // A refusal has now reached the wire; drop the connection.
                if let Some(reason) = self.deny.remove(&peer) {
                    self.emit(NodeEvent::PeerRejected { peer: peer.to_string(), reason });
                    let _ = self.swarm.disconnect_peer_id(peer);
                }
            }
            request_response::Event::OutboundFailure { peer, error, .. } => {
                tracing::debug!(%peer, %error, "outbound request failed");
            }
            request_response::Event::InboundFailure { peer, error, .. } => {
                tracing::debug!(%peer, %error, "inbound request failed");
            }
        }
    }

    fn on_request(&mut self, peer: PeerId, req: Req, channel: ResponseChannel<Resp>) {
        let resp = match req {
            Req::Hello { proto, cert, crl, replicates } => {
                if proto != PROTOCOL_VERSION {
                    self.refuse(peer, format!("protocol version {proto} is not supported"))
                } else {
                    match self.accept_peer(peer, &cert, &crl, replicates) {
                        Ok(()) => Resp::Hello {
                            proto: PROTOCOL_VERSION,
                            cert: self.enrollment.cert.clone(),
                            crl: self.enrollment.crl.clone(),
                            replicates: self.cfg.replicate,
                        },
                        Err(e) => self.refuse(peer, e.to_string()),
                    }
                }
            }
            // Everything below is only served to an authenticated peer.
            _ if !self.authed.contains_key(&peer) => {
                Resp::Denied { reason: "say hello first".into() }
            }
            // A discovery-only node answers truthfully — it holds nothing and
            // stores nothing — rather than refusing. Refusing would read as
            // hostility and cost us the connection we exist to provide.
            Req::Sync { .. } if !self.cfg.replicate => {
                Resp::Sync { changes: Vec::new(), has_more: false }
            }
            Req::Push { .. } if !self.cfg.replicate => Resp::Push { applied: 0, rejected: 0 },
            Req::Sync { vv, limit } => {
                let limit = (limit as usize).min(MAX_SYNC_CHANGES);
                match self.db.changes_since(&vv, limit) {
                    Ok(changes) => {
                        let has_more = changes.len() == limit;
                        Resp::Sync { changes, has_more }
                    }
                    Err(e) => {
                        // Our own storage failing is not the peer's fault, and
                        // must not read to them as a refusal.
                        tracing::error!(%peer, error = %e, "could not read changes to send");
                        Resp::Sync { changes: Vec::new(), has_more: false }
                    }
                }
            }
            Req::Push { changes } => {
                let (applied, rejected) = self.apply(peer, &changes);
                Resp::Push { applied, rejected }
            }
        };

        if self.swarm.behaviour_mut().rr.send_response(channel, resp).is_err() {
            tracing::debug!(%peer, "peer went away before the response was sent");
        }
    }

    fn on_response(&mut self, peer: PeerId, resp: Resp) {
        match resp {
            Resp::Hello { proto, cert, crl, replicates } => {
                if proto != PROTOCOL_VERSION {
                    self.emit(NodeEvent::PeerRejected {
                        peer: peer.to_string(),
                        reason: format!("protocol version {proto} is not supported"),
                    });
                    let _ = self.swarm.disconnect_peer_id(peer);
                    return;
                }
                match self.accept_peer(peer, &cert, &crl, replicates) {
                    Ok(()) => self.request_sync(peer),
                    Err(e) => {
                        self.emit(NodeEvent::PeerRejected {
                            peer: peer.to_string(),
                            reason: e.to_string(),
                        });
                        let _ = self.swarm.disconnect_peer_id(peer);
                    }
                }
            }
            Resp::Sync { changes, has_more } => {
                if !changes.is_empty() {
                    self.apply(peer, &changes);
                }
                if has_more {
                    // Our version vector has moved on; ask for the next page.
                    self.request_sync(peer);
                }
            }
            Resp::Push { rejected, .. } => {
                if rejected > 0 {
                    tracing::warn!(%peer, rejected, "peer refused some of our changes");
                }
            }
            Resp::Denied { reason } => {
                self.emit(NodeEvent::PeerRejected { peer: peer.to_string(), reason });
                let _ = self.swarm.disconnect_peer_id(peer);
            }
        }
    }

    fn refuse(&mut self, peer: PeerId, reason: String) -> Resp {
        self.deny.insert(peer, reason.clone());
        Resp::Denied { reason }
    }

    /// The trust gate. Everything a peer may do afterwards depends on this.
    fn accept_peer(
        &mut self,
        peer: PeerId,
        cert: &crate::identity::DeviceCert,
        crl: &crate::identity::RevocationList,
        replicates: bool,
    ) -> Result<()> {
        // Adopt a newer revocation list before judging the certificate, so a
        // peer that hands us fresh revocation news cannot use it to smuggle
        // itself in, and so revocations spread across the mesh on their own.
        if self.enrollment.merge_crl(crl) {
            let _ = self.enrollment.save(&self.db);
        }

        cert.verify(
            &self.enrollment.org_id,
            Some(&peer),
            Some(&self.enrollment.crl),
            now_ms(),
        )?;

        let info = AuthedPeer {
            user_id: cert.claims.user_id.clone(),
            display_name: cert.claims.display_name.clone(),
            role: cert.claims.role,
            since_ms: now_ms(),
            replicates,
        };
        // Both sides greet each other, so a successful handshake is seen twice:
        // once as an inbound Hello and once as the reply to our own. Announce
        // the peer only on the transition into the authenticated set.
        let newly_authenticated = !self.authed.contains_key(&peer);
        if info.role.may_write() {
            self.readonly_origins.remove(&peer.to_string());
        } else {
            self.readonly_origins.insert(peer.to_string());
        }
        let _ = self.db.record_peer(
            &peer.to_string(),
            &info.user_id,
            &info.display_name,
            info.role.as_str(),
            cert.claims.serial as i64,
            &serde_json::to_string(cert).unwrap_or_default(),
        );
        if newly_authenticated {
            self.emit(NodeEvent::PeerConnected {
                peer: peer.to_string(),
                user_id: info.user_id.clone(),
                display_name: info.display_name.clone(),
                role: info.role.as_str().to_string(),
            });
        }
        self.authed.insert(peer, info);
        Ok(())
    }
}

/// Read-only devices remembered from previous sessions.
fn db_readonly_origins(db: &Db) -> HashSet<String> {
    db.devices_with_role(Role::ReadOnly.as_str()).unwrap_or_default().into_iter().collect()
}

/// Splits `/ip4/…/tcp/…/p2p/<id>` into the peer id and the dialable prefix.
fn split_p2p(addr: &Multiaddr) -> Option<(PeerId, Multiaddr)> {
    let mut base = addr.clone();
    match base.pop() {
        Some(Protocol::P2p(peer)) => Some((peer, base)),
        _ => None,
    }
}

/// Suggested keep-alive for callers that want to poll status.
pub const STATUS_POLL_HINT: Duration = Duration::from_secs(2);
