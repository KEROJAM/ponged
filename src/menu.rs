use bevy::prelude::*;
use libp2p::core::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};

use super::AppState;
use crate::history::MatchHistory;
use crate::networking::{short_peer, GatewayState, NetChannels, NetCommand, NetEvent};
use crate::networking_demo::{IsHost, Peers, RemoteWorld};
use crate::sim::{self, MatchSim};
use proyecto_final::protocol::{GatewayRequest, GatewayResponse, Request as GameRequest};

/// Default gateway address to dial. Override with `PONG_GATEWAY`.
const GATEWAY_DEFAULT_ADDR: &str = "/ip4/127.0.0.1/tcp/4001";

/// What the local player is currently doing on the matchmaking screen.
#[derive(Resource, Clone, Copy, Default, PartialEq, Eq)]
pub enum MatchIntent {
    #[default]
    Idle,
    Hosting,
}

/// Who we are (or will be) playing against. `None` while in the menu.
#[derive(Resource, Default)]
pub struct Opponent(pub Option<PeerId>);

/// A match request that arrived while we were already mid-game. We fall back to
/// the menu and finish joining (fresh round) on the next frame.
#[derive(Resource, Default)]
pub struct PendingMatch(pub Option<PeerId>);

/// Our own `PeerId`, reported asynchronously by the swarm.
#[derive(Resource, Default)]
pub struct LocalPeerId(pub Option<PeerId>);

/// The opponent the gateway matched us with; we dial them and auto-challenge
/// on connect.
#[derive(Resource, Default)]
pub struct GatewayMatch(pub Option<PeerId>);

// --- UI marker components -------------------------------------------------

#[derive(Component)]
pub struct MenuRoot;
#[derive(Component)]
pub struct YourIdText;
#[derive(Component)]
pub struct StatusText;
#[derive(Component)]
pub struct GatewayStatusText;
#[derive(Component)]
pub struct HostButton;
#[derive(Component)]
pub struct GatewayButton;
#[derive(Component)]
pub struct JoinQueueButton;
#[derive(Component)]
pub struct LeaveQueueButton;
#[derive(Component)]
pub struct PlayerList;
/// One entry in the player list; holds the peer that button challenges.
#[derive(Component)]
pub struct ChallengeButton(PeerId);
#[derive(Component)]
pub struct HistoryList;

/// Identifies which status line a `Text` node belongs to, so `update_menu` can
/// address all of them through a single query.
#[derive(Clone, Copy, Component, PartialEq, Eq)]
pub enum MenuLabel {
    You,
    Status,
    Gateway,
}

/// Identifies which menu button a UI node is, so `update_menu` can find all
/// four through a single query (keeps the system under Bevy's 16-param limit).
#[derive(Clone, Copy, Component)]
pub enum MenuButton {
    Host,
    ConnectGateway,
    JoinQueue,
    LeaveQueue,
}

/// Tracks what the player list currently shows, so we rebuild it when the set
/// of connected peers changes (regardless of change-detection timing).
#[derive(Resource, Default)]
pub struct PlayerListState(Vec<PeerId>);

/// Tracks the rendered revision of the match history panel.
#[derive(Resource, Default)]
pub struct HistoryRender {
    pub rev: u32,
}

const ACCENT: Color = Color::srgb(0.15, 0.55, 0.95);
const GREEN: Color = Color::srgb(0.2, 0.7, 0.35);
const DIM: Color = Color::srgb(0.85, 0.85, 0.9);

