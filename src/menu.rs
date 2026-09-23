use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

use bevy::asset::AssetId;
use bevy::prelude::*;
use bevy::text::{EditableText, Font, TextCursorStyle, TextEdit};
use bevy::window::PrimaryWindow;
use serde_json::{from_str, Value};
use bevy::input_focus::{AutoFocus, FocusCause, InputFocus};
use libp2p::core::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};

use super::AppState;
use crate::config::Config;
use crate::history::{MatchHistory, MatchRecord, HISTORY_LIMIT};
use crate::networking::{GatewayState, NetChannels, NetCommand, NetEvent};
use crate::networking_demo::{IsHost, Peers};
use crate::sim::{self};
use ponged::protocol::{
    next_rank_name, rank_for_rating, rank_progress, GatewayRequest, GatewayResponse,
    Request as GameRequest,
};

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

/// A server entry with its address and measured ping latency.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ServerEntry {
    pub address: String,
    #[serde(default)]
    pub ping_ms: Option<u64>,
}

/// List of known gateway addresses that the client may connect to,
/// sorted by ping latency (lowest first).
/// This can be overridden by the `PONG_GATEWAY` environment variable or by
/// placing a `gateways.json` file in the project's `assets` directory.
#[derive(Resource, Default)]
pub struct GatewayAddresses(pub Vec<ServerEntry>);

/// State for reconnection after a peer disconnects during a match.
#[derive(Resource, Default)]
pub struct ReconnectionState {
    /// The peer we're trying to reconnect to.
    pub target: Option<PeerId>,
    /// Remaining seconds before giving up.
    pub countdown: Option<Timer>,
    /// Whether a reconnection attempt is in progress.
    pub active: bool,
    /// The addresses we used to reach the opponent (the relayed circuit
    /// address the gateway hands out in `MatchFound`). Re-dialed on a short
    /// interval while a drop is being reconnected, instead of waiting on
    /// libp2p's background reconnect whose exponential backoff can overshoot
    /// the grace window.
    pub addresses: Vec<Multiaddr>,
    /// Seconds accumulated since the last re-dial attempt, paced by
    /// [`REDIAL_EVERY_SECS`].
    pub redial_accum: f32,
    /// True once a *direct* (hole-punched, LAN or port-forwarded) connection to
    /// the current opponent exists. Direct links are preferred on re-dials and
    /// stop the relay from being in the match path.
    pub direct: bool,
}

/// Latest update information fetched from the GitHub API.
#[derive(Resource, Default)]
pub struct UpdateInfo {
    /// Latest version tag, if available.
    pub latest_version: Option<String>,
    /// Whether an update is available.
    pub available: bool,
    /// The URL to the release page.
    pub release_url: Option<String>,
}

/// Resource indicating an update check should be performed.
#[derive(Resource)]
pub struct UpdateTimer(Timer);

impl Default for UpdateTimer {
    fn default() -> Self {
        UpdateTimer(Timer::from_seconds(300.0, TimerMode::Once))
    }
}

/// Chat message sent between players.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub from_name: String,
    pub text: String,
    pub is_local: bool,
}

/// Buffer of recent chat messages displayed in the UI.
#[derive(Resource)]
pub struct ChatBuffer {
    pub messages: Vec<ChatMessage>,
}

impl Default for ChatBuffer {
    fn default() -> Self {
        ChatBuffer {
            messages: Vec::new(),
        }
    }
}

/// Whether the chat panel is currently visible.
#[derive(Resource, Default)]
pub struct ChatOpen(pub bool);

impl ChatBuffer {
    const MAX_MESSAGES: usize = 50;

    pub fn push(&mut self, msg: ChatMessage) {
        if self.messages.len() >= Self::MAX_MESSAGES {
            self.messages.remove(0);
        }
        self.messages.push(msg);
    }
}

/// Ping history entries for server latency tracking.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PingEntry {
    pub timestamp: f64,
    pub latency_ms: u64,
}

/// Per-server ping history for the graph.
#[derive(Resource, Default)]
#[allow(dead_code)]
pub struct PingHistory {
    pub entries: HashMap<String, Vec<PingEntry>>,
}

/// Display names learned from connected peers via `Request::Hello { name }`.
///
/// Peers send their name over the `Hello` request, and these names are
/// collected in a resource for display in the menu.
#[derive(Resource, Default)]
pub struct PeerNames(pub HashMap<PeerId, String>);

/// When true, "Jugar" keeps the hunt running: gateway queue plus automatic
/// invitations to every LAN player that appears.
#[derive(Resource, Default)]
pub struct AutoSearch(pub bool);

/// Whether the options overlay is on screen.
#[derive(Resource, Default)]
pub struct OptionsOpen(pub bool);

/// Whether the match history overlay is on screen.
#[derive(Resource, Default)]
pub struct HistoryOpen(pub bool);

/// True until the player has picked a name on first launch.
#[derive(Resource, Default)]
pub struct NeedsOnboarding(pub bool);

/// How often the rendezvous server is re-queried for new players.
const DISCOVERY_INTERVAL_SECS: f32 = 5.0;
/// How often the rendezvous registration is renewed. The server drops
/// registrations after their TTL (300 s), so clients must re-register to stay
/// discoverable; 120 s keeps a generous margin.
const REGISTER_INTERVAL_SECS: f32 = 120.0;

/// Periodic rendezvous discovery poll (re-queries for new players), active
/// while the gateway relay is connected so seated players still see others.
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

/// Periodic rendezvous re-registration so the client stays discoverable on the
/// gateway's server (registrations expire after their TTL even if the client
/// connection stays up).
#[derive(Resource)]
pub struct RegistrationTimer(Timer);

