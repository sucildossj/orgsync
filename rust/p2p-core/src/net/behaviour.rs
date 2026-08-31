//! The composed libp2p behaviour every org device runs.
//!
//! A seed server and a phone run the *same* stack; they differ only in which
//! toggles are on. That keeps one protocol implementation in the codebase and
//! makes "the seed server is just a well-known peer" true rather than
//! aspirational.

use std::time::Duration;

use libp2p::{
    dcutr, gossipsub, identify, kad, mdns, ping, relay,
    request_response::{self, ProtocolSupport},
    swarm::{behaviour::toggle::Toggle, NetworkBehaviour},
    identity::Keypair,
    StreamProtocol,
};

use super::config::NodeConfig;
use super::proto::{Req, Resp, IDENTIFY_PROTOCOL, KAD_PROTOCOL, SYNC_PROTOCOL};
use crate::error::{Error, Result};

#[derive(NetworkBehaviour)]
pub struct NodeBehaviour {
    pub identify: identify::Behaviour,
    pub ping: ping::Behaviour,
    /// Peer discovery beyond the LAN. Runs on a private protocol name so we
    /// never join or leak into the public IPFS DHT.
    pub kad: kad::Behaviour<kad::store::MemoryStore>,
    /// Multi-hop fan-out of fresh changes.
    pub gossipsub: gossipsub::Behaviour,
    /// Handshake, anti-entropy and direct push.
    pub rr: request_response::cbor::Behaviour<Req, Resp>,
    /// LAN discovery, so an office keeps syncing with the internet down.
    pub mdns: Toggle<mdns::tokio::Behaviour>,
    pub relay_client: relay::client::Behaviour,
    pub relay_server: Toggle<relay::Behaviour>,
    /// Upgrades a relayed connection to a direct one by hole punching.
    pub dcutr: dcutr::Behaviour,
}

pub fn build(
    keypair: &Keypair,
    relay_client: relay::client::Behaviour,
    cfg: &NodeConfig,
) -> Result<NodeBehaviour> {
    let peer_id = keypair.public().to_peer_id();

    let gossip_cfg = gossipsub::ConfigBuilder::default()
        .heartbeat_interval(Duration::from_secs(1))
        // Every message must carry the author's signature.
        .validation_mode(gossipsub::ValidationMode::Strict)
        // Content-addressed ids, so the same batch arriving by two routes is
        // recognised as one message instead of being flooded twice.
        .message_id_fn(|m: &gossipsub::Message| {
            gossipsub::MessageId::from(blake3::hash(&m.data).as_bytes()[..16].to_vec())
        })
        .max_transmit_size(512 * 1024)
        .build()
        .map_err(|e| Error::Network(format!("gossipsub config: {e}")))?;

    let gossipsub = gossipsub::Behaviour::new(
        gossipsub::MessageAuthenticity::Signed(keypair.clone()),
        gossip_cfg,
    )
    .map_err(|e| Error::Network(format!("gossipsub: {e}")))?;

    let mut kad_cfg = kad::Config::new(StreamProtocol::new(KAD_PROTOCOL));
    kad_cfg.set_query_timeout(Duration::from_secs(30));
    let mut kad =
        kad::Behaviour::with_config(peer_id, kad::store::MemoryStore::new(peer_id), kad_cfg);
    kad.set_mode(Some(if cfg.kad_server { kad::Mode::Server } else { kad::Mode::Client }));

    let mdns = Toggle::from(if cfg.enable_mdns {
        Some(mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id)?)
    } else {
        None
    });

    let relay_server = Toggle::from(if cfg.enable_relay_server {
        Some(relay::Behaviour::new(peer_id, relay::Config::default()))
    } else {
        None
    });

    Ok(NodeBehaviour {
        identify: identify::Behaviour::new(
            identify::Config::new(IDENTIFY_PROTOCOL.to_string(), keypair.public())
                .with_agent_version(format!("orgsync/{}", env!("CARGO_PKG_VERSION"))),
        ),
        ping: ping::Behaviour::new(ping::Config::new()),
        kad,
        gossipsub,
        rr: request_response::cbor::Behaviour::new(
            [(StreamProtocol::new(SYNC_PROTOCOL), ProtocolSupport::Full)],
            request_response::Config::default().with_request_timeout(Duration::from_secs(30)),
        ),
        mdns,
        relay_client,
        relay_server,
        dcutr: dcutr::Behaviour::new(peer_id),
    })
}