/// Builds the matchmaking screen.
pub fn spawn_menu(mut commands: Commands) {
    commands.spawn((
        MenuRoot,
        Node {
            width: percent(100.),
            height: percent(100.),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            row_gap: px(12.),
            ..default()
        },
        children![
            (
                Text::new("P O N G"),
                TextFont::from_font_size(88.0),
                TextColor(Color::WHITE),
                TextLayout::justify(Justify::Center),
            ),
            (YourIdText, MenuLabel::You, Text::new("You are ..."), TextFont::from_font_size(16.0), TextColor(DIM)),
            (StatusText, MenuLabel::Status, Text::new("Scanning the local network..."), TextFont::from_font_size(20.0), TextColor(DIM)),
            (GatewayStatusText, MenuLabel::Gateway, Text::new("Gateway: not connected"), TextFont::from_font_size(16.0), TextColor(DIM)),
            (
                Text::new("How to play"),
                TextFont::from_font_size(13.0),
                TextColor(DIM),
                TextLayout::justify(Justify::Center),
            ),
            (
                HostButton,
                MenuButton::Host,
                Button,
                Node {
                    min_width: px(220.),
                    padding: UiRect::axes(px(22.), px(8.)),
                    border: UiRect::all(px(2.)),
                    ..default()
                },
                BackgroundColor(ACCENT),
                BorderColor::all(Color::WHITE),
                children![(
                    Text::new("Host a match"),
                    TextFont::from_font_size(22.0),
                    TextColor(Color::WHITE),
                )],
            ),
            (
                Node {
                    flex_direction: FlexDirection::Row,
                    align_items: AlignItems::Center,
                    justify_content: JustifyContent::Center,
                    column_gap: px(10.),
                    ..default()
                },
                children![
                    (
                        GatewayButton,
                        MenuButton::ConnectGateway,
                        Button,
                        Node {
                            min_width: px(190.),
                            padding: UiRect::axes(px(16.), px(6.)),
                            border: UiRect::all(px(2.)),
                            ..default()
                        },
                        BackgroundColor(GREEN),
                        BorderColor::all(Color::WHITE),
                        children![(
                            Text::new("Connect to gateway"),
                            TextFont::from_font_size(18.0),
                            TextColor(Color::WHITE),
                        )],
                    ),
                    (
                        JoinQueueButton,
                        MenuButton::JoinQueue,
                        Button,
                        Node {
                            min_width: px(190.),
                            padding: UiRect::axes(px(16.), px(6.)),
                            border: UiRect::all(px(2.)),
                            ..default()
                        },
                        BackgroundColor(ACCENT),
                        BorderColor::all(Color::WHITE),
                        children![(
                            Text::new("Join WAN queue"),
                            TextFont::from_font_size(18.0),
                            TextColor(Color::WHITE),
                        )],
                    ),
                    (
                        LeaveQueueButton,
                        MenuButton::LeaveQueue,
                        Button,
                        Node {
                            min_width: px(150.),
                            padding: UiRect::axes(px(16.), px(6.)),
                            border: UiRect::all(px(2.)),
                            ..default()
                        },
                        BackgroundColor(Color::srgb(0.65, 0.2, 0.2)),
                        BorderColor::all(Color::WHITE),
                        children![(
                            Text::new("Leave queue"),
                            TextFont::from_font_size(18.0),
                            TextColor(Color::WHITE),
                        )],
                    ),
                ],
            ),
            (
                Text::new("Players found:"),
                TextFont::from_font_size(18.0),
                TextColor(DIM),
            ),
            (
                PlayerList,
                Node {
                    flex_direction: FlexDirection::Column,
                    align_items: AlignItems::Center,
                    row_gap: px(6.),
                    ..default()
                },
            ),
            (
                Text::new("Match history & ranking:"),
                TextFont::from_font_size(18.0),
                TextColor(DIM),
            ),
            (
                HistoryList,
                Node {
                    flex_direction: FlexDirection::Column,
                    align_items: AlignItems::Center,
                    row_gap: px(4.),
                    ..default()
                },
            ),
        ],
    ));
}

/// Removes the matchmaking screen (children despawn with the root).
pub fn despawn_menu(mut commands: Commands, menu: Query<Entity, With<MenuRoot>>) {
    for entity in &menu {
        commands.entity(entity).despawn();
    }
}

/// Records our own peer id for display in the menu.
pub fn on_local_peer_id(ev: On<NetEvent>, mut local: ResMut<LocalPeerId>) {
    if let NetEvent::LocalPeerId(id) = ev.event() {
        local.0 = Some(*id);
    }
}

/// Learns about the gateway via `identify`: gets its `PeerId` and asks for a
/// relay reservation (NAT traversal path through the gateway).
pub fn on_identity(
    ev: On<NetEvent>,
    mut gateway: ResMut<GatewayState>,
    channels: Res<NetChannels>,
) {
    let NetEvent::Identity { peer, agent } = ev.event() else {
        return;
    };
    if !crate::networking::is_gateway_agent(agent) {
        return;
    }
    info!("Identified gateway {peer}");
    gateway.peer = Some(*peer);
    gateway.connected = true;
    // Reserve a relay circuit through the gateway: listen on the combined
    // "<gateway addr>/p2p/<gateway>/p2p-circuit" address.
    if let Some(mut addr) = gateway.base_addr() {
        addr.push(Protocol::P2p(*peer));
        addr.push(Protocol::P2pCircuit);
        let _ = channels.commands.send(NetCommand::Listen(addr));
    }
}