impl Default for RegistrationTimer {
    fn default() -> Self {
        RegistrationTimer(Timer::from_seconds(
            REGISTER_INTERVAL_SECS,
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

/// The opponent the gateway matched us with; we dial them and show the
/// pre-match confirmation dialog.
#[derive(Resource, Default)]
pub struct GatewayMatch(pub Option<PeerId>);

/// The gateway-assigned id of the match currently being played (`0` for
/// LAN/local matches). Correlates the `ReportResult` the gateway stores and
/// lets a later `MatchRevoked` revocation reach the right local record.
#[derive(Resource, Default)]
pub struct ActiveMatch(pub u64);

/// Seconds the other player has to answer the pair request.
const PREMATCH_ACCEPT_SECS: f32 = 20.0;
/// Countdown shown once both players accept before the match starts.
const PREMATCH_COUNTDOWN_SECS: f32 = 5.0;
/// Backoff before hunting again after a pairing falls through, so we don't
/// instantly re-pair with the same player on the gateway.
const PREMATCH_RESUME_SECS: f32 = 8.0;

/// How long we wait on a dropped opponent connection before abandoning a
/// match. Relay circuits over NAT frequently blip for a second or two and then
/// re-establish by themselves; abandoning instantly turns every such blip into
/// a free (and unreportable) "result". We keep actively re-dialing the
/// opponent (and re-arming the relay reservation) throughout the window, so
/// even a slower recovery usually lands before it expires.
const RECONNECT_GRACE_SECS: f32 = 10.0;

/// How often `update_reconnection` re-dials the opponent's addresses while a
/// drop is being reconnected.
const REDIAL_EVERY_SECS: f32 = 2.0;

/// A pending pairing that is waiting for both players to press "Aceptar" (M6).
/// After a gateway `MatchFound` or a LAN `InviteToPlay` we show a dialog; the
/// match only starts once both sides send [`GameRequest::AcceptMatch`] and the
/// countdown runs out. A decline or a timeout cancels it and resumes searching.
#[derive(Resource, Default)]
pub struct PreMatch {
    pub opponent: Option<PeerId>,
    pub self_accepted: bool,
    pub opp_accepted: bool,
    /// The opponent of the most recent finished match. Kept out of the
    /// automatic-invitation loop so pressing "Jugar" right after a match
    /// doesn't instantly drag the player we just faced into a new pairing —
    /// and so an idle player on the menu is never auto-queued by a former
    /// opponent's hunt. Rematches still work through the gateway queue.
    pub previous: Option<PeerId>,
    /// Acceptance window remaining while we wait for the other player.
    pub waiting: Option<Timer>,
    /// Five-second countdown after both players accept.
    pub countdown: Option<Timer>,
    /// Backoff before re-hunting after a cancelled pairing.
    pub resume: Option<Timer>,
    /// Peers that declined or were cancelled this search session.
    pub rejected: HashSet<PeerId>,
}

impl PreMatch {
    /// Offers a match to `opponent` and opens the confirmation dialog.
    pub fn begin(&mut self, opponent: PeerId) {
        self.opponent = Some(opponent);
        self.self_accepted = false;
        self.opp_accepted = false;
        self.waiting = Some(Timer::from_seconds(
            PREMATCH_ACCEPT_SECS,
            TimerMode::Once,
        ));
        self.countdown = None;
        self.resume = None;
    }

    pub fn idle(&self) -> bool {
        self.opponent.is_none()
    }

    /// Records the other player accepting; returns true if it changed anything.
    pub fn accepted_peer(&mut self, peer: PeerId) -> bool {
        if self.opponent != Some(peer) {
            return false;
        }
        self.opp_accepted = true;
        self.start_countdown_if_ready();
        true
    }

    /// Records our own acceptance and turns on the countdown once both sides
    /// have accepted.
    pub fn accept(&mut self) {
        self.self_accepted = true;
        self.start_countdown_if_ready();
    }

    /// Marks the pairing as failed and remembers the opponent so we don't
    /// instantly challenge them again.
    pub fn cancel(&mut self) {
        if let Some(peer) = self.opponent.take() {
            self.rejected.insert(peer);
        }
        self.self_accepted = false;
        self.opp_accepted = false;
        self.waiting = None;
        self.countdown = None;
    }

    /// The pairing is confirmed and the match is about to start.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    fn start_countdown_if_ready(&mut self) {
        if self.self_accepted && self.opp_accepted && self.countdown.is_none() {
            self.waiting = None;
            self.countdown = Some(Timer::from_seconds(
                PREMATCH_COUNTDOWN_SECS,
                TimerMode::Once,
            ));
            info!("Both players accepted; match starts in {PREMATCH_COUNTDOWN_SECS}s");
        }
    }
}

/// Width in pixels of the ELO progress bar under the player's name.
const ELO_BAR_WIDTH: f32 = 260.0;

// --- Marker components -----------------------------------------------------

#[derive(Component)]
pub struct MenuRoot;
#[derive(Component)]
pub struct OptionsRoot;
#[derive(Component)]
pub struct OptionsInput;
#[derive(Component)]
pub struct HistoryRoot;
#[derive(Component)]
pub struct HistoryBackButton;
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

/// Progress bar under the player's name showing ELO progress to next rank.
#[derive(Component)]
pub struct EloBar;
#[derive(Component)]
pub struct EloBarFill;
#[derive(Component)]
pub struct EloLabel;

/// Confirmation overlay shown when a pairing waits for both players.
#[derive(Component)]
pub struct PreMatchRoot;
#[derive(Component)]
pub struct PreMatchText;
#[derive(Clone, Copy, Component, PartialEq, Eq)]
pub enum PreMatchButton {
    Accept,
    Reject,
}

/// Which text line a label node belongs to.
#[derive(Clone, Copy, Component, PartialEq, Eq)]
pub enum TextLine {
    You,
    Status,
    Gateway,
    Record,
    /// "Partidas pasadas" section header (hidden while there is no history).
    HistoryHeader,
    /// One pre-spawned row of the recent-matches panel, index into the list.
    History(usize),
}

/// The main staircase menu buttons.
#[derive(Clone, Copy, Component, PartialEq, Eq)]
pub enum MenuButton {
    Play,
    History,
    Options,
    Quit,
}

/// Buttons on the options overlay.
#[derive(Clone, Copy, Component, PartialEq, Eq)]
pub enum OptionsButton {
    Save,
    Back,
}

// --- Chat UI components ---

#[derive(Component)]
pub struct ChatRoot;
#[derive(Component)]
pub struct ChatMessagesContainer;
#[derive(Component)]
pub struct ChatInput;
#[derive(Component)]
pub struct ChatSendButton;
#[derive(Component)]
pub struct ChatToggleButton;
#[derive(Component)]
pub struct ChatReopenButton;
#[derive(Component)]
pub struct ChatMessageLine;
#[derive(Component)]
pub struct ChatHeader;

// --- Expanded settings components ---
#[derive(Component)]
pub struct SettingsKeyUpButton;
#[derive(Component)]
pub struct SettingsKeyDownButton;
#[derive(Component)]
pub struct SettingsKeyUpLabel;
#[derive(Component)]
pub struct SettingsKeyDownLabel;
#[derive(Component)]
pub struct SettingsWindowScaleUp;
#[derive(Component)]
pub struct SettingsWindowScaleDown;
#[derive(Component)]
pub struct SettingsWindowScaleLabel;
#[derive(Component)]
pub struct SettingsVsyncToggle;
#[derive(Component)]
pub struct SettingsVsyncLabel;

/// Loads persisted settings at startup and seeds the username + ELO resources.
pub fn load_settings(mut commands: Commands, mut history: ResMut<MatchHistory>) {
    history.ensure_open();
    let stored = history.load_username().filter(|n| !n.trim().is_empty());
    let username = stored.clone().unwrap_or_else(|| "Player".to_string());
    commands.insert_resource(Username(username));
    commands.insert_resource(NeedsOnboarding(stored.is_none()));
    info!("Local username: {:?}", stored);

    // The client is the source of truth for ELO: seed the gateway state from
    // the locally-kept, gateway-signed rating proof so the menu (and the next
    // Register) never wait on — or depend on — a gateway's SQLite surviving
    // an outage.
    let proof = history.load_rating_proof();
    let rating = proof
        .as_ref()
        .map(|p| p.rating)
        .unwrap_or_else(|| history.load_rating());
    commands.insert_resource(GatewayState {
        rating,
        rank: Some(rank_for_rating(rating).to_string()),
        proof,
        ..Default::default()
    });
    info!("Local ELO rating: {rating} ({})", rank_for_rating(rating));

    // Load gateway addresses from assets/gateways.json, env var, or default.
    let gw_addr = load_gateway_addresses();
    let count = gw_addr.len();
    commands.insert_resource(GatewayAddresses(gw_addr));
    info!("Loaded {} gateway address(es)", count);
}

/// Gateway addresses compiled into the binary from assets/gateways.json.
/// Release binaries always have the production gateways baked in.
const GATEWAYS_JSON: &str = include_str!("../assets/gateways.json");

/// Tries to load gateway addresses in order of precedence:
/// 1. PONG_GATEWAY environment variable (comma‑separated, for overrides)
/// 2. assets/gateways.json file on disk (for development)
/// 3. Compiled-in addresses from GATEWAYS_JSON (for release binaries)
/// 4. a single default address.
pub fn load_gateway_addresses() -> Vec<ServerEntry> {
    // 1) Environment variable override.
    if let Ok(env) = std::env::var("PONG_GATEWAY") {
        if !env.trim().is_empty() {
            return env
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .map(|address| ServerEntry {
                    address,
                    ping_ms: None,
                })
                .collect();
        }
    }
    // 2) Disk file (for development: change addresses without recompiling).
    let path = std::path::Path::new("assets/gateways.json");
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(values) = from_str::<Vec<ServerEntry>>(&content) {
                if !values.is_empty() {
                    return sort_servers_by_ping(values);
                }
            }
        }
    }
    // 3) Compiled-in addresses (works in release binaries without the file).
    if let Ok(values) = from_str::<Vec<ServerEntry>>(GATEWAYS_JSON) {
        if !values.is_empty() {
            return sort_servers_by_ping(values);
        }
    }
    // 4) Hardcoded fallback.
    vec![ServerEntry {
        address: GATEWAY_DEFAULT_ADDR.to_string(),
        ping_ms: None,
    }]
}

/// Measures ping latency to each server and returns a sorted list
/// (lowest latency first). Servers with unmeasured ping are placed at the end.
fn sort_servers_by_ping(mut servers: Vec<ServerEntry>) -> Vec<ServerEntry> {
    for entry in &mut servers {
        entry.ping_ms = measure_ping(&entry.address);
    }
    servers.sort_by(|a, b| {
        let a_ping = a.ping_ms.unwrap_or(u64::MAX);
        let b_ping = b.ping_ms.unwrap_or(u64::MAX);
        a_ping.cmp(&b_ping)
    });
    servers
}

/// Attempts to measure the round-trip TCP connection time to a server
/// address, returning the latency in milliseconds.
fn measure_ping(addr_str: &str) -> Option<u64> {
    let addr: Multiaddr = addr_str.parse().ok()?;
    let socket = multiaddr_to_socket(&addr)?;
    let start = std::time::Instant::now();
    if std::net::TcpStream::connect(&socket).is_ok() {
        Some(start.elapsed().as_millis() as u64)
    } else {
        None
    }
}

/// Converts a libp2p Multiaddr to a `std::net::SocketAddr` for TCP pinging.
fn multiaddr_to_socket(addr: &Multiaddr) -> Option<SocketAddr> {
    let mut ip: Option<std::net::IpAddr> = None;
    let mut port: Option<u16> = None;
    for proto in addr.iter() {
        match proto {
            Protocol::Ip4(a) => ip = Some(a.into()),
            Protocol::Ip6(a) => ip = Some(a.into()),
            Protocol::Tcp(p) => port = Some(p),
            _ => {}
        }
    }
    match (ip, port) {
        (Some(ip), Some(port)) => Some(SocketAddr::new(ip, port)),
        _ => None,
    }
}

/// Refreshes ping measurements for all gateway addresses and re-sorts them.
/// Called periodically to keep the server list ordered by current latency.
pub fn refresh_server_pings(addrs: &mut GatewayAddresses) {
    for entry in &mut addrs.0 {
        entry.ping_ms = measure_ping(&entry.address);
    }
    addrs.0.sort_by(|a, b| {
        let a_ping = a.ping_ms.unwrap_or(u64::MAX);
        let b_ping = b.ping_ms.unwrap_or(u64::MAX);
        a_ping.cmp(&b_ping)
    });
}

/// How often we re-measure server ping latency (30 seconds).
const SERVER_PING_INTERVAL_SECS: f32 = 30.0;

/// Timer for periodic server ping refresh.
#[derive(Resource)]
pub struct ServerPingTimer(Timer);

impl Default for ServerPingTimer {
    fn default() -> Self {
        ServerPingTimer(Timer::from_seconds(
            SERVER_PING_INTERVAL_SECS,
            TimerMode::Repeating,
        ))
    }
}

/// Periodically refreshes server ping measurements and re-sorts the list.
pub fn update_server_ping_refresh(
    time: Res<Time>,
    mut timer: ResMut<ServerPingTimer>,
    mut gateway_addrs: ResMut<GatewayAddresses>,
) {
    timer.0.tick(time.delta());
    if timer.0.just_finished() {
        refresh_server_pings(&mut gateway_addrs);
    }
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
                EloBar,
                Node {
                    width: px(ELO_BAR_WIDTH),
                    height: px(12.),
                    border: UiRect::all(px(2.)),
                    ..default()
                },
                BackgroundColor(Color::NONE),
                BorderColor::all(Color::WHITE),
                children![(
                    EloBarFill,
                    Node {
                        width: percent(100.),
                        height: percent(100.),
                        ..default()
                    },
                    BackgroundColor(Color::WHITE),
                )],
            ),
            (
                EloLabel,
                Text::new(""),
                TextFont::from_font_size(13.0),
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
            menu_button(MenuButton::History, "Historial"),
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

    // --- Settings panel (expanded) ---
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
                row_gap: px(12.),
                padding: UiRect::all(px(32.)),
                border: UiRect::all(px(2.)),
                ..default()
            },
            BackgroundColor(Color::BLACK),
            BorderColor::all(Color::WHITE),
            children![
                (
                    Text::new("Opciones"),
                    TextFont::from_font_size(42.0),
                    TextColor(Color::WHITE),
                ),
                // --- Username ---
                (
                    Text::new("Tu nombre:"),
                    TextFont::from_font_size(16.0),
                    TextColor(DIM),
                ),
                (
                    OptionsInput,
                    EditableText::new(username.0.clone()),
                    TextCursorStyle::default(),
                    Interaction::None,
                    TextFont::from_font_size(22.0),
                    TextColor(Color::WHITE),
                    Node {
                        width: px(260.),
                        height: px(40.),
                        border: UiRect::all(px(2.)),
                        padding: UiRect::axes(px(10.), px(6.)),
                        ..default()
                    },
                    BorderColor::all(Color::WHITE),
                ),
                // --- Separator ---
                (
                    Node {
                        width: px(320.),
                        height: px(1.),
                        ..default()
                    },
                    BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.2)),
                ),
                // --- Keybinds ---
                (
                    Node {
                        flex_direction: FlexDirection::Row,
                        column_gap: px(12.),
                        align_items: AlignItems::Center,
                        ..default()
                    },
                    children![
                        (
                            Text::new("Mover arriba:"),
                            TextFont::from_font_size(16.0),
                            TextColor(DIM),
                        ),
                        (
                            SettingsKeyUpButton,
                            Button,
                            Node {
                                min_width: px(100.),
                                padding: UiRect::axes(px(12.), px(6.)),
                                border: UiRect::all(px(2.)),
                                ..default()
                            },
                            BackgroundColor(Color::NONE),
                            BorderColor::all(Color::WHITE),
                            children![(
                                SettingsKeyUpLabel,
                                Text::new("ArrowUp"),
                                TextFont::from_font_size(16.0),
                                TextColor(Color::WHITE),
                            )],
                        ),
                    ],
                ),
                (
                    Node {
                        flex_direction: FlexDirection::Row,
                        column_gap: px(12.),
                        align_items: AlignItems::Center,
                        ..default()
                    },
                    children![
                        (
                            Text::new("Mover abajo:"),
                            TextFont::from_font_size(16.0),
                            TextColor(DIM),
                        ),
                        (
                            SettingsKeyDownButton,
                            Button,
                            Node {
                                min_width: px(100.),
                                padding: UiRect::axes(px(12.), px(6.)),
                                border: UiRect::all(px(2.)),
                                ..default()
                            },
                            BackgroundColor(Color::NONE),
                            BorderColor::all(Color::WHITE),
                            children![(
                                SettingsKeyDownLabel,
                                Text::new("ArrowDown"),
                                TextFont::from_font_size(16.0),
                                TextColor(Color::WHITE),
                            )],
                        ),
                    ],
                ),
                // --- Window scale ---
                (
                    Node {
                        flex_direction: FlexDirection::Row,
                        column_gap: px(12.),
                        align_items: AlignItems::Center,
                        ..default()
                    },
                    children![
                        (
                            Text::new("Escala ventana:"),
                            TextFont::from_font_size(16.0),
                            TextColor(DIM),
                        ),
                        settings_button(SettingsWindowScaleDown, "-"),
                        (
                            SettingsWindowScaleLabel,
                            Text::new("1.0x"),
                            TextFont::from_font_size(18.0),
                            TextColor(Color::WHITE),
                        ),
                        settings_button(SettingsWindowScaleUp, "+"),
                    ],
                ),
                // --- Vsync ---
                (
                    Node {
                        flex_direction: FlexDirection::Row,
                        column_gap: px(12.),
                        align_items: AlignItems::Center,
                        ..default()
                    },
                    children![
                        (
                            Text::new("VSync:"),
                            TextFont::from_font_size(16.0),
                            TextColor(DIM),
                        ),
                        (
                            SettingsVsyncToggle,
                            Button,
                            Node {
                                min_width: px(80.),
                                padding: UiRect::axes(px(12.), px(6.)),
                                border: UiRect::all(px(2.)),
                                ..default()
                            },
                            BackgroundColor(Color::NONE),
                            BorderColor::all(Color::WHITE),
                            children![(
                                ButtonText,
                                SettingsVsyncLabel,
                                Text::new("ON"),
                                TextFont::from_font_size(16.0),
                                TextColor(Color::WHITE),
                            )],
                        ),
                    ],
                ),
                // --- Separator ---
                (
                    Node {
                        width: px(320.),
                        height: px(1.),
                        ..default()
                    },
                    BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.2)),
                ),
                // --- Buttons ---
                (
                    Node {
                        flex_direction: FlexDirection::Row,
                        column_gap: px(16.),
                        ..default()
                    },
                    children![
                        (
                            OptionsButton::Save,
                            Button,
                            Node {
                                min_width: px(160.),
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
                                min_width: px(160.),
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
                ),
            ],
        ),],
    ));

    // --- Match history panel ---
    commands.spawn((
        HistoryRoot,
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
                row_gap: px(10.),
                padding: UiRect::all(px(32.)),
                border: UiRect::all(px(2.)),
                ..default()
            },
            BackgroundColor(Color::BLACK),
            BorderColor::all(Color::WHITE),
            children![
                (
                    Text::new("Historial"),
                    TextFont::from_font_size(42.0),
                    TextColor(Color::WHITE),
                ),
                (
                    TextLine::Record,
                    Text::new(""),
                    TextFont::from_font_size(14.0),
                    TextColor(DIM),
                ),
                (
                    TextLine::HistoryHeader,
                    Text::new(""),
                    TextFont::from_font_size(16.0),
                    TextColor(Color::WHITE),
                ),
                (
                    Node {
                        flex_direction: FlexDirection::Column,
                        align_items: AlignItems::FlexStart,
                        min_width: px(380.),
                        row_gap: px(16.),
                        ..default()
                    },
                    children![
                        (
                            TextLine::History(0),
                            Text::new(""),
                            TextFont::from_font_size(14.0),
                            TextColor(DIM),
                        ),
                        (
                            TextLine::History(1),
                            Text::new(""),
                            TextFont::from_font_size(14.0),
                            TextColor(DIM),
                        ),
                        (
                            TextLine::History(2),
                            Text::new(""),
                            TextFont::from_font_size(14.0),
                            TextColor(DIM),
                        ),
                        (
                            TextLine::History(3),
                            Text::new(""),
                            TextFont::from_font_size(14.0),
                            TextColor(DIM),
                        ),
                        (
                            TextLine::History(4),
                            Text::new(""),
                            TextFont::from_font_size(14.0),
                            TextColor(DIM),
                        ),
                        (
                            TextLine::History(5),
                            Text::new(""),
                            TextFont::from_font_size(14.0),
                            TextColor(DIM),
                        ),
                        (
                            TextLine::History(6),
                            Text::new(""),
                            TextFont::from_font_size(14.0),
                            TextColor(DIM),
                        ),
                        (
                            TextLine::History(7),
                            Text::new(""),
                            TextFont::from_font_size(14.0),
                            TextColor(DIM),
                        ),
                        (
                            TextLine::History(8),
                            Text::new(""),
                            TextFont::from_font_size(14.0),
                            TextColor(DIM),
                        ),
                        (
                            TextLine::History(9),
                            Text::new(""),
                            TextFont::from_font_size(14.0),
                            TextColor(DIM),
                        ),
                    ],
                ),
                // --- Buttons ---
                (
                    Node {
                        flex_direction: FlexDirection::Row,
                        column_gap: px(16.),
                        ..default()
                    },
                    children![
                        (
                            HistoryBackButton,
                            Button,
                            Node {
                                min_width: px(160.),
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
                    TextCursorStyle::default(),
                    Interaction::None,
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

    commands.spawn((
        PreMatchRoot,
        Node {
            width: percent(100.),
            height: percent(100.),
            position_type: PositionType::Absolute,
            display: Display::None,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.95)),
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
                    PreMatchText,
                    Text::new(""),
                    TextFont::from_font_size(20.0),
                    TextColor(Color::WHITE),
                    TextLayout::justify(Justify::Center),
                ),
                (
                    Node {
                        flex_direction: FlexDirection::Row,
                        column_gap: px(20.),
                        ..default()
                    },
                    children![
                        prematch_button(PreMatchButton::Accept, "Aceptar partida"),
                        prematch_button(PreMatchButton::Reject, "Rechazar"),
                    ],
                ),
            ],
        ),],
    ));

    // --- Chat panel (right side of lobby) ---
    commands.insert_resource(ChatOpen(true));
    commands.spawn((
        ChatRoot,
        Node {
            position_type: PositionType::Absolute,
            right: px(16.),
            top: px(16.),
            bottom: px(16.),
            width: px(320.),
            flex_direction: FlexDirection::Column,
            border: UiRect::all(px(2.)),
            row_gap: px(0.),
            display: Display::Flex,
            ..default()
        },
        BackgroundColor(Color::srgba(0.05, 0.05, 0.1, 0.85)),
        BorderColor::all(Color::srgba(1.0, 1.0, 1.0, 0.3)),
        children![
            // Header
            (
                ChatHeader,
                Node {
                    width: percent(100.),
                    padding: UiRect::axes(px(12.), px(8.)),
                    justify_content: JustifyContent::SpaceBetween,
                    align_items: AlignItems::Center,
                    ..default()
                },
                BackgroundColor(Color::srgba(0.15, 0.15, 0.2, 0.9)),
                children![(
                    Text::new("Chat"),
                    TextFont::from_font_size(18.0),
                    TextColor(Color::WHITE),
                ), (
                    ChatToggleButton,
                    Button,
                    Node {
                        width: px(24.),
                        height: px(24.),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        border: UiRect::all(px(1.)),
                        ..default()
                    },
                    BackgroundColor(Color::NONE),
                    BorderColor::all(Color::srgba(1.0, 1.0, 1.0, 0.4)),
                    children![(
                        ButtonText,
                        Text::new("x"),
                        TextFont::from_font_size(14.0),
                        TextColor(Color::WHITE),
                    )],
                )],
            ),
            // Messages area
            (
                ChatMessagesContainer,
                Node {
                    width: percent(100.),
                    flex_grow: 1.0,
                    flex_direction: FlexDirection::Column,
                    padding: UiRect::axes(px(8.), px(4.)),
                    row_gap: px(2.),
                    overflow: Overflow::scroll_y(),
                    ..default()
                },
            ),
            // Input row
            (
                Node {
                    width: percent(100.),
                    flex_direction: FlexDirection::Row,
                    padding: UiRect::axes(px(8.), px(8.)),
                    column_gap: px(6.),
                    align_items: AlignItems::Center,
                    ..default()
                },
                children![
                    (
                        ChatInput,
                        EditableText::new("".to_string()),
                        TextCursorStyle::default(),
                        Interaction::None,
                        TextFont::from_font_size(14.0),
                        TextColor(Color::WHITE),
                        Node {
                            flex_grow: 1.0,
                            height: px(32.),
                            border: UiRect::all(px(1.)),
                            padding: UiRect::axes(px(8.), px(4.)),
                            ..default()
                        },
                        BorderColor::all(Color::srgba(1.0, 1.0, 1.0, 0.4)),
                    ),
                    (
                        ChatSendButton,
                        Button,
                        Node {
                            width: px(60.),
                            height: px(32.),
                            justify_content: JustifyContent::Center,
                            align_items: AlignItems::Center,
                            border: UiRect::all(px(1.)),
                            ..default()
                        },
                        BackgroundColor(Color::srgba(0.3, 0.5, 0.8, 0.8)),
                        BorderColor::all(Color::srgba(0.4, 0.6, 0.9, 1.0)),
                        children![(
                            ButtonText,
                            Text::new("Enviar"),
                            TextFont::from_font_size(14.0),
                            TextColor(Color::WHITE),
                        )],
                    ),
                ],
            ),
        ],
    ));

    commands.spawn((
        ChatReopenButton,
        ChatToggleButton,
        Button,
        Node {
            position_type: PositionType::Absolute,
            right: px(16.),
            top: px(16.),
            padding: UiRect::axes(px(10.), px(6.)),
            border: UiRect::all(px(1.)),
            display: Display::None,
            ..default()
        },
        BackgroundColor(Color::srgba(0.05, 0.05, 0.1, 0.85)),
        BorderColor::all(Color::srgba(1.0, 1.0, 1.0, 0.3)),
        children![(
            ButtonText,
            Text::new("Chat"),
            TextFont::from_font_size(14.0),
            TextColor(Color::WHITE),
        )],
    ));
}

