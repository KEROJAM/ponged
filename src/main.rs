use bevy::camera::ScalingMode;
use bevy::math::bounding::{Aabb2d, BoundingVolume, IntersectsVolume};
use bevy::prelude::*;
use bevy::ui_widgets::ImeSystems;
use bevy::winit::{UpdateMode, WinitSettings};
use ponged::protocol;
use std::time::Duration;

use crate::config::Config;

mod config;
mod history;
mod menu;
mod networking;
mod sim;

#[derive(Component, Default)]
#[require(Transform)]
struct Position(Vec2);

#[derive(Component)]
#[require(Position, Velocity = Velocity(Vec2::new(-BALL_SPEED, BALL_SPEED)), Collider = Collider(Rectangle::new(BALL_SIZE, BALL_SIZE)))]
struct Ball;

#[derive(Component)]
#[require(Position, Collider = Collider(PADDLE_SHAPE), Velocity)]
struct Paddle;

#[derive(Component, Default)]
struct Velocity(Vec2);

#[derive(Component, Default)]
struct Collider(Rectangle);

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
enum Collision {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Component)]
struct Player;

/// The paddle on the far side; driven by the remote player over the network.
#[derive(Component)]
struct Opponent;

#[derive(Resource)]
struct Score {
    player: u32,
    opponent: u32,
}

/// Top-level game screens. Matchmaking happens in `Menu`; the actual game runs
/// in `Playing` (entities and gameplay systems are scoped to it).
#[derive(States, Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
enum AppState {
    #[default]
    Menu,
    Playing,
}

#[derive(Component)]
struct PlayerScore;

#[derive(Component)]
struct OpponentScore;

/// Marks the in-game HUD (scoreboard) root so it can be cleaned up on exit.
#[derive(Component)]
struct Hud;

/// Overlay shown while a dropped opponent connection is within its reconnect
/// grace window, so the frozen field reads as "reconnecting" instead of broken.
#[derive(Component)]
struct ReconnectOverlay;

#[derive(Component)]
#[require(Position, Collider)]
struct Gutter;

const GUTTER_COLOR: Color = Color::srgb(255., 255., 255.);
const GUTTER_HEIGHT: f32 = 20.;

/// The playfield in world units. Kept fixed so the layout, physics and goal
/// detection never depend on the current window size (a tiling window manager
/// may resize the window at any point during a match).
const FIELD_SIZE: Vec2 = Vec2::new(800., 600.);

const PADDLE_SHAPE: Rectangle = Rectangle::new(10., 50.);
const PADDLE_COLOR: Color = Color::srgb(255., 255., 255.);
const PADDLE_SPEED: f32 = 5.;

const BALL_SIZE: f32 = 5.0;
const BALL_SHAPE: Circle = Circle::new(BALL_SIZE);
const BALL_COLOR: Color = Color::srgb(255., 255., 255.);
const BALL_SPEED: f32 = 2.;

// Gutter
fn spawn_gutters(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
) {
    let material = materials.add(GUTTER_COLOR);
    let padding = 20.;

    let gutter_shape = Rectangle::new(FIELD_SIZE.x, GUTTER_HEIGHT);
    let mesh = meshes.add(gutter_shape);

    let top_gutter_position = Vec2::new(0., FIELD_SIZE.y / 2. - padding);

    commands.spawn((
        Gutter,
        Mesh2d(mesh.clone()),
        MeshMaterial2d(material.clone()),
        Position(top_gutter_position),
        Collider(gutter_shape),
    ));

    let bottom_gutter_position = Vec2::new(0., -FIELD_SIZE.y / 2. + padding);

    commands.spawn((
        Gutter,
        Mesh2d(mesh.clone()),
        MeshMaterial2d(material.clone()),
        Position(bottom_gutter_position),
        Collider(gutter_shape),
    ));
}

// Paddles
fn spawn_paddles(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
) {
    let mesh = meshes.add(PADDLE_SHAPE);
    let material = materials.add(PADDLE_COLOR);
    let half_field_size = FIELD_SIZE / 2.;
    let padding = 20.;

    let player_position = Vec2::new(-half_field_size.x + padding, 0.);
    commands.spawn((
        Player,
        Paddle,
        Mesh2d(mesh.clone()),
        MeshMaterial2d(material.clone()),
        Position(player_position),
    ));

    let opponent_position = Vec2::new(half_field_size.x - padding, 0.);

    commands.spawn((
        Opponent,
        Paddle,
        Mesh2d(mesh.clone()),
        MeshMaterial2d(material.clone()),
        Position(opponent_position),
    ));
}