/// Once the relay reservation is confirmed, register with the gateway and
/// publish ourselves on its rendezvous server.
pub fn on_relay_reservation(
    ev: On<NetEvent>,
    mut gateway: ResMut<GatewayState>,
    local: Res<LocalPeerId>,
    channels: Res<NetChannels>,
) {
    let NetEvent::RelayReservation { relay_peer, success } = ev.event() else {
        return;
    };
    if !success {
        return;
    }
    if gateway.peer.is_some_and(|p| p == *relay_peer) {
        gateway.reserved = true;
        info!("Relay reservation ready on gateway {relay_peer}");
        if let Some(local) = local.0 {
            let username = format!("player-{}", short_peer(local));
            let _ = channels.commands.send(NetCommand::SendGatewayRequest {
                peer: *relay_peer,
                request: GatewayRequest::Register {
                    username: username.clone(),
                },
            });
            // Publish on the rendezvous namespace so other clients can find us.
            let _ = channels.commands.send(NetCommand::RendezvousRegister {
                peer: *relay_peer,
                namespace: "/pong/all".to_string(),
            });
        }
    }
}

/// Consumes gateway matchmaking replies (M6 → M5).
pub fn on_gateway_response(
    ev: On<NetEvent>,
    mut gateway: ResMut<GatewayState>,
    channels: Res<NetChannels>,
) {
    let NetEvent::GatewayResponse { peer, response } = ev.event() else {
        return;
    };
    match response {
        GatewayResponse::Registered { username, rating } => {
            info!("Registered on gateway as {username} (rating {rating})");
            gateway.queued = false;
            let _ = channels.commands.send(NetCommand::SendGatewayRequest {
                peer: *peer,
                request: GatewayRequest::QueueMatch,
            });
        }
        GatewayResponse::Queued { position } => {
            info!("In matchmaking queue (position {position})");
            gateway.queued = true;
        }
        GatewayResponse::Dequeued => {
            info!("Left the matchmaking queue");
            gateway.queued = false;
        }
        GatewayResponse::Rating { rating } => {
            info!("Gateway rating is now {rating}");
        }
        GatewayResponse::Pong => {}
        GatewayResponse::Error(err) => {
            warn!("Gateway error: {err}");
        }
    }
}

/// Consumes gateway *requests* — currently the `MatchFound` push announcing a
/// reserved match (M6 → M5).
pub fn on_gateway_request(
    ev: On<NetEvent>,
    mut matched: ResMut<GatewayMatch>,
    mut intent: ResMut<MatchIntent>,
    channels: Res<NetChannels>,
) {
    let NetEvent::GatewayRequest { peer, request } = ev.event() else {
        return;
    };
    match request {
        GatewayRequest::MatchFound { opponent, addresses } => {
            let Ok(opponent_peer) = opponent.parse::<PeerId>() else {
                warn!("Gateway sent unparseable opponent id: {opponent}");
                return;
            };
            info!("Gateway matched us with {opponent_peer}; dialing via {addresses:?}");
            matched.0 = Some(opponent_peer);
            *intent = MatchIntent::Hosting;
            for addr in addresses {
                if let Ok(multiaddr) = addr.parse::<Multiaddr>() {
                    let _ = channels.commands.send(NetCommand::Dial(multiaddr));
                }
            }
        }
        _ => {
            debug!("Gateway request from {peer}: {request:?}");
        }
    }
}

/// Dial newly discovered rendezvous peers so they land in the roster.
pub fn on_rendezvous_discovered(
    ev: On<NetEvent>,
    peers: Res<Peers>,
    channels: Res<NetChannels>,
) {
    let NetEvent::RendezvousDiscovered { peers: found } = ev.event() else {
        return;
    };
    for (peer, addrs) in found {
        if peers.0.contains(peer) {
            continue;
        }
        info!("Rendezvous found {peer}");
        for addr in addrs {
            let _ = channels.commands.send(NetCommand::Dial(addr.clone()));
        }
    }
}