/// Width of each staircase button: "Jugar" is the longest and the rest step
/// down like a bar chart.
fn menu_button_width(action: MenuButton) -> f32 {
    match action {
        MenuButton::Play => 380.0,
        MenuButton::History => 340.0,
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

/// A small settings button (+ / -).
fn settings_button(action: impl Component, label: &str) -> impl Bundle {
    (
        action,
        Button,
        Node {
            width: px(36.),
            height: px(32.),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            border: UiRect::all(px(2.)),
            ..default()
        },
        BackgroundColor(Color::NONE),
        BorderColor::all(Color::WHITE),
        children![(
            ButtonText,
            Text::new(label),
            TextFont::from_font_size(20.0),
            TextColor(Color::WHITE),
        )],
    )
}

/// A button on the pre-match confirmation dialog.
fn prematch_button(action: PreMatchButton, label: &str) -> impl Bundle {
    (
        action,
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
            Text::new(label),
            TextFont::from_font_size(24.0),
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
            With<HistoryRoot>,
            With<OnboardingRoot>,
            With<PreMatchRoot>,
            With<ChatRoot>,
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
    // Every gateway we meet is recorded so the menu never mistakes one for a
    // player. Only the first becomes the active gateway we queue on.
    gateway.known.insert(*peer);
    if gateway.peer == Some(*peer) {
        // Our active gateway (re)connected — e.g. a mid-match drop that the
        // swarm re-established by itself. Restore the state and, if the relay
        // reservation was lost meanwhile, ask for a fresh one so relayed
        // circuits to us keep working.
        gateway.connected = true;
        if !gateway.reserved {
            info!("Gateway {peer} reconnected; re-requesting relay reservation");
            if let Some(mut addr) = gateway.base_addr() {
                addr.push(Protocol::P2p(*peer));
                addr.push(Protocol::P2pCircuit);
                let _ = channels.commands.send(NetCommand::Listen(addr));
            }
        }
        return;
    }
    if gateway.peer.is_some() {
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

/// Offers a match to a freshly connected peer once `identify` has confirmed it
/// is a player. `on_peer_connected` cannot do this safely: a gateway connects
/// and could be mistaken for an opponent before its agent version is known.
pub fn on_peer_identified(
    ev: On<NetEvent>,
    search: Res<AutoSearch>,
    mut pre: ResMut<PreMatch>,
    gateway: Res<GatewayState>,
    peers: Res<Peers>,
    channels: Res<NetChannels>,
) {
    let NetEvent::Identity { peer, agent } = ev.event() else {
        return;
    };
    if crate::networking::is_gateway_agent(agent) {
        return;
    }
    if !peers.0.contains(peer) {
        return;
    }
    if search.0
        && pre.idle()
        && !gateway.known.contains(peer)
        && gateway.peer != Some(*peer)
        && pre.previous != Some(*peer)
        && !pre.rejected.contains(peer)
    {
        info!("Offering a match to identified peer {peer}");
        pre.begin(*peer);
        let _ = channels.commands.send(NetCommand::SendRequest {
            peer: *peer,
            request: GameRequest::InviteToPlay,
        });
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
                rating: gateway.rating,
                proof: gateway.proof.clone(),
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
    history: ResMut<MatchHistory>,
) {
    let NetEvent::GatewayResponse { peer, response } = ev.event() else {
        return;
    };
    match response {
        GatewayResponse::Registered {
            username,
            rating,
            rank,
            proof,
        } => {
            info!("Registered on gateway as {username} (rating {rating}, rank {rank})");
            gateway.rank = Some(rank.clone());
            gateway.rating = *rating;
            history.save_rating(*rating);
            if let Some(proof) = proof {
                gateway.proof = Some(proof.clone());
                history.save_rating_proof(proof);
            }
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
        GatewayResponse::Rating { rating, rank, proof } => {
            info!("Gateway rating is now {rating} (rank {rank})");
            gateway.rank = Some(rank.clone());
            gateway.rating = *rating;
            history.save_rating(*rating);
            if let Some(proof) = proof {
                gateway.proof = Some(proof.clone());
                history.save_rating_proof(proof);
            }
        }
        GatewayResponse::Pong => {}
        GatewayResponse::Error(err) => {
            warn!("Gateway error: {err}");
        }
    }
}

/// Consumes gateway *requests* — the `MatchFound` push announcing a reserved
/// match (M6 → M5), the `MatchRevoked` moderation push, and the `MatchCorrected`
/// score-correction push. A pairing opens the confirmation dialog instead of
/// auto-challenging.
#[allow(clippy::too_many_arguments)]
pub fn on_gateway_request(
    ev: On<NetEvent>,
    state: Res<State<AppState>>,
    mut matched: ResMut<GatewayMatch>,
    mut intent: ResMut<MatchIntent>,
    mut opponent: ResMut<Opponent>,
    mut pre: ResMut<PreMatch>,
    mut gateway: ResMut<GatewayState>,
    mut active: ResMut<ActiveMatch>,
    mut history: ResMut<MatchHistory>,
    mut reconn: ResMut<ReconnectionState>,
    channels: Res<NetChannels>,
) {
    let NetEvent::GatewayRequest { peer, request } = ev.event() else {
        return;
    };
    match request {
        GatewayRequest::MatchFound {
            match_id,
            opponent: opponent_str,
            addresses,
        } => {
            let Ok(opponent_peer) = opponent_str.parse::<PeerId>() else {
                warn!("Gateway sent unparseable opponent id: {opponent_str}");
                return;
            };
            if gateway.known.contains(&opponent_peer) {
                warn!("Gateway {peer} proposed a match with another gateway ({opponent_peer}); ignoring");
                return;
            }
            info!("Gateway matched us with {opponent_peer}; dialing via {addresses:?}");
            // First-come, first-served: a client may only have one pending
            // pairing at a time, so ignore a fresh MatchFound while one is
            // still open (its window will time out on its own). Ignore one
            // entirely while we are already playing a match.
            if *state == AppState::Playing {
                info!("Already in a match; ignoring MatchFound for {opponent_peer}");
                return;
            }
            if let Some(already) = pre.opponent {
                info!("Already pending a match with {already}; ignoring MatchFound for {opponent_peer}");
                return;
            }
            pre.begin(opponent_peer);
            opponent.0 = Some(opponent_peer);
            matched.0 = Some(opponent_peer);
            *intent = MatchIntent::Hosting;
            // Remember the gateway-assigned id so the eventual result report
            // can be correlated (and revoked) server-side.
            active.0 = *match_id;
            // Keep the dial addresses around so a mid-match drop can re-dial
            // the opponent directly instead of waiting on libp2p's (slow,
            // backoff-heavy) background reconnect.
            reconn.addresses = addresses
                .iter()
                .filter_map(|addr| addr.parse::<Multiaddr>().ok())
                .collect();
            // (Re)start the direct-connection tracking for this opponent: the
            // gateway hands out direct *and* circuit addresses; a hole punch
            // may or may not land before the match starts.
            reconn.direct = false;
            for addr in &reconn.addresses {
                let _ = channels.commands.send(NetCommand::Dial(addr.clone()));
            }
        }
        GatewayRequest::MatchRevoked {
            match_id,
            rating,
            proof,
        } => {
            info!("Match #{match_id} revoked by a moderator; rating restored to {rating}");
            gateway.rating = *rating;
            gateway.rank = Some(rank_for_rating(*rating).to_string());
            if let Some(p) = proof {
                gateway.proof = Some(p.clone());
                history.save_rating_proof(p);
            }
            history.save_rating(*rating);
            if active.0 == *match_id {
                active.0 = 0;
            }
            history.mark_match_revoked(*match_id);
        }
        GatewayRequest::MatchCorrected {
            match_id,
            my_score,
            opponent_score,
            rating,
            proof,
        } => {
            // A moderator fixed the final score of this match (each number is
            // this player's own score from their point of view). Recompute the
            // ELO move and keep the local history honest.
            info!(
                "Match #{match_id} score corrected by a moderator: {my_score}-{opponent_score}; rating {rating}"
            );
            gateway.rating = *rating;
            gateway.rank = Some(rank_for_rating(*rating).to_string());
            if let Some(p) = proof {
                gateway.proof = Some(p.clone());
                history.save_rating_proof(p);
            }
            history.save_rating(*rating);
            history.correct_score(*match_id, *my_score, *opponent_score);
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

/// Learns we can reach a peer over a direct (non-relayed) path: a successful
/// DCUtR hole punch, or simply a peer that was directly reachable (LAN /
/// port-forward). Gateways are direct too, so they are ignored; only a peer we
/// are actually about to play (or reconnecting to) marks the match as direct,
/// which makes re-dials prefer the direct address and stops the relay from
/// carrying the match traffic.
pub fn on_direct_connected(
    ev: On<NetEvent>,
    gateway: Res<GatewayState>,
    pre: Res<PreMatch>,
    matched: Res<GatewayMatch>,
    mut reconn: ResMut<ReconnectionState>,
) {
    let NetEvent::DirectConnected { peer } = ev.event() else {
        return;
    };
    if gateway.known.contains(peer) || gateway.peer == Some(*peer) {
        return;
    }
    let is_opponent = pre.opponent == Some(*peer)
        || reconn.target == Some(*peer)
        || matched.0 == Some(*peer);
    if !is_opponent {
        info!("Direct connection to {peer} (not an active opponent)");
        return;
    }
    reconn.direct = true;
    info!("Direct (hole-punched) connection established with opponent {peer}");
}

/// Re-queries the gateway's rendezvous server so newly arrived players (WAN)
/// show up in the roster without a restart, and periodically re-registers so we
/// keep appearing for others. Runs for as long as the gateway relay is
/// connected (not only while "Jugar" is active), so players who are seated in
/// the lobby remain reachable for chat; match invites stay gated by `search`.
///
/// Also dials the gateway once from the lobby: before, the gateway was only
/// dialed by "Jugar", so a seated player had no connection, discovery never
/// ran, and chat had nobody to reach until a hunt started.
pub fn update_discovery(
    time: Res<Time>,
    mut timer: ResMut<DiscoveryTimer>,
    mut register: ResMut<RegistrationTimer>,
    mut gateway: ResMut<GatewayState>,
    gw_addrs: Res<GatewayAddresses>,
    channels: Res<NetChannels>,
    mut was_ready: Local<bool>,
) {
    timer.0.tick(time.delta());
    register.0.tick(time.delta());

    // One-shot lobby connect so rendezvous discovery, the roster and chat all
    // work before "Jugar" is pressed. `start_search` re-dials if this fails.
    if gateway.peer.is_none() && gateway.addr.is_none() {
        let addr_str = (*gw_addrs)
            .0
            .first()
            .map(|e| e.address.clone())
            .unwrap_or_else(|| GATEWAY_DEFAULT_ADDR.to_string());
        let addr: Multiaddr = addr_str.parse().unwrap_or_else(|e| {
            warn!("Bad gateway address: {e}");
            GATEWAY_DEFAULT_ADDR.parse().expect("default address is valid")
        });
        gateway.addr = Some(addr.clone());
        let _ = channels.commands.send(NetCommand::Dial(addr));
        info!("Dialing gateway from lobby for discovery/chat");
    }

    let peer = gateway.peer;
    let gateway_address = match peer {
        Some(p) if gateway.connected && gateway.reserved => p,
        _ => {
            timer.0.reset();
            register.0.reset();
            *was_ready = false;
            return;
        }
    };

    let fire = !*was_ready || timer.0.just_finished();
    if fire {
        let _ = channels.commands.send(NetCommand::RendezvousDiscover {
            peer: gateway_address,
            namespace: "/pong/all".to_string(),
        });
    }
    if !*was_ready || register.0.just_finished() {
        info!("Refreshing rendezvous registration (TTL {REGISTER_INTERVAL_SECS:.0}s)");
        let _ = channels.commands.send(NetCommand::RendezvousRegister {
            peer: gateway_address,
            namespace: "/pong/all".to_string(),
        });
    }
    *was_ready = true;
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
    mut pre: ResMut<PreMatch>,
    mut commands: Commands,
    match_over: Res<crate::sim::MatchOver>,
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
            if *state == AppState::Menu {
                if pre.opponent.is_none() {
                    info!("{peer} invited us to play; asking for confirmation");
                    pre.begin(*peer);
                }
            } else {
                // Already mid-game: fall back to accepting the challenge right
                // away and join a fresh round next frame.
                info!("{peer} invited us to play");
                opponent.0 = Some(*peer);
                *intent = MatchIntent::Idle;
                let _ = channels.commands.send(NetCommand::SendRequest {
                    peer: *peer,
                    request: GameRequest::MatchStart,
                });
                start_match(*peer, &state, &mut next, &mut pending, &local, &mut is_host);
            }
        }
        GameRequest::AcceptMatch => {
            if pre.accepted_peer(*peer) {
                info!("{peer} accepted the match");
            } else {
                debug!("AcceptMatch from non-opponent {peer}");
            }
        }
        GameRequest::DeclineMatch => {
            if pre.opponent == Some(*peer) {
                info!("{peer} declined the match; searching for another rival");
                cancel_pairing(&mut pre, &channels, "rival rejected the offer");
            }
        }
        GameRequest::MatchStart => {
            info!("{peer} accepted our challenge");
            opponent.0 = Some(*peer);
            *intent = MatchIntent::Idle;
            start_match(*peer, &state, &mut next, &mut pending, &local, &mut is_host);
        }
        GameRequest::MatchAbort => {
            info!("{peer} ended the match; back to menu");
            // A finished match must still be recorded and reported on exit
            // (`record_match` reads `Opponent`). Only discard the pairing when
            // the match had no winner yet — otherwise the side that *receives*
            // the abort (the one who pressed ESC second) would lose its match
            // from the local history and never confirm the report at the
            // gateway, leaving the match stuck "pending" in moderation.
            if *state != AppState::Playing || !match_over.0 {
                pre.previous = Some(*peer);
                opponent.0 = None;
            }
            pending.0 = None;
            if *state == AppState::Playing {
                next.set(AppState::Menu);
            }
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
    mut reconn: ResMut<ReconnectionState>,
) {
    search.0 = false;
    // A fresh match always starts with a clean reconnection state. The dial
    // addresses are kept: they were recorded before entering `Playing` and
    // are what `update_reconnection` uses to bring a mid-match drop back.
    reconn.active = false;
    reconn.countdown = None;
    reconn.target = None;
    reconn.redial_accum = 0.0;
    reconn.direct = false;
    sim::set_peer_link_up(true);
    if gateway.queued
        && let Some(peer) = gateway.peer
    {
        let _ = channels.commands.send(NetCommand::SendGatewayRequest {
            peer,
            request: GatewayRequest::LeaveQueue,
        });
    }
}

/// Runs when leaving `Playing`: stops the hunt, clears the stale `MatchFound`
/// marker and remembers who we just played so the next hunt doesn't instantly
/// re-invite them (and an idle player is never auto-queued by a former
/// opponent). When a fresh match is already lined up — a remote request
/// arrived while we were mid-game — the swap state is left intact so
/// [`enter_pending_match`] can finish it on the next frame.
#[allow(clippy::too_many_arguments)]
pub fn on_exit_playing(
    mut search: ResMut<AutoSearch>,
    mut pre: ResMut<PreMatch>,
    mut opponent: ResMut<Opponent>,
    mut matched: ResMut<GatewayMatch>,
    mut active: ResMut<ActiveMatch>,
    mut intent: ResMut<MatchIntent>,
    mut reconn: ResMut<ReconnectionState>,
    pending: Res<PendingMatch>,
) {
    search.0 = false;
    matched.0 = None;
    // The match that just ended has been recorded (`record_match` runs before
    // this system); a fresh pairing will set it again.
    active.0 = 0;
    *intent = MatchIntent::Idle;
    // The next match gets its own addresses from its own `MatchFound` (or from
    // the relay reconstruction fallback), so don't carry the last opponent's.
    reconn.addresses.clear();
    reconn.direct = false;
    // A queued replacement match (Playing → Menu → Playing) is on its way:
    // keep the new opponent and the pending marker.
    if pending.0.is_some() {
        return;
    }
    if let Some(peer) = opponent.0.take() {
        pre.previous = Some(peer);
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

/// Leave the current match (ESCAP) and return to the menu. The opponent is told
/// the match ended so both sides go back to the lobby instead of letting one
/// side keep a host simulation running against an empty field.
#[allow(clippy::too_many_arguments)]
pub fn leave_match(
    keyboard_input: Res<ButtonInput<KeyCode>>,
    state: Res<State<AppState>>,
    mut next: ResMut<NextState<AppState>>,
    opponent: Res<Opponent>,
    mut pending: ResMut<PendingMatch>,
    channels: Res<NetChannels>,
) {
    if !keyboard_input.just_pressed(KeyCode::Escape) {
        return;
    }
    if *state == AppState::Playing
        && let Some(peer) = opponent.0
    {
        info!("Ending the match with {peer}");
        let _ = channels.commands.send(NetCommand::SendRequest {
            peer,
            request: GameRequest::MatchAbort,
        });
    }
    info!("Leaving the match");
    pending.0 = None;
    next.set(AppState::Menu);
}

/// Leave the match when the opponent drops. No host migration: with the
/// opponent gone there is nobody to play, so both sides return to the menu. An
/// abrupt drop mid-match first waits out [`RECONNECT_GRACE_SECS`] in case the
/// relay circuit re-establishes (it usually does within a second); only if the
/// opponent has not come back by then do we abandon the match.
#[allow(clippy::too_many_arguments)]
pub fn on_peer_disconnected(
    ev: On<NetEvent>,
    state: Res<State<AppState>>,
    match_over: Res<crate::sim::MatchOver>,
    mut next: ResMut<NextState<AppState>>,
    mut opponent: ResMut<Opponent>,
    mut pending: ResMut<PendingMatch>,
    mut reconn: ResMut<ReconnectionState>,
    mut pre: ResMut<PreMatch>,
    mut gateway: ResMut<GatewayState>,
) {
    let NetEvent::PeerDisconnected(peer) = ev.event() else {
        return;
    };
    // If the gateway/relay itself went down, remember that its state is stale
    // so `on_identity` (on reconnect) and `update_reconnection` re-arm the
    // relay reservation; without it, relayed circuits between the players can
    // never come back.
    if gateway.peer == Some(*peer) {
        gateway.connected = false;
        gateway.reserved = false;
        info!("Gateway {peer} disconnected");
    }
    if opponent.0 != Some(*peer) {
        return;
    }

    // A settled match (or one already finished) is left right away and its
    // result recorded on exit. A mid-match drop waits for the relay circuit to
    // come back instead of ending the game on the first network blip.
    if *state == AppState::Playing && !match_over.0 {
        info!("Opponent {peer} connection dropped; waiting {RECONNECT_GRACE_SECS:.0}s for the circuit to come back");
        // Pause the host's snapshot broadcast so it stops hammering the dead
        // peer with ~30 failed sends/sec; the sim thread resumes it as soon as
        // the link is marked up again.
        sim::set_peer_link_up(false);
        pending.0 = None;
        reconn.target = Some(*peer);
        reconn.active = true;
        reconn.countdown = Some(Timer::from_seconds(RECONNECT_GRACE_SECS, TimerMode::Once));
        // Dial immediately on the next frame instead of after the first
        // REDIAL_EVERY_SECS pause.
        reconn.redial_accum = REDIAL_EVERY_SECS;
        return;
    }

    info!("Opponent {peer} disconnected, back to menu");
    pre.previous = Some(*peer);
    pending.0 = None;
    if *state == AppState::Playing {
        next.set(AppState::Menu);
    } else {
        opponent.0 = None;
    }
}

// --- Auto-update system ----------------------------------------------------

/// Checks for updates from the GitHub API.
pub fn update_update_checker(
    mut timer: ResMut<UpdateTimer>,
    mut update_info: ResMut<UpdateInfo>,
    time: Res<Time>,
) {
    timer.0.tick(time.delta());
    if !timer.0.just_finished() {
        return;
    }
    timer.0.reset();
    match fetch_latest_version() {
        Ok((version, url)) => {
            update_info.latest_version = Some(version);
            update_info.available = true;
            update_info.release_url = Some(url);
        }
        Err(_) => {}
    }
}

/// Fetches the latest release version from GitHub synchronously.
fn fetch_latest_version() -> Result<(String, String), Box<dyn std::error::Error>> {
    let url = "https://api.github.com/repos/KEROJAM/ponged/releases/latest";
    let resp = reqwest::blocking::get(url)?;
    let body: Value = resp.json()?;
    let tag = body["tag_name"].as_str().unwrap_or("unknown").to_string();
    let html_url = body["html_url"].as_str().unwrap_or("").to_string();
    Ok((tag, html_url))
}

// --- Chat system -----------------------------------------------------------

/// Sends a chat message to all connected peers (gateways are skipped: they
/// don't speak the game protocol, so sending to them just fails).
pub fn send_chat_message(
    text: String,
    peers: &Peers,
    gateway: &GatewayState,
    channels: &NetChannels,
) {
    if text.trim().is_empty() {
        return;
    }
    for peer in &peers.0 {
        if gateway.peer == Some(*peer) || gateway.known.contains(peer) {
            continue;
        }
        let _ = channels.commands.send(NetCommand::SendRequest {
            peer: *peer,
            request: GameRequest::Chat { text: text.clone() },
        });
    }
    info!("Chat sent: {}", text);
}

/// Handles chat message display in the menu: processes input, sends messages,
/// and updates the message list UI.
#[allow(clippy::too_many_arguments)]
pub fn update_chat(
    keyboard_input: Res<ButtonInput<KeyCode>>,
    mut chat_open: ResMut<ChatOpen>,
    mut chat_buffer: ResMut<ChatBuffer>,
    username: Res<Username>,
    peers: Res<Peers>,
    gateway: Res<GatewayState>,
    channels: Res<NetChannels>,
    mut chat_input: Single<&mut EditableText, With<ChatInput>>,
    mut chat_root: Single<&mut Node, (With<ChatRoot>, Without<ChatReopenButton>)>,
    mut reopen_button: Single<&mut Node, (With<ChatReopenButton>, Without<ChatRoot>)>,
    send_button: Query<Ref<Interaction>, With<ChatSendButton>>,
    chat_toggle: Query<Ref<Interaction>, With<ChatToggleButton>>,
    messages_container: Query<
        (Entity, Option<&Children>),
        (With<ChatMessagesContainer>, Without<ChatInput>),
    >,
    mut message_texts: Query<&mut Text, With<ChatMessageLine>>,
    mut commands: Commands,
) {
    // Update chat panel visibility based on ChatOpen resource
    chat_root.display = if chat_open.0 {
        Display::Flex
    } else {
        Display::None
    };
    reopen_button.display = if chat_open.0 {
        Display::None
    } else {
        Display::Flex
    };

    // Handle chat toggle button
    for interaction in &chat_toggle {
        if interaction.is_changed() && *interaction == Interaction::Pressed {
            chat_open.0 = !chat_open.0;
            return; // Skip other updates when toggling
        }
    }

    // Check if send was clicked or Enter pressed
    let mut should_send = false;
    for interaction in &send_button {
        if interaction.is_changed() && *interaction == Interaction::Pressed {
            should_send = true;
        }
    }
    if keyboard_input.just_pressed(KeyCode::Enter)
        || keyboard_input.just_pressed(KeyCode::NumpadEnter)
    {
        should_send = true;
    }

    if should_send {
        let text = chat_input.value().to_string().trim().to_string();
        if !text.is_empty() {
            // Add to local buffer
            chat_buffer.push(ChatMessage {
                from_name: username.0.clone(),
                text: text.clone(),
                is_local: true,
            });
            // Send to all peers
            send_chat_message(text, &peers, &gateway, &channels);
            // Clear input via edits (keeps focus and cursor intact)
            chat_input.queue_edit(TextEdit::SelectAll);
            chat_input.queue_edit(TextEdit::Delete);
        }
    }

    // Sync message display
    if let Some((container_entity, children)) = messages_container.iter().next() {
        let expected = chat_buffer.messages.len();
        let current = children.map_or(0, |c| c.len());

        // Remove excess text entities if buffer shrunk
        if current > expected {
            if let Some(children) = children {
                for entity in children.iter().skip(expected) {
                    commands.entity(entity).despawn();
                }
            }
        }

        // Update or create message text entities
        for (i, msg) in chat_buffer.messages.iter().enumerate() {
            if let Some(children) = children
                && i < current
                && let Ok(mut text) = message_texts.get_mut(children[i])
            {
                // Update existing message text
                let prefix = format!("{}: ", msg.from_name);
                let new_text = format!("{prefix}{}", msg.text);
                if text.0 != new_text {
                    text.0 = new_text;
                }
            } else {
                // Spawn new message entity
                let prefix = format!("{}: ", msg.from_name);
                let full_text = format!("{}{}", prefix, msg.text);
                let color = if msg.is_local {
                    Color::srgb(0.6, 0.85, 1.0)
                } else {
                    Color::srgba(0.9, 0.9, 0.95, 1.0)
                };
                commands.entity(container_entity).with_children(|parent| {
                    parent.spawn((
                        ChatMessageLine,
                        Text::new(full_text),
                        TextFont::from_font_size(13.0),
                        TextColor(color),
                        Node {
                            width: percent(100.),
                            ..default()
                        },
                    ));
                });
                info!("[chat] spawned message {}: {}", i, msg.text);
            }
        }
    }
}

/// Fallback: gives focus to a text box on click, ensuring typing works even if
/// the widget plugin's pointer-focus path is not present.
pub fn focus_text_input(
    mut focus: ResMut<InputFocus>,
    inputs: Query<
        (Entity, Ref<Interaction>),
        Or<(With<OptionsInput>, With<ChatInput>, With<OnboardingInput>)>,
    >,
) {
    for (entity, interaction) in &inputs {
        if interaction.is_changed() && *interaction == Interaction::Pressed {
            focus.set(entity, FocusCause::Pressed);
        }
    }
}

/// Keeps the OS input method (IME) off the whole game window.
///
/// Bevy auto-enables the IME whenever an `EditableText` has focus. With an
/// active input method (e.g. Fcitx on Linux) that routes key events through
/// itself, the physical Enter is consumed to commit composition, so
/// `just_pressed(KeyCode::Enter)` never fires and chat messages can't be sent
/// with Enter. The game's text fields (chat, username, options) only need
/// plain Latin input, so the IME adds nothing here while breaking Enter.
pub fn keep_ime_disabled(mut windows: Query<&mut Window, With<PrimaryWindow>>) {
    for mut window in &mut windows {
        window.ime_enabled = false;
    }
}

/// Handles incoming chat messages from peers: adds them to the ChatBuffer.
pub fn on_chat_message(
    ev: On<NetEvent>,
    names: Res<PeerNames>,
    mut chat_buffer: ResMut<ChatBuffer>,
) {
    let NetEvent::GameRequest { peer, request } = ev.event() else {
        return;
    };
    if let GameRequest::Chat { text } = request {
        let from_name = names.0.get(peer).cloned().unwrap_or_else(|| "Jugador".to_string());
        chat_buffer.push(ChatMessage {
            from_name: from_name.clone(),
            text: text.clone(),
            is_local: false,
        });
        info!("Chat from {from_name}: {text}");
    }
}

// --- Ping history system ---------------------------------------------------

/// Records a ping measurement for each server address.
pub fn record_ping(
    mut ping_history: ResMut<PingHistory>,
    gateway_addrs: Res<GatewayAddresses>,
) {
    for entry in &gateway_addrs.0 {
        let ping_ms = entry.ping_ms.unwrap_or(0);
        let addr = entry.address.clone();
        let entries = ping_history.entries.entry(addr).or_default();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        entries.push(PingEntry {
            timestamp: now,
            latency_ms: ping_ms,
        });
        if entries.len() > 60 {
            entries.remove(0);
        }
    }
}

/// Saves config when options are closed.
pub fn save_config_on_options_close(
    options_open: ResMut<OptionsOpen>,
    config: Res<Config>,
) {
    if !options_open.0 {
        config.save();
    }
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
    username: Res<Username>,
    search: Res<AutoSearch>,
    peers: Res<Peers>,
    gateway: Res<GatewayState>,
    matched: Res<GatewayMatch>,
    pre: Res<PreMatch>,
    history: Res<MatchHistory>,
    names: Res<PeerNames>,
    mut labels: Query<(&mut Text, &TextLine)>,
) {
    set_text(
        &mut labels,
        TextLine::You,
        match &gateway.rank {
            Some(rank) => format!("{}  ·  {}", username.0, rank),
            None => username.0.clone(),
        },
    );

    let visible = peers.0.iter().filter(|p| gateway.peer != Some(**p)).count();
    set_text(
        &mut labels,
        TextLine::Status,
        if pre.opponent.is_some() {
            "¡Partida encontrada! Confirma en la ventana para empezar.".to_string()
        } else if search.0 {
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
        gateway_line(&gateway, &matched, &names),
    );

    let (wins, losses, draws) = history.wins_losses();
    set_text(
        &mut labels,
        TextLine::Record,
        if wins + losses + draws == 0 {
            match history.revoked_count() {
                0 => String::new(),
                n => format!("{n} partida{} revocada{}", if n == 1 { "" } else { "s" }, if n == 1 { "" } else { "s" }),
            }
        } else {
            let base = format!("Récord: {wins} Victoria / {losses} Derrota / {draws} Empate");
            match history.revoked_count() {
                0 => base,
                n => format!("{base} · {n} revocada{}", if n == 1 { "" } else { "s" }),
            }
        },
    );

    set_text(
        &mut labels,
        TextLine::HistoryHeader,
        if history.records.is_empty() {
            String::new()
        } else {
            "Partidas pasadas:".to_string()
        },
    );
    for (i, rec) in history.records.iter().take(HISTORY_LIMIT).enumerate() {
        set_text(&mut labels, TextLine::History(i), history_line(rec, &names));
    }
    for i in history.records.len().min(HISTORY_LIMIT)..HISTORY_LIMIT {
        set_text(&mut labels, TextLine::History(i), String::new());
    }
}

fn history_line(rec: &MatchRecord, names: &PeerNames) -> String {
    let result = if rec.my_score > rec.opp_score {
        "Victoria"
    } else if rec.my_score < rec.opp_score {
        "Derrota"
    } else {
        "Empate"
    };
    let when = rec.happened_at.get(5..16).unwrap_or(&rec.happened_at);
    let rival = rival_label(rec, names);
    let revoked = if rec.revoked { " · revocada" } else { "" };
    format!(
        "{when}  {rival}  {my}-{opp}  {result}{revoked}",
        my = rec.my_score,
        opp = rec.opp_score,
    )
}

/// Best display name for a match's opponent: the current session name when we
/// know their peer id, otherwise the name stored alongside the record. Also
/// recognizes legacy rows whose `rival` column holds a shortened peer id like
/// `12D3KooX...abcd` and maps them to the connected peer's name.
fn rival_label(rec: &MatchRecord, names: &PeerNames) -> String {
    if let Some(pid) = rec.rival_peer.as_deref()
        && let Ok(peer) = pid.parse::<PeerId>()
        && let Some(name) = names.0.get(&peer)
        && !name.trim().is_empty()
    {
        return name.clone();
    }
    if let Some((head, tail)) = rec.rival.split_once("...") {
        for (peer, name) in &names.0 {
            if name.trim().is_empty() {
                continue;
            }
            let full = peer.to_base58();
            if full.starts_with(head) && full.ends_with(tail) {
                return name.clone();
            }
        }
    }
    rec.rival.clone()
}

fn gateway_line(gateway: &GatewayState, matched: &GatewayMatch, names: &PeerNames) -> String {
    if let Some(peer) = matched.0 {
        let who = match names.0.get(&peer) {
            Some(name) if !name.trim().is_empty() => name.clone(),
            _ => "Rival".to_string(),
        };
        return format!("¡Emparejado! Conectando con {who}");
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

/// Refreshes the ELO progress bar under the player's name: the fill grows from
/// the floor of the current rank toward the next threshold.
pub fn update_elo_bar(
    gateway: Res<GatewayState>,
    mut bar: Single<&mut Node, With<EloBarFill>>,
    mut label: Single<&mut Text, With<EloLabel>>,
) {
    let Some(rank) = gateway.rank.as_deref() else {
        bar.width = px(0.0);
        label.0 = String::new();
        return;
    };
    let rating = gateway.rating;
    let progress = rank_progress(rating);
    bar.width = px(ELO_BAR_WIDTH * progress);
    label.0 = match next_rank_name(rating) {
        Some(next) => {
            let pct = (progress * 100.0).round() as i32;
            format!("{rating} ELO · {pct}% para {next}")
        }
        None => format!("{rating} ELO · {rank}"),
    };
}

/// Handles the main menu buttons (hover feedback + presses).
#[allow(clippy::too_many_arguments)]
pub fn update_buttons(
    mut buttons: Query<(Ref<Interaction>, &MenuButton, &Children, &mut BackgroundColor)>,
    mut button_texts: Query<&mut TextColor, (With<ButtonText>, Without<MenuButton>)>,
    mut search: ResMut<AutoSearch>,
    mut gateway: ResMut<GatewayState>,
    peers: Res<Peers>,
    username: Res<Username>,
    channels: Res<NetChannels>,
    mut options_open: ResMut<OptionsOpen>,
    mut history_open: ResMut<HistoryOpen>,
    mut pre: ResMut<PreMatch>,
    gw_addrs: Res<GatewayAddresses>,
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
        if !interaction.is_changed() || *interaction != Interaction::Pressed {
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
                    if pre.opponent.is_some() {
                        cancel_pairing(&mut pre, &channels, "search stopped");
                    }
                    info!("Search stopped");
                } else {
                    search.0 = true;
                    pre.rejected.clear();
                    start_search(&mut gateway, &channels, &username, &peers, &mut pre, &gw_addrs);
                    info!("Searching for opponents (LAN + WAN)");
                }
            }
            MenuButton::History => {
                options_open.0 = false;
                history_open.0 = true;
                info!("Opening match history");
            }
            MenuButton::Options => {
                history_open.0 = false;
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
    pre: &mut PreMatch,
    gw_addrs: &Res<GatewayAddresses>,
) {
    let addr_str = (*gw_addrs)
        .0
        .first()
        .map(|e| e.address.clone())
        .unwrap_or_else(|| GATEWAY_DEFAULT_ADDR.to_string());
    let addr: Multiaddr = addr_str.parse().unwrap_or_else(|e| {
        warn!("Bad gateway address: {e}");
        GATEWAY_DEFAULT_ADDR.parse().expect("default address is valid")
    });

    match (gateway.peer, gateway.connected, gateway.reserved) {
        (None, _, _) => {
            *gateway = GatewayState {
                addr: Some(addr),
                connected: false,
                reserved: false,
                queued: false,
                peer: None,
                // Rating + proof + rank live on the client (seeded at startup)
                // and must survive re-dialing a gateway, so keep them.
                rank: gateway.rank.clone(),
                rating: gateway.rating,
                proof: gateway.proof.clone(),
                // Gateways identified earlier in the session stay flagged as
                // such even while we re-dial the active one.
                known: gateway.known.clone(),
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
                    rating: gateway.rating,
                    proof: gateway.proof.clone(),
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

    invite_next_peer(pre, gateway, peers, channels);
}

/// Offers a match to the first known non-gateway, non-rejected peer. Callers
/// run this when the hunt starts or resumes after a failed pairing.
fn invite_next_peer(
    pre: &mut PreMatch,
    gateway: &GatewayState,
    peers: &Peers,
    channels: &NetChannels,
) {
    if !pre.idle() {
        return;
    }
    for peer in &peers.0 {
        if gateway.known.contains(peer)
            || gateway.peer == Some(*peer)
            || pre.rejected.contains(peer)
            || pre.previous == Some(*peer)
        {
            continue;
        }
        info!("Offering a match to {peer}");
        pre.begin(*peer);
        let _ = channels.commands.send(NetCommand::SendRequest {
            peer: *peer,
            request: GameRequest::InviteToPlay,
        });
        return;
    }
}

/// Tears down the current pairing: tells the opponent we won't play and starts
/// the backoff before we hunt again.
fn cancel_pairing(pre: &mut PreMatch, channels: &NetChannels, reason: &str) {
    if let Some(peer) = pre.opponent {
        info!("Cancelling pairing with {peer}: {reason}");
        let _ = channels.commands.send(NetCommand::SendRequest {
            peer,
            request: GameRequest::DeclineMatch,
        });
        pre.cancel();
    }
    pre.resume = Some(Timer::from_seconds(
        PREMATCH_RESUME_SECS,
        TimerMode::Once,
    ));
}

/// Back into the hunt after a cancelled pairing: re-enter the gateway queue
/// (if we were queued) and offer a match to the next LAN peer.
fn resume_search(
    pre: &mut PreMatch,
    search: &AutoSearch,
    gateway: &GatewayState,
    peers: &Peers,
    channels: &NetChannels,
) {
    if !pre.idle() || !search.0 {
        return;
    }
    // The gateway removes both sides from its queue on a match without sending
    // `Dequeued`, so our local `queued` flag can be stale. Re-queue anyway: the
    // gateway dedups players already queued.
    if gateway.connected
        && gateway.reserved
        && let Some(gw) = gateway.peer
    {
        info!("Re-entering the gateway matchmaking queue");
        let _ = channels.commands.send(NetCommand::SendGatewayRequest {
            peer: gw,
            request: GatewayRequest::QueueMatch,
        });
    }
    invite_next_peer(pre, gateway, peers, channels);
}

/// The other player's display name when we know it, its short peer id otherwise.
fn peer_label(names: &PeerNames, peer: PeerId) -> String {
    match names.0.get(&peer) {
        Some(name) if !name.trim().is_empty() => name.clone(),
        _ => "Jugador".to_string(),
    }
}

/// Drives the pre-match confirmation dialog: shows/hides it, ticks the
/// acceptance window and the countdown, and starts the match once the countdown
/// ends. A decline or a timeout cancels the pairing and resumes the hunt.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn update_prematch(
    state: Res<State<AppState>>,
    mut next: ResMut<NextState<AppState>>,
    time: Res<Time>,
    mut pre: ResMut<PreMatch>,
    search: Res<AutoSearch>,
    gateway: Res<GatewayState>,
    peers: Res<Peers>,
    mut pending: ResMut<PendingMatch>,
    local: Res<LocalPeerId>,
    mut is_host: ResMut<IsHost>,
    names: Res<PeerNames>,
    channels: Res<NetChannels>,
    mut root: Single<&mut Node, With<PreMatchRoot>>,
    mut label: Single<&mut Text, With<PreMatchText>>,
    mut buttons: Query<(
        Ref<Interaction>,
        &PreMatchButton,
        &Children,
        &mut BackgroundColor,
    )>,
    mut button_texts: Query<&mut TextColor, (With<ButtonText>, Without<PreMatchButton>)>,
) {
    // Backoff: after a cancelled pairing, wait a moment before re-hunting.
    let resume_finished = if let Some(resume) = &mut pre.resume {
        resume.tick(time.delta()).just_finished()
    } else {
        false
    };
    if resume_finished {
        pre.resume = None;
        resume_search(
            &mut pre,
            &search,
            &gateway,
            &peers,
            &channels,
        );
    }

    let Some(opponent) = pre.opponent else {
        root.display = Display::None;
        return;
    };
    root.display = Display::Flex;

    for (interaction, action, children, mut bg) in &mut buttons {
        let hovered = *interaction == Interaction::Hovered;
        bg.0 = if hovered { Color::WHITE } else { Color::NONE };
        for child in children {
            if let Ok(mut tc) = button_texts.get_mut(*child) {
                tc.0 = if hovered { Color::BLACK } else { Color::WHITE };
            }
        }
        if !interaction.is_changed() || *interaction != Interaction::Pressed {
            continue;
        }
        match action {
            PreMatchButton::Accept => {
                if !pre.self_accepted {
                    info!("Accepting the match against {opponent}");
                    let _ = channels.commands.send(NetCommand::SendRequest {
                        peer: opponent,
                        request: GameRequest::AcceptMatch,
                    });
                    pre.accept();
                }
            }
            PreMatchButton::Reject => {
                cancel_pairing(&mut pre, &channels, "player declined");
                return;
            }
        }
    }

    // Acceptance window: once it runs out without both accepting, cancel.
    if let Some(waiting) = &mut pre.waiting
        && waiting.tick(time.delta()).just_finished()
    {
        cancel_pairing(&mut pre, &channels, "confirmation timed out");
        return;
    }

    // Countdown after both players accept: start the match when it finishes.
    if let Some(countdown) = &mut pre.countdown
        && countdown.tick(time.delta()).just_finished()
    {
        start_match(opponent, &state, &mut next, &mut pending, &local, &mut is_host);
        pre.reset();
        return;
    }

    let who = peer_label(&names, opponent);
    label.0 = if let Some(waiting) = &pre.waiting {
        format!(
            "Emparejado con {who}\n¿Aceptas la partida?  ({} s)",
            waiting.remaining_secs().ceil().max(0.0) as i32
        )
    } else if let Some(countdown) = &pre.countdown {
        format!(
            "¡Aceptada! {who}\nLa partida empieza en {}…",
            countdown.remaining_secs().ceil().max(0.0) as i32
        )
    } else {
        format!("Emparejado con {who}…")
    };
}

/// Options overlay: save the username, or go back.
#[allow(clippy::too_many_arguments)]
pub fn update_options(
    mut root: Single<&mut Node, With<OptionsRoot>>,
    input: Single<&mut EditableText, With<OptionsInput>>,
    mut buttons: Query<(
        Ref<Interaction>,
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
    config: Res<Config>,
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
        if !interaction.is_changed() || *interaction != Interaction::Pressed {
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
                            request: GatewayRequest::Register {
                                username: name,
                                rating: gateway.rating,
                                proof: gateway.proof.clone(),
                            },
                        });
                    }
                }
                config.save();
                options_open.0 = false;
            }
            OptionsButton::Back => {
                options_open.0 = false;
            }
        }
    }
}

/// Match history overlay: shows the recent matches and a "Volver" button.
pub fn update_history(
    mut root: Single<&mut Node, With<HistoryRoot>>,
    button: Query<(Ref<Interaction>, &Children), With<HistoryBackButton>>,
    mut button_texts: Query<&mut TextColor, (With<ButtonText>, Without<HistoryBackButton>)>,
    mut history_open: ResMut<HistoryOpen>,
) {
    if !history_open.0 {
        root.display = Display::None;
        return;
    }
    root.display = Display::Flex;

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
        if interaction.is_changed() && *interaction == Interaction::Pressed {
            history_open.0 = false;
        }
    }
}

/// Handles settings controls: paddle speed, keybinds, window scale, vsync.
#[allow(clippy::too_many_arguments)]
pub fn update_settings_controls(
    options_open: Res<OptionsOpen>,
    mut config: ResMut<Config>,
    key_up_btn: Query<Ref<Interaction>, With<SettingsKeyUpButton>>,
    key_down_btn: Query<Ref<Interaction>, With<SettingsKeyDownButton>>,
    mut key_up_label: Single<&mut Text, (With<SettingsKeyUpLabel>, Without<SettingsKeyDownLabel>, Without<SettingsWindowScaleLabel>, Without<SettingsVsyncLabel>)>,
    mut key_down_label: Single<&mut Text, (With<SettingsKeyDownLabel>, Without<SettingsKeyUpLabel>, Without<SettingsWindowScaleLabel>, Without<SettingsVsyncLabel>)>,
    scale_up: Query<Ref<Interaction>, (With<SettingsWindowScaleUp>, Without<SettingsWindowScaleDown>)>,
    scale_down: Query<Ref<Interaction>, (With<SettingsWindowScaleDown>, Without<SettingsWindowScaleUp>)>,
    mut scale_label: Single<&mut Text, (With<SettingsWindowScaleLabel>, Without<SettingsKeyUpLabel>, Without<SettingsKeyDownLabel>, Without<SettingsVsyncLabel>)>,
    vsync_btn: Query<Ref<Interaction>, With<SettingsVsyncToggle>>,
    mut vsync_label: Single<&mut Text, (With<SettingsVsyncLabel>, Without<SettingsKeyUpLabel>, Without<SettingsKeyDownLabel>, Without<SettingsWindowScaleLabel>)>,
) {
    if !options_open.0 {
        return;
    }

    // Update display labels
    scale_label.0 = format!("{:.1}x", config.window_scale);
    key_up_label.0 = key_code_to_short(config.key_up);
    key_down_label.0 = key_code_to_short(config.key_down);
    vsync_label.0 = if config.vsync { "ON" } else { "OFF" }.to_string();

    // Handle keybind buttons: cycle through available keys
    if let Ok(i) = key_up_btn.single() {
        if i.is_changed() && *i == Interaction::Pressed {
            config.key_up = cycle_keycode(config.key_up);
        }
    }
    if let Ok(i) = key_down_btn.single() {
        if i.is_changed() && *i == Interaction::Pressed {
            config.key_down = cycle_keycode(config.key_down);
        }
    }

    // Handle window scale +/-
    if let Ok(i) = scale_up.single() {
        if i.is_changed() && *i == Interaction::Pressed {
            config.window_scale = (config.window_scale + 0.25).min(3.0);
        }
    }
    if let Ok(i) = scale_down.single() {
        if i.is_changed() && *i == Interaction::Pressed {
            config.window_scale = (config.window_scale - 0.25).max(0.5);
        }
    }

    // Handle vsync toggle
    if let Ok(i) = vsync_btn.single() {
        if i.is_changed() && *i == Interaction::Pressed {
            config.vsync = !config.vsync;
        }
    }
}

/// Cycle through available keybind options.
fn cycle_keycode(current: KeyCode) -> KeyCode {
    match current {
        KeyCode::ArrowUp => KeyCode::KeyW,
        KeyCode::KeyW => KeyCode::ArrowUp,
        KeyCode::ArrowDown => KeyCode::KeyS,
        KeyCode::KeyS => KeyCode::ArrowDown,
        _ => KeyCode::ArrowUp,
    }
}

/// Short display name for a keycode.
fn key_code_to_short(kc: KeyCode) -> String {
    match kc {
        KeyCode::ArrowUp => "ArrowUp".to_string(),
        KeyCode::ArrowDown => "ArrowDown".to_string(),
        KeyCode::KeyW => "W".to_string(),
        KeyCode::KeyS => "S".to_string(),
        _ => "?".to_string(),
    }
}

/// First-run onboarding: asks for the player name, persists it and closes.
#[allow(clippy::too_many_arguments)]
pub fn update_onboarding(
    mut root: Single<&mut Node, With<OnboardingRoot>>,
    input: Single<&mut EditableText, With<OnboardingInput>>,
    button: Query<(Ref<Interaction>, &Children), With<OnboardingButton>>,
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
        if interaction.is_changed() && *interaction == Interaction::Pressed {
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
                request: GatewayRequest::Register {
                    username: name,
                    rating: gateway.rating,
                    proof: gateway.proof.clone(),
                },
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
        Text2d::new("Jugador"),
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
            Without<NodeHubLabel>,
        ),
    >,
    mut links: Query<
        (&NodeOf, &mut Transform),
        (
            With<NodeLink>,
            Without<NodeBubble>,
            Without<NodeLabel>,
            Without<NodeHub>,
            Without<NodeHubLabel>,
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
                .unwrap_or_else(|| "Jugador".to_string());
            if txt.0 != name {
                txt.0 = name;
            }
        }
    }
}

// --- Hole-punch driving ---------------------------------------------------

/// While the pre-match confirmation dialog is open, re-dial the opponent's
/// direct and circuit addresses on a short interval. Each fresh inbound relayed
/// circuit to us makes libp2p's DCUtR attempt a hole punch, so re-dialing gives
/// a missed or raced punch another chance before the match starts; the direct
/// addresses (now included in `MatchFound`) surface an already-reachable LAN /
/// port-forwarded link immediately, without waiting for the relay at all.
/// Stops once [`ReconnectionState::direct`] reports a direct link exists.
pub fn update_prematch_punch(
    time: Res<Time>,
    pre: Res<PreMatch>,
    gateway: Res<GatewayState>,
    reconn: Res<ReconnectionState>,
    channels: Res<NetChannels>,
    mut accum: Local<f32>,
) {
    let Some(opponent) = pre.opponent else {
        *accum = 0.0;
        return;
    };
    if reconn.direct {
        return;
    }
    *accum += time.delta_secs();
    if *accum < REDIAL_EVERY_SECS {
        return;
    }
    *accum = 0.0;
    info!("Re-dialing opponent {opponent} to (re)trigger the hole punch");
    for addr in &reconn.addresses {
        let _ = channels.commands.send(NetCommand::Dial(addr.clone()));
    }
    if let Some(circuit_addr) = opponent_relay_addr(&gateway, &opponent) {
        let _ = channels.commands.send(NetCommand::Dial(circuit_addr));
    }
}

// --- Reconnection system ---------------------------------------------------

/// Handles reconnection attempts when a peer disconnects during a match. Keeps
/// the match on screen for [`RECONNECT_GRACE_SECS`]; if the circuit doesn't
/// come back in time, abandons the match and returns to the menu (an unfinished
/// match records no result). If the same opponent reconnects first, the
/// abandonment is cancelled by `on_peer_connected` and play resumes.
///
/// Instead of trusting libp2p's background reconnect alone (its exponential
/// backoff can leave the whole grace window without an attempt), every
/// [`REDIAL_EVERY_SECS`] we re-dial the addresses that first reached the
/// opponent — the relayed circuit address the gateway handed out in
/// `MatchFound`, or a fresh one rebuilt from the live gateway connection. If
/// the gateway link itself dropped, we also re-dial it and re-request the
/// relay reservation, since a dead reservation makes those circuit addresses
/// unreachable no matter how often we dial them.
#[allow(clippy::too_many_arguments)]
pub fn update_reconnection(
    mut reconn: ResMut<ReconnectionState>,
    mut next: ResMut<NextState<AppState>>,
    state: Res<State<AppState>>,
    opponent: Res<Opponent>,
    mut pending: ResMut<PendingMatch>,
    channels: Res<NetChannels>,
    gateway: Res<GatewayState>,
    time: Res<Time>,
) {
    let Some(target) = reconn.target else {
        return;
    };
    if !reconn.active {
        return;
    }
    // Stale state (e.g. a fresh match against someone else) must never
    // abandon a match it doesn't belong to.
    if opponent.0 != Some(target) {
        reconn.active = false;
        reconn.countdown = None;
        reconn.target = None;
        return;
    }
    let Some(ref mut countdown) = reconn.countdown else {
        return;
    };
    countdown.tick(time.delta());
    if countdown.just_finished() {
        info!("Opponent {target} did not come back; abandoning the match");
        reconn.active = false;
        reconn.target = None;
        reconn.addresses.clear();
        pending.0 = None;
        if *state == AppState::Playing {
            next.set(AppState::Menu);
        }
        return;
    }

    // Give the swarm a fresh nudge every couple of seconds. Dial the
    // addresses the opponent was originally reached through, plus a
    // just-in-time circuit address rebuilt from the live gateway.
    reconn.redial_accum += time.delta_secs();
    if reconn.redial_accum < REDIAL_EVERY_SECS {
        return;
    }
    reconn.redial_accum = 0.0;

    info!("Re-dialing opponent {target}");
    if reconn.direct {
        // The hole punch landed: keep the match on the direct link and only
        // re-dial non-relayed addresses, instead of dragging it back through
        // the gateway relay every couple of seconds.
        for addr in &reconn.addresses {
            if !crate::networking::is_circuit(addr) {
                let _ = channels.commands.send(NetCommand::Dial(addr.clone()));
            }
        }
    } else {
        for addr in &reconn.addresses {
            let _ = channels.commands.send(NetCommand::Dial(addr.clone()));
        }
        if let Some(circuit_addr) = opponent_relay_addr(&gateway, &target) {
            let _ = channels.commands.send(NetCommand::Dial(circuit_addr));
        }
    }

    // The gateway/relay link may have been the thing that dropped. Re-dial it
    // and, if the reservation is gone, ask for a fresh one so relayed
    // circuits to us (and re-dials of the addresses above) start working
    // again.
    if let Some(gw) = gateway.peer
        && let Some(base) = gateway.base_addr()
    {
        if !gateway.connected {
            let _ = channels.commands.send(NetCommand::Dial(base.clone()));
        }
        if !gateway.reserved {
            let mut circuit = base;
            circuit.push(Protocol::P2p(gw));
            circuit.push(Protocol::P2pCircuit);
            let _ = channels.commands.send(NetCommand::Listen(circuit));
        }
    }
}

/// Builds a relayed circuit address for `opponent` through the active gateway,
/// the same shape the gateway sends in `MatchFound`. Used as a fallback when
/// the original dial addresses are gone (e.g. a match that started from a LAN
/// invite while a gateway reservation was still alive).
fn opponent_relay_addr(gateway: &GatewayState, opponent: &PeerId) -> Option<Multiaddr> {
    let gw = gateway.peer?;
    let mut addr = gateway.base_addr()?;
    addr.push(Protocol::P2p(gw));
    addr.push(Protocol::P2pCircuit);
    addr.push(Protocol::P2p(*opponent));
    Some(addr)
}

