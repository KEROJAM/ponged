//! `ponged-gateway` — the matchmaking + connectivity gateway for the game (M6).
//!
//! A headless libp2p node that provides:
//!   * **Relay** (circuit-relay v2): NAT-traversed connectivity for clients
//!     via `/p2p/<gateway>/p2p-circuit` addresses.
//!   * **Rendezvous server**: the shared discovery point where clients
//!     publish/query each other.
//!   * **Kademlia server**: a DHT bootstrap node for discovery fallback.
//!   * **Matchmaking**: an ELO-rating queue that pairs
//!     clients and pushes them `GatewayRequest::MatchFound` with dialable
//!     relayed circuit addresses for their opponent.
//!   * **Signed rating ledger**: the gateway certifies every rating it accepts
//!     or computes by signing `rating ‖ seq ‖ <player peer id>` with its
//!     keypair (see [`RatingProof`]). Clients keep the newest proof locally, so
//!     a gateway that loses its SQLite — or a new gateway sharing the same
//!     `--key` file — can recover every player's ELO from their own claims.
//!   * **Match ledger + moderation**: every finished match is stored in SQLite
//!     (the two complementary `ReportResult`s merge into one row). An HTTP
//!     monitor page (`--http <port>`, default `8080`) lists those matches; a
//!     moderator can **revoke** a match played unfairly (rolls both players'
//!     ELO back and pushes `GatewayRequest::MatchRevoked` to the clients) or
//!     **correct the final score** of a reported match (recomputes the ELO
//!     from the honest outcome and pushes `GatewayRequest::MatchCorrected`).
//!     The page is **locked**: only a registered moderator login (see
//!     `--add-moderator`) can view, edit or revoke.
//!
//! Client ↔ gateway RPC flows over the [`GATEWAY_PROTOCOL`] cbor channel
//! (`Register` → `QueueMatch` → gateway pushes `MatchFound` → both sides dial
//! the relayed address → DC‑UTP upgrades the circuit to a direct connection).
//!
//! Usage:
//! ```text
//! ponged-gateway [--listen /ip4/0.0.0.0/tcp/4001]
//!              [--public HOST_OR_IP]        # advertise this host in relay addr
//!              [--db gateway.sqlite]
//!              [--key gateway.key]
//!              [--http PORT]                # monitor + moderación page
//! ponged-gateway --add-moderator <user>       # create/reset a moderator account
//! ponged-gateway --remove-moderator <user>    # delete a moderator account
//! ```
//!
//! The gateway's own `PeerId` is printed at startup; players connect to it
//! with `NetCommand::Dial(<listen addr>)`.

use std::collections::HashMap;
use std::io::BufRead;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use libp2p::core::multiaddr::Protocol;
use libp2p::futures::StreamExt;
use libp2p::identity::Keypair;
use libp2p::kad::store::MemoryStore;
use libp2p::request_response::{self, ProtocolSupport, ResponseChannel, cbor};
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{
    Multiaddr, PeerId, SwarmBuilder, identify, kad, noise, ping, relay, rendezvous, tcp, yamux,
};
use ponged::protocol::{
    DEFAULT_RATING, GATEWAY_AGENT_VERSION, GATEWAY_PROTOCOL, GatewayRequest, GatewayResponse,
    RatingProof, rank_for_rating,
};
use rand::Rng as _;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;
use sha2::digest::Output;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::MissedTickBehavior;

const DEFAULT_LISTEN: &str = "/ip4/0.0.0.0/tcp/4001";
const DEFAULT_HOST_FALLBACK: &str = "127.0.0.1";
/// Port of the HTTP monitor + moderation page (0 disables it).
const DEFAULT_HTTP_PORT: u16 = 8080;
/// Rating granted to new players — inside the Brick range (< 1000).
/// Mirrors the client's local default; the client is the ELO source of truth
/// and always reports its stored rating at `Register`.
const START_RATING: i32 = DEFAULT_RATING;
/// Do not pair players whose ELO differs by more than this.
const MAX_ELO_GAP: i32 = 600;
/// Ceiling for a (moderator-edited) final score: games resolve at 5 but may
/// run longer on deuce; anything above this is almost certainly a typo.
const MAX_SCORE: u32 = 25;
/// How often the matchmaking queue is scanned for pairs.
const QUEUE_INTERVAL: Duration = Duration::from_secs(2);
/// Iterations for the moderator-password hash (stretched SHA-256).
const PASSWORD_ITERATIONS: u32 = 60_000;
/// Lifetime of a moderator HTTP session (sliding on use).
const SESSION_MAX_AGE_SECS: u64 = 8 * 3600;
/// Cookie that holds the moderator session token.
const COOKIE_NAME: &str = "pong_mod";
const USAGE: &str = "\
ponged-gateway — matchmaking + relay gateway for the P2P pong game

USAGE:
    ponged-gateway [OPTIONS]
    ponged-gateway --add-moderator <user>
    ponged-gateway --remove-moderator <user>

OPTIONS:
    --listen <multiaddr>   Address to listen on   [default: /ip4/0.0.0.0/tcp/4001]
    --public <host>        Host/ip advertised in relay addresses (needed when
                           the gateway is behind a NAT / not on localhost)
    --db <path>            SQLite rating database [default: gateway.sqlite]
    --key <path>           Ed25519 private key, created if missing [default: gateway.key]
    --http <port>          HTTP monitor + moderation page port (0 disables)
                           [default: 8080]
    --add-moderator <user> Create (or reset the password of) a moderator
                           account; the password is read from stdin, then exits
    --remove-moderator <user>  Delete a moderator account, then exits
    --help                 Print this help
";

/// Server behaviours (M6).
#[derive(NetworkBehaviour)]
struct Behaviour {
    ping: ping::Behaviour,
    identify: identify::Behaviour,
    /// DHT bootstrap node; stores peer records for discovery fallback.
    kademlia: kad::Behaviour<MemoryStore>,
    rendezvous: rendezvous::server::Behaviour,
    /// Circuit-relay v2 server: this is the traffic relay NAT-traversed peers
    /// dial *through* (matched via `/p2p/<this>/p2p-circuit` addresses).
    relay: relay::Behaviour,
    /// Matchmaking RPC: clients ask for queueing/registration, the server
    /// *pushes* `GatewayRequest::MatchFound` when a pairing succeeds.
    gateway: cbor::Behaviour<GatewayRequest, GatewayResponse>,
}

/// Per-player lobby state (in-memory mirror of the SQLite ratings table).
#[derive(Default)]
struct PlayerEntry {
    username: Option<String>,
    rating: i32,
    /// Highest proof sequence this player has been certified with; used to
    /// reject replayed (older, possibly higher) proofs.
    last_seq: u64,
    /// Gateway base addresses (no `/p2p-circuit` suffix) usable to build a
    /// relayed route to this player, from that player's own vantage.
    base_addresses: Vec<Multiaddr>,
}

/// Canonical identity of a match: the two players (deterministically sorted)
/// plus each player's final score. Both players report the same match from
/// opposite perspectives; canonicalizing collapses those two messages onto one
/// identity so the gateway stores (and later revokes) a single match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct MatchCanon {
    pa: PeerId,
    pb: PeerId,
    score_a: u32,
    score_b: u32,
}

/// A match reported by one side but not yet confirmed by the other (or rebuilt
/// from SQLite after a gateway restart). The SQLite row is the authoritative
/// copy for revocation; the extra fields here document the outcome for the
/// short window before the opponent confirms.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct PendingMatch {
    canon: MatchCanon,
    username_a: String,
    username_b: String,
    before_a: i32,
    before_b: i32,
    after_a: i32,
    after_b: i32,
    reported: [bool; 2],
}

/// All mutable matching/lobby state, kept in one place so the loop, the HTTP
/// moderation handler and matchmaking share a single view.
#[derive(Default)]
struct Lobby {
    players: HashMap<PeerId, PlayerEntry>,
    queue: Vec<PeerId>,
    /// Open (fully-not-confirmed) matches keyed by the gateway-assigned id.
    open: HashMap<u64, PendingMatch>,
    /// Canonical identity → open match id, so a second (or legacy id-less)
    /// report merges with the first.
    by_canon: HashMap<MatchCanon, u64>,
    /// Next gateway-assigned match id (kept above every stored row's id).
    next_id: u64,
}