/// Handles inbound matchmaking messages. Match requests are accepted in any
/// state; if we're already in a match, we tear that round down and start a
/// fresh one with the challenger right after.
#[allow(clippy::too_many_arguments)]
pub fn on_game_request(
    ev: On<NetEvent>,
    state: Res<State<AppState>>,
    mut next: ResMut<NextState<AppState>>,
    mut opponent: ResMut<Opponent>,
    mut intent: ResMut<MatchIntent>,
    mut pending: ResMut<PendingMatch>,
    local: Res<LocalPeerId>,
    mut is_host: ResMut<IsHost>,
    channels: Res<NetChannels>,
    mut commands: Commands,
) {
    let NetEvent::GameRequest { peer, request } = ev.event() else {
        return;
    };

    match request {
        GameRequest::InviteToPlay => {
            info!("{peer} invited us to play");
            opponent.0 = Some(*peer);
            *intent = MatchIntent::Idle;
            // Tell the challenger the match is on.
            let _ = channels.commands.send(NetCommand::SendRequest {
                peer: *peer,
                request: GameRequest::MatchStart,
            });
            start_match(*peer, &state, &mut next, &mut pending, &local, &mut is_host);
        }
        GameRequest::MatchStart => {
            info!("{peer} accepted our challenge");
            opponent.0 = Some(*peer);
            *intent = MatchIntent::Idle;
            start_match(*peer, &state, &mut next, &mut pending, &local, &mut is_host);
        }
        GameRequest::MigrateHost(snapshot) => {
            // M4/M7: the host hands us authority. Take over the simulation
            // from its final authoritative state without leaving Playing.
            info!("{peer} transferred host authority to us");
            opponent.0 = Some(*peer);
            *intent = MatchIntent::Idle;
            is_host.0 = true;
            if *state == AppState::Playing {
                sim::start_seeded_match_sim(&mut commands, &is_host, &opponent, &channels, Some(*snapshot));
                let _ = channels.commands.send(NetCommand::SendRequest {
                    peer: *peer,
                    request: GameRequest::HostMigrated,
                });
            } else {
                next.set(AppState::Playing);
            }
        }
        GameRequest::HostMigrated => {
            info!("{peer} confirmed host migration");
        }
        _ => {}
    }
}

/// Enters `Playing` now, or if a round is already running, queues a fresh start
/// for the next frame (a Menu transition tears the old round down cleanly).
fn start_match(
    peer: PeerId,
    state: &State<AppState>,
    next: &mut NextState<AppState>,
    pending: &mut PendingMatch,
    local: &LocalPeerId,
    is_host: &mut IsHost,
) {
    // Deterministic choice so both sides agree on who simulates the ball and
    // score: whichever peer id is smaller hosts.
    is_host.0 = match local.0 {
        Some(own) => own.to_bytes() < peer.to_bytes(),
        None => true,
    };
    info!("Playing as {}", if is_host.0 { "host" } else { "client" });

    if *state == AppState::Playing {
        pending.0 = Some(peer);
        next.set(AppState::Menu);
    } else {
        next.set(AppState::Playing);
    }
}

/// Finishes joining a match that was requested while we were playing.
pub fn enter_pending_match(mut pending: ResMut<PendingMatch>, mut next: ResMut<NextState<AppState>>) {
    if let Some(peer) = pending.0.take() {
        info!("Starting fresh match with {peer}");
        next.set(AppState::Playing);
    }
}

/// Leave the current match (ESCAP) and return to the menu. If we are the host,
/// hand the current authoritative state to the guest first (M4/M7) so the
/// match can survive our departure.
#[allow(clippy::too_many_arguments)]
pub fn leave_match(
    keyboard_input: Res<ButtonInput<KeyCode>>,
    state: Res<State<AppState>>,
    is_host: Res<IsHost>,
    mut next: ResMut<NextState<AppState>>,
    opponent: Res<Opponent>,
    mut pending: ResMut<PendingMatch>,
    sim: Option<Res<MatchSim>>,
    channels: Res<NetChannels>,
) {
    if !keyboard_input.just_pressed(KeyCode::Escape) {
        return;
    }
    if *state == AppState::Playing
        && is_host.0
        && let Some(sim) = &sim
        && let Some(peer) = opponent.0
    {
        let snapshot = sim::build_host_snapshot(sim);
        info!("Migrating host to {peer}");
        let _ = channels.commands.send(NetCommand::SendRequest {
            peer,
            request: GameRequest::MigrateHost(snapshot),
        });
    }
    info!("Leaving the match");
    // Opponent stays set on purpose: `history::record_match` runs on
    // `OnExit(Playing)` and needs the rival + final score to persist.
    pending.0 = None;
    next.set(AppState::Menu);
}

