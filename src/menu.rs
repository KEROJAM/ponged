use std::collections::HashMap;

use bevy::asset::AssetId;
use bevy::prelude::*;
use bevy::text::{EditableText, Font};
use bevy::input_focus::AutoFocus;
use libp2p::core::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};

use super::AppState;
use crate::history::MatchHistory;
use crate::networking::{GatewayState, NetChannels, NetCommand, NetEvent, short_peer};
use crate::networking_demo::{IsHost, Peers, RemoteWorld};
use crate::sim::{self, MatchSim};
use ponged::protocol::{GatewayRequest, GatewayResponse, Request as GameRequest};

/// Default gateway address to dial. Override with `PONG_GATEWAY`.
const GATEWAY_DEFAULT_ADDR: &str = "/ip4/127.0.0.1/tcp/4001";

/// World-space orbit for the network of players.
const ORBIT_CENTER: Vec2 = Vec2::new(240.0, 0.0);
const ORBIT_RADIUS: f32 = 130.0;
/// Base angular speed (rad/s) of the player bubbles around the hub.
const ORBIT_SPEED: f32 = 0.9;
/// Per-bubble speed is this factor times the base, so they drift apart over
/// time instead of moving in lockstep.
const ORBIT_SPEED_MIN: f32 = 0.7;
const ORBIT_SPEED_MAX: f32 = 1.3;
/// How strongly the central hub breathes (scale oscillation).
const HUB_PULSE_AMOUNT: f32 = 0.15;
/// How strongly each player bubble breathes while orbiting.
const BUBBLE_PULSE_AMOUNT: f32 = 0.18;
/// Orbital animation speed multiplier.
const ORBIT_ANIM_BASE: f32 = 2.0;
const NODE_LABEL_OFFSET: Vec2 = Vec2::new(0.0, 22.0);
/// How many player bubbles the graph keeps on screen at once.
const MAX_NODES: usize = 12;

const DIM: Color = Color::srgb(0.7, 0.7, 0.75);
const NODE_FONT: f32 = 16.0;

/// What the local player is currently doing on the matchmaking screen.
#[derive(Resource, Clone, Copy, Default, PartialEq, Eq)]
pub enum MatchIntent {
    #[default]
    Idle,
    Hosting,
}

/// Our display name. Loaded from the local settings DB, editable from the
/// options screen and sent to the gateway and to other players.
#[derive(Resource)]
pub struct Username(pub String);

impl Default for Username {
    fn default() -> Self {
        Username("Player".to_string())
    }
}

/// Display names learned from connected peers via `Request::Hello { name }`.
#[derive(Resource, Default)]
pub struct PeerNames(pub HashMap<PeerId, String>);

/// When true, "Jugar" keeps the hunt running: gateway queue plus automatic
/// invitations to every LAN player that appears.
#[derive(Resource, Default)]
pub struct AutoSearch(pub bool);

/// Whether the options overlay is on screen.
#[derive(Resource, Default)]
pub struct OptionsOpen(pub bool);

/// True until the player has picked a name on first launch.
#[derive(Resource, Default)]
pub struct NeedsOnboarding(pub bool);

/// How often we re-query the gateway's rendezvous server for players while
/// searching (M3), so new hosts appear in the roster in real time.
const DISCOVERY_INTERVAL_SECS: f32 = 5.0;

/// Periodic rendezvous discovery poll, active while "Jugar" is running.
#[derive(Resource)]
pub struct DiscoveryTimer(Timer);

impl Default for DiscoveryTimer {
    fn default() -> Self {
        DiscoveryTimer(Timer::from_seconds(
            DISCOVERY_INTERVAL_SECS,
            TimerMode::Repeating,
        ))
    }
}

/// Per-player orbit angle for the node network.
#[derive(Resource, Default)]
pub struct OrbitState(pub HashMap<PeerId, f32>);