/// Requests the HTTP monitor/moderation page makes of the main loop (the only
/// place allowed to touch the swarm and the SQLite handle).
enum HttpCmd {
    List(tokio::sync::oneshot::Sender<Result<String, String>>),
    Revoke {
        id: u64,
        resp: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    Edit {
        id: u64,
        score_a: u32,
        score_b: u32,
        resp: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
}

fn main() {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("could not start tokio runtime: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = runtime.block_on(run()) {
        eprintln!("gateway error: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut listen_addr = DEFAULT_LISTEN.parse::<Multiaddr>()?;
    let mut public_host = DEFAULT_HOST_FALLBACK.to_string();
    let mut db_path = PathBuf::from("gateway.sqlite");
    let mut key_path = PathBuf::from("gateway.key");
    let mut http_port = DEFAULT_HTTP_PORT;
    let mut add_mod: Option<String> = None;
    let mut remove_mod: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen_addr = args.next().ok_or("--listen needs a value")?.parse()?,
            "--public" => public_host = args.next().ok_or("--public needs a value")?,
            "--db" => db_path = args.next().ok_or("--db needs a value")?.into(),
            "--key" => key_path = args.next().ok_or("--key needs a value")?.into(),
            "--http" => http_port = args.next().ok_or("--http needs a value")?.parse()?,
            "--add-moderator" => add_mod = Some(args.next().ok_or("--add-moderator needs a value")?),
            "--remove-moderator" => {
                remove_mod = Some(args.next().ok_or("--remove-moderator needs a value")?)
            }
            "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            other => return Err(format!("unknown argument '{other}'\n\n{USAGE}").into()),
        }
    }

    let keypair = load_or_create_key(&key_path)?;
    let gateway_peer = keypair.public().to_peer_id();

    println!("ponged-gateway starting");
    println!("  peer id : {gateway_peer}");
    println!("  identity: {key_path:?}");
    println!("  db      : {db_path:?} (SQLite ratings)");
    println!("  listen  : {listen_addr}");
    println!("  public  : {public_host}");

    let mut rating_db = open_ratings(&db_path)?;

    // One-shot moderator administration: create/delete an account and exit.
    if let Some(user) = add_mod {
        let password = read_password(&user)?;
        set_moderator(&rating_db, &user, &password)?;
        let total = moderator_count(&rating_db);
        println!("moderador '{user}' guardado ({total} en total)");
        return Ok(());
    }
    if let Some(user) = remove_mod {
        match remove_moderator(&rating_db, &user)? {
            true => println!("moderador '{user}' eliminado"),
            false => eprintln!("no existe ningún moderador llamado '{user}'"),
        }
        return Ok(());
    }

    let mods = moderator_count(&rating_db);
    if mods == 0 {
        println!("  aviso   : NO hay moderadores registrados — la página de moderación");
        println!("            queda bloqueada. Crea uno con: --add-moderator <usuario>");
    }

    if http_port != 0 {
        println!("  http    : http://0.0.0.0:{http_port}/ (monitor + moderación, requiere login)");
    }

    // The signing key certifies every rating we issue; clients keep the proofs
    // and re-present them, so losing the DB never costs a player their ELO.
    let signing_key = keypair.clone();

    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(
            |keypair| -> Result<Behaviour, Box<dyn std::error::Error + Send + Sync>> {
                let peer_id = keypair.public().to_peer_id();
                let mut behaviour = Behaviour {
                    ping: ping::Behaviour::default(),
                    identify: identify::Behaviour::new(identify::Config::new(
                        GATEWAY_AGENT_VERSION.to_string(),
                        keypair.public(),
                    )),
                    kademlia: kad::Behaviour::new(peer_id, MemoryStore::new(peer_id)),
                    rendezvous: rendezvous::server::Behaviour::new(
                        rendezvous::server::Config::default(),
                    ),
                    relay: relay::Behaviour::new(peer_id, relay::Config::default()),
                    gateway: cbor::Behaviour::new(
                        [(GATEWAY_PROTOCOL, ProtocolSupport::Full)],
                        request_response::Config::default(),
                    ),
                };
                // The gateway is a pure DHT *server*: it stores records but runs
                // no client queries of its own.
                behaviour.kademlia.set_mode(Some(kad::Mode::Server));
                Ok(behaviour)
            },
        )?
        // Keep server connections alive (ping keeps them warm); default idle
        // timeouts would drop clients between pings.
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();

    let default_base: Multiaddr =
        format!("/ip4/{public_host}/tcp/{}", port_of(&listen_addr)?).parse()?;

    // The relay server builds the reservation responses (and circuit routes)
    // from its external addresses; with none advertised, relay clients reject
    // the reservation as `NoAddressesInReservation` and never register.
    swarm.add_external_address(default_base.clone());
    println!("  external: {default_base}");

    swarm.listen_on(listen_addr)?;

    // Everything the loop touches lives in one `Lobby`; the HTTP moderation
    // page talks to it through a command channel (only the loop may use the
    // swarm and the SQLite connection).
    let mut lobby: Lobby = Lobby::default();
    let (http_tx, mut http_rx) = tokio::sync::mpsc::unbounded_channel::<HttpCmd>();
    // In-memory moderator sessions for the HTTP page (the account hashes live
    // in SQLite; the HTTP server opens a read-only view of them per login).
    let auth = Auth::new();

    if http_port != 0 {
        let bind: SocketAddr = format!("0.0.0.0:{http_port}").parse()?;
        match TcpListener::bind(bind).await {
            Ok(listener) => {
                println!("  http    : http://{bind}/ (monitor + moderación)");
                let tx = http_tx.clone();
                let db = db_path.clone();
                let auth = auth.clone();
                tokio::task::spawn(async move {
                    run_http_server(listener, tx, db, auth).await;
                });
            }
            Err(e) => {
                eprintln!("  http    : could not bind {bind}: {e} (monitor disabled)");
            }
        }
    }

    let mut matchmaking = tokio::time::interval(QUEUE_INTERVAL);
    matchmaking.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = matchmaking.tick() => {
                run_matchmaking(&mut swarm, &gateway_peer, &mut lobby, &rating_db, &default_base);
            }
            Some(cmd) = http_rx.recv() => {
                on_http_command(&mut swarm, &mut lobby, &mut rating_db, &signing_key, &gateway_peer, cmd);
            }
            Some(event) = swarm.next() => match event {
                SwarmEvent::NewListenAddr { address, .. } => {
                    info_listening(&address);
                }
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    println!("client connected: {peer_id}");
                    let (stored_rating, stored_seq) =
                        load_rating(&rating_db, &peer_id).unwrap_or((START_RATING, 0));
                    let entry = lobby.players.entry(peer_id).or_insert_with(|| PlayerEntry {
                        rating: stored_rating,
                        last_seq: stored_seq,
                        ..Default::default()
                    });
                    // We don't know the client's path to us yet; seed with the
                    // default advertised base. `identify` refines it below.
                    if !entry.base_addresses.contains(&default_base) {
                        entry.base_addresses.push(default_base.clone());
                    }
                }
                SwarmEvent::ConnectionClosed { peer_id, num_established, .. } => {
                    if num_established == 0 {
                        lobby.queue.retain(|p| *p != peer_id);
                        lobby.players.remove(&peer_id);
                        println!("client disconnected: {peer_id}");
                    }
                }
                SwarmEvent::Behaviour(behaviour_event) => match behaviour_event {
                    BehaviourEvent::Identify(event) => {
                        if let identify::Event::Received { peer_id, info, .. } = event {
                            // The client's observed view of the gateway is the
                            // best (and often only) public route to use when
                            // building a relayed address *for that client*.
                            let entry = lobby.players.entry(peer_id).or_default();
                            if !entry.base_addresses.contains(&info.observed_addr) {
                                entry.base_addresses.push(info.observed_addr);
                            }
                            if entry.rating == 0 {
                                let (rating, seq) =
                                    load_rating(&rating_db, &peer_id).unwrap_or((START_RATING, 0));
                                entry.rating = rating;
                                entry.last_seq = entry.last_seq.max(seq);
                            }
                        }
                    }
                    BehaviourEvent::Gateway(request_response::Event::Message {
                        peer,
                        message: request_response::Message::Request { request, channel, .. },
                        ..
                    }) => {
                        on_gateway_request(
                            &mut swarm,
                            &mut lobby,
                            &mut rating_db,
                            &signing_key,
                            &gateway_peer,
                            peer,
                            request,
                            channel,
                        );
                    }
                    BehaviourEvent::Gateway(request_response::Event::Message {
                        peer,
                        message: request_response::Message::Response { response, .. },
                        ..
                    }) => {
                        if response != GatewayResponse::Pong {
                            println!("client {peer} replied: {response:?}");
                        }
                    }
                    BehaviourEvent::Gateway(request_response::Event::OutboundFailure { peer, error, .. }) => {
                        println!("matchmaking message to {peer} failed: {error:?}");
                        lobby.queue.retain(|p| *p != peer);
                        lobby.players.remove(&peer);
                    }
                    BehaviourEvent::Gateway(other) => {
                        let _ = other; // InboundFailure / ResponseSent: nothing to do.
                    }
                    BehaviourEvent::Kademlia(event) => {
                        let _ = event; // DHT server bookkeeping, nothing to do.
                    }
                    BehaviourEvent::Rendezvous(event) => {
                        let _ = event;
                    }
                    BehaviourEvent::Relay(event) => {
                        println!("relay event: {event:?}");
                    }
                    BehaviourEvent::Ping(event) => {
                        if let libp2p::ping::Event { peer, result: Err(e), .. } = event {
                            println!("ping failed for {peer}: {e:?}");
                        }
                    }
                },
                other => {
                    let _ = other;
                }
            },
        }
    }
}

/// Loads (or creates) the gateway's persistent Ed25519 identity so its
/// `PeerId` is stable across restarts.
fn load_or_create_key(path: &PathBuf) -> Result<Keypair, Box<dyn std::error::Error + Send + Sync>> {
    if let Ok(bytes) = std::fs::read(path) {
        let mut bytes = bytes;
        if let Ok(keypair) = Keypair::ed25519_from_bytes(bytes.as_mut_slice()) {
            return Ok(keypair);
        }
        println!("{path:?} is not a valid ed25519 key, generating a new one");
    }
    let keypair = Keypair::generate_ed25519();
    if let Ok(ed) = keypair.clone().try_into_ed25519() {
        std::fs::write(path, ed.secret().as_ref())?;
    }
    Ok(keypair)
}

/// Extracts the `/tcp/<port>` portion of a listen address (0.0.0.0 is fine).
fn port_of(addr: &Multiaddr) -> Result<u16, Box<dyn std::error::Error + Send + Sync>> {
    for protocol in addr.iter() {
        if let Protocol::Tcp(port) = protocol {
            return Ok(port);
        }
    }
    Err("listen address has no tcp component".into())
}

fn info_listening(addr: &Multiaddr) {
    if addr.iter().any(|p| matches!(p, Protocol::P2pCircuit)) {
        return;
    }
    println!("   listening on {addr}");
}

/// Opens (creating tables on first boot) the SQLite ratings database.
fn open_ratings(path: &PathBuf) -> Result<Connection, Box<dyn std::error::Error + Send + Sync>> {
    let conn = Connection::open(path)?;
    init_schema(&conn)?;
    Ok(conn)
}

// ---------------------------------------------------------------------------
// Moderator accounts (who may log into the HTTP moderation page).
// ---------------------------------------------------------------------------

/// Creates a moderator account (or resets its password). The password is
/// never stored: only a salted, iterated SHA-256 hash.
fn set_moderator(conn: &Connection, username: &str, password: &str) -> Result<(), String> {
    let user = username.trim().to_ascii_lowercase();
    if user.is_empty() {
        return Err("el nombre de moderador no puede estar vacío".into());
    }
    if password.is_empty() {
        return Err("la contraseña no puede estar vacía".into());
    }
    let mut salt = [0u8; 16];
    rand::rng().fill(&mut salt);
    let pass_hash = hash_password(&salt, password);
    conn.execute(
        "INSERT INTO moderators (username, salt, pass_hash, created_at)
         VALUES (?1, ?2, ?3, datetime('now'))
         ON CONFLICT(username) DO UPDATE
           SET salt = excluded.salt, pass_hash = excluded.pass_hash",
        params![user, salt.as_slice(), pass_hash.as_slice()],
    )
    .map_err(|e| format!("no se pudo guardar el moderador: {e}"))?;
    Ok(())
}

fn remove_moderator(conn: &Connection, username: &str) -> Result<bool, String> {
    let user = username.trim().to_ascii_lowercase();
    let removed = conn
        .execute("DELETE FROM moderators WHERE username = ?1", params![user])
        .map_err(|e| format!("no se pudo eliminar el moderador: {e}"))?;
    Ok(removed > 0)
}

/// Verifies a username/password pair against the stored salted hash.
fn verify_moderator(conn: &Connection, username: &str, password: &str) -> bool {
    let user = username.trim().to_ascii_lowercase();
    let Ok(mut stmt) =
        conn.prepare("SELECT salt, pass_hash FROM moderators WHERE username = ?1")
    else {
        return false;
    };
    let Ok(mut rows) = stmt.query_map(params![user], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
    }) else {
        return false;
    };
    let Some(Ok((salt, stored))) = rows.next() else {
        return false;
    };
    let computed = hash_password(&salt, password);
    ct_eq(&computed, &stored)
}

fn moderator_count(conn: &Connection) -> usize {
    conn.query_row("SELECT COUNT(*) FROM moderators", [], |row| row.get(0))
        .unwrap_or(0)
}

/// Reads the new moderator's password from stdin (kept out of the command
/// line, argv is visible to other processes).
fn read_password(username: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    println!("Contraseña para el moderador '{username}' (se lee por stdin):");
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let password = line.trim_end_matches(['\r', '\n']).to_string();
    if password.is_empty() {
        return Err("la contraseña no puede estar vacía".into());
    }
    Ok(password)
}

/// Salted, iterated SHA-256 hash of a password (PBKDF-like, no extra deps).
fn hash_password(salt: &[u8], password: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(salt);
    h.update(password.as_bytes());
    let mut out: Output<Sha256> = h.finalize();
    let mut digest = [0u8; 32];
    for _ in 1..PASSWORD_ITERATIONS {
        out = Sha256::digest(out.as_slice());
    }
    digest.copy_from_slice(&out);
    digest
}