fn handle_player_input(
    keyboard_input: Res<ButtonInput<KeyCode>>,
    mut paddle_velocity: Single<&mut Velocity, With<Player>>,
    config: Res<Config>,
) {
    let speed = config.paddle_speed;
    if keyboard_input.pressed(config.key_up) {
        paddle_velocity.0.y = speed;
    } else if keyboard_input.pressed(config.key_down) {
        paddle_velocity.0.y = -speed;
    } else {
        paddle_velocity.0.y = 0.;
    }
}

fn move_paddles(mut paddles: Query<(&mut Position, &Velocity), With<Paddle>>) {
    for (mut position, velocity) in &mut paddles {
        position.0 += velocity.0;
    }
}

fn constrain_paddle_position(
    mut paddles: Query<(&mut Position, &Collider), (With<Paddle>, Without<Gutter>)>,
    gutters: Query<(&Position, &Collider), (With<Gutter>, Without<Paddle>)>,
) {
    for (mut paddle_position, paddle_collider) in &mut paddles {
        for (gutter_position, gutter_collider) in &gutters {
            let paddle_aabb = Aabb2d::new(paddle_position.0, paddle_collider.half_size());
            let gutter_aabb = Aabb2d::new(gutter_position.0, gutter_collider.half_size());

            if let Some(collision) = collide_with_side(paddle_aabb, gutter_aabb) {
                match collision {
                    Collision::Top => {
                        paddle_position.0.y = gutter_position.0.y
                            + gutter_collider.half_size().y
                            + paddle_collider.half_size().y;
                    }
                    Collision::Bottom => {
                        paddle_position.0.y = gutter_position.0.y
                            - gutter_collider.half_size().y
                            - paddle_collider.half_size().y;
                    }
                    _ => {}
                }
            }
        }
    }
}

// Ball

fn spawn_ball(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
) {
    let mesh = meshes.add(BALL_SHAPE);
    let material = materials.add(BALL_COLOR);
    commands.spawn((Ball, Mesh2d(mesh), MeshMaterial2d(material)));
}

fn spawn_camera(mut commands: Commands) {
    // Frame the fixed playfield regardless of window size/aspect, so resizing
    // the window (e.g. by a tiling WM) never changes the visible game board.
    commands.spawn((
        Camera2d,
        Projection::Orthographic(OrthographicProjection {
            scaling_mode: ScalingMode::AutoMin {
                min_width: FIELD_SIZE.x,
                min_height: FIELD_SIZE.y,
            },
            ..OrthographicProjection::default_2d()
        }),
    ));
}

/// Fresh match, fresh score (the guest always starts from the menu).
fn reset_score(mut score: ResMut<Score>) {
    score.player = 0;
    score.opponent = 0;
}

fn spawn_scoreboard(mut commands: Commands) {
    // Create a container that will center everything
    let container = Node {
        width: percent(100.0),
        height: percent(100.0),
        justify_content: JustifyContent::Center,
        ..default()
    };

    let header = Node {
        width: px(200.),
        height: px(100.),
        ..default()
    };

    let player_score = (
        PlayerScore,
        Text::new("0"),
        TextFont::from_font_size(72.0),
        TextColor(Color::WHITE),
        TextLayout::justify(Justify::Center),
        Node {
            position_type: PositionType::Absolute,
            top: px(5.0),
            left: px(25.0),
            ..default()
        },
    );

    let opponent_score = (
        OpponentScore,
        Text::new("0"),
        TextFont::from_font_size(72.0),
        TextColor(Color::WHITE),
        TextLayout::justify(Justify::Center),
        Node {
            position_type: PositionType::Absolute,
            top: px(5.0),
            right: px(25.0),
            ..default()
        },
    );

    commands.spawn((
        Hud,
        container,
        children![(header, children![player_score, opponent_score])],
    ));
}

fn update_scoreboard(
    mut player_score: Single<&mut Text, (With<PlayerScore>, Without<OpponentScore>)>,
    mut opponent_score: Single<&mut Text, (With<OpponentScore>, Without<PlayerScore>)>,
    host: Res<networking_demo::IsHost>,
    score: Res<Score>,
    world: Res<networking_demo::RemoteWorld>,
) {
    // The host keeps its own authoritative score; the guest displays the
    // host's numbers (crossed over, since the host's "player" is our
    // "opponent").
    let (player, opponent) = if host.0 {
        (score.player, score.opponent)
    } else {
        match world.curr {
            Some(snap) => (snap.opponent_score, snap.player_score),
            None => return,
        }
    };

    let player_text = player.to_string();
    let opponent_text = opponent.to_string();
    if player_score.0 != player_text || opponent_score.0 != opponent_text {
        player_score.0 = player_text;
        opponent_score.0 = opponent_text;
    }
}