/// Shared meshes/materials for the node network, created on menu entry.
#[derive(Resource)]
pub struct GraphAssets {
    dot: Handle<Mesh>,
    line: Handle<Mesh>,
    material: Handle<ColorMaterial>,
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

// --- Marker components -----------------------------------------------------

#[derive(Component)]
pub struct MenuRoot;
#[derive(Component)]
pub struct OptionsRoot;
#[derive(Component)]
pub struct OptionsInput;
#[derive(Component)]
pub struct OnboardingRoot;
#[derive(Component)]
pub struct OnboardingInput;
#[derive(Component)]
pub struct OnboardingButton;
#[derive(Component)]
pub struct MenuField;
#[derive(Component)]
pub struct NodeHub;
#[derive(Component)]
pub struct NodeHubLabel;
#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub struct NodeOf(pub PeerId);
#[derive(Component)]
pub struct NodeBubble;
#[derive(Component)]
pub struct NodeLink;
#[derive(Component)]
pub struct NodeLabel;
#[derive(Component)]
pub struct ButtonText;

/// Which text line a label node belongs to.
#[derive(Clone, Copy, Component, PartialEq, Eq)]
pub enum TextLine {
    You,
    Status,
    Gateway,
    Record,
}

/// The main staircase menu buttons.
#[derive(Clone, Copy, Component, PartialEq, Eq)]
pub enum MenuButton {
    Play,
    Options,
    Quit,
}

/// Buttons on the options overlay.
#[derive(Clone, Copy, Component, PartialEq, Eq)]
pub enum OptionsButton {
    Save,
    Back,
}

/// Loads persisted settings at startup and seeds the username resource.
pub fn load_settings(mut commands: Commands, mut history: ResMut<MatchHistory>) {
    history.ensure_open();
    let stored = history.load_username().filter(|n| !n.trim().is_empty());
    let username = stored.clone().unwrap_or_else(|| "Player".to_string());
    commands.insert_resource(Username(username));
    commands.insert_resource(NeedsOnboarding(stored.is_none()));
    info!("Local username: {:?}", stored);
}

/// Replaces Bevy's stock default font (a small Fira Mono subset that lacks the
/// accented Spanish characters) with the bundled DejaVu Sans, which covers
/// á é í ó ú ñ ¡ ¿ · … everywhere text is rendered.
pub fn install_default_font(mut fonts: ResMut<Assets<Font>>) {
    let Some(font) = fonts.get_mut_untracked(AssetId::default()) else {
        return;
    };
    const DEJAVU_SANS: &[u8] = include_bytes!("../assets/fonts/DejaVuSans.ttf");
    *font = Font::from_bytes(DEJAVU_SANS.to_vec());
}

/// Builds the matchmaking screen: transparent UI over the pong field (black
/// background), left-aligned title, staircase buttons and the player network.
pub fn spawn_menu(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    username: Res<Username>,
) {
    let gutter_mesh = meshes.add(Rectangle::new(800.0, 20.0));
    let net_mesh = meshes.add(Rectangle::new(4.0, 560.0));
    let dot = meshes.add(Circle::new(9.0));
    let hub_dot = meshes.add(Circle::new(13.0));
    let line = meshes.add(Rectangle::new(1.0, 1.0));
    let white = materials.add(Color::WHITE);

    commands.insert_resource(GraphAssets {
        dot: dot.clone(),
        line: line.clone(),
        material: white.clone(),
    });

    commands.spawn((
        MenuField,
        Mesh2d(gutter_mesh.clone()),
        MeshMaterial2d(white.clone()),
        Transform::from_translation(Vec3::new(0.0, 280.0, 0.0)),
    ));
    commands.spawn((
        MenuField,
        Mesh2d(gutter_mesh),
        MeshMaterial2d(white.clone()),
        Transform::from_translation(Vec3::new(0.0, -280.0, 0.0)),
    ));
    commands.spawn((
        MenuField,
        Mesh2d(net_mesh),
        MeshMaterial2d(white.clone()),
        Transform::from_translation(Vec3::new(0.0, 0.0, 0.0)),
    ));

    commands.spawn((
        NodeHub,
        Mesh2d(hub_dot),
        MeshMaterial2d(white.clone()),
        Transform::from_translation(ORBIT_CENTER.extend(0.0)),
    ));
    commands.spawn((
        NodeHubLabel,
        Text2d::new(username.0.clone()),
        TextFont::from_font_size(NODE_FONT),
        TextColor(Color::WHITE),
        Transform::from_translation((ORBIT_CENTER + Vec2::new(0.0, -34.0)).extend(0.0)),
    ));

    commands.spawn((
        MenuRoot,
        Node {
            width: percent(100.),
            height: percent(100.),
            flex_direction: FlexDirection::Column,
            justify_content: JustifyContent::Center,
            align_items: AlignItems::FlexStart,
            row_gap: px(8.),
            padding: UiRect::left(px(80.)),
            ..default()
        },
        children![
            (
                Text::new("ponged"),
                TextFont::from_font_size(96.0),
                TextColor(Color::WHITE),
                TextLayout::justify(Justify::Left),
            ),
            (
                TextLine::You,
                Text::new("Player"),
                TextFont::from_font_size(16.0),
                TextColor(DIM),
            ),
            (
                TextLine::Status,
                Text::new("Listo. Pulsa Jugar para buscar rivales (LAN + WAN)."),
                TextFont::from_font_size(18.0),
                TextColor(DIM),
            ),
            (
                TextLine::Gateway,
                Text::new("Gateway: no conectado"),
                TextFont::from_font_size(14.0),
                TextColor(DIM),
            ),
            menu_button(MenuButton::Play, "Jugar"),
            menu_button(MenuButton::Options, "Opciones"),
            menu_button(MenuButton::Quit, "Salir"),
            (
                TextLine::Record,
                Text::new(""),
                TextFont::from_font_size(14.0),
                TextColor(DIM),
            ),
        ],
    ));

    commands.spawn((
        OptionsRoot,
        Node {
            width: percent(100.),
            height: percent(100.),
            position_type: PositionType::Absolute,
            display: Display::None,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.72)),
        children![(
            Node {
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                row_gap: px(16.),
                padding: UiRect::all(px(36.)),
                border: UiRect::all(px(2.)),
                ..default()
            },
            BackgroundColor(Color::BLACK),
            BorderColor::all(Color::WHITE),
            children![
                (
                    Text::new("Opciones"),
                    TextFont::from_font_size(48.0),
                    TextColor(Color::WHITE),
                ),
                (
                    Text::new("Tu nombre:"),
                    TextFont::from_font_size(18.0),
                    TextColor(DIM),
                ),
                (
                    OptionsInput,
                    EditableText::new(username.0.clone()),
                    TextFont::from_font_size(24.0),
                    TextColor(Color::WHITE),
                    Node {
                        width: px(260.),
                        height: px(44.),
                        border: UiRect::all(px(2.)),
                        padding: UiRect::axes(px(10.), px(8.)),
                        ..default()
                    },
                    BorderColor::all(Color::WHITE),
                ),
                (
                    OptionsButton::Save,
                    Button,
                    Node {
                        min_width: px(180.),
                        padding: UiRect::axes(px(26.), px(10.)),
                        border: UiRect::all(px(2.)),
                        ..default()
                    },
                    BackgroundColor(Color::WHITE),
                    BorderColor::all(Color::WHITE),
                    children![(
                        ButtonText,
                        Text::new("Guardar"),
                        TextFont::from_font_size(22.0),
                        TextColor(Color::BLACK),
                    )],
                ),
                (
                    OptionsButton::Back,
                    Button,
                    Node {
                        min_width: px(180.),
                        padding: UiRect::axes(px(26.), px(10.)),
                        border: UiRect::all(px(2.)),
                        ..default()
                    },
                    BackgroundColor(Color::NONE),
                    BorderColor::all(Color::WHITE),
                    children![(
                        ButtonText,
                        Text::new("Volver"),
                        TextFont::from_font_size(22.0),
                        TextColor(Color::WHITE),
                    )],
                ),
            ],
        ),],
    ));