/// Constant-time byte comparison (avoids leaking hash equality via timing).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Creates/migrates the gateway's tables: the `players` ratings ledger and the
/// `matches` log that the moderation page reads and revokes.
fn init_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS players (
            peer_id   TEXT PRIMARY KEY,
            username  TEXT NOT NULL,
            rating    INTEGER NOT NULL DEFAULT 800,
            rating_seq INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS matches (
            id               INTEGER PRIMARY KEY,
            created_at       TEXT    NOT NULL,
            player_a         TEXT    NOT NULL,
            username_a       TEXT    NOT NULL DEFAULT '',
            rating_a_before  INTEGER NOT NULL,
            rating_a_after   INTEGER NOT NULL,
            score_a          INTEGER NOT NULL DEFAULT 0,
            reported_a       INTEGER NOT NULL DEFAULT 0,
            player_b         TEXT    NOT NULL,
            username_b       TEXT    NOT NULL DEFAULT '',
            rating_b_before  INTEGER NOT NULL,
            rating_b_after   INTEGER NOT NULL,
            score_b          INTEGER NOT NULL DEFAULT 0,
            reported_b       INTEGER NOT NULL DEFAULT 0,
            status           TEXT    NOT NULL DEFAULT 'pending',
            revoked_at       TEXT,
            edited_at        TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_matches_status ON matches(status);
        CREATE TABLE IF NOT EXISTS moderators (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            username   TEXT    NOT NULL UNIQUE,
            salt       BLOB    NOT NULL,
            pass_hash  BLOB    NOT NULL,
            created_at TEXT    NOT NULL
        );",
    )?;
    // Migrate databases created before the proof-sequence column existed.
    let has_seq = conn
        .prepare("SELECT 1 FROM pragma_table_info('players') WHERE name = 'rating_seq'")?
        .query_row([], |_| Ok(()))
        .optional()?
        .is_some();
    if !has_seq {
        conn.execute(
            "ALTER TABLE players ADD COLUMN rating_seq INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    // And before the score-correction timestamp column existed.
    let has_edited = conn
        .prepare("SELECT 1 FROM pragma_table_info('matches') WHERE name = 'edited_at'")?
        .query_row([], |_| Ok(()))
        .optional()?
        .is_some();
    if !has_edited {
        conn.execute("ALTER TABLE matches ADD COLUMN edited_at TEXT", [])?;
    }
    Ok(())
}

fn load_rating(conn: &Connection, peer: &PeerId) -> Option<(i32, u64)> {
    conn.query_row(
        "SELECT rating, rating_seq FROM players WHERE peer_id = ?1",
        params![peer.to_base58()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .ok()
    .flatten()
}

fn upsert_rating(conn: &mut Connection, peer: &PeerId, username: &str, rating: i32, seq: u64) {
    let result = conn.execute(
        "INSERT INTO players (peer_id, username, rating, rating_seq) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(peer_id) DO UPDATE SET
            username = excluded.username,
            rating = excluded.rating,
            rating_seq = excluded.rating_seq",
        params![peer.to_base58(), username, rating, seq],
    );
    if let Err(e) = result {
        println!("rating persist error for {peer}: {e}");
    }
}

/// Signed proof payload: `rating ‖ seq ‖ client-peer-id`. The client's peer
/// id binds the proof to its holder (its identity is a persistent key) so a
/// proof can't be handed to a different player.
fn proof_message(rating: i32, seq: u64, client: &PeerId) -> Vec<u8> {
    let mut msg = Vec::with_capacity(4 + 8 + client.to_bytes().len());
    msg.extend_from_slice(&rating.to_be_bytes());
    msg.extend_from_slice(&seq.to_be_bytes());
    msg.extend_from_slice(&client.to_bytes());
    msg
}

/// Certifies `rating` for `client` at sequence `seq`, or `None` if signing
/// is unavailable (only ever happens for a broken keypair).
fn proof_for(
    key: &Keypair,
    gateway: &PeerId,
    client: &PeerId,
    rating: i32,
    seq: u64,
) -> Option<RatingProof> {
    Some(RatingProof {
        rating,
        seq,
        gateway: gateway.to_base58(),
        signature: key.sign(&proof_message(rating, seq, client)).ok()?,
    })
}

/// Verifies a proof with the local matchmaking keypair. Gateways in one
/// deployment share the same key file, so they can each validate the others'
/// certificates.
fn verify_proof(key: &Keypair, client: &PeerId, proof: &RatingProof) -> bool {
    key.public().verify(
        &proof_message(proof.rating, proof.seq, client),
        &proof.signature,
    )
}

/// Client → gateway RPC handler.
fn on_gateway_request(
    swarm: &mut libp2p::Swarm<Behaviour>,
    lobby: &mut Lobby,
    rating_db: &mut Connection,
    signing_key: &Keypair,
    gateway_peer: &PeerId,
    peer: PeerId,
    request: GatewayRequest,
    channel: ResponseChannel<GatewayResponse>,
) {
    let reply = |swarm: &mut libp2p::Swarm<Behaviour>,
                 channel: ResponseChannel<GatewayResponse>,
                 response: GatewayResponse| {
        let _ = swarm
            .behaviour_mut()
            .gateway
            .send_response(channel, response);
    };

    match request {
        GatewayRequest::Register {
            username,
            proof,
            ..
        } => {
            // The client owns its ELO: it presents a gateway-signed proof
            // (kept locally), so a gateway outage never resets anyone. We
            // trust a valid proof from the highest sequence we've seen;
            // stale or forged proofs fall back to our cached rating, and a
            // brand-new player starts at START_RATING.
            let claimed = proof.as_ref().filter(|p| verify_proof(signing_key, &peer, p));
            let accepted =
                match (claimed, lobby.players.get(&peer).map(|e| (e.rating, e.last_seq))) {
                    (Some(p), Some((cached_rating, cached_seq))) if p.seq <= cached_seq => {
                        (cached_rating, cached_seq)
                    }
                    (Some(p), _) => (p.rating, p.seq),
                    (None, Some((cached_rating, cached_seq))) => (cached_rating, cached_seq),
                    (None, None) => (START_RATING, 0),
                };
            let entry = lobby.players.entry(peer).or_insert_with(|| PlayerEntry {
                username: Some(username.clone()),
                rating: accepted.0,
                last_seq: accepted.1,
                ..Default::default()
            });
            entry.username = Some(username.clone());
            entry.rating = accepted.0;
            entry.last_seq = entry.last_seq.max(accepted.1);
            let seq = entry.last_seq + 1;
            upsert_rating(rating_db, &peer, &username, entry.rating, seq);
            let signed = proof_for(signing_key, gateway_peer, &peer, entry.rating, seq);
            let rank = rank_for_rating(entry.rating);
            println!(
                "{peer} registered as '{username}' (rating {}, rank {rank}, proof seq {seq})",
                entry.rating
            );
            reply(
                swarm,
                channel,
                GatewayResponse::Registered {
                    username,
                    rating: entry.rating,
                    rank: rank.to_string(),
                    proof: signed,
                },
            );
        }
        GatewayRequest::QueueMatch => {
            if !lobby.queue.contains(&peer) {
                lobby.queue.push(peer);
            }
            let position = lobby.queue.iter().position(|p| *p == peer).unwrap_or(0) as u32;
            println!(
                "{peer} queued (position {position}, queue size {})",
                lobby.queue.len()
            );
            reply(swarm, channel, GatewayResponse::Queued { position });
        }
        GatewayRequest::LeaveQueue => {
            lobby.queue.retain(|p| *p != peer);
            println!("{peer} left the queue");
            reply(swarm, channel, GatewayResponse::Dequeued);
        }
        GatewayRequest::ReportResult {
            match_id,
            opponent,
            my_score,
            opponent_score,
        } => {
            let (new_rating, proof) = report_result(
                lobby,
                rating_db,
                signing_key,
                gateway_peer,
                peer,
                opponent.clone(),
                match_id,
                my_score,
                opponent_score,
            );
            let rank = rank_for_rating(new_rating);
            println!(
                "{peer} reported match #{match_id} ({my_score}-{opponent_score} vs {opponent}) → ELO {new_rating} (rank {rank})"
            );
            reply(
                swarm,
                channel,
                GatewayResponse::Rating {
                    rating: new_rating,
                    rank: rank.to_string(),
                    proof,
                },
            );
        }
        GatewayRequest::Ping => {
            reply(swarm, channel, GatewayResponse::Pong);
        }
        GatewayRequest::MatchFound { .. } => {
            reply(
                swarm,
                channel,
                GatewayResponse::Error("MatchFound is server→client only".into()),
            );
        }
        GatewayRequest::MatchRevoked { .. } => {
            reply(
                swarm,
                channel,
                GatewayResponse::Error("MatchRevoked is server→client only".into()),
            );
        }
        GatewayRequest::MatchCorrected { .. } => {
            reply(
                swarm,
                channel,
                GatewayResponse::Error("MatchCorrected is server→client only".into()),
            );
        }
    }
}

/// Records a finished-match report, merging the two complementary reports
/// (one per player) into a single stored match. Returns the reporter's new
/// ELO and a freshly-signed proof for it.
fn report_result(
    lobby: &mut Lobby,
    rating_db: &mut Connection,
    signing_key: &Keypair,
    gateway_peer: &PeerId,
    reporter: PeerId,
    opponent: String,
    match_id: u64,
    my_score: u32,
    opponent_score: u32,
) -> (i32, Option<RatingProof>) {
    let Ok(opp) = opponent.parse::<PeerId>() else {
        return current_rating_reply(lobby, rating_db, signing_key, gateway_peer, &reporter);
    };

    // Canonicalize so both perspectives map onto one match identity.
    let (pa, pb) = if reporter < opp { (reporter, opp) } else { (opp, reporter) };
    let reporter_idx = if pa == reporter { 0 } else { 1 };
    let (score_a, score_b) = if reporter_idx == 0 {
        (my_score, opponent_score)
    } else {
        (opponent_score, my_score)
    };
    let canon = MatchCanon {
        pa,
        pb,
        score_a,
        score_b,
    };

    // Which match does this report belong to? Prefer the id the gateway
    // assigned at pairing time; fall back to a canonical match already open,
    // a pending row recovered from SQLite (gateway restarted mid-match), or a
    // fresh id.
    let id = {
        if let Some(id) = lobby.by_canon.get(&canon).copied() {
            id
        } else if match_id != 0 && lobby.open.get(&match_id).map(|m| m.canon) == Some(canon) {
            match_id
        } else if match_id != 0 {
            match pending_by_id(rating_db, match_id, pa, pb) {
                Some(m) => {
                    lobby.by_canon.insert(m.canon, match_id);
                    lobby.open.insert(match_id, m);
                    match_id
                }
                None if rated_match_exists(rating_db, match_id) => {
                    // A duplicate report of a match that was already counted.
                    return current_rating_reply(lobby, rating_db, signing_key, gateway_peer, &reporter);
                }
                None => {
                    let id = alloc_match_id(lobby, rating_db);
                    lobby.by_canon.insert(canon, id);
                    id
                }
            }
        } else {
            let id = alloc_match_id(lobby, rating_db);
            lobby.by_canon.insert(canon, id);
            id
        }
    };

    // Already open: merge the second report (or ignore a repeat) — ratings
    // were already moved by the first report, so this is purely confirmation.
    if let Some(entry) = lobby.open.get_mut(&id) {
        if entry.reported[reporter_idx] {
            return current_rating_reply(lobby, rating_db, signing_key, gateway_peer, &reporter);
        }
        entry.reported[reporter_idx] = true;
        let _ = rating_db.execute(
            "UPDATE matches SET reported_a = ?1, reported_b = ?2 WHERE id = ?3",
            params![i64::from(entry.reported[0]), i64::from(entry.reported[1]), id],
        );
        if entry.reported[0] && entry.reported[1] {
            let _ = rating_db.execute(
                "UPDATE matches SET status = 'played' WHERE id = ?1",
                params![id],
            );
            lobby.open.remove(&id);
            lobby.by_canon.remove(&canon);
            println!("match #{id} confirmed by both players → played");
        }
        return current_rating_reply(lobby, rating_db, signing_key, gateway_peer, &reporter);
    }

    // First report for this pairing: compute and apply the ELO move once,
    // then store the match as pending until the opponent confirms.
    let before_a = current_rating(lobby, rating_db, &pa);
    let before_b = current_rating(lobby, rating_db, &pb);
    let score = match score_a.cmp(&score_b) {
        std::cmp::Ordering::Greater => 1.0,
        std::cmp::Ordering::Less => 0.0,
        std::cmp::Ordering::Equal => 0.5,
    };
    let (after_a, after_b) = elo(before_a, before_b, score);
    let username_a = username_of(lobby, rating_db, &pa);
    let username_b = username_of(lobby, rating_db, &pb);
    let seq_a = set_rating(lobby, rating_db, pa, &username_a, after_a);
    let seq_b = set_rating(lobby, rating_db, pb, &username_b, after_b);
    let reporter_seq = if reporter_idx == 0 { seq_a } else { seq_b };
    let reported = [reporter_idx == 0, reporter_idx == 1];
    let _ = rating_db.execute(
        "INSERT INTO matches
            (id, created_at, player_a, username_a, rating_a_before, rating_a_after,
             score_a, reported_a, player_b, username_b, rating_b_before, rating_b_after,
             score_b, reported_b, status)
         VALUES
            (?1, datetime('now'), ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 'pending')",
        params![
            id,
            pa.to_base58(),
            username_a,
            before_a,
            after_a,
            score_a,
            i64::from(reported[0]),
            pb.to_base58(),
            username_b,
            before_b,
            after_b,
            score_b,
            i64::from(reported[1]),
        ],
    );
    lobby.open.insert(
        id,
        PendingMatch {
            canon,
            username_a,
            username_b,
            before_a,
            before_b,
            after_a,
            after_b,
            reported,
        },
    );
    let new_rating = if reporter_idx == 0 { after_a } else { after_b };
    let proof = proof_for(signing_key, gateway_peer, &reporter, new_rating, reporter_seq);
    (new_rating, proof)
}

fn alloc_match_id(lobby: &mut Lobby, rating_db: &Connection) -> u64 {
    let max = rating_db
        .query_row("SELECT COALESCE(MAX(id), 0) FROM matches", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap_or(0)
        .max(0) as u64;
    lobby.next_id = lobby.next_id.max(max + 1);
    let id = lobby.next_id;
    lobby.next_id += 1;
    id
}

fn current_rating(lobby: &Lobby, rating_db: &Connection, peer: &PeerId) -> i32 {
    lobby
        .players
        .get(peer)
        .map(|e| e.rating)
        .or_else(|| load_rating(rating_db, peer).map(|(r, _)| r))
        .unwrap_or(START_RATING)
}

fn seq_for(lobby: &Lobby, rating_db: &Connection, peer: &PeerId) -> u64 {
    if let Some(e) = lobby.players.get(peer) {
        e.last_seq + 1
    } else {
        load_rating(rating_db, peer).map(|(_, s)| s).unwrap_or(0) + 1
    }
}

fn username_of(lobby: &Lobby, rating_db: &Connection, peer: &PeerId) -> String {
    if let Some(u) = lobby.players.get(peer).and_then(|e| e.username.clone()) {
        return u;
    }
    rating_db
        .query_row(
            "SELECT username FROM players WHERE peer_id = ?1",
            params![peer.to_base58()],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Moves a player to `after` (bumping its proof sequence) both in memory and in
/// SQLite; returns the proof sequence used.
fn set_rating(lobby: &mut Lobby, rating_db: &mut Connection, peer: PeerId, username: &str, after: i32) -> u64 {
    let seq = seq_for(lobby, rating_db, &peer);
    if let Some(entry) = lobby.players.get_mut(&peer) {
        entry.rating = after;
        entry.last_seq = seq;
    }
    upsert_rating(rating_db, &peer, username, after, seq);
    seq
}

fn current_rating_reply(
    lobby: &Lobby,
    rating_db: &Connection,
    signing_key: &Keypair,
    gateway_peer: &PeerId,
    reporter: &PeerId,
) -> (i32, Option<RatingProof>) {
    let rating = current_rating(lobby, rating_db, reporter);
    let seq = seq_for(lobby, rating_db, reporter);
    (rating, proof_for(signing_key, gateway_peer, reporter, rating, seq))
}

/// Rebuilds a pending match from SQLite (a gateway restart between the two
/// complementary reports is survived this way).
fn pending_by_id(rating_db: &Connection, id: u64, pa: PeerId, pb: PeerId) -> Option<PendingMatch> {
    let row = rating_db
        .query_row(
            "SELECT username_a, rating_a_before, rating_a_after, score_a, reported_a,
                    username_b, rating_b_before, rating_b_after, score_b, reported_b
             FROM matches WHERE id = ?1 AND status = 'pending'",
            params![id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i32>(1)?,
                    r.get::<_, i32>(2)?,
                    r.get::<_, u32>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, i32>(6)?,
                    r.get::<_, i32>(7)?,
                    r.get::<_, u32>(8)?,
                    r.get::<_, i64>(9)?,
                ))
            },
        )
        .optional()
        .ok()
        .flatten()?;
    let reported_a = row.4 != 0;
    let reported_b = row.9 != 0;
    Some(PendingMatch {
        canon: MatchCanon {
            pa,
            pb,
            score_a: row.3,
            score_b: row.8,
        },
        username_a: row.0,
        username_b: row.5,
        before_a: row.1,
        before_b: row.6,
        after_a: row.2,
        after_b: row.7,
        reported: [reported_a, reported_b],
    })
}

fn rated_match_exists(rating_db: &Connection, id: u64) -> bool {
    rating_db
        .query_row("SELECT 1 FROM matches WHERE id = ?1", params![id], |_| Ok(()))
        .optional()
        .ok()
        .flatten()
        .is_some()
}

/// A player whose rating a revocation restored, ready to be notified.
struct RevokedPlayer {
    peer: PeerId,
    before: i32,
    proof: Option<RatingProof>,
}

/// A player whose rating a score correction recomputed, ready to be notified
/// (scores are per-player, from each player's own perspective).
struct CorrectedPlayer {
    peer: PeerId,
    rating: i32,
    proof: Option<RatingProof>,
    my_score: u32,
    opponent_score: u32,
}

/// Revokes match `id`: rolls both players' ELO back to the rating they had
/// before it, marks the match `revoked`, and returns the players to notify.
/// The actual `MatchRevoked` push is sent by the caller (which owns the swarm).
fn revoke_match_state(
    lobby: &mut Lobby,
    rating_db: &mut Connection,
    signing_key: &Keypair,
    gateway_peer: &PeerId,
    id: u64,
) -> Result<Vec<RevokedPlayer>, String> {
    let (player_a, username_a, before_a, player_b, username_b, before_b, status): (
        String,
        String,
        i32,
        String,
        String,
        i32,
        String,
    ) = rating_db
        .query_row(
            "SELECT player_a, username_a, rating_a_before,
                    player_b, username_b, rating_b_before, status
             FROM matches WHERE id = ?1",
            params![id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .optional()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("la partida #{id} no existe"))?;

    if status == "revoked" {
        return Err(format!("la partida #{id} ya fue revocada"));
    }
    let (Ok(a), Ok(b)) = (player_a.parse::<PeerId>(), player_b.parse::<PeerId>()) else {
        return Err(format!("la partida #{id} tiene participantes inválidos"));
    };

    // A pending (not-yet-confirmed) match stops being open once revoked.
    lobby.open.remove(&id);
    lobby.by_canon.retain(|_, v| *v != id);

    let seq_a = set_rating(lobby, rating_db, a, &username_a, before_a);
    let seq_b = set_rating(lobby, rating_db, b, &username_b, before_b);
    let _ = rating_db.execute(
        "UPDATE matches SET status = 'revoked', revoked_at = datetime('now') WHERE id = ?1",
        params![id],
    );
    println!(
        "match #{id} REVOKED: {a} → ELO {before_a}, {b} → ELO {before_b}"
    );

    Ok(vec![
        RevokedPlayer {
            peer: a,
            before: before_a,
            proof: proof_for(signing_key, gateway_peer, &a, before_a, seq_a),
        },
        RevokedPlayer {
            peer: b,
            before: before_b,
            proof: proof_for(signing_key, gateway_peer, &b, before_b, seq_b),
        },
    ])
}

/// Corrects the final score of match `id` (moderator action): the outcome is
/// recomputed from the corrected scores against the ratings both players had
/// *before* the match, so the ELO delta reflects the honest result. Returns
/// the two corrected players (with their perspective scores) to notify.
fn edit_match_state(
    lobby: &mut Lobby,
    rating_db: &mut Connection,
    signing_key: &Keypair,
    gateway_peer: &PeerId,
    id: u64,
    new_score_a: u32,
    new_score_b: u32,
) -> Result<Vec<CorrectedPlayer>, String> {
    if new_score_a > MAX_SCORE || new_score_b > MAX_SCORE {
        return Err(format!("marcador inválido (permitido 0..={MAX_SCORE})"));
    }
    let (player_a, username_a, before_a, player_b, username_b, before_b, status): (
        String,
        String,
        i32,
        String,
        String,
        i32,
        String,
    ) = rating_db
        .query_row(
            "SELECT player_a, username_a, rating_a_before,
                    player_b, username_b, rating_b_before, status
             FROM matches WHERE id = ?1",
            params![id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .optional()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("la partida #{id} no existe"))?;

    if status == "revoked" {
        return Err(format!("la partida #{id} ya fue revocada"));
    }
    let (Ok(a), Ok(b)) = (player_a.parse::<PeerId>(), player_b.parse::<PeerId>()) else {
        return Err(format!("la partida #{id} tiene participantes inválidos"));
    };

    // Recompute the ELO move from the corrected outcome, anchored on the
    // ratings the players had before the match (same baseline as the report).
    let outcome = match new_score_a.cmp(&new_score_b) {
        std::cmp::Ordering::Greater => 1.0,
        std::cmp::Ordering::Less => 0.0,
        std::cmp::Ordering::Equal => 0.5,
    };
    let (after_a, after_b) = elo(before_a, before_b, outcome);
    let seq_a = set_rating(lobby, rating_db, a, &username_a, after_a);
    let seq_b = set_rating(lobby, rating_db, b, &username_b, after_b);

    // The match is no longer pending-open under its old identity; a second
    // report now merges by id (it can never re-apply the old ELO move).
    lobby.open.remove(&id);
    lobby.by_canon.retain(|_, v| *v != id);

    let _ = rating_db.execute(
        "UPDATE matches
            SET score_a = ?1, score_b = ?2,
                rating_a_after = ?3, rating_b_after = ?4,
                edited_at = datetime('now')
          WHERE id = ?5",
        params![new_score_a, new_score_b, after_a, after_b, id],
    );
    println!("match #{id} score CORRECTED: {a} {after_a} ELO, {b} {after_b} ELO ({new_score_a}-{new_score_b})");

    Ok(vec![
        CorrectedPlayer {
            peer: a,
            rating: after_a,
            proof: proof_for(signing_key, gateway_peer, &a, after_a, seq_a),
            my_score: new_score_a,
            opponent_score: new_score_b,
        },
        CorrectedPlayer {
            peer: b,
            rating: after_b,
            proof: proof_for(signing_key, gateway_peer, &b, after_b, seq_b),
            my_score: new_score_b,
            opponent_score: new_score_a,
        },
    ])
}

/// Routes a moderation-page command from the HTTP server to the loop state.
fn on_http_command(
    swarm: &mut libp2p::Swarm<Behaviour>,
    lobby: &mut Lobby,
    rating_db: &mut Connection,
    signing_key: &Keypair,
    gateway_peer: &PeerId,
    cmd: HttpCmd,
) {
    match cmd {
        HttpCmd::List(resp) => {
            let body = list_matches_json(rating_db);
            let _ = resp.send(Ok(body));
        }
        HttpCmd::Revoke { id, resp } => {
            let result = revoke_match_state(lobby, rating_db, signing_key, gateway_peer, id).map(
                |players| {
                    // Tell the clients their match was revoked so they drop the
                    // result, restore their local rating and re-sign-track it.
                    for p in &players {
                        if lobby.players.contains_key(&p.peer) {
                            swarm.behaviour_mut().gateway.send_request(
                                &p.peer,
                                GatewayRequest::MatchRevoked {
                                    match_id: id,
                                    rating: p.before,
                                    proof: p.proof.clone(),
                                },
                            );
                        }
                    }
                    json!({ "ok": true, "match_id": id }).to_string()
                },
            );
            let _ = resp.send(result);
        }
        HttpCmd::Edit {
            id,
            score_a,
            score_b,
            resp,
        } => {
            let result = edit_match_state(lobby, rating_db, signing_key, gateway_peer, id, score_a, score_b)
                .map(|players| {
                    // Push the corrected score + rating to any connected client;
                    // offline ones pick it up at their next Register (the cached
                    // seq is now higher than the stale proof they carry).
                    for p in &players {
                        if lobby.players.contains_key(&p.peer) {
                            swarm.behaviour_mut().gateway.send_request(
                                &p.peer,
                                GatewayRequest::MatchCorrected {
                                    match_id: id,
                                    my_score: p.my_score,
                                    opponent_score: p.opponent_score,
                                    rating: p.rating,
                                    proof: p.proof.clone(),
                                },
                            );
                        }
                    }
                    json!({ "ok": true, "match_id": id, "score_a": score_a, "score_b": score_b })
                        .to_string()
                });
            let _ = resp.send(result);
        }
    }
}

/// JSON feed for the moderation page.
fn list_matches_json(conn: &Connection) -> String {
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, created_at, player_a, username_a, rating_a_before, rating_a_after,
                score_a, reported_a, player_b, username_b, rating_b_before, rating_b_after,
                score_b, reported_b, status, revoked_at, edited_at
         FROM matches ORDER BY id DESC LIMIT 300",
    ) else {
        return "{\"matches\":[]}".to_string();
    };
    let rows = stmt.query_map([], |row| {
        Ok(json!({
            "id": row.get::<_, i64>(0)?,
            "created_at": row.get::<_, String>(1)?,
            "player_a": row.get::<_, String>(2)?,
            "username_a": row.get::<_, String>(3)?,
            "rating_a_before": row.get::<_, i32>(4)?,
            "rating_a_after": row.get::<_, i32>(5)?,
            "score_a": row.get::<_, i32>(6)?,
            "reported_a": row.get::<_, i64>(7)? != 0,
            "player_b": row.get::<_, String>(8)?,
            "username_b": row.get::<_, String>(9)?,
            "rating_b_before": row.get::<_, i32>(10)?,
            "rating_b_after": row.get::<_, i32>(11)?,
            "score_b": row.get::<_, i32>(12)?,
            "reported_b": row.get::<_, i64>(13)? != 0,
            "status": row.get::<_, String>(14)?,
            "revoked_at": row.get::<_, Option<String>>(15)?,
            "edited_at": row.get::<_, Option<String>>(16)?,
        }))
    });
    match rows {
        Ok(rows) => {
            json!({ "matches": rows.filter_map(Result::ok).collect::<Vec<_>>() }).to_string()
        }
        Err(_) => "{\"matches\":[]}".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tiny HTTP/1.1 server: moderator login + monitor page + JSON API for
// moderation. Everything except `/login` requires a valid moderator session.
// ---------------------------------------------------------------------------

/// In-memory moderator sessions (token → username + sliding expiry). Session
/// tokens are random and kept only in RAM, so a restart logs everyone out.
struct Auth {
    sessions: Mutex<HashMap<String, (String, Instant)>>,
}

impl Auth {
    fn new() -> Arc<Self> {
        Arc::new(Auth {
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// Checks credentials against the SQLite hashes and returns a fresh token.
    fn login(&self, db_path: &Path, username: &str, password: &str) -> Option<String> {
        let user = username.trim().to_ascii_lowercase();
        if user.is_empty() || password.is_empty() {
            return None;
        }
        let conn = Connection::open(db_path).ok()?;
        if !verify_moderator(&conn, &user, password) {
            return None;
        }
        let token = session_token();
        self.sessions
            .lock()
            .unwrap()
            .insert(token.clone(), (user, Instant::now()));
        Some(token)
    }

    /// The moderator's username if `token` is a live session (sliding TTL).
    fn who(&self, token: Option<String>) -> Option<String> {
        let token = token?;
        let mut sessions = self.sessions.lock().unwrap();
        let entry = sessions.get_mut(&token)?;
        if entry.1.elapsed() > Duration::from_secs(SESSION_MAX_AGE_SECS) {
            sessions.remove(&token);
            return None;
        }
        entry.1 = Instant::now();
        Some(entry.0.clone())
    }

    fn logout(&self, token: Option<String>) {
        if let Some(token) = token {
            self.sessions.lock().unwrap().remove(&token);
        }
    }
}

/// 32 random bytes as a hex string — the moderator session token.
fn session_token() -> String {
    let bytes: [u8; 32] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// An HTTP response the tiny server knows how to serialize.
struct HttpResp {
    status: u16,
    content_type: &'static str,
    body: String,
    headers: Vec<(String, String)>,
}

impl HttpResp {
    fn html(status: u16, body: String) -> Self {
        Self {
            status,
            content_type: "text/html; charset=utf-8",
            body,
            headers: Vec::new(),
        }
    }
    fn json(status: u16, body: String) -> Self {
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body,
            headers: Vec::new(),
        }
    }
    fn redirect(location: &str) -> Self {
        Self {
            status: 302,
            content_type: "text/plain; charset=utf-8",
            body: String::new(),
            headers: vec![("Location".to_string(), location.to_string())],
        }
    }
}

async fn run_http_server(
    listener: TcpListener,
    tx: UnboundedSender<HttpCmd>,
    db_path: PathBuf,
    auth: Arc<Auth>,
) {
    loop {
        let (sock, _addr) = match listener.accept().await {
            Ok(socket) => socket,
            Err(_) => continue,
        };
        let tx = tx.clone();
        let db = db_path.clone();
        let auth = auth.clone();
        tokio::task::spawn(async move {
            let mut sock = sock;
            let _ = handle_http_connection(&mut sock, &tx, &db, &auth).await;
        });
    }
}

/// Reads the request head (up to `\r\n\r\n`) plus any `Content-Length` body,
/// then routes it and writes the reply.
async fn handle_http_connection(
    sock: &mut TcpStream,
    tx: &UnboundedSender<HttpCmd>,
    db_path: &Path,
    auth: &Auth,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 65536 {
            return Ok(());
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let content_length: usize = head
        .lines()
        .skip(1)
        .find_map(|line| line.split_once(':').and_then(|(k, v)| {
            k.eq_ignore_ascii_case("content-length").then(|| v.trim())
        }))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut body_bytes = buf[head_end + 4..].to_vec();
    while body_bytes.len() < content_length {
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body_bytes.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&body_bytes[..content_length.min(body_bytes.len())])
        .into_owned();

    let cookie = head
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("cookie").then(|| value.trim())
        })
        .next()
        .and_then(cookie_token);

    let mut lines = head.lines();
    let mut parts = lines.next().unwrap_or("").split_whitespace();
    let method = parts.next().unwrap_or("");
    let raw_path = parts.next().unwrap_or("/");
    let resp = route_request(method, raw_path, cookie, &body, db_path, auth, tx).await;

    let mut response = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close",
        resp.status,
        resp.content_type,
        resp.body.len()
    );
    for (name, value) in &resp.headers {
        response.push_str(&format!("\r\n{name}: {value}"));
    }
    response.push_str("\r\n\r\n");
    response.push_str(&resp.body);
    sock.write_all(response.as_bytes()).await?;
    sock.flush().await
}

/// Extracts this page's session token from a `Cookie` header value.
fn cookie_token(value: &str) -> Option<String> {
    value.split(';').map(str::trim).find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case(COOKIE_NAME)
            .then(|| v.trim().to_string())
    })
}

fn set_cookie_header(token: &str) -> String {
    format!(
        "{COOKIE_NAME}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={SESSION_MAX_AGE_SECS}"
    )
}

fn clear_cookie_header() -> String {
    format!("{COOKIE_NAME}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0")
}

/// Decodes `application/x-www-form-urlencoded` field values enough for the
/// login form (spaces and `%XX` escapes).
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(byte);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn form_field(body: &str, key: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| url_decode(v))
    })
}

async fn route_request(
    method: &str,
    raw_path: &str,
    cookie: Option<String>,
    body: &str,
    db_path: &Path,
    auth: &Auth,
    tx: &UnboundedSender<HttpCmd>,
) -> HttpResp {
    let (path, query) = match raw_path.split_once('?') {
        Some((p, q)) => (p, q),
        None => (raw_path, ""),
    };

    match (method, path) {
        ("GET", "/login") => {
            if auth.who(cookie.clone()).is_some() {
                HttpResp::redirect("/")
            } else {
                HttpResp::html(200, login_page(!query.is_empty()))
            }
        }
        ("GET", "/logout") => {
            auth.logout(cookie);
            let mut resp = HttpResp::redirect("/login");
            resp.headers.push(("Set-Cookie".into(), clear_cookie_header()));
            resp
        }
        ("POST", "/login") => {
            let username = form_field(body, "username").unwrap_or_default();
            let password = form_field(body, "password").unwrap_or_default();
            match auth.login(db_path, &username, &password) {
                Some(token) => {
                    let mut resp = HttpResp::redirect("/");
                    resp.headers
                        .push(("Set-Cookie".into(), set_cookie_header(&token)));
                    resp
                }
                None => HttpResp::redirect("/login?error=1"),
            }
        }
        ("GET", "/") | ("GET", "/index.html") => match auth.who(cookie) {
            Some(user) => HttpResp::html(200, monitor_page(&user)),
            None => HttpResp::redirect("/login"),
        },
        ("GET", "/api/matches") => {
            if auth.who(cookie).is_none() {
                return HttpResp::json(401, err_json("no autenticado"));
            }
            let (code, body) = http_ask(tx, HttpCmd::List).await;
            HttpResp::json(code, body)
        }
        ("POST", _) => {
            if auth.who(cookie).is_none() {
                return HttpResp::json(401, err_json("no autenticado"));
            }
            if let Some(id) = score_path(path) {
                let parse = |field: &str| {
                    form_field(body, field)
                        .and_then(|v| v.parse::<u32>().ok())
                        .ok_or_else(|| err_json(format!("campo '{field}' inválido")))
                };
                return match (parse("score_a"), parse("score_b")) {
                    (Ok(score_a), Ok(score_b)) => {
                        if score_a > MAX_SCORE || score_b > MAX_SCORE {
                            HttpResp::json(
                                400,
                                err_json(format!("marcador inválido (permitido 0..={MAX_SCORE})")),
                            )
                        } else {
                            let (code, body) = http_ask(
                                tx,
                                move |resp| HttpCmd::Edit {
                                    id,
                                    score_a,
                                    score_b,
                                    resp,
                                },
                            )
                            .await;
                            HttpResp::json(code, body)
                        }
                    }
                    (Err(e), _) | (_, Err(e)) => HttpResp::json(400, e),
                };
            }
            match revoke_id(path) {
                Some(id) => {
                    let (code, body) =
                        http_ask(tx, move |resp| HttpCmd::Revoke { id, resp }).await;
                    HttpResp::json(code, body)
                }
                None => HttpResp::json(
                    404,
                    err_json("ruta no encontrada"),
                ),
            }
        }
        _ => HttpResp::json(404, err_json("ruta no encontrada")),
    }
}

/// Sends an HTTP-backed command to the main loop and maps the reply into an
/// HTTP response status + body.
async fn http_ask(
    tx: &UnboundedSender<HttpCmd>,
    make: impl FnOnce(tokio::sync::oneshot::Sender<Result<String, String>>) -> HttpCmd,
) -> (u16, String) {
    let (send, recv) = tokio::sync::oneshot::channel();
    if tx.send(make(send)).is_err() {
        return (503, err_json("gateway ocupado"));
    }
    match tokio::time::timeout(Duration::from_secs(5), recv).await {
        Ok(Ok(Ok(body))) => (200, body),
        Ok(Ok(Err(e))) => (409, err_json(e)),
        _ => (504, err_json("el gateway no respondió")),
    }
}

fn revoke_id(path: &str) -> Option<u64> {
    const PREFIX: &str = "/api/matches/";
    const SUFFIX: &str = "/revoke";
    let rest = path.strip_prefix(PREFIX)?;
    let id = rest.strip_suffix(SUFFIX)?;
    id.parse().ok()
}

/// `/api/matches/<id>/score` → the match id (moderator score correction).
fn score_path(path: &str) -> Option<u64> {
    const PREFIX: &str = "/api/matches/";
    const SUFFIX: &str = "/score";
    let rest = path.strip_prefix(PREFIX)?;
    let id = rest.strip_suffix(SUFFIX)?;
    id.parse().ok()
}

fn err_json(msg: impl Into<String>) -> String {
    json!({ "ok": false, "error": msg.into() }).to_string()
}

/// Self-contained moderation page (loaded from `GET /`). Monitors the match
/// log and lets a moderator revoke any played match; revocation rolls both
/// players' ELO back and pushes `MatchRevoked` to the clients.
const MONITOR_HTML: &str = r#"<!doctype html>
<html lang="es">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Pong Gateway · Monitor de partidas</title>
<style>
  :root { color-scheme: light; }
  * { box-sizing: border-box; }
  body { margin: 0; font-family: ui-monospace, "Cascadia Mono", "DejaVu Sans Mono", Menlo, Consolas, monospace; background: #ffffff; color: #000; }
  header { display: flex; align-items: baseline; gap: 14px; padding: 18px 24px; border-bottom: 1px solid #000; flex-wrap: wrap; }
  h1 { font-size: 20px; margin: 0; letter-spacing: .5px; }
  header p { color: #000; opacity: .6; margin: 0; font-size: 12px; }
  header .mod { margin-left: auto; display: flex; align-items: center; gap: 8px; }
  header .mod a { color: #000; text-decoration: none; font-size: 12px; border-bottom: 1px solid #000; }
  main { padding: 18px 24px; max-width: 1200px; margin: 0 auto; }
  .stats { display: flex; gap: 12px; margin: 0 0 16px; flex-wrap: wrap; }
  .stat { background: #fff; border: 1px solid #000; padding: 10px 16px; min-width: 150px; }
  .stat b { display: block; font-size: 22px; }
  .stat span { color: #000; font-size: 11px; text-transform: uppercase; letter-spacing: .5px; opacity: .6; }
  table { width: 100%; border-collapse: collapse; border: 1px solid #000; }
  th, td { padding: 10px 12px; text-align: left; font-size: 12px; border-bottom: 1px solid #000; vertical-align: top; }
  th { background: #000; color: #fff; text-transform: uppercase; font-size: 11px; letter-spacing: 1px; font-weight: 700; }
  .badge { display: inline-block; padding: 2px 8px; font-size: 10px; font-weight: 700; letter-spacing: .5px; text-transform: uppercase; }
  .played { color: #000; border: 1px solid #000; }
  .pending { color: #000; border: 1px dashed #000; }
  .revoked { color: #000; text-decoration: line-through; border: 1px solid #000; opacity: .5; }
  .edited { color: #fff; background: #000; border: 1px solid #000; }
  input.score { width: 46px; background: #fff; border: 1px solid #000; color: #000;
                padding: 5px 6px; font-size: 13px; text-align: center; }
  input.score:focus { outline: none; border-width: 2px; }
  button.ghost { background: #fff; color: #000; border: 1px solid #000; }
  button.ghost:hover { background: #000; color: #fff; }
  button { background: #000; color: #fff; border: 1px solid #000; padding: 6px 11px; font-size: 12px; cursor: pointer; }
  button:hover { background: #fff; color: #000; }
  button:disabled { background: #fff; color: #999; border-color: #999; cursor: not-allowed; }
  .vs { text-align: center; white-space: nowrap; font-weight: 700; }
  .who { font-weight: 700; }
  .meta { font-size: 11px; opacity: .6; }
  .sub { font-size: 11px; opacity: .5; }
  #toast { position: fixed; bottom: 16px; right: 16px; background: #000; color: #fff; padding: 10px 14px; font-size: 12px; opacity: 0; transition: opacity .2s; max-width: 420px; }
  #toast.show { opacity: 1; }
</style>
</head>
<body>
<header>
  <h1>PONG GATEWAY · Monitor de partidas</h1>
  <p id="stamp">cargando…</p>
  <p class="mod">%MODERADOR% · <a href="/logout">salir</a></p>
</header>
<main>
  <div class="stats">
    <div class="stat"><b id="stTotal">–</b><span>partidas</span></div>
    <div class="stat"><b id="stPlayed">–</b><span>jugadas</span></div>
    <div class="stat"><b id="stPending">–</b><span>pendientes</span></div>
    <div class="stat"><b id="stRevoked">–</b><span>revocadas</span></div>
  </div>
  <table>
    <thead><tr><th>#</th><th>Fecha</th><th>Jugador A</th><th>Marcador</th><th>Jugador B</th><th>Estado</th><th>Moderación</th></tr></thead>
    <tbody id="rows"></tbody>
  </table>
</main>
<div id="toast"></div>
<script>
const esc = s => String(s ?? "").replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const short = id => id.length <= 20 ? id : id.slice(0, 10) + '…' + id.slice(-4);

async function api(path, method) {
  try {
    const r = await fetch(path, { method: method || 'GET' });
    if (r.status === 401) { location.href = '/login'; return { ok: false, body: {} }; }
    return { ok: r.ok, body: await r.json() };
  } catch (e) {
    return { ok: false, body: { error: String(e) } };
  }
}

let busy = null;
let editMode = null;

function badge(m) {
  if (m.status === 'revoked') return '<span class="badge revoked">revocada</span>';
  if (m.status === 'pending') return '<span class="badge pending">pendiente</span>';
  const edited = m.edited_at ? ' <span class="badge edited">editada</span>' : '';
  return '<span class="badge played">jugada</span>' + edited;
}

function scoreForm(m) {
  return '<input id="sa' + m.id + '" class="score" type="number" min="0" max="' + 25 + '" value="' + m.score_a + '">'
    + ' : '
    + '<input id="sb' + m.id + '" class="score" type="number" min="0" max="' + 25 + '" value="' + m.score_b + '"> '
    + '<input id="ed' + m.id + '" type="hidden" value="' + m.id + '">'
    + '<button onclick="editScore(' + m.id + ')">Guardar</button> '
    + '<button class="ghost" onclick="editMode=null; refresh()" title="Cancelar">✕</button>';
}

function actionCell(m) {
  if (m.status === 'revoked') return '<span class="sub">revocada</span>';
  if (m.status === 'pending') return '<span class="sub">—</span>';
  if (editMode === m.id) return scoreForm(m);
  return '<button onclick="editMode=' + m.id + '; refresh()">Editar marcador</button> '
    + '<button onclick="revoke(' + m.id + ', this)">Revocar</button>';
}

async function refresh() {
  const { ok, body } = await api('/api/matches');
  if (!ok) { document.getElementById('stamp').textContent = 'error: ' + (body.error || '?'); return; }
  const list = body.matches || [];
  const count = s => list.filter(m => m.status === s).length;
  document.getElementById('stTotal').textContent = list.length;
  document.getElementById('stPlayed').textContent = count('played');
  document.getElementById('stPending').textContent = count('pending');
  document.getElementById('stRevoked').textContent = count('revoked');
  document.getElementById('stamp').textContent =
    'actualizado ' + new Date().toLocaleTimeString() + ' · ' + list.length + ' partidas';
  const rows = document.getElementById('rows');
  if (!list.length) {
    rows.innerHTML = '<tr><td colspan="7" class="sub">Aún no hay partidas reportadas a este gateway.</td></tr>';
    return;
  }
  rows.innerHTML = list.map(m => {
    const delta = (b, a) => a === b ? esc(b) : esc(b) + ' → ' + esc(a);
    return '<tr>'
      + '<td>' + m.id + '</td>'
      + '<td class="sub">' + esc(m.created_at) + '</td>'
      + '<td><span class="who">' + esc(m.username_a || short(m.player_a)) + '</span><br>'
      +   '<span class="meta">ELO ' + delta(m.rating_a_before, m.rating_a_after) + '</span> <span class="sub">' + short(m.player_a) + '</span></td>'
      + '<td class="vs"><b>' + m.score_a + '</b> : <b>' + m.score_b + '</b></td>'
      + '<td><span class="who">' + esc(m.username_b || short(m.player_b)) + '</span><br>'
      +   '<span class="meta">ELO ' + delta(m.rating_b_before, m.rating_b_after) + '</span> <span class="sub">' + short(m.player_b) + '</span></td>'
      + '<td>' + badge(m) + '</td>'
      + '<td>' + actionCell(m) + '</td>'
      + '</tr>';
  }).join('');
}

function toast(msg) {
  const t = document.getElementById('toast');
  t.textContent = msg;
  t.classList.add('show');
  clearTimeout(t._timer);
  t._timer = setTimeout(() => t.classList.remove('show'), 5000);
}

async function revoke(id, btn) {
  btn.disabled = true;
  const { ok, body } = await api('/api/matches/' + id + '/revoke', 'POST');
  if (ok) {
    toast('Partida #' + id + ' revocada: ELO restaurado en ambos jugadores.');
  } else {
    toast('No se pudo revocar: ' + (body.error || 'vuelve a intentarlo'));
    btn.disabled = false;
  }
  refresh();
}

async function editScore(id) {
  const a = document.getElementById('sa' + id).value;
  const b = document.getElementById('sb' + id).value;
  try {
    const r = await fetch('/api/matches/' + id + '/score', {
      method: 'POST',
      headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
      body: 'score_a=' + encodeURIComponent(a) + '&score_b=' + encodeURIComponent(b)
    });
    if (r.status === 401) { location.href = '/login'; return; }
    const body = await r.json().catch(() => ({}));
    if (r.ok) {
      toast('Marcador #' + id + ' corregido a ' + a + '–' + b + ': ELO recalculado.');
      editMode = null;
    } else {
      toast('No se pudo editar: ' + (body.error || 'vuelve a intentarlo'));
    }
  } catch (e) {
    toast('Error: ' + e);
  }
  refresh();
}

refresh();
setInterval(refresh, 5000);
</script>
</body>
</html>
"#;

/// The monitor page, personalized with the logged-in moderator's username
/// (injected at a placeholder so the rest stays one static blob).
fn monitor_page(moderator: &str) -> String {
    MONITOR_HTML.replace("%MODERADOR%", moderator)
}

/// Login form shown instead of the monitor page until a moderator signs in.
fn login_page(with_error: bool) -> String {
    LOGIN_HTML.replace(
        "%ERROR%",
        if with_error {
            r##"<p class="err">Usuario o contraseña incorrectos.</p>"##
        } else {
            ""
        },
    )
}

const LOGIN_HTML: &str = r#"<!doctype html>
<html lang="es">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Pong Gateway · Acceso moderador</title>
<style>
  :root { color-scheme: light; }
  * { box-sizing: border-box; }
  body { margin: 0; min-height: 100vh; display: grid; place-items: center;
         font-family: ui-monospace, "Cascadia Mono", "DejaVu Sans Mono", Menlo, Consolas, monospace;
         background: #fff; color: #000; padding: 24px; }
  .card { background: #fff; border: 1px solid #000; padding: 32px; width: 100%; max-width: 360px; }
  h1 { font-size: 18px; margin: 0 0 6px; letter-spacing: .5px; }
  .sub { font-size: 12px; margin: 0 0 20px; opacity: .6; }
  .err { background: #000; color: #fff; font-size: 12px; padding: 9px 12px; margin: 0 0 16px; }
  label { display: block; font-size: 11px; text-transform: uppercase; letter-spacing: .5px; margin: 0 0 14px; opacity: .8; }
  label span { display: block; margin-bottom: 5px; }
  input { width: 100%; background: #fff; border: 1px solid #000; color: #000; padding: 10px 12px; font-size: 14px; }
  input:focus { outline: none; border-width: 2px; }
  button { width: 100%; margin-top: 6px; background: #000; color: #fff; border: 1px solid #000; padding: 11px; font-size: 14px; cursor: pointer; }
  button:hover { background: #fff; color: #000; }
  .foot { margin-top: 18px; font-size: 11px; opacity: .6; text-align: center; }
</style>
</head>
<body>
<main class="card">
  <h1>PONG GATEWAY · Moderación</h1>
  <p class="sub">El monitor de partidas está bloqueado. Inicia sesión con tu cuenta de moderador.</p>
  %ERROR%
  <form method="post" action="/login" autocomplete="off">
    <label><span>Usuario</span><input name="username" required autofocus autocomplete="username"></label>
    <label><span>Contraseña</span><input type="password" name="password" required autocomplete="current-password"></label>
    <button>Entrar</button>
  </form>
  <p class="foot">Las cuentas de moderador se crean en el gateway con <code>--add-moderator &lt;usuario&gt;</code>.</p>
</main>
</body>
</html>
"#;

/// Pairwise ELO update. `score_a` is `1.0` (A won), `0.0` (A lost) or `0.5`.
fn elo(a: i32, b: i32, score_a: f64) -> (i32, i32) {
    let expected = 1.0 / (1.0 + 10f64.powf((b - a) as f64 / 400.0));
    let k = 32.0;
    let new_a = (a as f64 + k * (score_a - expected)).round() as i32;
    let new_b = (b as f64 + k * ((1.0 - score_a) - (1.0 - expected))).round() as i32;
    (new_a, new_b)
}

/// Builds the relayed addresses `target` can be reached at through this
/// gateway, from the perspective of the bases the requester knows about.
fn circuit_addresses(bases: &[Multiaddr], relay_peer: PeerId, target: PeerId) -> Vec<String> {
    bases
        .iter()
        .map(|base| {
            let mut addr = base.clone();
            if matches!(addr.iter().last(), Some(Protocol::P2p(_))) {
                addr.pop();
            }
            addr.push(Protocol::P2p(relay_peer));
            addr.push(Protocol::P2pCircuit);
            addr.push(Protocol::P2p(target));
            addr.to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_db() -> Connection {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("pong-test-auth-{nonce}.sqlite"));
        let conn = Connection::open(&path).unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn moderator_password_roundtrip() {
        let conn = tmp_db();
        set_moderator(&conn, "ALicia", "supersecret").unwrap();
        // Usernames are normalized; the right password verifies.
        assert!(verify_moderator(&conn, "alicia", "supersecret"));
        assert!(verify_moderator(&conn, "ALICIA", "supersecret"));
        // Wrong passwords are rejected; hashes are salted (different rows).
        assert!(!verify_moderator(&conn, "alicia", "wrong"));
        assert!(!verify_moderator(&conn, "bob", "supersecret"));
        assert_eq!(moderator_count(&conn), 1);
        // Resetting the password keeps a single account.
        set_moderator(&conn, "alicia", "nueva").unwrap();
        assert!(verify_moderator(&conn, "alicia", "nueva"));
        assert!(!verify_moderator(&conn, "alicia", "supersecret"));
        assert_eq!(moderator_count(&conn), 1);
        // Removal makes login impossible.
        assert!(remove_moderator(&conn, "alicia").unwrap());
        assert!(!verify_moderator(&conn, "alicia", "nueva"));
        assert_eq!(moderator_count(&conn), 0);
    }

    #[test]
    fn hash_password_is_salted() {
        let a = hash_password(b"saltsalt", "pass");
        let b = hash_password(b"saltsalt", "pass");
        let c = hash_password(b"othersalt", "pass");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(ct_eq(&a, &b));
        assert!(!ct_eq(&a, &c));
    }

    #[test]
    fn login_form_decoding() {
        assert_eq!(url_decode("a+b%20c%24"), "a b c$");
        assert_eq!(
            form_field("username=caro%2B1&password=se%C3%B1a", "username").as_deref(),
            Some("caro+1")
        );
        assert_eq!(
            form_field("username=caro%2B1&password=se%C3%B1a", "password").as_deref(),
            Some("seña")
        );
        assert_eq!(cookie_token("other=1; pong_mod=abc123; x=2").as_deref(), Some("abc123"));
        assert_eq!(cookie_token("pong_mod=").as_deref(), Some(""));
        assert_eq!(cookie_token("nomod=1"), None);
    }

    #[test]
    fn equal_ratings_win_moves_expected_amount() {
        let (a, b) = elo(1200, 1200, 1.0);
        // Equal rating → expected 0.5, K=32 → winner +16, loser −16.
        assert_eq!(a, 1216);
        assert_eq!(b, 1184);
    }

    #[test]
    fn equal_ratings_draw_keeps_both() {
        let (a, b) = elo(1200, 1200, 0.5);
        assert_eq!(a, 1200);
        assert_eq!(b, 1200);
    }

    #[test]
    fn huge_upset_moves_more() {
        // Brick (400) beats Pong Legend (2400): a huge upset.
        let (a, b) = elo(400, 2400, 1.0);
        assert!(a > 400 + 30, "winner should gain ~32, got {a}");
        assert_eq!(b, 2368);
    }

    #[test]
    fn favorite_win_moves_little() {
        // Pong Legend (2400) beats Brick (400): nearly no movement.
        let (a, b) = elo(2400, 400, 1.0);
        assert_eq!(a, 2400);
        assert_eq!(b, 400);
    }

    #[test]
    fn ratings_swap_symmetrically() {
        let (a, b) = elo(1500, 1300, 0.0);
        let (a2, b2) = elo(1300, 1500, 1.0);
        assert_eq!(a, b2);
        assert_eq!(b, a2);
    }

    #[test]
    fn new_players_start_as_brick() {
        assert_eq!(rank_for_rating(START_RATING), "Brick");
    }

    #[test]
    fn proof_roundtrips_sign_and_verify() {
        let key = Keypair::generate_ed25519();
        let gateway = key.public().to_peer_id();
        let client = Keypair::generate_ed25519().public().to_peer_id();
        let proof = proof_for(&key, &gateway, &client, 1420, 9).expect("signs");
        assert_eq!(proof.gateway, gateway.to_base58());
        assert!(verify_proof(&key, &client, &proof));
    }

    #[test]
    fn tampered_rating_is_rejected() {
        let key = Keypair::generate_ed25519();
        let gateway = key.public().to_peer_id();
        let client = Keypair::generate_ed25519().public().to_peer_id();
        let mut proof = proof_for(&key, &gateway, &client, 1420, 9).expect("signs");
        proof.rating += 100;
        assert!(!verify_proof(&key, &client, &proof));
    }

    #[test]
    fn proof_bound_to_other_player_is_rejected() {
        let key = Keypair::generate_ed25519();
        let gateway = key.public().to_peer_id();
        let alice = Keypair::generate_ed25519().public().to_peer_id();
        let bob = Keypair::generate_ed25519().public().to_peer_id();
        let proof = proof_for(&key, &gateway, &alice, 1420, 9).expect("signs");
        assert!(!verify_proof(&key, &bob, &proof));
    }

    #[test]
    fn replayed_old_seq_is_beat_by_cached_higher() {
        // A client that lost matches and replays its older, higher proof must
        // stay on the cached (higher-seq, current) rating.
        let key = Keypair::generate_ed25519();
        let gateway = key.public().to_peer_id();
        let client = Keypair::generate_ed25519().public().to_peer_id();
        let stale = proof_for(&key, &gateway, &client, 1500, 3).expect("signs");
        // The gateway already certified this player at seq 5, rating 1200.
        let cached_rating = 1200_i32;
        let cached_seq = 5_u64;
        let accepted = match (Some(&stale), Some((cached_rating, cached_seq))) {
            (Some(p), Some((cached_rating, cached_seq))) if p.seq <= cached_seq => {
                (cached_rating, cached_seq)
            }
            (Some(p), _) => (p.rating, p.seq),
            (None, Some((rating, seq))) => (rating, seq),
            (None, None) => (START_RATING, 0),
        };
        assert_eq!(accepted.0, 1200);
        assert_eq!(accepted.1, 5);
    }

    fn test_lobby_with(players: &[(PeerId, &str, i32)]) -> (Lobby, Keypair, PeerId) {
        let mut lobby = Lobby {
            next_id: 1,
            ..Default::default()
        };
        for (peer, name, rating) in players {
            lobby.players.insert(
                *peer,
                PlayerEntry {
                    username: Some((*name).to_string()),
                    rating: *rating,
                    last_seq: 0,
                    ..Default::default()
                },
            );
        }
        let key = Keypair::generate_ed25519();
        let gateway = key.public().to_peer_id();
        (lobby, key, gateway)
    }

    #[test]
    fn two_complementary_reports_store_one_match_and_move_elo_once() {
        let (a, b) = (PeerId::random(), PeerId::random());
        let (mut lobby, key, gateway) =
            test_lobby_with(&[(a, "alice", 1200), (b, "bob", 1200)]);
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let mut conn = conn;

        // Alice reports a 5-3 win (first report → ELO applied).
        let (ra, proof_a) =
            report_result(&mut lobby, &mut conn, &key, &gateway, a, b.to_base58(), 1, 5, 3);
        assert_eq!(ra, 1216);
        assert!(proof_a.is_some());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM matches", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        // Bob reports the complementary 3-5 (confirmation → no double ELO).
        let (rb, _) = report_result(&mut lobby, &mut conn, &key, &gateway, b, a.to_base58(), 1, 3, 5);
        assert_eq!(rb, 1184);
        let status: String = conn
            .query_row("SELECT status FROM matches WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "played");

        // A repeat of an already-counted match changes nothing.
        let (ra2, _) =
            report_result(&mut lobby, &mut conn, &key, &gateway, a, b.to_base58(), 1, 5, 3);
        assert_eq!(ra2, 1216);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM matches", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(lobby.players[&a].rating, 1216);
        assert_eq!(lobby.players[&b].rating, 1184);
    }

    #[test]
    fn legacy_reports_without_match_id_merge_canonically() {
        let (a, b) = (PeerId::random(), PeerId::random());
        let (mut lobby, key, gateway) =
            test_lobby_with(&[(a, "alice", 1400), (b, "bob", 1400)]);
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let mut conn = conn;

        // Old clients omit match_id (0) but still report complementary scores.
        let (ra, _) =
            report_result(&mut lobby, &mut conn, &key, &gateway, a, b.to_base58(), 0, 5, 2);
        assert_eq!(ra, 1416);
        let (rb, _) =
            report_result(&mut lobby, &mut conn, &key, &gateway, b, a.to_base58(), 0, 2, 5);
        assert_eq!(rb, 1384);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM matches", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let status: String = conn
            .query_row("SELECT status FROM matches WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "played");
    }

    #[test]
    fn revoked_match_restores_before_ratings() {
        let (a, b) = (PeerId::random(), PeerId::random());
        let (mut lobby, key, gateway) =
            test_lobby_with(&[(a, "alice", 1200), (b, "bob", 1200)]);
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let mut conn = conn;

        report_result(&mut lobby, &mut conn, &key, &gateway, a, b.to_base58(), 1, 5, 3);
        report_result(&mut lobby, &mut conn, &key, &gateway, b, a.to_base58(), 1, 3, 5);
        assert_eq!(lobby.players[&a].rating, 1216);
        assert_eq!(lobby.players[&b].rating, 1184);

        let revoked = revoke_match_state(&mut lobby, &mut conn, &key, &gateway, 1).unwrap();
        assert_eq!(revoked.len(), 2);
        assert_eq!(revoked[0].before, 1200);
        assert_eq!(revoked[1].before, 1200);
        assert!(revoked[0].proof.is_some());
        assert!(revoked[1].proof.is_some());

        assert_eq!(lobby.players[&a].rating, 1200);
        assert_eq!(lobby.players[&b].rating, 1200);
        // Each revocation bump advances the proof sequence (nothing to replay).
        assert_eq!(lobby.players[&a].last_seq, 2);
        assert_eq!(lobby.players[&b].last_seq, 2);

        let status: String = conn
            .query_row("SELECT status FROM matches WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "revoked");

        // A second revocation is rejected.
        assert!(revoke_match_state(&mut lobby, &mut conn, &key, &gateway, 1).is_err());
    }

    #[test]
    fn revoking_unknown_match_fails() {
        let (a, b) = (PeerId::random(), PeerId::random());
        let (mut lobby, key, gateway) =
            test_lobby_with(&[(a, "alice", 1200), (b, "bob", 1200)]);
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let mut conn = conn;
        assert!(revoke_match_state(&mut lobby, &mut conn, &key, &gateway, 999).is_err());
    }

    #[test]
    fn edit_match_score_recomputes_elo_and_updates_storage() {
        let (a, b) = (PeerId::random(), PeerId::random());
        let (mut lobby, key, gateway) =
            test_lobby_with(&[(a, "alice", 1200), (b, "bob", 1200)]);
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let mut conn = conn;

        // Alice 5-3 → she becomes 1216, Bob 1184.
        report_result(&mut lobby, &mut conn, &key, &gateway, a, b.to_base58(), 1, 5, 3);
        report_result(&mut lobby, &mut conn, &key, &gateway, b, a.to_base58(), 1, 3, 5);
        assert_eq!(lobby.players[&a].rating, 1216);
        assert_eq!(lobby.players[&b].rating, 1184);

        // The moderator corrects it to a 3-5 Bob win: ratings flip around the
        // pre-match baseline (both were 1200). Roles A/B in the stored row are
        // sorted by peer id, so assert on the per-player returned view.
        let edited = edit_match_state(&mut lobby, &mut conn, &key, &gateway, 1, 3, 5).unwrap();
        assert_eq!(edited.len(), 2);
        for p in &edited {
            // The player who won (their own score 5) must end at 1216; the
            // loser (own score 3) at 1184 — scores are mirrored per player.
            let expected = if p.my_score == 5 { 1216 } else { 1184 };
            assert_eq!(p.rating, expected);
            assert_eq!(p.opponent_score, 8 - p.my_score);
            assert!(p.proof.is_some());
        }
        assert_eq!(lobby.players[&a].rating + lobby.players[&b].rating, 2400);
        assert_ne!(lobby.players[&a].rating, lobby.players[&b].rating);
        assert!(lobby.players[&a].rating == 1216 || lobby.players[&b].rating == 1216);
        let (score_a, score_b, after_a, after_b, edited_at): (i32, i32, i32, i32, Option<String>) =
            conn.query_row(
                "SELECT score_a, score_b, rating_a_after, rating_b_after, edited_at
                 FROM matches WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!((score_a, score_b), (3, 5));
        assert_eq!((after_a, after_b), (1184, 1216));
        assert!(edited_at.is_some());

        // Oversized scores and revoked matches are rejected.
        assert!(edit_match_state(&mut lobby, &mut conn, &key, &gateway, 1, 99, 5).is_err());
        revoke_match_state(&mut lobby, &mut conn, &key, &gateway, 1).unwrap();
        assert!(edit_match_state(&mut lobby, &mut conn, &key, &gateway, 1, 5, 5).is_err());
    }

    #[test]
    fn list_matches_json_is_valid() {
        let (a, b) = (PeerId::random(), PeerId::random());
        let (mut lobby, key, gateway) =
            test_lobby_with(&[(a, "alice", 1500), (b, "bob", 1500)]);
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let mut conn = conn;
        report_result(&mut lobby, &mut conn, &key, &gateway, a, b.to_base58(), 1, 5, 4);
        let body = list_matches_json(&conn);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        let matches = parsed["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["id"], 1);
        // Canonical order sorts the peer ids, so the "A"/"B" roles depend on
        // the random ids — what matters is both players appear with their score.
        let usernames = [
            matches[0]["username_a"].as_str().unwrap(),
            matches[0]["username_b"].as_str().unwrap(),
        ];
        assert!(usernames.contains(&"alice"));
        assert!(usernames.contains(&"bob"));
        let scores = [matches[0]["score_a"].as_i64().unwrap(), matches[0]["score_b"].as_i64().unwrap()];
        assert_eq!(scores.iter().min(), Some(&4));
        assert_eq!(scores.iter().max(), Some(&5));
        assert_eq!(matches[0]["status"], "pending");
    }
}

/// Scans the queue and pairs adjacent players by ELO, then pushes both sides a
/// `MatchFound` announcing the opponent, their relayed addresses and a fresh
/// `match_id` used for later result reports and moderation.
fn run_matchmaking(
    swarm: &mut libp2p::Swarm<Behaviour>,
    relay_peer: &PeerId,
    lobby: &mut Lobby,
    rating_db: &Connection,
    default_base: &Multiaddr,
) {
    if lobby.queue.len() < 2 {
        return;
    }
    lobby
        .queue
        .sort_by_key(|p| lobby.players.get(p).map(|e| e.rating).unwrap_or(START_RATING));

    let mut pairs = Vec::new();
    let mut i = 0;
    while i + 1 < lobby.queue.len() {
        let a = lobby.queue[i];
        let b = lobby.queue[i + 1];
        let ra = lobby.players.get(&a).map(|e| e.rating).unwrap_or(START_RATING);
        let rb = lobby.players.get(&b).map(|e| e.rating).unwrap_or(START_RATING);
        if (ra - rb).abs() <= MAX_ELO_GAP {
            pairs.push((a, b));
            i += 2;
        } else {
            i += 1;
        }
    }

    for (a, b) in pairs {
        lobby.queue.retain(|p| *p != a && *p != b);
        let bases_a = lobby
            .players
            .get(&a)
            .map(|e| e.base_addresses.to_vec())
            .unwrap_or_else(|| vec![(*default_base).clone()]);
        let bases_b = lobby
            .players
            .get(&b)
            .map(|e| e.base_addresses.to_vec())
            .unwrap_or_else(|| vec![(*default_base).clone()]);

        let addrs_a = circuit_addresses(&bases_a, *relay_peer, b);
        let addrs_b = circuit_addresses(&bases_b, *relay_peer, a);

        let id = alloc_match_id(lobby, rating_db);
        println!("matching {a} ↔ {b} (match #{id})");
        swarm.behaviour_mut().gateway.send_request(
            &a,
            GatewayRequest::MatchFound {
                match_id: id,
                opponent: b.to_base58(),
                addresses: addrs_a,
            },
        );
        swarm.behaviour_mut().gateway.send_request(
            &b,
            GatewayRequest::MatchFound {
                match_id: id,
                opponent: a.to_base58(),
                addresses: addrs_b,
            },
        );
    }
}
