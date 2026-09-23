use std::time::Duration;

use bevy::prelude::*;
use libp2p::futures::StreamExt;
use libp2p::kad::store::MemoryStore;
use libp2p::request_response::{self, ProtocolSupport, cbor};
use libp2p::{
    Multiaddr, PeerId, StreamProtocol, SwarmBuilder, dcutr, identify, kad, mdns, noise, ping,
    relay, rendezvous, swarm::NetworkBehaviour, swarm::SwarmEvent, tcp, yamux,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::protocol::{
    GATEWAY_AGENT_VERSION, GATEWAY_PROTOCOL, GatewayRequest, GatewayResponse, RatingProof,
    Request as GameRequest, Response as GameResponse,
};
use crate::sim::{SimCommand, remote_paddle_sink};

/// The libp2p protocol id used for game requests/responses.
const GAME_PROTOCOL: StreamProtocol = StreamProtocol::new("/pong/state/1.0.0");

/// Agent version the game advertises over `identify` (the gateway advertises
/// [`GATEWAY_AGENT_VERSION`] so clients can tell them apart).
const AGENT_VERSION: &str = "pong/1.0.0";

/// Commands sent from the game to the network thread.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum NetCommand {
    Dial(Multiaddr),
    Listen(Multiaddr),
    /// Fire a game request at a specific peer via the request-response protocol.
    SendRequest {
        peer: PeerId,
        request: GameRequest,
    },
    /// Fire a gateway (matchmaking) request at a specific peer.
    SendGatewayRequest {
        peer: PeerId,
        request: GatewayRequest,
    },
    /// Register ourselves on the rendezvous server so other clients can
    /// discover us (M3 publish).
    RendezvousRegister {
        peer: PeerId,
        namespace: String,
    },
    /// Ask the rendezvous server who else is currently registered in a
    /// namespace (M3 discover). Sent periodically while searching so available
    /// players show up in real time.
    RendezvousDiscover {
        peer: PeerId,
        namespace: String,
    },
}

/// Events emitted by the network thread into the Bevy world. Trigger them and
/// handle them with observers, e.g. `app.add_observer(on_peer_connected)`
/// where `fn on_peer_connected(ev: On<NetEvent>)`.
#[derive(Debug, Clone, Event)]
#[allow(dead_code)]
pub enum NetEvent {
    /// This node's own [`PeerId`] (sent once at startup).
    LocalPeerId(PeerId),
    Listening(Multiaddr),
    PeerConnected(PeerId),
    PeerDisconnected(PeerId),
    PeerPinged {
        peer: PeerId,
        rtt: Duration,
    },
    /// Another node identified itself over the `identify` protocol.
    Identity {
        peer: PeerId,
        agent: String,
    },
    /// An inbound game request arrived (a response was already sent).
    GameRequest {
        peer: PeerId,
        request: GameRequest,
    },
    /// A reply to a game request we sent.
    GameResponse {
        peer: PeerId,
        response: GameResponse,
    },
    /// A reply to a gateway (matchmaking) request we sent.
    GatewayResponse {
        peer: PeerId,
        response: GatewayResponse,
    },
    /// An inbound gateway request (normally never sent by the server to us).
    GatewayRequest {
        peer: PeerId,
        request: GatewayRequest,
    },
    /// Our relay reservation on a relay server (the gateway) succeeded.
    RelayReservation {
        relay_peer: PeerId,
        success: bool,
    },
    /// The rendezvous server reported peers registered in our namespace.
    RendezvousDiscovered {
        peers: Vec<(PeerId, Vec<Multiaddr>)>,
    },
    /// A direct (hole-punched) connection was established with `peer`.
    DcutrEstablished {
        peer: PeerId,
    },
    /// A non-relayed (direct) connection exists to `peer`. Emitted for every
    /// established connection whose path does not go through a relay, so the
    /// lobby can tell "the opponent is reachable directly" (LAN, port-forwarded,
    /// or a hole-punched link) from "traffic is bouncing through the gateway".
    ///
    /// Gateways are direct too, so consumers must filter known gateway peers
    /// before treating this as a player being directly reachable.
    DirectConnected {
        peer: PeerId,
    },
    Error(String),
}