/// If the opponent drops, abandon the match (and any queued rematch) — unless
/// we are the guest and the HOST dropped, in which case we take over the
/// simulation (M7) instead of returning to the menu.
#[allow(clippy::too_many_arguments)]
pub fn on_peer_disconnected(
    ev: On<NetEvent>,
    state: Res<State<AppState>>,
    mut next: ResMut<NextState<AppState>>,
    mut opponent: ResMut<Opponent>,
    mut pending: ResMut<PendingMatch>,
    mut is_host: ResMut<IsHost>,
    world: Res<RemoteWorld>,
    sim: Option<Res<MatchSim>>,
    channels: Res<NetChannels>,
    mut commands: Commands,
) {
    let NetEvent::PeerDisconnected(peer) = ev.event() else {
        return;
    };
    if opponent.0 != Some(*peer) {
        return;
    }

    // M7: the host vanished → the guest becomes the host and resumes from the
    // last authoritative snapshot. `seq` continues from the seed so promoted
    // snapshots stay strictly increasing.
    if *state == AppState::Playing && !is_host.0 && sim.is_none() {
        info!("Opponent {peer} disconnected; migrating to host");
        is_host.0 = true;
        let seed = world.curr;
        sim::start_seeded_match_sim(&mut commands, &is_host, &opponent, &channels, seed);
        return;
    }

    info!("Opponent {peer} disconnected, back to menu");
    pending.0 = None;
    if *state == AppState::Playing {
        // Opponent stays set so `history::record_match` (OnExit) persists the
        // result of the interrupted match.
        next.set(AppState::Menu);
    } else {
        opponent.0 = None;
    }
    let _ = channels;
}

fn set_label(labels: &mut Query<(&mut Text, &MenuLabel)>, kind: MenuLabel, text: String) {
    for (mut label, node_kind) in labels.iter_mut() {
        if *node_kind == kind {
            label.0 = text.clone();
        }
    }
}

