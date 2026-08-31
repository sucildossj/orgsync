//! A full org peer that runs in a terminal.
//!
//! Identical to what the phone runs — same crate, same protocol, same replica
//! format — which makes it the quickest way to check that a seed server, a
//! tunnel or a certificate actually works, without building the mobile app.
//! Two of these on different machines sync with each other exactly as two
//! phones would.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use p2p_core::db::{Db, SqlValue};
use p2p_core::enroll::{build_enroll_request, EnrollResponse, Enrollment};
use p2p_core::identity::DeviceIdentity;
use p2p_core::net::node::{Node, NodeEvent};
use p2p_core::net::NodeConfig;

#[derive(Parser)]
#[command(name = "p2p-cli", version, about = "A desktop peer for an org's p2p replica")]
struct Cli {
    /// Where this peer keeps its key and its copy of the data.
    #[arg(long, env = "P2P_DATA_DIR", default_value = "./peer-data", global = true)]
    data_dir: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Join an organisation with an invite code.
    Enroll {
        /// Base URL of the seed server, e.g. https://xyz.trycloudflare.com
        #[arg(long, env = "SEED_URL")]
        url: String,
        /// The invite code, in any format; dashes and case are ignored.
        #[arg(long)]
        code: String,
        #[arg(long, default_value = "desktop")]
        name: String,
    },
    /// Run the node and print what happens.
    Run {
        /// Also print every message as it arrives.
        #[arg(long)]
        watch: bool,
    },
    /// Post a message, then keep running so it reaches peers.
    Send {
        text: String,
        #[arg(long, default_value = "general")]
        room: String,
        /// How long to stay connected while the message propagates.
        #[arg(long, default_value_t = 10)]
        seconds: u64,
    },
    /// Print the messages this peer holds.
    Log {
        #[arg(long, default_value = "general")]
        room: String,
    },
    /// Run a read-only SQL query against the local replica.
    Sql { query: String },
    /// Show identity and enrolment state.
    Whoami,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "p2p_cli=info,p2p_core=warn".into()),
        )
        .without_time()
        .init();

    let cli = Cli::parse();
    std::fs::create_dir_all(&cli.data_dir)?;
    let identity = load_or_create_identity(&cli.data_dir)?;
    let db = Arc::new(Db::open_authenticated(cli.data_dir.join("replica.db"), &identity)?);

    match cli.cmd {
        Cmd::Enroll { url, code, name } => enroll(&url, &code, &name, &identity, &db)?,
        Cmd::Whoami => whoami(&identity, &db)?,
        Cmd::Log { room } => print_log(&db, &room)?,
        Cmd::Sql { query } => run_sql(&db, &query)?,
        Cmd::Run { watch } => runtime()?.block_on(run(identity, db, watch, None))?,
        Cmd::Send { text, room, seconds } => {
            runtime()?.block_on(run(identity, db, true, Some((room, text, seconds))))?
        }
    }
    Ok(())
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread().enable_all().build()?)
}

fn enroll(
    url: &str,
    code: &str,
    name: &str,
    identity: &DeviceIdentity,
    db: &Db,
) -> Result<()> {
    let base = url.trim_end_matches('/');
    let request = build_enroll_request(identity, code, name, "desktop");

    let response: EnrollResponse = ureq::post(&format!("{base}/v1/enroll"))
        .send_json(&request)
        .map_err(|e| match e {
            // The server explains refusals in plain language; surface that
            // rather than a bare status code.
            ureq::Error::StatusCode(code) => {
                anyhow::anyhow!("the seed server refused the enrolment (HTTP {code})")
            }
            other => anyhow::anyhow!("could not reach {base}: {other}"),
        })?
        .body_mut()
        .read_json()
        .context("the server's reply was not a valid enrolment response")?;

    let enrollment = Enrollment::accept(response, identity)
        .context("the certificate the server returned did not check out")?;
    enrollment.save(db)?;

    println!("Joined {} ({}).", enrollment.org_name, &enrollment.org_id[..12]);
    println!("  peer id  {}", identity.peer_id());
    println!("  role     {}", enrollment.cert.claims.role.as_str());
    if enrollment.bootstrap.is_empty() {
        println!("\n  The server advertised no dialable address, so this peer will find");
        println!("  others on the local network only. That is expected behind an");
        println!("  HTTP-only tunnel.");
    } else {
        println!("  seeds    {}", enrollment.bootstrap.len());
    }
    Ok(())
}

fn whoami(identity: &DeviceIdentity, db: &Db) -> Result<()> {
    println!("peer id   {}", identity.peer_id());
    match Enrollment::load(db)? {
        Some(e) => {
            println!("org       {} ({})", e.org_name, &e.org_id[..12]);
            println!("user      {}", e.cert.claims.user_id);
            println!("device    {}", e.cert.claims.display_name);
            println!("role      {}", e.cert.claims.role.as_str());
            println!("serial    {}", e.cert.claims.serial);
            match e.validate(identity) {
                Ok(()) => println!("status    valid"),
                Err(err) => println!("status    NOT USABLE: {err}"),
            }
            for b in &e.bootstrap {
                println!("seed      {b}");
            }
        }
        None => println!("org       not enrolled — run `p2p-cli enroll`"),
    }
    let s = db.stats()?;
    println!("replica   {} changes from {} devices", s.changes, s.known_devices);
    Ok(())
}