fn project_positions(mut positionables: Query<(&mut Transform, &Position)>) {
    for (mut transform, position) in &mut positionables {
        transform.translation = position.0.extend(0.);
    }
}

// --- Game over UI ----------------------------------------------------------

#[derive(Component)]
struct GameOverRoot;

#[derive(Component)]
struct GameOverTitle;

/// Resets the match_over flag when entering a new match.
fn reset_match_over(mut match_over: ResMut<sim::MatchOver>) {
    match_over.0 = false;
}

/// Spawns the game-over overlay (hidden by default, shown when match ends).
fn spawn_game_over_ui(mut commands: Commands) {
    commands.spawn((
        GameOverRoot,
        Node {
            width: percent(100.0),
            height: percent(100.0),
            position_type: PositionType::Absolute,
            top: px(0.0),
            left: px(0.0),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            display: Display::None,
            row_gap: px(16.0),
            ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.85)),
        children![
            (
                GameOverTitle,
                Text::new(""),
                TextFont::from_font_size(64.0),
                TextColor(Color::WHITE),
                TextLayout::justify(Justify::Center),
            ),
            (
                Text::new("Press ESC to return to menu"),
                TextFont::from_font_size(20.0),
                TextColor(Color::srgb(0.7, 0.7, 0.7)),
                TextLayout::justify(Justify::Center),
            ),
        ],
    ));
}

/// Shows the game-over overlay and sets the appropriate text when the match ends.
fn update_game_over_ui(
    match_over: Res<sim::MatchOver>,
    score: Res<Score>,
    host: Res<networking_demo::IsHost>,
    world: Res<networking_demo::RemoteWorld>,
    mut root: Query<&mut Node, With<GameOverRoot>>,
    mut title: Single<&mut Text, With<GameOverTitle>>,
) {
    if !match_over.is_changed() {
        return;
    }
    for mut node in &mut root {
        if match_over.0 {
            node.display = Display::Flex;
            let (player_score, opponent_score) = if host.0 {
                (score.player, score.opponent)
            } else {
                match world.curr {
                    Some(snap) => (snap.opponent_score, snap.player_score),
                    None => return,
                }
            };
            title.0 = if player_score > opponent_score {
                format!("You win! {player_score} - {opponent_score}")
            } else if player_score == 0 && opponent_score >= sim::WIN_SCORE {
                "YOU JUST GOT PONGED!".to_string()
            } else {
                format!("You lose! {player_score} - {opponent_score}")
            };
        } else {
            node.display = Display::None;
        }
    }
}

fn collide_with_side(ball: Aabb2d, wall: Aabb2d) -> Option<Collision> {
    if !ball.intersects(&wall) {
        return None;
    }

    let closest_point = wall.closest_point(ball.center());
    let offset = ball.center() - closest_point;

    let side = if offset.x.abs() > offset.y.abs() {
        if offset.x < 0. {
            Collision::Left
        } else {
            Collision::Right
        }
    } else if offset.y > 0. {
        Collision::Top
    } else {
        Collision::Bottom
    };

    Some(side)
}

impl Collider {
    fn half_size(&self) -> Vec2 {
        self.0.half_size
    }
}

fn cleanup_playing(
    mut commands: Commands,
    game: Query<
        Entity,
        Or<(
            With<Ball>,
            With<Paddle>,
            With<Gutter>,
            With<Hud>,
            With<GameOverRoot>,
            With<ReconnectOverlay>,
        )>,
    >,
) {
    for entity in &game {
        commands.entity(entity).despawn();
    }
}

/// Spawns the in-game reconnect overlay (hidden by default, shown while a
/// dropped opponent is within its reconnect grace window).
fn spawn_reconnect_overlay(mut commands: Commands) {
    commands.spawn((
        ReconnectOverlay,
        Node {
            width: percent(100.0),
            height: percent(100.0),
            position_type: PositionType::Absolute,
            top: px(0.0),
            left: px(0.0),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            display: Display::None,
            ..default()
        },
        children![(
            Text::new("Reconectando… volviendo a conectar con el rival"),
            TextFont::from_font_size(24.0),
            TextColor(Color::srgb(1.0, 0.85, 0.4)),
            TextLayout::justify(Justify::Center),
        )],
    ));
}