/// Drives the menu: handles button presses and keeps the player list fresh.
#[allow(clippy::too_many_arguments)]
pub fn update_menu(
    mut commands: Commands,
    peers: Res<Peers>,
    local: Res<LocalPeerId>,
    mut gateway: ResMut<GatewayState>,
    history: Res<MatchHistory>,
    mut labels: Query<(&mut Text, &MenuLabel)>,
    buttons: Query<(&Interaction, &MenuButton)>,
    challenges: Query<(&Interaction, &ChallengeButton)>,
    player_list: Single<Entity, With<PlayerList>>,
    history_list: Single<Entity, With<HistoryList>>,
    mut intent: ResMut<MatchIntent>,
    mut list_state: ResMut<PlayerListState>,
    mut render_state: ResMut<HistoryRender>,
    channels: Res<NetChannels>,
) {
    set_label(
        &mut labels,
        MenuLabel::You,
        match local.0 {
            Some(id) => format!("You are {id}"),
            None => "You are ...".to_string(),
        },
    );

    set_label(
        &mut labels,
        MenuLabel::Status,
        match *intent {
            MatchIntent::Hosting => "Hosting: waiting for an opponent to pick you...".to_string(),
            MatchIntent::Idle if peers.0.is_empty() => {
                "No players found yet. Start a second instance on this network or join the WAN queue."
                    .to_string()
            }
            MatchIntent::Idle => "Pick an opponent below to start a match.".to_string(),
        },
    );

    set_label(
        &mut labels,
        MenuLabel::Gateway,
        match (gateway.connected, gateway.reserved, gateway.queued) {
            (false, _, _) => "Gateway: not connected".to_string(),
            (true, false, _) => "Gateway: connected, reserving relay...".to_string(),
            (true, true, false) => "Gateway: connected. Ready — join the queue.".to_string(),
            (true, true, true) => {
                "Gateway: in matchmaking queue. Waiting for a rival...".to_string()
            }
        },
    );

    // --- Buttons ---------------------------------------------------------
    for (interaction, button) in &buttons {
        if *interaction != Interaction::Pressed {
            continue;
        }
        match button {
            MenuButton::Host => {
                *intent = MatchIntent::Hosting;
                info!("Hosting a match");
            }
            MenuButton::ConnectGateway => {
                let addr: Multiaddr = std::env::var("PONG_GATEWAY")
                    .unwrap_or_else(|_| GATEWAY_DEFAULT_ADDR.to_string())
                    .parse()
                    .unwrap_or_else(|e| {
                        warn!("Bad PONG_GATEWAY address: {e}");
                        GATEWAY_DEFAULT_ADDR
                            .to_string()
                            .parse()
                            .expect("default address is valid")
                    });
                info!("Connecting to gateway at {addr}");
                *gateway = GatewayState {
                    addr: Some(addr.clone()),
                    connected: false,
                    reserved: false,
                    queued: false,
                    peer: None,
                };
                let _ = channels.commands.send(NetCommand::Dial(addr));
            }
            MenuButton::JoinQueue => {
                if let Some(peer) = gateway.peer
                    && gateway.reserved
                {
                    info!("Joining gateway matchmaking queue");
                    let _ = channels.commands.send(NetCommand::SendGatewayRequest {
                        peer,
                        request: GatewayRequest::QueueMatch,
                    });
                }
            }
            MenuButton::LeaveQueue => {
                if let Some(peer) = gateway.peer {
                    info!("Leaving gateway matchmaking queue");
                    let _ = channels.commands.send(NetCommand::SendGatewayRequest {
                        peer,
                        request: GatewayRequest::LeaveQueue,
                    });
                }
            }
        }
    }

    for (interaction, button) in &challenges {
        if *interaction == Interaction::Pressed {
            info!("Challenging {}", button.0);
            *intent = MatchIntent::Idle;
            let _ = channels.commands.send(NetCommand::SendRequest {
                peer: button.0,
                request: GameRequest::InviteToPlay,
            });
        }
    }

    // --- Rebuild the opponent list whenever the peer set changes ----------
    if list_state.0 != peers.0 {
        rebuild_player_list(&mut commands, &player_list, &peers);
        list_state.0 = peers.0.clone();
    }

    // --- Rebuild the history panel when a new match was recorded ----------
    if render_state.rev != history.rev {
        render_state.rev = history.rev;
        rebuild_history(&mut commands, &history_list, &history);
    }
}

fn rebuild_player_list(commands: &mut Commands, list_entity: &Entity, peers: &Peers) {
    commands.entity(*list_entity).despawn_children();
    commands.entity(*list_entity).with_children(|parent| {
        for peer in &peers.0 {
            parent.spawn((
                ChallengeButton(*peer),
                Button,
                Node {
                    min_width: px(200.),
                    padding: UiRect::axes(px(16.), px(6.)),
                    border: UiRect::all(px(2.)),
                    ..default()
                },
                BackgroundColor(Color::srgb(0.2, 0.2, 0.25)),
                BorderColor::all(DIM),
                children![(
                    Text::new(format!("Play vs {}", short_peer(*peer))),
                    TextFont::from_font_size(20.0),
                    TextColor(Color::WHITE),
                )],
            ));
        }
    });
}

fn rebuild_history(commands: &mut Commands, list_entity: &Entity, history: &MatchHistory) {
    commands.entity(*list_entity).despawn_children();
    commands.entity(*list_entity).with_children(|parent| {
        if history.records.is_empty() {
            parent.spawn((
                Text::new("No matches played yet."),
                TextFont::from_font_size(16.0),
                TextColor(DIM),
            ));
            return;
        }
        let (wins, losses, draws) = history.wins_losses();
        parent.spawn((
            Text::new(format!("Record: {wins}W / {losses}L / {draws}D")),
            TextFont::from_font_size(18.0),
            TextColor(Color::WHITE),
        ));
        for rec in &history.records {
            let winner = if rec.my_score > rec.opp_score { "W" } else { "L" };
            parent.spawn((
                Text::new(format!(
                    "{winner}  {}-{}  vs {}  ({})",
                    rec.my_score,
                    rec.opp_score,
                    rec.rival,
                    if rec.was_host { "host" } else { "guest" },
                )),
                TextFont::from_font_size(14.0),
                TextColor(DIM),
            ));
        }
    });
}