    commands.spawn((
        OnboardingRoot,
        Node {
            width: percent(100.),
            height: percent(100.),
            position_type: PositionType::Absolute,
            display: Display::None,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.72)),
        children![(
            Node {
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                row_gap: px(16.),
                padding: UiRect::all(px(36.)),
                border: UiRect::all(px(2.)),
                ..default()
            },
            BackgroundColor(Color::BLACK),
            BorderColor::all(Color::WHITE),
            children![
                (
                    Text::new("¡Bienvenido a PONG!"),
                    TextFont::from_font_size(48.0),
                    TextColor(Color::WHITE),
                ),
                (
                    Text::new("¿Cómo te llamas?"),
                    TextFont::from_font_size(18.0),
                    TextColor(DIM),
                ),
                (
                    OnboardingInput,
                    AutoFocus,
                    EditableText::new(username.0.clone()),
                    TextFont::from_font_size(24.0),
                    TextColor(Color::WHITE),
                    Node {
                        width: px(260.),
                        height: px(44.),
                        border: UiRect::all(px(2.)),
                        padding: UiRect::axes(px(10.), px(8.)),
                        ..default()
                    },
                    BorderColor::all(Color::WHITE),
                ),
                (
                    OnboardingButton,
                    Button,
                    Node {
                        min_width: px(180.),
                        padding: UiRect::axes(px(26.), px(10.)),
                        border: UiRect::all(px(2.)),
                        ..default()
                    },
                    BackgroundColor(Color::WHITE),
                    BorderColor::all(Color::WHITE),
                    children![(
                        ButtonText,
                        Text::new("Continuar"),
                        TextFont::from_font_size(22.0),
                        TextColor(Color::BLACK),
                    )],
                ),
            ],
        ),],
    ));
}

/// Width of each staircase button: "Jugar" is the longest and the rest step
/// down like a bar chart.
fn menu_button_width(action: MenuButton) -> f32 {
    match action {
        MenuButton::Play => 380.0,
        MenuButton::Options => 300.0,
        MenuButton::Quit => 220.0,
    }
}