/// Show/hide the reconnect overlay with the reconnection state.
fn update_reconnect_overlay(
    reconn: Res<menu::ReconnectionState>,
    mut overlay: Query<&mut Node, With<ReconnectOverlay>>,
) {
    for mut node in &mut overlay {
        let show = reconn.active && reconn.target.is_some();
        node.display = if show { Display::Flex } else { Display::None };
    }
}

/// Tracks the current `AppState` to keep `WinitSettings` energy mode in sync:
/// the menu may idle at ~10 Hz while unfocused (big idle CPU/GPU savings), but
/// a running match must keep rendering continuously even unfocused so remote
/// snapshots and input never lag behind.
fn apply_winit_energy_mode(
    state: Res<State<AppState>>,
    mut winit: ResMut<WinitSettings>,
) {
    let desired = if *state.get() == AppState::Menu {
        UpdateMode::reactive_low_power(Duration::from_millis(100))
    } else {
        UpdateMode::Continuous
    };
    if winit.unfocused_mode != desired {
        winit.unfocused_mode = desired;
    }
}

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(networking::NetworkingPlugin)
        .insert_resource(WinitSettings {
            // The lobby animates its player network, so keep rendering while
            // focused. When the window loses focus (menu is often left in the
            // background) the engine drops to a ~10 Hz redraw, cutting the
            // GPU/CPU idle burn that otherwise keeps the whole app busy.
            focused_mode: UpdateMode::Continuous,
            unfocused_mode: UpdateMode::reactive_low_power(Duration::from_millis(100)),
        })
        .insert_resource(ClearColor(Color::BLACK))
        .insert_resource(Score {
            player: 0,
            opponent: 0,
        })
        .init_state::<AppState>()
        .init_resource::<networking_demo::Peers>()
        .init_resource::<networking_demo::LatestSnapshot>()
        .init_resource::<networking_demo::RemoteWorld>()
        .init_resource::<networking_demo::IsHost>()
        .init_resource::<networking_demo::SnapshotSeq>()
        .init_resource::<networking::GatewayState>()
        .init_resource::<menu::ServerPingTimer>()
        .init_resource::<config::Config>()
        .init_resource::<menu::Username>()
        .init_resource::<menu::LocalPeerId>()
        .init_resource::<menu::Opponent>()
        .init_resource::<menu::PendingMatch>()
        .init_resource::<menu::MatchIntent>()
        .init_resource::<menu::GatewayMatch>()
        .init_resource::<menu::ActiveMatch>()
        .init_resource::<menu::PreMatch>()
        .init_resource::<menu::AutoSearch>()
        .init_resource::<menu::PeerNames>()
        .init_resource::<menu::OptionsOpen>()
        .init_resource::<menu::HistoryOpen>()
        .init_resource::<menu::NeedsOnboarding>()
        .init_resource::<menu::DiscoveryTimer>()
        .init_resource::<menu::RegistrationTimer>()
        .init_resource::<menu::OrbitState>()
        .init_resource::<history::MatchHistory>()
        .init_resource::<sim::MatchOver>()
        .init_resource::<menu::ReconnectionState>()
        .init_resource::<menu::UpdateInfo>()
        .init_resource::<menu::UpdateTimer>()
        .init_resource::<menu::ChatBuffer>()
        .init_resource::<menu::PingHistory>()
        .add_systems(Startup, (config::load_config, menu::load_settings, menu::install_default_font, spawn_camera, networking_demo::setup))
        .add_systems(
            PreUpdate,
            menu::keep_ime_disabled.after(ImeSystems::ToggleWindowIMEInput),
        )
        .add_systems(OnEnter(AppState::Menu), menu::spawn_menu)
        .add_systems(OnEnter(AppState::Menu), history::refresh_on_menu)
        .add_systems(OnExit(AppState::Menu), menu::despawn_menu)
        .add_systems(Update, menu::update_menu.run_if(in_state(AppState::Menu)))
        .add_systems(
            Update,
            menu::update_buttons.run_if(in_state(AppState::Menu)),
        )
        .add_systems(
            Update,
            menu::update_options.run_if(in_state(AppState::Menu)),
        )
        .add_systems(
            Update,
            menu::update_history.run_if(in_state(AppState::Menu)),
        )
        .add_systems(
            Update,
            menu::update_settings_controls.run_if(in_state(AppState::Menu)),
        )
        .add_systems(
            Update,
            menu::update_onboarding.run_if(in_state(AppState::Menu)),
        )
        .add_systems(
            Update,
            menu::update_node_graph.run_if(in_state(AppState::Menu)),
        )
        .add_systems(
            Update,
            menu::update_discovery.run_if(in_state(AppState::Menu)),
        )
        .add_systems(
            Update,
            menu::update_prematch.run_if(in_state(AppState::Menu)),
        )
        .add_systems(
            Update,
            menu::update_elo_bar.run_if(in_state(AppState::Menu)),
        )
        .add_systems(
            Update,
            menu::update_server_ping_refresh.run_if(in_state(AppState::Menu)),
        )
        .add_systems(
            Update,
            (
                menu::update_reconnection.run_if(in_state(AppState::Playing)),
                menu::update_update_checker.run_if(in_state(AppState::Menu)),
                menu::update_chat.run_if(in_state(AppState::Menu)),
                menu::focus_text_input.run_if(in_state(AppState::Menu)),
            )
                .chain(),
        )
        .add_systems(
            Update,
            (
                menu::record_ping.run_if(in_state(AppState::Menu)),
                menu::save_config_on_options_close,
                apply_winit_energy_mode,
            )
                .chain(),
        )
        .add_systems(
            Update,
            menu::enter_pending_match.run_if(in_state(AppState::Menu)),
        )
        .add_systems(
            Update,
            menu::leave_match.run_if(in_state(AppState::Playing)),
        )
        .add_systems(
            OnEnter(AppState::Playing),
            (
                spawn_paddles,
                spawn_gutters,
                spawn_ball,
                spawn_scoreboard,
                spawn_game_over_ui,
                spawn_reconnect_overlay,
                reset_score,
                reset_match_over,
                networking_demo::reset_match_state,
                sim::start_match_sim.run_if(resource_equals(networking_demo::IsHost(true))),
                history::arm_record,
                menu::on_enter_playing,
            ),
        )
        .add_systems(
            OnExit(AppState::Playing),
            (
                cleanup_playing,
                sim::stop_match_sim.run_if(resource_equals(networking_demo::IsHost(true))),
                // `on_exit_playing` consumes the `Opponent` and resets
                // `ActiveMatch`; the match must be recorded (and its result
                // reported to the gateway) before that happens. The ordering
                // is not implied by Bevy's scheduler, so it must be explicit.
                history::record_match.before(menu::on_exit_playing),
                menu::on_exit_playing,
            ),
        )
        // Only the guest pushes snapshots from Bevy; the host's authoritative
        // state is sent by its simulation thread (see `src/sim.rs`), which
        // keeps playing even while this window is minimized/occluded.
        .add_systems(
            Update,
            networking_demo::broadcast_snapshot
                .run_if(in_state(AppState::Playing))
                .run_if(resource_equals(networking_demo::IsHost(false))),
        )
        .add_systems(
            FixedUpdate,
            (
                networking_demo::apply_remote_world
                    .run_if(resource_equals(networking_demo::IsHost(false))),
                handle_player_input.run_if(resource_equals(networking_demo::IsHost(false))),
                move_paddles.run_if(resource_equals(networking_demo::IsHost(false))),
                constrain_paddle_position.run_if(resource_equals(networking_demo::IsHost(false))),
                sim::forward_player_input.run_if(resource_equals(networking_demo::IsHost(true))),
                sim::pull_sim_state.run_if(resource_equals(networking_demo::IsHost(true))),
                project_positions,
                update_scoreboard,
            )
                .run_if(in_state(AppState::Playing)),
        )
        .add_systems(
            Update,
            update_game_over_ui.run_if(in_state(AppState::Playing)),
        )
        .add_systems(
            Update,
            update_reconnect_overlay.run_if(in_state(AppState::Playing)),
        )
        .add_observer(networking_demo::on_peer_connected)
        .add_observer(networking_demo::on_peer_disconnected)
        .add_observer(networking_demo::on_game_request)
        .add_observer(networking_demo::on_game_response)
        .add_observer(menu::on_local_peer_id)
        .add_observer(menu::on_identity)
        .add_observer(menu::on_peer_identified)
        .add_observer(menu::on_relay_reservation)
        .add_observer(menu::on_gateway_response)
        .add_observer(menu::on_gateway_request)
        .add_observer(menu::on_rendezvous_discovered)
        .add_observer(menu::on_game_request)
        .add_observer(menu::on_chat_message)
        .add_observer(menu::on_peer_disconnected)
        .run();
}

