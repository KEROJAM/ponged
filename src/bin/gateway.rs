//! `pong-gateway` — the matchmaking + connectivity gateway for the game (M6).
//!
//! A headless libp2p node that provides:
//!   * **Relay** (circuit-relay v2): NAT-traversed connectivity for clients
//!     via `/p2p/<gateway>/p2p-circuit` addresses.
//!   * **Rendezvous server**: the shared discovery point where clients
//!     publish/query each other.
//!   * **Kademlia server**: a DHT bootstrap node for discovery fallback.
//!   * **Matchmaking**: an ELO-rating queue (persisted in SQLite) that pairs
//!     clients and pushes them `GatewayRequest::MatchFound` with dialable
//!     relayed circuit addresses for their opponent.
//!
//! Client ↔ gateway RPC flows over the [`GATEWAY_PROTOCOL`] cbor channel
//! (`Register` → `QueueMatch` → gateway pushes `MatchFound` → both sides dial
//! the relayed address → DC‑UTP upgrades the circuit to a direct connection).
//!
//! Usage:
//! ```text
//! pong-gateway [--listen /ip4/0.0.0.0/tcp/4001]
//!              [--public HOST_OR_IP]        # advertise this host in relay addr
//!              [--db gateway.sqlite]
//!              [--key gateway.key]
//! ```
//!
//! The gateway's own `PeerId` is printed at startup; players connect to it
//! with `NetCommand::Dial(<listen addr>)`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use libp2p::core::multiaddr::Protocol;
use libp2p::futures::StreamExt;
use libp2p::identity::Keypair;
use libp2p::kad::store::MemoryStore;
use libp2p::request_response::{self, cbor, ProtocolSupport, ResponseChannel};
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{
    identify, kad, noise, ping, relay, rendezvous, tcp, yamux, Multiaddr, PeerId, SwarmBuilder,
};
use proyecto_final::protocol::{
    GatewayRequest, GatewayResponse, GATEWAY_AGENT_VERSION, GATEWAY_PROTOCOL,
};
use rusqlite::{params, Connection, OptionalExtension};
use tokio::time::MissedTickBehavior;

const DEFAULT_LISTEN: &str = "/ip4/0.0.0.0/tcp/4001";
const DEFAULT_HOST_FALLBACK: &str = "127.0.0.1";
const START_RATING: i32 = 1200;
/// Do not pair players whose ELO differs by more than this.
const MAX_ELO_GAP: i32 = 600;
/// How often the matchmaking queue is scanned for pairs.
const QUEUE_INTERVAL: Duration = Duration::from_secs(2);
const USAGE: &str = "\
pong-gateway — matchmaking + relay gateway for the P2P pong game

USAGE:
    pong-gateway [OPTIONS]

