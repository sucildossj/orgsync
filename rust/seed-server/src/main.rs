//! The org seed server.
//!
//! It does four jobs, and deliberately no more:
//!
//! * **Certificate authority** — holds the org root key and mints device
//!   certificates in exchange for a single-use invite code.
//! * **Rendezvous** — a well-known peer whose address never changes, so a
//!   phone with no other information can find the rest of the organisation.
//! * **Relay** — lends its public address to devices stuck behind NAT until
//!   DCUtR can hole-punch a direct connection between them.
//! * **Optional replica** (`--replica`) — an always-on member so devices that
//!   are never online at the same moment still converge.
//!
//! Messages between phones do not pass through it. Once two devices know
//! about each other they talk directly, and the LAN case never involves this
//! process at all.

mod http;
mod store;

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use p2p_core::db::Db;
use p2p_core::enroll::Enrollment;
use p2p_core::identity::Role;
use p2p_core::net::node::{Node, NodeEvent};
use p2p_core::net::NodeConfig;
use store::Store;

const DAY_MS: u64 = 24 * 3_600_000;
/// Serial 0 is reserved for the server's own certificate; devices start at 1.
const SERVER_SERIAL: u64 = 0;

#[derive(Parser)]
#[command(name = "seed-server", version, about = "Org seed server: certificate authority, rendezvous and relay")]
struct Cli {
    /// Where the org key, invites and device registry live.
    #[arg(long, env = "SEED_DATA_DIR", default_value = "./seed-data", global = true)]
    data_dir: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the organisation. Run once; it generates the root key.
    Init {
        #[arg(long, default_value = "My Organisation")]
        org_name: String,
    },
    /// Run the server.
    Run(RunArgs),
    /// Mint a single-use invite code for a person.
    Invite {
        #[arg(long)]
        user: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value = "member", value_parser = ["admin", "member", "readonly"])]
        role: String,
        #[arg(long, default_value_t = 24)]
        hours: u64,
    },
    /// List enrolled devices.
    Devices,
    /// Revoke a device by serial. Takes effect across the mesh, not just here.
    Revoke {
        #[arg(long)]
        serial: u64,
    },
    /// Print the admin API token.
    Token,
}

#[derive(Args)]
struct RunArgs {
    #[arg(long, default_value_t = 8080)]
    http_port: u16,
    #[arg(long, default_value = "0.0.0.0")]
    http_bind: String,
    #[arg(long, default_value_t = 4001)]
    quic_port: u16,
    #[arg(long, default_value_t = 4001)]
    tcp_port: u16,
    /// Public hostname or IP devices should dial. Repeatable. Without it the
    /// server can only advertise the addresses it discovers locally, which is
    /// usually not what you want in production.
    #[arg(long)]
    announce: Vec<String>,
    /// Keep a full copy of the org's data so devices that are never online
    /// together still converge.
    #[arg(long)]
    replica: bool,
    /// Lifetime of newly issued device certificates.
    #[arg(long, default_value_t = 365)]
    cert_days: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "seed_server=info,p2p_core=info".into()),
        )
        .init();

    let cli = Cli::parse();
    std::fs::create_dir_all(&cli.data_dir)
        .with_context(|| format!("creating data directory {}", cli.data_dir.display()))?;
    let store = Arc::new(Store::open(cli.data_dir.join("seed.db"))?);

    match cli.cmd {
        Cmd::Init { org_name } => {
            let (org_id, admin_token) = store.init(&org_name)?;
            let peer_id = store.node_identity()?.peer_id();
            println!("Organisation created.\n");
            println!("  name         {org_name}");
            println!("  org id       {org_id}");
            println!("  server peer  {peer_id}");
            println!("  admin token  {admin_token}\n");
            println!("Keep {} safe: it holds the org root key, and losing it", cli.data_dir.display());
            println!("invalidates every certificate ever issued.");
        }
        Cmd::Run(args) => run(store, args, cli.data_dir.clone()).await?,
        Cmd::Invite { user, name, role, hours } => {
            let display = name.unwrap_or_else(|| user.clone());
            let invite = store.create_invite(
                &user,
                &display,
                Role::from_str_lossy(&role),
                hours.saturating_mul(3_600_000),
            )?;
            println!("Invite code for {user} ({role}), valid {hours}h:\n");
            println!("    {}\n", invite.invite_code_display());
        }
        Cmd::Devices => {
            let devices = store.list_devices()?;
            if devices.is_empty() {
                println!("No devices enrolled yet.");
            }
            for d in devices {
                println!(
                    "{:>4}  {:<10} {:<18} {:<9} {}{}",
                    d.serial,
                    d.role,
                    d.display_name,
                    d.platform,
                    d.peer_id,
                    if d.revoked_at_ms.is_some() { "   [REVOKED]" } else { "" }
                );
            }
        }
        Cmd::Revoke { serial } => {
            store.revoke(serial)?;
            println!("Device {serial} revoked. The signed revocation list now names it;");
            println!("devices pick it up from the server or from each other.");
        }
        Cmd::Token => println!("{}", store.admin_token()?),
    }
    Ok(())
}

