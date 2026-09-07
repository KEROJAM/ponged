use std::time::Duration;

use bevy::prelude::*;
use libp2p::futures::StreamExt;
use libp2p::request_response::{self, cbor, ProtocolSupport};
use libp2p::{
    identify, mdns, noise, ping, swarm::NetworkBehaviour, swarm::SwarmEvent, tcp, yamux, Multiaddr,
    PeerId, StreamProtocol, SwarmBuilder,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::protocol::{Request as GameRequest, Response as GameResponse};

/// Commands sent from the game to the network thread.
#[derive(Debug, Clone)]
pub enum NetCommand {
    Dial(Multiaddr),
    Listen(Multiaddr),
    /// Fire a game request at a specific peer via the request-response protocol.
    SendRequest { peer: PeerId, request: GameRequest },
}

/// Events emitted by the network thread into the Bevy world. Trigger them and
/// handle them with observers, e.g. `app.add_observer(on_peer_connected)`
/// where `fn on_peer_connected(ev: On<NetEvent>)`.
#[derive(Debug, Clone, Event)]
pub enum NetEvent {
    Listening(Multiaddr),
    PeerConnected(PeerId),
    PeerDisconnected(PeerId),
    PeerPinged { peer: PeerId, rtt: Duration },
    /// An inbound game request arrived (a response was already sent).
    GameRequest { peer: PeerId, request: GameRequest },
    /// A reply to a game request we sent.
    GameResponse { peer: PeerId, response: GameResponse },
    Error(String),
}

/// The libp2p protocol id used for game requests/responses.
const GAME_PROTOCOL: StreamProtocol = StreamProtocol::new("/pong/state/1.0.0");

/// Sub-behaviours of the swarm. The `#[derive(NetworkBehaviour)]` macro wires
/// them together so a single `fn next()` drives all of them. The generated
/// event enum is called `BehaviourEvent` and has one variant per field
/// (`Ping`, `Identify`, `Mdns`, `Game`).
#[derive(NetworkBehaviour)]
struct Behaviour {
    ping: ping::Behaviour,
    identify: identify::Behaviour,
    mdns: mdns::tokio::Behaviour,
    game: cbor::Behaviour<GameRequest, GameResponse>,
}

/// Bridge between Bevy (sync, single-threaded) and the tokio swarm task.
#[derive(Resource)]
pub struct NetChannels {
    pub commands: UnboundedSender<NetCommand>,
    events: std::sync::Mutex<UnboundedReceiver<NetEvent>>,
}

pub struct NetworkingPlugin;

impl Plugin for NetworkingPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, setup).add_systems(Update, poll_net_events);
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
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Runtime::new() {
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

async fn run_swarm(
    mut command_rx: UnboundedReceiver<NetCommand>,
    event_tx: UnboundedSender<NetEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut swarm = SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(tcp::Config::default(), noise::Config::new, yamux::Config::default)?
        .with_behaviour(
            |keypair| -> Result<Behaviour, Box<dyn std::error::Error + Send + Sync>> {
                Ok(Behaviour {
                    ping: ping::Behaviour::default(),
                    identify: identify::Behaviour::new(identify::Config::new(
                        "/pong/1.0.0".to_string(),
                        keypair.public(),
                    )),
                    mdns: mdns::tokio::Behaviour::new(
                        mdns::Config::default(),
                        keypair.public().to_peer_id(),
                    )?,
                    game: cbor::Behaviour::new(
                        [(GAME_PROTOCOL, ProtocolSupport::Full)],
                        request_response::Config::default(),
                    ),
                })
            },
        )?
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(u64::MAX)))
        .build();

    // Listen on all interfaces on an OS-assigned port. The actual address is
    // reported back through `NetEvent::Listening`.
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;
    info!("Local peer id: {:?}", swarm.local_peer_id());

    loop {
        tokio::select! {
            Some(command) = command_rx.recv() => match command {
                NetCommand::Dial(addr) => {
                    if let Err(e) = swarm.dial(addr) {
                        let _ = event_tx.send(NetEvent::Error(e.to_string()));
                    }
                }
                NetCommand::Listen(addr) => {
                    if let Err(e) = swarm.listen_on(addr) {
                        let _ = event_tx.send(NetEvent::Error(e.to_string()));
                    }
                }
                NetCommand::SendRequest { peer, request } => {
                    swarm.behaviour_mut().game.send_request(&peer, request);
                }
            },
            Some(event) = swarm.next() => match event {
                SwarmEvent::NewListenAddr { address, .. } => {
                    info!("Listening on {address:?}");
                    let _ = event_tx.send(NetEvent::Listening(address));
                }
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    info!("Connected to {peer_id}");
                    let _ = event_tx.send(NetEvent::PeerConnected(peer_id));
                }
                SwarmEvent::ConnectionClosed { peer_id, .. } => {
                    info!("Disconnected from {peer_id}");
                    let _ = event_tx.send(NetEvent::PeerDisconnected(peer_id));
                }
                SwarmEvent::Behaviour(behaviour_event) => match behaviour_event {
                    BehaviourEvent::Ping(libp2p::ping::Event {
                        peer,
                        result: Ok(rtt),
                        ..
                    }) => {
                        let _ = event_tx.send(NetEvent::PeerPinged { peer, rtt });
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
                    other => {
                        info!("Behaviour event: {other:?}");
                    }
                },
                _ => {}
            },
        }
    }
}