fn menu_button(action: MenuButton, label: &str) -> impl Bundle {
    (
        action,
        Button,
        Node {
            width: px(menu_button_width(action)),
            padding: UiRect::axes(px(26.), px(10.)),
            border: UiRect::all(px(2.)),
            ..default()
        },
        BackgroundColor(Color::NONE),
        BorderColor::all(Color::WHITE),
        children![(
            ButtonText,
            Text::new(label),
            TextFont::from_font_size(26.0),
            TextColor(Color::WHITE),
        )],
    )
}

/// Removes the matchmaking screen, the field décor and the player network.
#[allow(clippy::type_complexity)]
pub fn despawn_menu(
    mut commands: Commands,
    menu: Query<
        Entity,
        Or<(
            With<MenuRoot>,
            With<OptionsRoot>,
            With<OnboardingRoot>,
            With<MenuField>,
            With<NodeHub>,
            With<NodeHubLabel>,
            With<NodeBubble>,
            With<NodeLink>,
            With<NodeLabel>,
        )>,
    >,
) {
    for entity in &menu {
        commands.entity(entity).despawn();
    }
    commands.remove_resource::<GraphAssets>();
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
    username: Res<Username>,
    channels: Res<NetChannels>,
) {
    let NetEvent::RelayReservation {
        relay_peer,
        success,
    } = ev.event()
    else {
        return;
    };
    if !success {
        return;
    }
    if gateway.peer.is_some_and(|p| p == *relay_peer) {
        gateway.reserved = true;
        info!("Relay reservation ready on gateway {relay_peer}");
        let _ = channels.commands.send(NetCommand::SendGatewayRequest {
            peer: *relay_peer,
            request: GatewayRequest::Register {
                username: username.0.clone(),
            },
        });
        let _ = channels.commands.send(NetCommand::RendezvousRegister {
            peer: *relay_peer,
            namespace: "/pong/all".to_string(),
        });
    }
}