/// Sub-behaviours of the swarm. The `#[derive(NetworkBehaviour)]` macro wires
/// them together so a single `fn next()` drives all of them. The generated
/// event enum is called `BehaviourEvent` and has one variant per field
/// (`Ping`, `Identify`, `Mdns`, `Game`, `Gateway`, `Rendezvous`, `Kademlia`,
/// `Relay`, `Dcutr`).
#[derive(NetworkBehaviour)]
struct Behaviour {
    ping: ping::Behaviour,
    identify: identify::Behaviour,
    mdns: mdns::tokio::Behaviour,
    game: cbor::Behaviour<GameRequest, GameResponse>,
    /// Matchmaking RPC channel towards the gateway (M6).
    gateway: cbor::Behaviour<GatewayRequest, GatewayResponse>,
    /// Rendezvous client (M3): publish/discover players through the gateway.
    rendezvous: rendezvous::client::Behaviour,
    /// Kademlia (M3): DHT bootstrap / fallback discovery.
    kademlia: kad::Behaviour<MemoryStore>,
    /// Relay client (M3): route traffic through the gateway past NATs.
    relay: relay::client::Behaviour,
    /// DC-UTP hole punching (M3): turn a relayed connection into a direct one.
    dcutr: dcutr::Behaviour,
}

/// Bridge between Bevy (sync, single-threaded) and the tokio swarm task.
#[derive(Resource)]
pub struct NetChannels {
    pub commands: UnboundedSender<NetCommand>,
    events: std::sync::Mutex<UnboundedReceiver<NetEvent>>,
}

/// What the lobby knows about the matchmaking gateway (M6): the address we
/// dialed, its identified `PeerId`, and how far the NAT-traversal handshake got.
#[derive(Resource, Default)]
pub struct GatewayState {
    /// The multiaddr we dialed (may include a trailing `/p2p/<peer>`).
    pub addr: Option<Multiaddr>,
    /// The gateway's `PeerId`, learned via `identify`.
    pub peer: Option<PeerId>,
    pub connected: bool,
    /// A relay circuit through the gateway was reserved.
    pub reserved: bool,
    /// We are currently in the matchmaking queue.
    pub queued: bool,
    /// Our current rank (e.g. "Silver"), as returned by the gateway.
    pub rank: Option<String>,
    /// Our current ELO rating.
    pub rating: i32,
    /// The newest gateway-signed certificate for our rating (kept locally so
    /// a gateway outage can never reset us). `None` until a gateway signs.
    pub proof: Option<RatingProof>,
    /// Every peer identified so far as a matchmaking gateway (a deployment may
    /// run several). We queue on the first one; the rest must never be picked
    /// as an opponent.
    pub known: std::collections::BTreeSet<PeerId>,
}

impl GatewayState {
    /// The dialed gateway address without a trailing `/p2p/<peer>` component,
    /// so we can build reservation/relay addresses from it.
    pub fn base_addr(&self) -> Option<Multiaddr> {
        let addr = self.addr.clone()?;
        let mut protocols = addr.into_iter().collect::<Vec<_>>();
        if matches!(
            protocols.last(),
            Some(libp2p::core::multiaddr::Protocol::P2p(_))
        ) {
            protocols.pop();
        }
        Some(protocols.into_iter().collect())
    }
}

pub struct NetworkingPlugin;

impl Plugin for NetworkingPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, setup)
            .add_systems(Update, poll_net_events);
    }
}