/// Contiguous network → game glue: tracks connected peers, pushes a snapshot
/// of the real game state to them ~10×/s, and keeps the last snapshot the
/// other side sent us.
mod networking_demo {
    use bevy::prelude::*;
    use libp2p::PeerId;

    use super::protocol::{self, GameSnapshot, Point2, Request};
    use super::{BALL_SPEED, Ball, Opponent, Player, Position, Score, Velocity};
    use crate::networking::{NetChannels, NetCommand, NetEvent};

    /// Peers we are currently connected to.
    #[derive(Resource, Default)]
    pub struct Peers(pub Vec<PeerId>);

    /// Whether this node is the authoritative simulation host for the current
    /// match. The player with the smaller `PeerId` becomes host, so both sides
    /// agree on who simulates the ball and the score.
    #[derive(Resource, Default, Clone, Copy, PartialEq, Eq)]
    pub struct IsHost(pub bool);

    /// The most recent raw snapshot received from the other side.
    #[derive(Resource, Default)]
    pub struct LatestSnapshot(Option<GameSnapshot>);

    /// The last two snapshots from the remote side, used to interpolate the
    /// opponent paddle (and the ball, on the guest) so movement is smooth, and
    /// to predict the ball between authoritative snapshots (M8).
    #[derive(Resource, Default)]
    pub struct RemoteWorld {
        pub prev: Option<GameSnapshot>,
        pub curr: Option<GameSnapshot>,
        /// Seconds since `curr` arrived.
        pub elapsed: f32,
        /// Measured seconds between the last two snapshots (falls back to
        /// `SNAPSHOT_INTERVAL` while unknown).
        pub segment: f32,
        /// Residual prediction error vs. the freshly arrived authoritative
        /// ball, eased away so corrections are smooth (M8).
        pub error: Vec2,
        /// How much of `error` is still applied each tick (0..=1).
        pub error_alpha: f32,
    }