fn print_log(db: &Db, room: &str) -> Result<()> {
    let rows = db.query(
        "SELECT author_name, body, sent_at_ms FROM messages WHERE room = ?1 ORDER BY sent_at_ms",
        &[SqlValue::Text(room.into())],
    )?;
    if rows.rows.is_empty() {
        println!("(no messages in #{room} yet)");
    }
    for row in rows.rows {
        println!("{}", format_message(&row[0], &row[1]));
    }
    Ok(())
}

fn run_sql(db: &Db, query: &str) -> Result<()> {
    let result = db.query(query, &[])?;
    println!("{}", result.columns.join(" | "));
    for row in result.rows {
        let cells: Vec<String> = row.iter().map(render).collect();
        println!("{}", cells.join(" | "));
    }
    Ok(())
}

async fn run(
    identity: DeviceIdentity,
    db: Arc<Db>,
    watch: bool,
    post: Option<(String, String, u64)>,
) -> Result<()> {
    let Some(enrollment) = Enrollment::load(&db)? else {
        bail!("this peer has not joined an organisation yet — run `p2p-cli enroll` first");
    };
    enrollment.validate(&identity).context("this peer's certificate is no longer usable")?;

    let node = Node::spawn(NodeConfig::default(), identity.clone(), enrollment, db.clone())?;
    let mut events = node.subscribe();

    if let Some((room, text, _)) = &post {
        let id = random_id();
        db.execute(
            "INSERT INTO messages (id, room, author, author_name, body, sent_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            &[
                SqlValue::Text(id),
                SqlValue::Text(room.clone()),
                SqlValue::Text(identity.peer_id().to_string()),
                SqlValue::Text(whoami_name(&db)),
                SqlValue::Text(text.clone()),
                SqlValue::Int(p2p_core::hlc::now_ms() as i64),
            ],
        )?;
        node.local_changed().await?;
        println!("posted to #{room}");
    }

    let deadline = post.as_ref().map(|(_, _, secs)| {
        tokio::time::Instant::now() + std::time::Duration::from_secs(*secs)
    });
    let mut seen = last_message_id(&db);

    loop {
        let recv = if let Some(d) = deadline {
            match tokio::time::timeout_at(d, events.recv()).await {
                Ok(r) => r,
                Err(_) => break,
            }
        } else {
            events.recv().await
        };

        let Ok(ev) = recv else { break };
        match ev {
            NodeEvent::Started { peer_id, org_id } => {
                println!("running as {peer_id}\n  org {}", &org_id[..12]);
            }
            NodeEvent::Listening { addr } => println!("  listening  {addr}"),
            NodeEvent::RelayReserved { addr } => println!("  reachable  {addr}"),
            NodeEvent::PeerConnected { display_name, role, .. } => {
                println!("+ {display_name} ({role}) joined");
            }
            NodeEvent::PeerDisconnected { peer } => println!("- {} left", &peer[..12.min(peer.len())]),
            NodeEvent::PeerRejected { peer, reason } => {
                println!("! refused {}: {reason}", &peer[..12.min(peer.len())]);
            }
            NodeEvent::Synced { applied, tables, .. } => {
                println!("  synced {applied} changes ({})", tables.join(", "));
                if watch {
                    seen = print_new_messages(&db, seen);
                }
            }
            NodeEvent::Error { message } => println!("! {message}"),
            _ => {}
        }
    }

    let _ = node.shutdown().await;
    Ok(())
}

fn print_new_messages(db: &Db, after: Option<String>) -> Option<String> {
    let rows = db
        .query(
            "SELECT id, author_name, body FROM messages ORDER BY sent_at_ms, id",
            &[],
        )
        .ok()?;
    let mut reached = after.is_none();
    let mut last = after.clone();
    for row in &rows.rows {
        let id = render(&row[0]);
        if !reached {
            if Some(&id) == after.as_ref() {
                reached = true;
            }
            continue;
        }
        println!("  {}", format_message(&row[1], &row[2]));
        last = Some(id);
    }
    last.or(after)
}

fn last_message_id(db: &Db) -> Option<String> {
    db.query("SELECT id FROM messages ORDER BY sent_at_ms DESC, id DESC LIMIT 1", &[])
        .ok()?
        .rows
        .first()
        .map(|r| render(&r[0]))
}

fn whoami_name(db: &Db) -> String {
    Enrollment::load(db)
        .ok()
        .flatten()
        .map(|e| e.cert.claims.display_name)
        .unwrap_or_else(|| "desktop".into())
}

fn format_message(author: &SqlValue, body: &SqlValue) -> String {
    format!("<{}> {}", render(author), render(body))
}

fn render(v: &SqlValue) -> String {
    match v {
        SqlValue::Null => "NULL".into(),
        SqlValue::Int(i) => i.to_string(),
        SqlValue::Real(f) => f.to_string(),
        SqlValue::Text(s) => s.clone(),
        SqlValue::Blob(b) => format!("<{} bytes>", b.len()),
    }
}

fn load_or_create_identity(dir: &Path) -> Result<DeviceIdentity> {
    let path = dir.join("device.key");
    if path.exists() {
        return Ok(DeviceIdentity::from_secret(&std::fs::read(&path)?)?);
    }
    let identity = DeviceIdentity::generate();
    std::fs::write(&path, identity.secret_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(identity)
}

fn random_id() -> String {
    use rand::RngCore;
    let mut raw = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut raw);
    hex::encode(raw)
}
