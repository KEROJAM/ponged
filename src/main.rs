use bevy::math::bounding::{Aabb2d, BoundingVolume, IntersectsVolume};
use bevy::prelude::*;

mod networking;
mod protocol;

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

#[derive(Component)]
struct Ai;

#[derive(Resource)]
struct Score {
    player: u32,
    ai: u32,
}

#[derive(EntityEvent)]
struct Scored {
    #[event_target]
    scorer: Entity,
}

#[derive(Component)]
struct PlayerScore;

#[derive(Component)]
struct AiScore;

#[derive(Component)]
#[require(Position, Collider)]

struct Gutter;

const GUTTER_COLOR: Color = Color::srgb(255., 255., 255.);
const GUTTER_HEIGHT: f32 = 20.;

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
    window: Single<&Window>,
) {
    let material = materials.add(GUTTER_COLOR);
    let padding = 20.;

    let gutter_shape = Rectangle::new(window.resolution.width(), GUTTER_HEIGHT);
    let mesh = meshes.add(gutter_shape);

    let top_gutter_position = Vec2::new(0., window.resolution.height() / 2. - padding);

    commands.spawn((
        Gutter,
        Mesh2d(mesh.clone()),
        MeshMaterial2d(material.clone()),
        Position(top_gutter_position),
        Collider(gutter_shape),
    ));

    let bottom_gutter_position = Vec2::new(0., -window.resolution.height() / 2. + padding);

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
    window: Single<&Window>,
) {
    let mesh = meshes.add(PADDLE_SHAPE);
    let material = materials.add(PADDLE_COLOR);
    let half_window_size = window.resolution.size() / 2.;
    let padding = 20.;

    let player_position = Vec2::new(-half_window_size.x + padding, 0.);
    commands.spawn((
        Player,
        Paddle,
        Mesh2d(mesh.clone()),
        MeshMaterial2d(material.clone()),
        Position(player_position),
    ));

    let ai_position = Vec2::new(half_window_size.x - padding, 0.);

    commands.spawn((
        Ai,
        Paddle,
        Mesh2d(mesh.clone()),
        MeshMaterial2d(material.clone()),
        Position(ai_position),
    ));
}