    /// Monotonic counter stamped into every snapshot we send.
    #[derive(Resource, Default)]
    pub struct SnapshotSeq(u64);

    /// Drives how often we push snapshots.
    #[derive(Resource)]
    pub struct SnapshotTimer(Timer);

    /// How often we push snapshots. 30 Hz balances smoothness with the
    /// per-request overhead of the request/response protocol.
    const SNAPSHOT_INTERVAL: f32 = 1.0 / 30.0;

    pub fn setup(mut commands: Commands) {
        commands.insert_resource(SnapshotTimer(Timer::from_seconds(
            SNAPSHOT_INTERVAL,
            TimerMode::Repeating,
        )));
    }

    /// Fresh match, fresh network state: drop the previous match's cached
    /// snapshots and re-seed the snapshot counter. The guest renders from
    /// `RemoteWorld`/`LatestSnapshot`; carrying them across matches would keep
    /// showing the last game's final scores (and its final winner). The first
    /// snapshot of the new match is also safer to accept this way: a fresh
    /// host simulation restarts its `seq` at 0, so a stale higher-seq frame
    /// would otherwise never be displaced.
    pub fn reset_match_state(
        mut world: ResMut<RemoteWorld>,
        mut latest: ResMut<LatestSnapshot>,
        mut seq: ResMut<SnapshotSeq>,
    ) {
        *world = RemoteWorld::default();
        *latest = LatestSnapshot::default();
        seq.0 = 0;
    }

    #[allow(clippy::too_many_arguments)]
    pub fn on_peer_connected(
        ev: On<NetEvent>,
        mut peers: ResMut<Peers>,
        mut gateway_match: ResMut<crate::menu::GatewayMatch>,
        pre: ResMut<crate::menu::PreMatch>,
        username: Res<crate::menu::Username>,
        channels: Res<NetChannels>,
        mut reconn: ResMut<crate::menu::ReconnectionState>,
    ) {
        if let NetEvent::PeerConnected(peer) = ev.event() {
            let is_matched = gateway_match.0 == Some(*peer);
            if !peers.0.contains(peer) {
                peers.0.push(*peer);
                info!("Peer {peer} connected, saying hello");
                let _ = channels.commands.send(NetCommand::SendRequest {
                    peer: *peer,
                    request: Request::Hello {
                        name: username.0.clone(),
                    },
                });
            }
            // A mid-match drop whose relay circuit came back within the grace
            // window: cancel the pending abandonment and keep playing.
            if reconn.active && reconn.target == Some(*peer) {
                info!("Opponent {peer} reconnected; resuming the match");
                reconn.active = false;
                reconn.countdown = None;
                reconn.target = None;
                // The link being back is what reactivates the host's snapshot
                // broadcast, which we parked while the peer was gone.
                crate::sim::set_peer_link_up(true);
            }
            // M6: the opponent the gateway matched us with connected. The match
            // itself is gated by the pre-match dialog; flush an acceptance that
            // was sent before the connection was up.
            if is_matched {
                gateway_match.0 = None;
                if pre.self_accepted && pre.opponent == Some(*peer) {
                    info!("Resending acceptance to {peer}");
                    let _ = channels.commands.send(NetCommand::SendRequest {
                        peer: *peer,
                        request: Request::AcceptMatch,
                    });
                }
            }
            // Match offers happen in `menu::on_peer_identified`, once `identify`
            // has told us the peer is a player and not a gateway.
        }
    }