fn setup(mut commands: Commands) {
    let (command_tx, command_rx) = unbounded_channel::<NetCommand>();
    let (event_tx, event_rx) = unbounded_channel::<NetEvent>();

    commands.insert_resource(NetChannels {
        commands: command_tx,
        events: std::sync::Mutex::new(event_rx),
    });

    // The swarm is async and must be driven continuously, so it lives on a
    // tokio runtime on its own thread, communicating with Bevy via channels.
    // A single-threaded runtime is all the client needs: one swarm future is
    // polled in a loop and everything it spawns (yamux subscribers, timers)
    // is I/O-bound. A full multi-thread runtime would spin up one worker per
    // CPU core just to idle, wasting RAM and context switches.
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(e) => {
                let _ = event_tx.send(NetEvent::Error(e.to_string()));
                return;
            }
        };
        if let Err(e) = runtime.block_on(run_swarm(command_rx, event_tx.clone())) {
            let _ = event_tx.send(NetEvent::Error(e.to_string()));
        }
    });
}

/// Reads whatever the swarm has queued and re-triggers it into the ECS.
pub fn poll_net_events(channels: Res<NetChannels>, mut commands: Commands) {
    let mut receiver = channels.events.lock().expect("net event channel poisoned");
    while let Ok(event) = receiver.try_recv() {
        commands.trigger(event);
    }
}

/// Path of the persistent client keypair, next to the history database.
fn client_key_path() -> std::path::PathBuf {
    dirs::data_local_dir()
        .map(|p| p.join("ponged").join("client.key"))
        .unwrap_or_else(|| std::path::PathBuf::from("client.key"))
}

/// Loads (or creates on first run) the client's ed25519 keypair so the
/// PeerId — and the gateway-signed rating proofs bound to it — survive
/// restarts. Falls back to a fresh in-memory key if the file is unreadable or
/// unwritable, in which case the identity (and any rating) is ephemeral.
fn load_or_create_client_key() -> libp2p::identity::Keypair {
    let path = client_key_path();
    if let Ok(bytes) = std::fs::read(&path) {
        let mut bytes = bytes;
        if let Ok(keypair) = libp2p::identity::Keypair::ed25519_from_bytes(bytes.as_mut_slice()) {
            return keypair;
        }
        warn!("{path:?} is not a valid ed25519 key; generating a new one");
    }
    let keypair = libp2p::identity::Keypair::generate_ed25519();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap_or_default();
    }
    if let Ok(ed25519) = keypair.clone().try_into_ed25519() {
        if std::fs::write(&path, ed25519.secret().as_ref()).is_err() {
            warn!(
                "Could not persist client key to {path:?}; identity is ephemeral this run"
            );
        }
    }
    keypair
}