OPTIONS:
    --listen <multiaddr>   Address to listen on   [default: /ip4/0.0.0.0/tcp/4001]
    --public <host>        Host/ip advertised in relay addresses (needed when
                           the gateway is behind a NAT / not on localhost)
    --db <path>            SQLite rating database [default: gateway.sqlite]
    --key <path>           Ed25519 private key, created if missing [default: gateway.key]
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
    /// Gateway base addresses (no `/p2p-circuit` suffix) usable to build a
    /// relayed route to this player, from that player's own vantage.
    base_addresses: Vec<Multiaddr>,
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

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen_addr = args.next().ok_or("--listen needs a value")?.parse()?,
            "--public" => public_host = args.next().ok_or("--public needs a value")?,
            "--db" => db_path = args.next().ok_or("--db needs a value")?.into(),
            "--key" => key_path = args.next().ok_or("--key needs a value")?.into(),
            "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            other => return Err(format!("unknown argument '{other}'\n\n{USAGE}").into()),
        }
    }

    let keypair = load_or_create_key(&key_path)?;
    let gateway_peer = keypair.public().to_peer_id();

    println!("pong-gateway starting");
    println!("  peer id : {gateway_peer}");
    println!("  identity: {key_path:?}");
    println!("  db      : {db_path:?} (SQLite ratings)");
    println!("  listen  : {listen_addr}");
    println!("  public  : {public_host}");

    let mut rating_db = open_ratings(&db_path)?;

    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(tcp::Config::default(), noise::Config::new, yamux::Config::default)?
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

    let default_base: Multiaddr = format!("/ip4/{public_host}/tcp/{}", port_of(&listen_addr)?)
        .parse()?;

    swarm.listen_on(listen_addr)?;

    // `players` keeps the in-memory lobby; the queue is simply our current
    // matchmaking candidates; `default_base` is the relay base we advertise.
    let mut players: HashMap<PeerId, PlayerEntry> = HashMap::new();
    let mut queue: Vec<PeerId> = Vec::new();

    let mut matchmaking = tokio::time::interval(QUEUE_INTERVAL);
    matchmaking.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = matchmaking.tick() => {
                run_matchmaking(&mut swarm, &gateway_peer, &mut players, &mut queue, &default_base);
            }
            Some(event) = swarm.next() => match event {
                SwarmEvent::NewListenAddr { address, .. } => {
                    info_listening(&address);
                }
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    println!("client connected: {peer_id}");
                    let entry = players.entry(peer_id).or_insert_with(|| PlayerEntry {
                        rating: load_rating(&rating_db, &peer_id).unwrap_or(START_RATING),
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
                        queue.retain(|p| *p != peer_id);
                        players.remove(&peer_id);
                        println!("client disconnected: {peer_id}");
                    }
                }
                SwarmEvent::Behaviour(behaviour_event) => match behaviour_event {
                    BehaviourEvent::Identify(event) => {
                        if let identify::Event::Received { peer_id, info, .. } = event {
                            // The client's observed view of the gateway is the
                            // best (and often only) public route to use when
                            // building a relayed address *for that client*.
                            let entry = players.entry(peer_id).or_default();
                            if !entry.base_addresses.contains(&info.observed_addr) {
                                entry.base_addresses.push(info.observed_addr);
                            }
                            if entry.rating == 0 {
                                entry.rating =
                                    load_rating(&rating_db, &peer_id).unwrap_or(START_RATING);
                            }
                        }
                    }
                    BehaviourEvent::Gateway(request_response::Event::Message {
                        peer,
                        message: request_response::Message::Request { request, channel, .. },
                        ..
                    }) => {
                        on_gateway_request(
                            &mut swarm, &mut players, &mut queue, &mut rating_db, peer, request, channel,
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
                        queue.retain(|p| *p != peer);
                        players.remove(&peer);
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
                        let _ = event;
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
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS players (
            peer_id  TEXT PRIMARY KEY,
            username TEXT NOT NULL,
            rating   INTEGER NOT NULL DEFAULT 1200
        );",
    )?;
    Ok(conn)
}

fn load_rating(conn: &Connection, peer: &PeerId) -> Option<i32> {
    conn.query_row(
        "SELECT rating FROM players WHERE peer_id = ?1",
        params![peer.to_base58()],
        |row| row.get(0),
    )
    .optional()
    .ok()
    .flatten()
}

fn upsert_rating(conn: &mut Connection, peer: &PeerId, username: &str, rating: i32) {
    let result = conn.execute(
        "INSERT INTO players (peer_id, username, rating) VALUES (?1, ?2, ?3)
         ON CONFLICT(peer_id) DO UPDATE SET username = excluded.username, rating = excluded.rating",
        params![peer.to_base58(), username, rating],
    );
    if let Err(e) = result {
        println!("rating persist error for {peer}: {e}");
    }
}

/// Client → gateway RPC handler.
fn on_gateway_request(
    swarm: &mut libp2p::Swarm<Behaviour>,
    players: &mut HashMap<PeerId, PlayerEntry>,
    queue: &mut Vec<PeerId>,
    rating_db: &mut Connection,
    peer: PeerId,
    request: GatewayRequest,
    channel: ResponseChannel<GatewayResponse>,
) {
    let reply = |swarm: &mut libp2p::Swarm<Behaviour>,
                 channel: ResponseChannel<GatewayResponse>,
                 response: GatewayResponse| {
        let _ = swarm.behaviour_mut().gateway.send_response(channel, response);
    };

    match request {
        GatewayRequest::Register { username } => {
            let entry = players.entry(peer).or_insert_with(|| PlayerEntry {
                username: Some(username.clone()),
                rating: START_RATING,
                ..Default::default()
            });
            let stored = load_rating(rating_db, &peer).unwrap_or(START_RATING);
            entry.username = Some(username.clone());
            entry.rating = stored;
            upsert_rating(rating_db, &peer, &username, stored);
            println!("{peer} registered as '{username}' (rating {stored})");
            reply(swarm, channel, GatewayResponse::Registered { username, rating: stored });
        }
        GatewayRequest::QueueMatch => {
            if !queue.contains(&peer) {
                queue.push(peer);
            }
            let position = queue.iter().position(|p| *p == peer).unwrap_or(0) as u32;
            println!("{peer} queued (position {position}, queue size {})", queue.len());
            reply(swarm, channel, GatewayResponse::Queued { position });
        }
        GatewayRequest::LeaveQueue => {
            queue.retain(|p| *p != peer);
            println!("{peer} left the queue");
            reply(swarm, channel, GatewayResponse::Dequeued);
        }
        GatewayRequest::ReportResult { opponent, my_score, opponent_score } => {
            let my_rating = players.get(&peer).map(|p| p.rating).unwrap_or(START_RATING);
            let mut reply_rating = my_rating;
            if let Ok(opponent_id) = opponent.parse::<PeerId>() {
                let opp_rating = players.get(&opponent_id).map(|p| p.rating).unwrap_or(START_RATING);
                let score_a = match my_score.cmp(&opponent_score) {
                    std::cmp::Ordering::Greater => 1.0,
                    std::cmp::Ordering::Less => 0.0,
                    std::cmp::Ordering::Equal => 0.5,
                };
                let (new_a, new_b) = elo(my_rating, opp_rating, score_a);
                if let Some(entry) = players.get_mut(&peer) {
                    entry.rating = new_a;
                }
                if let Some(entry) = players.get_mut(&opponent_id) {
                    entry.rating = new_b;
                }
                if let Some(username) = players.get(&peer).and_then(|p| p.username.clone()) {
                    upsert_rating(rating_db, &peer, &username, new_a);
                }
                if let Some(username) = players.get(&opponent_id).and_then(|p| p.username.clone()) {
                    upsert_rating(rating_db, &opponent_id, &username, new_b);
                }
                reply_rating = new_a;
                println!("{peer} reported {my_score}-{opponent_score} vs {opponent_id} → ELO {my_rating}→{new_a}");
            }
            reply(swarm, channel, GatewayResponse::Rating { rating: reply_rating });
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
    }
}

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
fn circuit_addresses(
    bases: &[Multiaddr],
    relay_peer: PeerId,
    target: PeerId,
) -> Vec<String> {
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

/// Scans the queue and pairs adjacent players by ELO, then pushes both sides a
/// `MatchFound` announcing the opponent and their relayed addresses.
fn run_matchmaking(
    swarm: &mut libp2p::Swarm<Behaviour>,
    relay_peer: &PeerId,
    players: &mut HashMap<PeerId, PlayerEntry>,
    queue: &mut Vec<PeerId>,
    default_base: &Multiaddr,
) {
    if queue.len() < 2 {
        return;
    }
    queue.sort_by_key(|p| players.get(p).map(|e| e.rating).unwrap_or(START_RATING));

    let mut pairs = Vec::new();
    let mut i = 0;
    while i + 1 < queue.len() {
        let a = queue[i];
        let b = queue[i + 1];
        let ra = players.get(&a).map(|e| e.rating).unwrap_or(START_RATING);
        let rb = players.get(&b).map(|e| e.rating).unwrap_or(START_RATING);
        if (ra - rb).abs() <= MAX_ELO_GAP {
            pairs.push((a, b));
            i += 2;
        } else {
            i += 1;
        }
    }

    for (a, b) in pairs {
        queue.retain(|p| *p != a && *p != b);
        let bases_a = players
            .get(&a)
            .map(|e| e.base_addresses.to_vec())
            .unwrap_or_else(|| vec![(*default_base).clone()]);
        let bases_b = players
            .get(&b)
            .map(|e| e.base_addresses.to_vec())
            .unwrap_or_else(|| vec![(*default_base).clone()]);

        let addrs_a = circuit_addresses(&bases_a, *relay_peer, b);
        let addrs_b = circuit_addresses(&bases_b, *relay_peer, a);

        println!("matching {a} ↔ {b}");
        swarm
            .behaviour_mut()
            .gateway
            .send_request(&a, GatewayRequest::MatchFound {
                opponent: b.to_base58(),
                addresses: addrs_a,
            });
        swarm
            .behaviour_mut()
            .gateway
            .send_request(&b, GatewayRequest::MatchFound {
                opponent: a.to_base58(),
                addresses: addrs_b,
            });
    }
}