async fn run(store: Arc<Store>, args: RunArgs, data_dir: PathBuf) -> Result<()> {
    let identity = store.node_identity()?;
    let org = store.org_keypair()?;
    let peer_id = identity.peer_id();

    // The server speaks the same authenticated protocol as every phone, so it
    // needs a certificate too. It signs its own, since it holds the root key.
    let cert = org.issue_cert(
        identity.public_bytes(),
        "seed-server",
        format!("{} seed server", store.org_name()?),
        Role::Admin,
        SERVER_SERIAL,
        args.cert_days.saturating_mul(DAY_MS),
    );

    let bootstrap = Arc::new(RwLock::new(announce_addrs(
        &args.announce,
        args.quic_port,
        args.tcp_port,
        &peer_id.to_string(),
    )));

    let enrollment = Enrollment {
        org_id: org.org_id(),
        org_name: store.org_name()?,
        cert,
        crl: store.crl()?,
        bootstrap: Vec::new(),
    };

    let db = Arc::new(Db::open_authenticated(data_dir.join("replica.db"), &identity)?);

    let mut cfg = NodeConfig::seed_server(args.quic_port, args.tcp_port);
    cfg.replicate = args.replica;
    let node = Node::spawn(cfg, identity, enrollment, db)?;

    // Learn the addresses we actually ended up on and advertise those too.
    {
        let bootstrap = bootstrap.clone();
        let mut events = node.subscribe();
        tokio::spawn(async move {
            while let Ok(ev) = events.recv().await {
                match ev {
                    NodeEvent::Listening { addr } => {
                        if is_advertisable(&addr) {
                            if let Ok(mut b) = bootstrap.write() {
                                if !b.contains(&addr) {
                                    tracing::info!(%addr, "listening");
                                    b.push(addr);
                                }
                            }
                        }
                    }
                    NodeEvent::PeerConnected { display_name, role, .. } => {
                        tracing::info!(peer = %display_name, %role, "device connected");
                    }
                    NodeEvent::PeerRejected { peer, reason } => {
                        tracing::warn!(%peer, %reason, "refused a peer");
                    }
                    NodeEvent::Error { message } => tracing::error!(%message, "node error"),
                    _ => {}
                }
            }
        });
    }

    let state = http::AppState {
        store: store.clone(),
        bootstrap,
        cert_ttl_ms: args.cert_days.saturating_mul(DAY_MS),
    };

    let bind: IpAddr = args.http_bind.parse().context("--http-bind is not an IP address")?;
    let addr = SocketAddr::new(bind, args.http_port);
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;

    tracing::info!(%peer_id, org = %org.org_id(), replica = args.replica, "seed server ready");
    tracing::info!("http    http://{addr}");
    tracing::info!("p2p     quic/{} tcp/{}", args.quic_port, args.tcp_port);
    if args.announce.is_empty() {
        tracing::warn!("no --announce given; devices outside this host may not be able to dial back");
    }

    axum::serve(listener, http::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await?;

    let _ = node.shutdown().await;
    Ok(())
}

/// Turns `--announce example.org` into the multiaddrs a device can dial.
fn announce_addrs(hosts: &[String], quic_port: u16, tcp_port: u16, peer_id: &str) -> Vec<String> {
    let mut out = Vec::new();
    for host in hosts {
        // Accept a full multiaddr as-is, for anything unusual.
        if host.starts_with('/') {
            out.push(if host.contains("/p2p/") {
                host.clone()
            } else {
                format!("{host}/p2p/{peer_id}")
            });
            continue;
        }
        let prefix = match host.parse::<IpAddr>() {
            Ok(IpAddr::V4(ip)) => format!("/ip4/{ip}"),
            Ok(IpAddr::V6(ip)) => format!("/ip6/{ip}"),
            Err(_) => format!("/dns4/{host}"),
        };
        out.push(format!("{prefix}/udp/{quic_port}/quic-v1/p2p/{peer_id}"));
        out.push(format!("{prefix}/tcp/{tcp_port}/p2p/{peer_id}"));
    }
    out
}

/// Loopback and unspecified addresses are useless to anybody else.
fn is_advertisable(addr: &str) -> bool {
    !addr.contains("/ip4/127.") && !addr.contains("/ip6/::1") && !addr.contains("/ip4/0.0.0.0")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEER: &str = "12D3KooWPrneQxv2mCWRX8qhNtcxc6AzR9TtwLUoXXkDtzjWi561";

    #[test]
    fn a_hostname_becomes_both_a_quic_and_a_tcp_address() {
        let addrs = announce_addrs(&["seed.acme.org".into()], 4001, 4001, PEER);
        assert_eq!(
            addrs,
            vec![
                format!("/dns4/seed.acme.org/udp/4001/quic-v1/p2p/{PEER}"),
                format!("/dns4/seed.acme.org/tcp/4001/p2p/{PEER}"),
            ]
        );
    }

    #[test]
    fn a_bare_ip_is_recognised_rather_than_treated_as_a_hostname() {
        let v4 = announce_addrs(&["203.0.113.7".into()], 4001, 4001, PEER);
        assert!(v4[0].starts_with("/ip4/203.0.113.7/udp/4001/quic-v1"));
        let v6 = announce_addrs(&["2001:db8::1".into()], 4001, 4001, PEER);
        assert!(v6[0].starts_with("/ip6/2001:db8::1/udp/4001/quic-v1"));
    }

    #[test]
    fn a_tunnel_endpoint_can_be_given_as_a_full_multiaddr() {
        // A TCP tunnel forwards a random public port to our local one and
        // carries no UDP, so neither the port pair nor the QUIC address can be
        // derived from a hostname. Passing the multiaddr outright is the way
        // to announce exactly one reachable path and nothing else.
        let addrs = announce_addrs(&["/dns4/7.tcp.ngrok.io/tcp/23456".into()], 4001, 4001, PEER);
        assert_eq!(addrs, vec![format!("/dns4/7.tcp.ngrok.io/tcp/23456/p2p/{PEER}")]);
    }

    #[test]
    fn a_multiaddr_that_already_names_the_peer_is_left_alone() {
        let given = format!("/dns4/seed.acme.org/tcp/4001/p2p/{PEER}");
        assert_eq!(announce_addrs(std::slice::from_ref(&given), 4001, 4001, PEER), vec![given]);
    }

    #[test]
    fn addresses_nobody_else_can_dial_are_not_advertised() {
        assert!(!is_advertisable("/ip4/127.0.0.1/tcp/4001"));
        assert!(!is_advertisable("/ip4/0.0.0.0/tcp/4001"));
        assert!(!is_advertisable("/ip6/::1/tcp/4001"));
        assert!(is_advertisable("/ip4/192.168.1.20/tcp/4001"));
        assert!(is_advertisable("/dns4/seed.acme.org/tcp/4001"));
    }
}