/// Consumes gateway matchmaking replies (M6 → M5).
pub fn on_gateway_response(
    ev: On<NetEvent>,
    mut gateway: ResMut<GatewayState>,
    search: Res<AutoSearch>,
    channels: Res<NetChannels>,
) {
    let NetEvent::GatewayResponse { peer, response } = ev.event() else {
        return;
    };
    match response {
        GatewayResponse::Registered {
            username,
            rating,
            rank,
        } => {
            info!("Registered on gateway as {username} (rating {rating}, rank {rank})");
            gateway.rank = Some(rank.clone());
            gateway.rating = *rating;
            gateway.queued = false;
            if search.0 {
                let _ = channels.commands.send(NetCommand::SendGatewayRequest {
                    peer: *peer,
                    request: GatewayRequest::QueueMatch,
                });
            }
        }
        GatewayResponse::Queued { position } => {
            info!("In matchmaking queue (position {position})");
            gateway.queued = true;
        }
        GatewayResponse::Dequeued => {
            info!("Left the matchmaking queue");
            gateway.queued = false;
        }
        GatewayResponse::Rating { rating, rank } => {
            info!("Gateway rating is now {rating} (rank {rank})");
            gateway.rank = Some(rank.clone());
            gateway.rating = *rating;
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
        GatewayRequest::MatchFound {
            opponent,
            addresses,
        } => {
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
pub fn on_rendezvous_discovered(ev: On<NetEvent>, peers: Res<Peers>, channels: Res<NetChannels>) {
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

/// Re-queries the gateway's rendezvous server while "Jugar" is running so that
/// newly arrived players (WAN) show up in the roster without a restart. Fires
/// immediately when the search becomes ready, then every few seconds.
pub fn update_discovery(
    time: Res<Time>,
    mut timer: ResMut<DiscoveryTimer>,
    search: Res<AutoSearch>,
    gateway: Res<GatewayState>,
    channels: Res<NetChannels>,
    mut was_ready: Local<bool>,
) {
    timer.0.tick(time.delta());

    let peer = gateway.peer;
    let ready = search.0 && gateway.connected && gateway.reserved && peer.is_some();
    if !ready {
        timer.0.reset();
        *was_ready = false;
        return;
    }
    let fire = !*was_ready || timer.0.just_finished();
    *was_ready = true;
    if fire {
        let self_peer = peer.expect("ready implies gateway peer");
        let _ = channels.commands.send(NetCommand::RendezvousDiscover {
            peer: self_peer,
            namespace: "/pong/all".to_string(),
        });
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
    mut names: ResMut<PeerNames>,
    mut commands: Commands,
) {
    let NetEvent::GameRequest { peer, request } = ev.event() else {
        return;
    };

    match request {
        GameRequest::Hello { name } => {
            if !name.is_empty() {
                names.0.insert(*peer, name.clone());
            }
        }
        GameRequest::InviteToPlay => {
            info!("{peer} invited us to play");
            opponent.0 = Some(*peer);
            *intent = MatchIntent::Idle;
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
            info!("{peer} transferred host authority to us");
            opponent.0 = Some(*peer);
            *intent = MatchIntent::Idle;
            is_host.0 = true;
            if *state == AppState::Playing {
                sim::start_seeded_match_sim(
                    &mut commands,
                    &is_host,
                    &opponent,
                    &channels,
                    Some(*snapshot),
                );
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

/// Stops the automatic hunt and leaves the gateway queue when a match starts.
pub fn on_enter_playing(
    mut search: ResMut<AutoSearch>,
    gateway: Res<GatewayState>,
    channels: Res<NetChannels>,
) {
    search.0 = false;
    if gateway.queued
        && let Some(peer) = gateway.peer
    {
        let _ = channels.commands.send(NetCommand::SendGatewayRequest {
            peer,
            request: GatewayRequest::LeaveQueue,
        });
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
pub fn enter_pending_match(
    mut pending: ResMut<PendingMatch>,
    mut next: ResMut<NextState<AppState>>,
) {
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
        next.set(AppState::Menu);
    } else {
        opponent.0 = None;
    }
    let _ = channels;
}

// --- UI drivers ------------------------------------------------------------

fn set_text(labels: &mut Query<(&mut Text, &TextLine)>, kind: TextLine, text: String) {
    for (mut label, node_kind) in labels.iter_mut() {
        if *node_kind == kind {
            label.0 = text.clone();
        }
    }
}

/// Refreshes the menu text lines.
#[allow(clippy::too_many_arguments)]
pub fn update_menu(
    local: Res<LocalPeerId>,
    username: Res<Username>,
    search: Res<AutoSearch>,
    peers: Res<Peers>,
    gateway: Res<GatewayState>,
    matched: Res<GatewayMatch>,
    history: Res<MatchHistory>,
    mut labels: Query<(&mut Text, &TextLine)>,
) {
    set_text(
        &mut labels,
        TextLine::You,
        match local.0 {
            Some(id) => format!("{}  ·  {}", username.0, short_peer(id)),
            None => username.0.clone(),
        },
    );

    let visible = peers.0.iter().filter(|p| gateway.peer != Some(**p)).count();
    set_text(
        &mut labels,
        TextLine::Status,
        if search.0 {
            if visible == 0 {
                "Buscando rivales en LAN y WAN…".to_string()
            } else {
                format!(
                    "Buscando… {visible} jugador{} en el área.",
                    if visible == 1 { "" } else { "es" }
                )
            }
        } else {
            "Listo. Pulsa Jugar para buscar rivales (LAN + WAN).".to_string()
        },
    );

    set_text(
        &mut labels,
        TextLine::Gateway,
        gateway_line(&gateway, &matched),
    );

    let (wins, losses, draws) = history.wins_losses();
    set_text(
        &mut labels,
        TextLine::Record,
        if wins + losses + draws == 0 {
            String::new()
        } else {
            format!("Récord: {wins}V / {losses}D / {draws}E")
        },
    );
}

fn gateway_line(gateway: &GatewayState, matched: &GatewayMatch) -> String {
    if let Some(peer) = matched.0 {
        return format!("¡Emparejado! Conectando con {}", short_peer(peer));
    }
    match (gateway.connected, gateway.reserved, gateway.queued) {
        (false, _, _) => "Gateway: no conectado".to_string(),
        (true, false, _) => "Gateway: conectado, reservando relay…".to_string(),
        (true, true, false) => match &gateway.rank {
            Some(rank) => format!("Gateway: listo · rango {rank}"),
            None => "Gateway: listo".to_string(),
        },
        (true, true, true) => match &gateway.rank {
            Some(rank) => format!("Gateway: en cola · rango {rank}"),
            None => "Gateway: buscando rival…".to_string(),
        },
    }
}

/// Handles the main menu buttons (hover feedback + presses).
#[allow(clippy::too_many_arguments)]
pub fn update_buttons(
    mut buttons: Query<(&Interaction, &MenuButton, &Children, &mut BackgroundColor)>,
    mut button_texts: Query<&mut TextColor, (With<ButtonText>, Without<MenuButton>)>,
    mut search: ResMut<AutoSearch>,
    mut gateway: ResMut<GatewayState>,
    peers: Res<Peers>,
    username: Res<Username>,
    channels: Res<NetChannels>,
    mut options_open: ResMut<OptionsOpen>,
    mut exit: MessageWriter<AppExit>,
) {
    for (interaction, action, children, mut bg) in &mut buttons {
        let hovered = *interaction == Interaction::Hovered;
        bg.0 = if hovered { Color::WHITE } else { Color::NONE };
        for child in children {
            if let Ok(mut tc) = button_texts.get_mut(*child) {
                tc.0 = if hovered { Color::BLACK } else { Color::WHITE };
            }
        }
        if *interaction != Interaction::Pressed {
            continue;
        }
        match action {
            MenuButton::Play => {
                if search.0 {
                    search.0 = false;
                    if gateway.queued
                        && let Some(peer) = gateway.peer
                    {
                        let _ = channels.commands.send(NetCommand::SendGatewayRequest {
                            peer,
                            request: GatewayRequest::LeaveQueue,
                        });
                    }
                    info!("Search stopped");
                } else {
                    search.0 = true;
                    start_search(&mut gateway, &channels, &username, &peers);
                    info!("Searching for opponents (LAN + WAN)");
                }
            }
            MenuButton::Options => {
                options_open.0 = true;
            }
            MenuButton::Quit => {
                info!("Quitting");
                exit.write(AppExit::Success);
            }
        }
    }
}

fn start_search(
    gateway: &mut GatewayState,
    channels: &NetChannels,
    username: &Username,
    peers: &Peers,
) {
    let gw_addr =
        std::env::var("PONG_GATEWAY").unwrap_or_else(|_| GATEWAY_DEFAULT_ADDR.to_string());
    let addr: Multiaddr = gw_addr.parse().unwrap_or_else(|e| {
        warn!("Bad PONG_GATEWAY address: {e}");
        GATEWAY_DEFAULT_ADDR
            .parse()
            .expect("default address is valid")
    });

    match (gateway.peer, gateway.connected, gateway.reserved) {
        (None, _, _) => {
            *gateway = GatewayState {
                addr: Some(addr),
                connected: false,
                reserved: false,
                queued: false,
                peer: None,
                rank: None,
                rating: 0,
            };
            if let Some(target) = gateway.addr.clone() {
                let _ = channels.commands.send(NetCommand::Dial(target));
            }
        }
        (Some(peer), true, true) => {
            let _ = channels.commands.send(NetCommand::SendGatewayRequest {
                peer,
                request: GatewayRequest::Register {
                    username: username.0.clone(),
                },
            });
        }
        (Some(peer), true, false) => {
            if let Some(mut base) = gateway.base_addr() {
                base.push(Protocol::P2p(peer));
                base.push(Protocol::P2pCircuit);
                let _ = channels.commands.send(NetCommand::Listen(base));
            }
        }
        _ => {}
    }

    for peer in &peers.0 {
        if gateway.peer == Some(*peer) {
            continue;
        }
        let _ = channels.commands.send(NetCommand::SendRequest {
            peer: *peer,
            request: GameRequest::InviteToPlay,
        });
    }
}

/// Options overlay: save the username, or go back.
#[allow(clippy::too_many_arguments)]
pub fn update_options(
    mut root: Single<&mut Node, With<OptionsRoot>>,
    input: Single<&mut EditableText, With<OptionsInput>>,
    mut buttons: Query<(
        &Interaction,
        &OptionsButton,
        &Children,
        &mut BackgroundColor,
    )>,
    mut button_texts: Query<&mut TextColor, (With<ButtonText>, Without<OptionsButton>)>,
    mut username: ResMut<Username>,
    history: Res<MatchHistory>,
    search: Res<AutoSearch>,
    gateway: Res<GatewayState>,
    channels: Res<NetChannels>,
    mut options_open: ResMut<OptionsOpen>,
) {
    if !options_open.0 {
        root.display = Display::None;
        return;
    }
    root.display = Display::Flex;

    for (interaction, kind, children, mut bg) in &mut buttons {
        let hovered = *interaction == Interaction::Hovered;
        if *kind == OptionsButton::Save && hovered {
            bg.0 = Color::srgb(0.85, 0.85, 0.9);
        } else if *kind == OptionsButton::Back && hovered {
            bg.0 = Color::srgb(0.3, 0.3, 0.32);
        } else if *kind == OptionsButton::Save {
            bg.0 = Color::WHITE;
        } else {
            bg.0 = Color::NONE;
        }
        for child in children {
            if let Ok(mut tc) = button_texts.get_mut(*child) {
                tc.0 = if *kind == OptionsButton::Save && !hovered {
                    Color::BLACK
                } else {
                    Color::WHITE
                };
            }
        }
        if *interaction != Interaction::Pressed {
            continue;
        }
        match kind {
            OptionsButton::Save => {
                let name = input.value().to_string().trim().to_string();
                if !name.is_empty() && username.0 != name {
                    username.0 = name.clone();
                    history.save_username(&name);
                    if search.0
                        && let Some(peer) = gateway.peer
                    {
                        let _ = channels.commands.send(NetCommand::SendGatewayRequest {
                            peer,
                            request: GatewayRequest::Register { username: name },
                        });
                    }
                }
                options_open.0 = false;
            }
            OptionsButton::Back => {
                options_open.0 = false;
            }
        }
    }
}

/// First-run onboarding: asks for the player name, persists it and closes.
#[allow(clippy::too_many_arguments)]
pub fn update_onboarding(
    mut root: Single<&mut Node, With<OnboardingRoot>>,
    input: Single<&mut EditableText, With<OnboardingInput>>,
    button: Query<(&Interaction, &Children), With<OnboardingButton>>,
    mut button_texts: Query<&mut TextColor, (With<ButtonText>, Without<OnboardingButton>)>,
    keyboard: Res<ButtonInput<KeyCode>>,
    mut username: ResMut<Username>,
    history: Res<MatchHistory>,
    search: Res<AutoSearch>,
    gateway: Res<GatewayState>,
    channels: Res<NetChannels>,
    mut onboarding: ResMut<NeedsOnboarding>,
) {
    root.display = if onboarding.0 { Display::Flex } else { Display::None };
    if !onboarding.0 {
        return;
    }

    let mut submit = keyboard.just_pressed(KeyCode::Enter);
    for (interaction, children) in &button {
        let hovered = *interaction == Interaction::Hovered;
        for child in children {
            if let Ok(mut tc) = button_texts.get_mut(*child) {
                tc.0 = if hovered {
                    Color::srgb(0.85, 0.85, 0.9)
                } else {
                    Color::BLACK
                };
            }
        }
        if *interaction == Interaction::Pressed {
            submit = true;
        }
    }
    if !submit {
        return;
    }

    let name = input.value().to_string().trim().to_string();
    if name.is_empty() {
        return;
    }
    if username.0 != name {
        username.0 = name.clone();
        history.save_username(&name);
        if search.0
            && let Some(peer) = gateway.peer
        {
            let _ = channels.commands.send(NetCommand::SendGatewayRequest {
                peer,
                request: GatewayRequest::Register { username: name },
            });
        }
    }
    onboarding.0 = false;
}

// --- Player network --------------------------------------------------------

fn peer_hash(peer: PeerId) -> u32 {
    peer.to_bytes()
        .into_iter()
        .fold(2166136261u32, |acc, b| (acc ^ b as u32).wrapping_mul(16777619))
}

fn initial_angle(peer: PeerId) -> f32 {
    (peer_hash(peer) % 628) as f32 / 100.0
}

/// Each player orbits at its own (deterministic, stable) angular speed so the
/// cloud of bubbles swirls around the hub instead of rotating rigidly.
fn orbit_speed(peer: PeerId) -> f32 {
    ORBIT_SPEED
        * (ORBIT_SPEED_MIN
            + (peer_hash(peer) % 100) as f32 / 100.0 * (ORBIT_SPEED_MAX - ORBIT_SPEED_MIN))
}

fn spawn_node(commands: &mut Commands, assets: &GraphAssets, peer: PeerId, angle: f32) {
    let dir = Vec2::from_angle(angle);
    commands.spawn((
        NodeOf(peer),
        NodeBubble,
        Mesh2d(assets.dot.clone()),
        MeshMaterial2d(assets.material.clone()),
        Transform::from_translation((ORBIT_CENTER + dir * ORBIT_RADIUS).extend(0.0)),
    ));
    commands.spawn((
        NodeOf(peer),
        NodeLink,
        Mesh2d(assets.line.clone()),
        MeshMaterial2d(assets.material.clone()),
        Transform::from_translation((ORBIT_CENTER + dir * (ORBIT_RADIUS / 2.0)).extend(0.0))
            .with_rotation(Quat::from_rotation_z(angle))
            .with_scale(Vec3::new(ORBIT_RADIUS, 2.0, 1.0)),
    ));
    commands.spawn((
        NodeOf(peer),
        NodeLabel,
        Text2d::new(short_peer(peer)),
        TextFont::from_font_size(NODE_FONT),
        TextColor(Color::WHITE),
        Transform::from_translation(
            (ORBIT_CENTER + dir * ORBIT_RADIUS + NODE_LABEL_OFFSET).extend(0.0),
        ),
    ));
}

/// Reconciles the player network with the current roster and keeps it orbiting.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::type_complexity)]
pub fn update_node_graph(
    time: Res<Time>,
    search: Res<AutoSearch>,
    username: Res<Username>,
    peers: Res<Peers>,
    gateway: Res<GatewayState>,
    names: Res<PeerNames>,
    assets: Res<GraphAssets>,
    mut orbit: ResMut<OrbitState>,
    mut commands: Commands,
    all_nodes: Query<(Entity, &NodeOf)>,
    mut hub_vis: Single<&mut Visibility, (With<NodeHub>, Without<NodeHubLabel>)>,
    mut hub_tf: Single<&mut Transform, With<NodeHub>>,
    mut labels: Query<
        (&NodeOf, &mut Transform, &mut Text2d),
        (With<NodeLabel>, Without<NodeHubLabel>, Without<NodeHub>),
    >,
    mut bubbles: Query<
        (&NodeOf, &mut Transform),
        (
            With<NodeBubble>,
            Without<NodeLink>,
            Without<NodeLabel>,
            Without<NodeHub>,
        ),
    >,
    mut links: Query<
        (&NodeOf, &mut Transform),
        (
            With<NodeLink>,
            Without<NodeBubble>,
            Without<NodeLabel>,
            Without<NodeHub>,
        ),
    >,
    mut hub_label_vis: Single<
        (&mut Visibility, &mut Text2d),
        (With<NodeHubLabel>, Without<NodeHub>),
    >,
) {
    if username.is_changed() {
        hub_label_vis.1.0 = username.0.clone();
    }

    // The player network only appears while hunting for a match ("Jugar"):
    // it shows the players that are connected right now. Otherwise the field
    // stays clean.
    if !search.0 {
        for (entity, _) in &all_nodes {
            commands.entity(entity).despawn();
        }
        hub_vis.set_if_neq(Visibility::Hidden);
        hub_label_vis.0.set_if_neq(Visibility::Hidden);
        return;
    }
    hub_vis.set_if_neq(Visibility::Visible);
    hub_label_vis.0.set_if_neq(Visibility::Visible);

    // The hub breathes: a slow, gentle scale oscillation.
    let now = time.elapsed_secs();
    hub_tf.scale = Vec3::splat((1.0 + HUB_PULSE_AMOUNT * (now * 2.5).sin()).max(0.5));

    let desired: Vec<PeerId> = peers
        .0
        .iter()
        .copied()
        .filter(|p| gateway.peer != Some(*p))
        .take(MAX_NODES)
        .collect();

    let mut have: Vec<PeerId> = labels.iter().map(|(node, _, _)| node.0).collect();

    for (entity, node) in &all_nodes {
        if !desired.contains(&node.0) {
            commands.entity(entity).despawn();
        }
    }
    have.retain(|p| desired.contains(p));

    let mut spawned: Vec<(PeerId, f32)> = Vec::new();
    for peer in &desired {
        if !have.contains(peer) {
            let angle = initial_angle(*peer);
            spawn_node(&mut commands, &assets, *peer, angle);
            spawned.push((*peer, angle));
        }
    }
    for (peer, angle) in spawned {
        orbit.0.insert(peer, angle);
    }

    let dt = time.delta_secs().min(0.1);
    for (node, mut tf) in &mut bubbles {
        if let Some(angle) = orbit.0.get_mut(&node.0) {
            *angle += dt * orbit_speed(node.0);
            let dir = Vec2::from_angle(*angle);
            tf.translation = (ORBIT_CENTER + dir * ORBIT_RADIUS).extend(0.0);
            // Each bubble breathes at its own phase while it orbits.
            let phase = (peer_hash(node.0) % 100) as f32;
            let pulse = 1.0 + BUBBLE_PULSE_AMOUNT * ((now * ORBIT_ANIM_BASE + phase) / 100.0).sin();
            tf.scale = Vec3::splat(pulse.max(0.5));
        }
    }
    for (node, mut tf) in &mut links {
        if let Some(angle) = orbit.0.get(&node.0) {
            let dir = Vec2::from_angle(*angle);
            tf.translation = (ORBIT_CENTER + dir * (ORBIT_RADIUS / 2.0)).extend(0.0);
            tf.rotation = Quat::from_rotation_z(*angle);
            tf.scale = Vec3::new(ORBIT_RADIUS, 2.0, 1.0);
        }
    }
    for (node, mut tf, mut txt) in &mut labels {
        if let Some(angle) = orbit.0.get(&node.0) {
            let dir = Vec2::from_angle(*angle);
            tf.translation = (ORBIT_CENTER + dir * ORBIT_RADIUS + NODE_LABEL_OFFSET).extend(0.0);
            let name = names
                .0
                .get(&node.0)
                .cloned()
                .unwrap_or_else(|| short_peer(node.0));
            if txt.0 != name {
                txt.0 = name;
            }
        }
    }
}