    pub fn on_peer_disconnected(ev: On<NetEvent>, mut peers: ResMut<Peers>) {
        if let NetEvent::PeerDisconnected(peer) = ev.event() {
            peers.0.retain(|p| p != peer);
            info!("Peer {peer} disconnected");
        }
    }

    pub fn on_game_request(ev: On<NetEvent>, mut latest: ResMut<LatestSnapshot>) {
        if let NetEvent::GameRequest { peer, request } = ev.event() {
            match request {
                Request::Hello { name } => info!("Greeting received from {peer}: {name}"),
                Request::State(snapshot) => latest.0 = Some(*snapshot),
                Request::InviteToPlay | Request::MatchStart => {
                    // Handled by `menu::on_game_request`.
                }
                Request::AcceptMatch | Request::DeclineMatch | Request::MatchAbort => {
                    // Pre-match confirmation handshake, handled by `menu::on_game_request`.
                }
                Request::MigrateHost(_) | Request::HostMigrated => {
                    // Host migration coordination, handled by `menu::on_game_request`.
                }
                Request::Chat { text } => {
                    // Chat messages are handled by `menu::on_chat_message`.
                    debug!("Chat from {peer}: {text}");
                }
            }
        }
    }

    pub fn on_game_response(ev: On<NetEvent>) {
        if let NetEvent::GameResponse { peer, response } = ev.event() {
            debug!("Response from {peer}: {response:?}");
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn broadcast_snapshot(
        time: Res<Time>,
        mut timer: ResMut<SnapshotTimer>,
        channels: Res<NetChannels>,
        host: Res<IsHost>,
        world: Res<RemoteWorld>,
        rival: Res<crate::menu::Opponent>,
        reconn: Res<crate::menu::ReconnectionState>,
        player: Single<&Position, (With<Player>, Without<Opponent>)>,
        opponent: Single<&Position, (With<Opponent>, Without<Player>)>,
        ball: Single<&Position, With<Ball>>,
        ball_velocity: Single<&Velocity, With<Ball>>,
        score: Res<Score>,
        mut seq: ResMut<SnapshotSeq>,
    ) {
        if !timer.0.tick(time.delta()).just_finished() {
            return;
        }
        // While an opponent drop is being given a reconnect grace window the
        // peer is gone; pushing snapshots to it would just fail, so stay quiet
        // until the circuit re-establishes (or the match is abandoned).
        if reconn.active {
            return;
        }
        // Push state only to the peer we're actually playing against. The
        // gateway (and any other lobby peer we're connected to) never speaks
        // `/pong/state/1.0.0`, so broadcasting to every connected peer just
        // produces a `UnsupportedProtocols` failure on every tick.
        let Some(rival) = rival.0 else {
            return;
        };

        let snapshot = GameSnapshot {
            seq: seq.0,
            is_host: host.0,
            ball: to_point(ball.0),
            ball_velocity: to_point(ball_velocity.0),
            player_paddle: to_point(player.0),
            opponent_paddle: to_point(opponent.0),
            player_score: score.player,
            opponent_score: score.opponent,
            match_over: false,
            ball_speed_mult: world.curr.map_or(1.0, |snap| snap.ball_speed_mult),
        };
        seq.0 += 1;

        let _ = channels.commands.send(NetCommand::SendRequest {
            peer: rival,
            request: Request::State(snapshot),
        });
    }

    fn lerp(prev: Point2, curr: Point2, t: f32) -> Point2 {
        Point2 {
            x: prev.x + (curr.x - prev.x) * t,
            y: prev.y + (curr.y - prev.y) * t,
        }
    }

    fn to_vec2(Point2 { x, y }: Point2) -> Vec2 {
        Vec2::new(x, y)
    }

    /// How often snapshots are supposed to arrive (used before the first
    /// measured gap).
    const ERROR_DECAY: f32 = 0.78;
    /// Conversion from the snapshot's per-tick velocity to units/second: the
    /// sim steps at Bevy's default 64 Hz, displacing `velocity * BALL_SPEED`
    /// each tick.
    const VELOCITY_SCALE: f32 = BALL_SPEED * 64.0;
    /// Never predict the ball further than this ahead of the newest snapshot.
    const EXTRAPOLATE_MAX: f32 = 0.25;

    /// Predicts the ball position for this moment: interpolant between the last
    /// two snapshots when we're inside the window, extrapolation with the
    /// newest velocity once we've passed it (M8).
    fn predict_ball(world: &RemoteWorld) -> Vec2 {
        let Some(curr) = world.curr else {
            return Vec2::ZERO;
        };
        let t = (world.elapsed / world.segment.max(1e-4)).clamp(0.0, 1.0);
        let base = match world.prev {
            Some(prev) => to_vec2(lerp(prev.ball, curr.ball, t)),
            None => to_vec2(curr.ball),
        };
        let extra = (world.elapsed - world.segment).clamp(0.0, EXTRAPOLATE_MAX);
        if extra > 0.0 {
            base + to_vec2(curr.ball_velocity) * (VELOCITY_SCALE * curr.ball_speed_mult * extra)
        } else {
            base
        }
    }

    /// Applies everything we know about the remote side each fixed tick:
    /// - the opponent paddle mirrors the remote player's paddle, interpolated
    ///   between the last two snapshots so movement stays smooth;
    /// - on the guest, the ball is interpolated between snapshots and *predicted*
    ///   past them (M8); a smoothed correction eases it back onto the path of
    ///   each newly arrived authoritative state instead of snapping.
    ///
    /// Paddles and ball are x-mirrored because the remote frame sits with its
    /// player on the same side as ours, so the remote's left is our right.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_remote_world(
        latest: Res<LatestSnapshot>,
        time: Res<Time<Fixed>>,
        host: Res<IsHost>,
        mut world: ResMut<RemoteWorld>,
        mut opponent: Single<&mut Position, (With<Opponent>, Without<Player>, Without<Ball>)>,
        mut ball: Single<&mut Position, (With<Ball>, Without<Player>, Without<Opponent>)>,
        mut match_over: ResMut<crate::sim::MatchOver>,
    ) {
        // Promote newly arrived snapshots into the interpolation window.
        if let Some(next) = latest.0 {
            let newer = world.curr.is_some_and(|curr| next.seq > curr.seq);
            if world.curr.is_none() || newer {
                if world.curr.is_some() {
                    // Measure the real inter-snapshot gap and remember the
                    // residual between what we predicted and the truth.
                    world.segment = world.elapsed.clamp(1.0 / 120.0, 1.0);
                    let predicted = predict_ball(&world);
                    world.error = to_vec2(next.ball) - predicted;
                    world.error_alpha = 1.0;
                } else {
                    world.segment = SNAPSHOT_INTERVAL;
                }
                world.prev = world.curr;
                world.curr = Some(next);
                world.elapsed = 0.0;
            }
            match_over.0 = next.match_over;
        }

        world.elapsed += time.delta_secs_f64() as f32;
        let t = (world.elapsed / world.segment.max(1e-4)).clamp(0.0, 1.0);

        match (world.prev, world.curr) {
            (Some(prev), Some(curr)) => {
                opponent.0 = mirror_x(to_vec2(lerp(prev.player_paddle, curr.player_paddle, t)));
            }
            (None, Some(curr)) => {
                opponent.0 = mirror_x(to_vec2(curr.player_paddle));
            }
            _ => {}
        }

        if !host.0 {
            let mut pos = predict_ball(&world);
            if world.error_alpha > 0.0 {
                pos += world.error * world.error_alpha;
                world.error_alpha *= ERROR_DECAY;
                if world.error_alpha < 0.01 {
                    world.error_alpha = 0.0;
                }
            }
            ball.0 = mirror_x(pos);
        }
    }

    fn to_point(Vec2 { x, y }: Vec2) -> protocol::Point2 {
        protocol::Point2 { x, y }
    }

    /// Both players sit on the left of their own screen, so a paddle the remote
    /// calls "player" is on the opposite side of ours: mirror its x.
    fn mirror_x(Vec2 { x, y }: Vec2) -> Vec2 {
        Vec2::new(-x, y)
    }
}