fn handle_player_input(
    keyboard_input: Res<ButtonInput<KeyCode>>,
    mut paddle_velocity: Single<&mut Velocity, With<Player>>,
) {
    if keyboard_input.pressed(KeyCode::ArrowUp) {
        paddle_velocity.0.y = PADDLE_SPEED;
    } else if keyboard_input.pressed(KeyCode::ArrowDown) {
        paddle_velocity.0.y = -PADDLE_SPEED;
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

fn move_ball(ball: Single<(&mut Position, &Velocity), With<Ball>>) {
    let (mut position, velocity) = ball.into_inner();
    position.0 += velocity.0 * BALL_SPEED;
}

fn spawn_camera(mut commands: Commands) {
    commands.spawn(Camera2d);
}

fn detect_goal(
    ball: Single<(&Position, &Collider), With<Ball>>,
    player: Single<Entity, (With<Player>, Without<Ai>)>,
    ai: Single<Entity, (With<Ai>, Without<Player>)>,
    window: Single<&Window>,
    mut commands: Commands,
) {
    let (ball_position, ball_collider) = ball.into_inner();
    let half_window_size = window.resolution.size() / 2.;

    if ball_position.0.x - ball_collider.half_size().x > half_window_size.x {
        commands.trigger(Scored { scorer: *player });
    }

    if ball_position.0.x + ball_collider.half_size().x < -half_window_size.x {
        commands.trigger(Scored { scorer: *ai });
    }
}
fn reset_ball(
    _event: On<Scored>,
    is_ai: Query<&Ai>,
    is_player: Query<&Player>,
    ball: Single<(&mut Position, &mut Velocity), With<Ball>>,
) {
    let (mut ball_position, mut ball_velocity) = ball.into_inner();
    ball_position.0 = Vec2::ZERO;
    if is_ai.get(_event.scorer).is_ok() {
        ball_velocity.0 = Vec2::new(BALL_SPEED, 2.);
    }

    if is_player.get(_event.scorer).is_ok() {
        ball_velocity.0 = Vec2::new(-BALL_SPEED, 2.);
    }
}

fn update_score(
    event: On<Scored>,
    mut score: ResMut<Score>,
    is_ai: Query<&Ai>,
    is_player: Query<&Player>,
) {
    if is_ai.get(event.scorer).is_ok() {
        score.ai += 1;
        info!("Ai Scored! {} - {}", score.player, score.ai);
    }

    if is_player.get(event.scorer).is_ok() {
        score.player += 1;
        info!("Player scored! {} - {}", score.player, score.ai);
    }
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

    let ai_score = (
        AiScore,
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
        container,
        children![(header, children![player_score, ai_score])],
    ));
}

fn move_ai(ai: Single<(&mut Velocity, &Position), With<Ai>>, ball: Single<&Position, With<Ball>>) {
    let (mut velocity, position) = ai.into_inner();
    let a_to_b = ball.0 - position.0;
    velocity.0.y = a_to_b.y.signum() * PADDLE_SPEED;
}

fn update_scoreboard(
    mut player_score: Single<&mut Text, (With<PlayerScore>, Without<AiScore>)>,
    mut ai_score: Single<&mut Text, (With<AiScore>, Without<PlayerScore>)>,
    score: Res<Score>,
) {
    if score.is_changed() {
        player_score.0 = score.player.to_string();
        ai_score.0 = score.ai.to_string();
    }
}

fn project_positions(mut positionables: Query<(&mut Transform, &Position)>) {
    for (mut transform, position) in &mut positionables {
        transform.translation = position.0.extend(0.);
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

fn handle_collisions(
    ball: Single<(&mut Velocity, &mut Position, &Collider), With<Ball>>,
    paddles: Query<&Position, (With<Paddle>, Without<Ball>)>,
    gutters: Query<(&Position, &Collider), (With<Gutter>, Without<Ball>)>,
) {
    let (mut ball_velocity, mut ball_position, ball_collider) = ball.into_inner();

    // Paddles always bounce the ball horizontally off their facing side
    for paddle_position in &paddles {
        let delta = ball_position.0 - paddle_position.0;
        let overlap = PADDLE_SHAPE.half_size + ball_collider.half_size() - delta.abs();

        if overlap.x > 0. && overlap.y > 0. {
            let dir = if delta.x != 0. {
                delta.x.signum()
            } else {
                -ball_velocity.0.x.signum()
            };
            ball_velocity.0.x = dir * ball_velocity.0.x.abs();
            ball_position.0.x += dir * overlap.x;
        }
    }

    for (gutter_position, gutter_collider) in &gutters {
        if let Some(collision) = collide_with_side(
            Aabb2d::new(ball_position.0, ball_collider.half_size()),
            Aabb2d::new(gutter_position.0, gutter_collider.half_size()),
        ) {
            match collision {
                Collision::Left | Collision::Right => {
                    ball_velocity.0.x *= -1.;
                }
                Collision::Top | Collision::Bottom => {
                    ball_velocity.0.y *= -1.;
                }
            }
        }
    }
}

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(networking::NetworkingPlugin)
        .insert_resource(Score { player: 0, ai: 0 })
        .init_resource::<networking_demo::Peers>()
        .init_resource::<networking_demo::RemoteSnapshot>()
        .add_systems(Startup, networking_demo::setup)
        .add_systems(
            Startup,
            (
                spawn_camera,
                spawn_paddles,
                spawn_gutters,
                spawn_ball,
                spawn_scoreboard,
            ),
        )
        .add_systems(Update, networking_demo::broadcast_snapshot)
        .add_systems(
            FixedUpdate,
            (
                project_positions,
                move_ball.after(project_positions),
                handle_collisions.after(move_ball),
                move_paddles.before(project_positions),
                handle_player_input.before(move_paddles),
                constrain_paddle_position.after(move_paddles),
                detect_goal.after(move_ball),
                update_scoreboard,
                move_ai,
            ),
        )
        .add_observer(reset_ball)
        .add_observer(update_score)
        .add_observer(networking_demo::on_peer_connected)
        .add_observer(networking_demo::on_peer_disconnected)
        .add_observer(networking_demo::on_game_request)
        .add_observer(networking_demo::on_game_response)
        .run();
}

/// Contiguous network → game glue: tracks connected peers, pushes a snapshot
/// of the real game state to them ~10×/s, and keeps the last snapshot the
/// other side sent us.
mod networking_demo {
    use bevy::prelude::*;
    use libp2p::PeerId;

    use super::protocol::{self, GameSnapshot, Request};
    use super::{Ai, Ball, Player, Position, Score};
    use crate::networking::{NetChannels, NetCommand, NetEvent};

    /// Peers we are currently connected to.
    #[derive(Resource, Default)]
    pub struct Peers(Vec<PeerId>);

    /// Last snapshot received from the other side, available to game systems.
    #[derive(Resource, Default)]
    pub struct RemoteSnapshot(Option<GameSnapshot>);

    /// Monotonic counter stamped into every snapshot we send.
    #[derive(Resource, Default)]
    pub struct SnapshotSeq(u64);

    /// Drives how often we push snapshots.
    #[derive(Resource)]
    pub struct SnapshotTimer(Timer);

    const SNAPSHOT_INTERVAL: f32 = 0.1;

    pub fn setup(mut commands: Commands) {
        commands.insert_resource(SnapshotTimer(Timer::from_seconds(
            SNAPSHOT_INTERVAL,
            TimerMode::Repeating,
        )));
    }

    pub fn on_peer_connected(
        ev: On<NetEvent>,
        mut peers: ResMut<Peers>,
        channels: Res<NetChannels>,
    ) {
        if let NetEvent::PeerConnected(peer) = ev.event() {
            if !peers.0.contains(peer) {
                peers.0.push(*peer);
                info!("Peer {peer} connected, saying hello");
                let _ = channels.commands.send(NetCommand::SendRequest {
                    peer: *peer,
                    request: Request::Hello,
                });
            }
        }
    }

    pub fn on_peer_disconnected(ev: On<NetEvent>, mut peers: ResMut<Peers>) {
        if let NetEvent::PeerDisconnected(peer) = ev.event() {
            peers.0.retain(|p| p != peer);
            info!("Peer {peer} disconnected");
        }
    }

    pub fn on_game_request(ev: On<NetEvent>, mut remote: ResMut<RemoteSnapshot>) {
        if let NetEvent::GameRequest { peer, request } = ev.event() {
            match request {
                Request::Hello => info!("Greeting received from {peer}"),
                Request::State(snapshot) => remote.0 = Some(*snapshot),
                Request::Paddle { y } => debug!("Paddle y={y} received from {peer}"),
            }
        }
    }

    pub fn on_game_response(ev: On<NetEvent>) {
        if let NetEvent::GameResponse { peer, response } = ev.event() {
            debug!("Response from {peer}: {response:?}");
        }
    }

    pub fn broadcast_snapshot(
        time: Res<Time>,
        mut timer: ResMut<SnapshotTimer>,
        channels: Res<NetChannels>,
        peers: Res<Peers>,
        ball: Single<&Position, With<Ball>>,
        player: Single<&Position, (With<Player>, Without<Ai>)>,
        ai: Single<&Position, (With<Ai>, Without<Player>)>,
        score: Res<Score>,
        mut seq: ResMut<SnapshotSeq>,
    ) {
        if !timer.0.tick(time.delta()).just_finished() {
            return;
        }

        let snapshot = GameSnapshot {
            seq: seq.0,
            ball: to_point(ball.0),
            player_paddle: to_point(player.0),
            opponent_paddle: to_point(ai.0),
            player_score: score.player,
            opponent_score: score.ai,
        };
        seq.0 += 1;

        for peer in &peers.0 {
            let _ = channels.commands.send(NetCommand::SendRequest {
                peer: *peer,
                request: Request::State(snapshot),
            });
        }
    }

    fn to_point(Vec2 { x, y }: Vec2) -> protocol::Point2 {
        protocol::Point2 { x, y }
    }
}