async fn run_swarm(
    mut command_rx: UnboundedReceiver<NetCommand>,
    event_tx: UnboundedSender<NetEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut swarm = SwarmBuilder::with_existing_identity(load_or_create_client_key())
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        // Add the relay client transport so we can dial /p2p-circuit addresses
        // (M3). `with_relay_client` hands the matching `relay::client::Behaviour`
        // to the `with_behaviour` closure below.
        .with_relay_client(noise::Config::new, yamux::Config::default)?
        .with_behaviour(
            |keypair,
             relay_client|
             -> Result<Behaviour, Box<dyn std::error::Error + Send + Sync>> {
                let peer_id = keypair.public().to_peer_id();
                Ok(Behaviour {
                    ping: ping::Behaviour::default(),
                    identify: identify::Behaviour::new(identify::Config::new(
                        AGENT_VERSION.to_string(),
                        keypair.public(),
                    )),
                    mdns: mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id)?,
                    game: cbor::Behaviour::new(
                        [(GAME_PROTOCOL, ProtocolSupport::Full)],
                        request_response::Config::default(),
                    ),
                    gateway: cbor::Behaviour::new(
                        [(GATEWAY_PROTOCOL, ProtocolSupport::Full)],
                        request_response::Config::default(),
                    ),
                    rendezvous: rendezvous::client::Behaviour::new(keypair.clone()),
                    kademlia: kad::Behaviour::new(peer_id, MemoryStore::new(peer_id)),
                    relay: relay_client,
                    dcutr: dcutr::Behaviour::new(peer_id),
                })
            },
        )?
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(u64::MAX)))
        .build();

    // Listen on all interfaces on an OS-assigned port. The actual address is
    // reported back through `NetEvent::Listening`.
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;
    info!("Local peer id: {:?}", swarm.local_peer_id());
    let _ = event_tx.send(NetEvent::LocalPeerId(*swarm.local_peer_id()));

    loop {
        tokio::select! {
            Some(command) = command_rx.recv() => match command {
                NetCommand::Dial(addr) => {
                    let dial_addr = addr.to_string();
                    if let Err(e) = swarm.dial(addr) {
                        info!("Dial {dial_addr} failed: {e:?}");
                        let _ = event_tx.send(NetEvent::Error(e.to_string()));
                    }
                }
                NetCommand::Listen(addr) => {
                    // A multiaddr ending in `/p2p-circuit` asks the relay
                    // transport to reserve a circuit on the named relay server
                    // (the gateway). Non-relayed addrs are rejected.
                    let listen_addr = addr.to_string();
                    if let Err(e) = swarm.listen_on(addr) {
                        info!("Listen on {listen_addr} failed: {e:?}");
                        let _ = event_tx.send(NetEvent::Error(e.to_string()));
                    }
                }
                NetCommand::SendRequest { peer, request } => {
                    swarm.behaviour_mut().game.send_request(&peer, request);
                }
                NetCommand::SendGatewayRequest { peer, request } => {
                    swarm.behaviour_mut().gateway.send_request(&peer, request);
                }
                NetCommand::RendezvousRegister { peer, namespace } => {
                    let ns = match libp2p::rendezvous::Namespace::new(namespace) {
                        Ok(ns) => ns,
                        Err(e) => {
                            warn!("Bad rendezvous namespace: {e:?}");
                            continue;
                        }
                    };
                    match swarm.behaviour_mut().rendezvous.register(ns, peer, Some(300)) {
                        Ok(_) => info!("Rendezvous registration sent to {peer}"),
                        Err(e) => warn!("Rendezvous registration failed: {e:?}"),
                    }
                }
                NetCommand::RendezvousDiscover { peer, namespace } => {
                    let ns = match libp2p::rendezvous::Namespace::new(namespace) {
                        Ok(ns) => ns,
                        Err(e) => {
                            warn!("Bad rendezvous namespace: {e:?}");
                            continue;
                        }
                    };
                    swarm.behaviour_mut().rendezvous.discover(Some(ns), None, None, peer);
                }
            },
            Some(event) = swarm.next() => match event {
                SwarmEvent::NewListenAddr { address, .. } => {
                    // `/p2p-circuit` addresses come from the relay client once a
                    // reservation is confirmed.
                    if is_circuit(&address) {
                        // Make the relayed address a trusted external address so
                        // the rendezvous client can publish it for discovery.
                        swarm.add_external_address(address.clone());
                        info!("Reserved relayed address {address:?}");
                        let _ = event_tx.send(NetEvent::RelayReservation {
                            relay_peer: *swarm.local_peer_id(),
                            success: true,
                        });
                    } else {
                        info!("Listening on {address:?}");
                        let _ = event_tx.send(NetEvent::Listening(address));
                    }
                }
                SwarmEvent::ConnectionEstablished {
                    peer_id, endpoint, ..
                } => {
                    info!("Connected to {peer_id}");
                    let _ = event_tx.send(NetEvent::PeerConnected(peer_id));
                    // A non-relayed path to `peer_id` means the punch went
                    // through (or the peer was directly reachable all along):
                    // report it so the game stops relying on the relay.
                    if !endpoint.is_relayed() {
                        let _ = event_tx.send(NetEvent::DirectConnected { peer: peer_id });
                    }
                }
                SwarmEvent::ConnectionClosed { peer_id, .. } => {
                    // A connection being closed doesn't mean the peer is gone
                    // (a redundant second connection may have been dropped), so
                    // only report it when nothing remains.
                    if !swarm.is_connected(&peer_id) {
                        info!("Disconnected from {peer_id}");
                        let _ = event_tx.send(NetEvent::PeerDisconnected(peer_id));
                    }
                }
                SwarmEvent::Behaviour(behaviour_event) => match behaviour_event {
                    BehaviourEvent::Ping(libp2p::ping::Event {
                        peer,
                        result: Ok(rtt),
                        ..
                    }) => {
                        let _ = event_tx.send(NetEvent::PeerPinged { peer, rtt });
                    }
                    BehaviourEvent::Identify(identify::Event::Received { peer_id, info, .. }) => {
                        let _ = event_tx.send(NetEvent::Identity {
                            peer: peer_id,
                            agent: info.protocol_version.clone(),
                        });
                    }
                    BehaviourEvent::Game(event) => match event {
                        request_response::Event::Message {
                            peer,
                            message:
                                request_response::Message::Request { request, channel, .. },
                            ..
                        } => {
                            // Inbound requests are re-triggered into the game,
                            // which mirrors its own state back on its next
                            // snapshot push. The Ack satisfies the protocol.
                            let _ = swarm
                                .behaviour_mut()
                                .game
                                .send_response(channel, GameResponse::Ack);
                            // When a host match is running, feed the guest's
                            // snapshot straight into the simulation thread so
                            // the host keeps playing (and sending snapshots)
                            // even if its window is minimized/occluded and
                            // Bevy stops updating. Snapshot `seq` is monotonic
                            // per sender, so the guest can simply drop stale
                            // arrivals.
                            if let GameRequest::State(snapshot) = &request
                                && let Some(sink) = remote_paddle_sink()
                            {
                                let _ = sink.send(SimCommand::RemotePaddle(snapshot.player_paddle.y));
                            }
                            let _ = event_tx.send(NetEvent::GameRequest { peer, request });
                        }
                        request_response::Event::Message {
                            peer,
                            message: request_response::Message::Response { response, .. },
                            ..
                        } => {
                            let _ = event_tx.send(NetEvent::GameResponse { peer, response });
                        }
                        request_response::Event::OutboundFailure { peer, error, .. } => {
                            info!("Request to {peer} failed: {error:?}");
                        }
                        other => {
                            info!("Game event: {other:?}");
                        }
                    },
                    BehaviourEvent::Gateway(event) => match event {
                        request_response::Event::Message {
                            peer,
                            message:
                                request_response::Message::Request { request, channel, .. },
                            ..
                        } => {
                            // The gateway is normally the responder only; if it
                            // ever pushes a request, ack it generically.
                            let _ = swarm
                                .behaviour_mut()
                                .gateway
                                .send_response(channel, GatewayResponse::Pong);
                            let _ = event_tx.send(NetEvent::GatewayRequest { peer, request });
                        }
                        request_response::Event::Message {
                            peer,
                            message: request_response::Message::Response { response, .. },
                            ..
                        } => {
                            let _ = event_tx.send(NetEvent::GatewayResponse { peer, response });
                        }
                        request_response::Event::OutboundFailure { peer, error, .. } => {
                            warn!("Gateway request to {peer} failed: {error:?}");
                        }
                        other => {
                            info!("Gateway event: {other:?}");
                        }
                    },
                    BehaviourEvent::Rendezvous(event) => match event {
                        rendezvous::client::Event::Discovered {
                            registrations,
                            rendezvous_node,
                            ..
                        } => {
                            let peers = registrations
                                .into_iter()
                                .map(|reg| (reg.record.peer_id(), reg.record.addresses().to_vec()))
                                .collect();
                            let _ = event_tx.send(NetEvent::RendezvousDiscovered { peers });
                            info!("{rendezvous_node}: rendezvous discovery returned peers");
                        }
                        rendezvous::client::Event::Registered { rendezvous_node, .. } => {
                            info!("Registered with rendezvous node {rendezvous_node}");
                        }
                        event => info!("Rendezvous event: {event:?}"),
                    },
                    BehaviourEvent::Kademlia(event) => if let kad::Event::OutboundQueryProgressed { result, .. } = event {
                        info!("Kademlia query progress: {result:?}");
                    },
                    BehaviourEvent::Relay(event) => match event {
                        relay::client::Event::ReservationReqAccepted { relay_peer_id, .. } => {
                            info!("Relay reservation accepted by {relay_peer_id}");
                            let _ = event_tx.send(NetEvent::RelayReservation {
                                relay_peer: relay_peer_id,
                                success: true,
                            });
                        }
                        relay::client::Event::OutboundCircuitEstablished { .. } => {
                            info!("Relay circuit established");
                        }
                        relay::client::Event::InboundCircuitEstablished { src_peer_id, .. } => {
                            info!("Inbound relay circuit from {src_peer_id}");
                        }
                    },
                    BehaviourEvent::Dcutr(event) => {
                        if event.result.is_ok() {
                            info!("Direct connection (DCUtR) established with {}", event.remote_peer_id);
                            let _ = event_tx.send(NetEvent::DcutrEstablished {
                                peer: event.remote_peer_id,
                            });
                        } else {
                            info!(
                                "Could not hole-punch with {}: {:?}",
                                event.remote_peer_id,
                                event.result.err()
                            );
                        }
                    }
                    BehaviourEvent::Mdns(event) => match event {
                        mdns::Event::Discovered(peers) => {
                            for (peer, addr) in peers {
                                // mDNS only registers the address; we must dial
                                // it ourselves for a connection (and thus a
                                // roster entry) to exist.
                                if !swarm.is_connected(&peer) {
                                    match swarm.dial(addr) {
                                        Ok(()) => info!("Dialing discovered peer {peer}"),
                                        Err(e) => info!("Dialing {peer} failed: {e:?}"),
                                    }
                                }
                            }
                        }
                        mdns::Event::Expired(_) => {}
                    },
                    other => {
                        info!("Behaviour event: {other:?}");
                    }
                },
                SwarmEvent::OutgoingConnectionError { error, .. } => {
                    info!("Outgoing connection error: {error:?}");
                }
                SwarmEvent::ListenerError { error, .. } => {
                    info!("Listener error: {error:?}");
                }
                _ => {}
            },
        }
    }
}

/// Extra `Multiaddr` helpers used by the lobby to build gateway addresses.
/// True when the address is a relayed `/p2p-circuit` address.
pub fn is_circuit(addr: &Multiaddr) -> bool {
    addr.into_iter()
        .any(|p| matches!(p, libp2p::core::multiaddr::Protocol::P2pCircuit))
}

/// The agent version constant is shared with `protocol.rs`; keep a local alias
/// so `crate::protocol::GATEWAY_AGENT_VERSION` stays reachable from the menu.
pub(crate) fn is_gateway_agent(agent: &str) -> bool {
    agent.contains(GATEWAY_AGENT_VERSION)
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::multiaddr::Protocol;

    fn fixed_peer(marker: u8) -> PeerId {
        let mut bytes = [0u8; 34];
        bytes[0] = 0x00;
        bytes[1] = 0x20;
        bytes[2] = 0x08;
        bytes[3] = 0x01;
        bytes[4] = marker;
        PeerId::from_bytes(&bytes).expect("valid identity prototype peer id")
    }

    #[test]
    fn circuit_address_is_detected() {
        let mut relayed = Multiaddr::empty();
        relayed.push(Protocol::Ip4("203.0.113.10".parse().unwrap()));
        relayed.push(Protocol::Tcp(4001));
        relayed.push(Protocol::P2p(fixed_peer(0x11)));
        relayed.push(Protocol::P2pCircuit);
        relayed.push(Protocol::P2p(fixed_peer(0x22)));
        assert!(is_circuit(&relayed));
    }

    #[test]
    fn direct_address_is_not_a_circuit() {
        let mut direct = Multiaddr::empty();
        direct.push(Protocol::Ip4("192.168.1.50".parse().unwrap()));
        direct.push(Protocol::Tcp(43321));
        direct.push(Protocol::P2p(fixed_peer(0x33)));
        assert!(!is_circuit(&direct));
    }
}
