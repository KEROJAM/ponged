//! Headless authoritative simulation for the host.
//!
//! Bevy's winit runner only steps the app when the window asks for a redraw.
//! On Wayland a minimized/occluded window stops receiving redraws, which used
//! to freeze the whole game for the opponent: the host stopped simulating the
//! ball and stopped pushing snapshots. Running the host's simulation (and its
//! snapshot broadcast) on a plain thread driven by a wall clock fixes that,
//! because the simulation no longer depends on the window at all.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use bevy::math::bounding::{Aabb2d, BoundingVolume, IntersectsVolume};
use bevy::prelude::*;
use libp2p::PeerId;

use crate::menu::Opponent;
use crate::networking::{NetChannels, NetCommand};
use crate::networking_demo::IsHost;
use crate::{Ball, Opponent as OpponentMarker, Player, Position, Score};
use proyecto_final::protocol::{GameSnapshot, Point2, Request};

// Geometry & tuning mirrors the ECS constants in `main.rs` (kept in sync so the
// ported simulation behaves exactly like the old ECS one).
use crate::{
    BALL_SIZE, BALL_SPEED, FIELD_SIZE, GUTTER_HEIGHT, PADDLE_SHAPE, PADDLE_SPEED,
};

/// Distance from the field edge to gutters and paddles (see `spawn_gutters` /
/// `spawn_paddles` in `main.rs`).
const EDGE: f32 = 20.0;

/// Interval between physics steps. Matches Bevy's default fixed timestep
/// (`Duration::from_micros(15625)` = 1/64 s) so the ball/paddle speeds the ECS
/// code used are preserved exactly.
const FIXED_DT: Duration = Duration::from_micros(15625);

/// How often the host pushes a snapshot to the guest (same as the old
/// `SNAPSHOT_INTERVAL` in `networking_demo`).
const SNAPSHOT_DT: Duration = Duration::from_micros(1_000_000 / 30);

/// Commands sent to the running host simulation from outside (Bevy keyboard
/// input, and the remote player's paddle forwarded by the network thread).
#[derive(Debug, Clone)]
pub enum SimCommand {
    /// The local player's paddle velocity (`±PADDLE_SPEED` or `0`).
    SetPlayerVelocity(f32),
    /// The most recent `y` of the remote player's paddle.
    RemotePaddle(f32),
}

/// The authoritative state the host renders from. Updated by the sim thread.
#[derive(Debug, Clone)]
pub struct SimOutput {
    pub ball: Vec2,
    pub ball_velocity: Vec2,
    pub player_paddle: Vec2,
    pub opponent_paddle: Vec2,
    pub score_player: u32,
    pub score_opponent: u32,
    /// Monotonic snapshot counter, kept so a migrated host can continue
    /// numbering snapshots from where the previous host stopped.
    pub seq: u64,
}

impl SimOutput {
    fn new() -> Self {
        SimOutput {
            ball: Vec2::ZERO,
            ball_velocity: Vec2::new(-BALL_SPEED, BALL_SPEED),
            player_paddle: Vec2::new(-FIELD_SIZE.x / 2.0 + EDGE, 0.0),
            opponent_paddle: Vec2::new(FIELD_SIZE.x / 2.0 - EDGE, 0.0),
            score_player: 0,
            score_opponent: 0,
            seq: 0,
        }
    }
}

/// Handle for the running match simulation. Dropping it (leaving the match)
/// drops the last command sender, so the thread exits on its next recv.
#[derive(Resource)]
pub struct MatchSim {
    output: Arc<Mutex<SimOutput>>,
    sender: Sender<SimCommand>,
}

/// Forwards inbound `State` snapshots from the networking thread into the
/// running host simulation. Only the host (which actually simulates) registers
/// a sink; guests keep routing snapshots through Bevy as before.
static REMOTE_PADDLE_SINK: Mutex<Option<Sender<SimCommand>>> = Mutex::new(None);

fn set_remote_paddle_sink(sink: Option<Sender<SimCommand>>) {
    *REMOTE_PADDLE_SINK.lock().expect("paddle sink poisoned") = sink;
}

/// Clone of the sink for the networking thread (or `None` if no match sim is
/// running, e.g. on the guest or in the menu).
pub fn remote_paddle_sink() -> Option<Sender<SimCommand>> {
    REMOTE_PADDLE_SINK.lock().expect("paddle sink poisoned").clone()
}

// --- Bevy glue ------------------------------------------------------------

/// Startup system (host only): spawns the simulation thread and registers the
/// remote-paddle sink so the match keeps running even if this window is
/// minimized/occluded and Bevy stops updating.
pub fn start_match_sim(
    mut commands: Commands,
    host: Res<IsHost>,
    opponent: Res<Opponent>,
    channels: Res<NetChannels>,
) {
    start_seeded_match_sim(&mut commands, &host, &opponent, &channels, None);
}

/// Starts the authoritative simulation from a given seed snapshot (M7). Used
/// both by the normal "host a fresh match" flow (seed `None`) and by host
/// migration, where the guest takes over with the last authoritative state so
/// the match continues without bouncing back to the menu.
pub fn start_seeded_match_sim(
    commands: &mut Commands,
    host: &IsHost,
    opponent: &Opponent,
    channels: &NetChannels,
    seed: Option<GameSnapshot>,
) {
    if !host.0 {
        return;
    }
    let Some(peer) = opponent.0 else {
        warn!("Started host match with no opponent");
        return;
    };

    let (sender, receiver) = channel::<SimCommand>();
    let output = Arc::new(Mutex::new(SimOutput::new()));
    let thread_output = output.clone();
    let net_commands = channels.commands.clone();

    thread::spawn(move || run_sim(receiver, peer, net_commands, thread_output, seed));

    set_remote_paddle_sink(Some(sender.clone()));
    commands.insert_resource(MatchSim { output, sender });
    info!("Host simulation thread started");
}

/// Teardown system (host only): drop the sim handle and the sink. The thread
/// notices the channel is gone and exits.
pub fn stop_match_sim(mut commands: Commands, sim: Option<Res<MatchSim>>) {
    if sim.is_some() {
        set_remote_paddle_sink(None);
        info!("Host simulation thread stopped");
    }
    commands.remove_resource::<MatchSim>();
}

/// (Host only) Forwards local keyboard input to the simulation thread.
pub fn forward_player_input(
    keyboard_input: Res<ButtonInput<KeyCode>>,
    sim: Option<Res<MatchSim>>,
) {
    let Some(sim) = sim else { return };
    let velocity = if keyboard_input.pressed(KeyCode::ArrowUp) {
        PADDLE_SPEED
    } else if keyboard_input.pressed(KeyCode::ArrowDown) {
        -PADDLE_SPEED
    } else {
        0.0
    };
    let _ = sim
        .sender
        .send(SimCommand::SetPlayerVelocity(velocity));
}

/// (Host only) Applies the authoritative simulation state to the entities we
/// render and to the `Score` resource the HUD reads.
pub fn pull_sim_state(
    sim: Option<Res<MatchSim>>,
    mut score: ResMut<Score>,
    mut player: Single<&mut Position, (With<Player>, Without<Ball>)>,
    mut opponent: Single<&mut Position, (With<OpponentMarker>, Without<Ball>)>,
    mut ball: Single<&mut Position, With<Ball>>,
) {
    let Some(sim) = sim else { return };
    let output = sim.output.lock().expect("sim output poisoned");
    player.0 = output.player_paddle;
    opponent.0 = output.opponent_paddle;
    ball.0 = output.ball;
    score.player = output.score_player;
    score.opponent = output.score_opponent;
}

// --- Simulation thread ----------------------------------------------------

#[derive(Debug)]
enum CollisionSide {
    Left,
    Right,
    Top,
    Bottom,
}

/// Mirrors `main::Collider::half_size()` for the ball: a `Rectangle::new(
/// BALL_SIZE, BALL_SIZE)` collider.
fn ball_half_size() -> Vec2 {
    Vec2::new(BALL_SIZE / 2.0, BALL_SIZE / 2.0)
}

/// Port of `main::handle_collisions` + `main::detect_goal` + friends, on a
/// plain data struct instead of ECS entities.
struct SimState {
    ball: Vec2,
    ball_velocity: Vec2,
    player_paddle: Vec2,
    opponent_paddle: Vec2,
    /// Interpolation target for the opponent paddle. Snapshots arrive at
    /// ~30 Hz but physics ticks at 64 Hz; easing toward the target keeps the
    /// remote paddle as smooth as the old two-sample lerp without re-anchoring
    /// state each tick.
    opponent_target_y: f32,
    player_velocity: f32,
    score_player: u32,
    score_opponent: u32,
    seq: u64,
}

impl SimState {
    fn new(seed: Option<GameSnapshot>) -> Self {
        let mut state = SimState {
            ball: Vec2::ZERO,
            ball_velocity: Vec2::new(-BALL_SPEED, BALL_SPEED),
            player_paddle: Vec2::new(-FIELD_SIZE.x / 2.0 + EDGE, 0.0),
            opponent_paddle: Vec2::new(FIELD_SIZE.x / 2.0 - EDGE, 0.0),
            opponent_target_y: 0.0,
            player_velocity: 0.0,
            score_player: 0,
            score_opponent: 0,
            seq: 0,
        };
        // Host migration (M7): the guest adopts the previous host's
        // authoritative frame. The snapshot is in the sender's frame, where
        // the sender's own paddle sits on the LEFT; as the new host our own
        // paddle is mirrored to the sender's RIGHT, and vice-versa for the old
        // host. Scores swap accordingly.
        if let Some(snap) = seed {
            state.ball = Vec2::new(snap.ball.x, snap.ball.y);
            state.ball_velocity = Vec2::new(snap.ball_velocity.x, snap.ball_velocity.y);
            state.player_paddle = Vec2::new(-FIELD_SIZE.x / 2.0 + EDGE, snap.opponent_paddle.y);
            state.opponent_paddle = Vec2::new(FIELD_SIZE.x / 2.0 - EDGE, snap.player_paddle.y);
            state.opponent_target_y = snap.player_paddle.y;
            state.score_player = snap.opponent_score;
            state.score_opponent = snap.player_score;
            state.seq = snap.seq;
        }
        state
    }

    fn step(&mut self) {
        // Same per-tick order as the old ECS FixedUpdate set: paddles move and
        // are constrained, then the ball moves, bounces, and scores.
        self.player_paddle.y += self.player_velocity;
        self.opponent_paddle.y += (self.opponent_target_y - self.opponent_paddle.y) * 0.8;
        constrain_paddle(&mut self.player_paddle);
        constrain_paddle(&mut self.opponent_paddle);

        self.ball += self.ball_velocity * BALL_SPEED;
        self.handle_paddle_bounces();
        self.handle_gutter_bounces();
        self.detect_goals();
    }

    fn handle_paddle_bounces(&mut self) {
        for paddle in [self.player_paddle, self.opponent_paddle] {
            let delta = self.ball - paddle;
            let overlap = PADDLE_SHAPE.half_size + ball_half_size() - delta.abs();

            if overlap.x > 0.0 && overlap.y > 0.0 {
                // Always bounce the ball horizontally off the paddle's facing
                // side (same logic as `main::handle_collisions`).
                let direction = if delta.x != 0.0 {
                    delta.x.signum()
                } else {
                    -self.ball_velocity.x.signum()
                };
                self.ball_velocity.x = direction * self.ball_velocity.x.abs();
                self.ball.x += direction * overlap.x;
            }
        }
    }

    fn handle_gutter_bounces(&mut self) {
        let ball_half = ball_half_size();
        let ball_box = Aabb2d::new(self.ball, ball_half);
        let gutter_height = GUTTER_HEIGHT;
        let gutter_half_size = Vec2::new(FIELD_SIZE.x / 2.0, gutter_height / 2.0);

        for gutter_y in [
            FIELD_SIZE.y / 2.0 - EDGE,
            -(FIELD_SIZE.y / 2.0 - EDGE),
        ] {
            let wall = Aabb2d::new(Vec2::new(0.0, gutter_y), gutter_half_size);
            if let Some(side) = collide_with_side(ball_box, wall) {
                match side {
                    CollisionSide::Left | CollisionSide::Right => self.ball_velocity.x *= -1.0,
                    CollisionSide::Top | CollisionSide::Bottom => self.ball_velocity.y *= -1.0,
                }
            }
        }
    }

    fn detect_goals(&mut self) {
        let half_field = FIELD_SIZE / 2.0;
        let ball_half = ball_half_size();

        if self.ball.x - ball_half.x > half_field.x {
            // Ball left the field on the opponent's side: we scored.
            self.score_player += 1;
            self.reset_ball(Vec2::new(-BALL_SPEED, 2.0));
        } else if self.ball.x + ball_half.x < -half_field.x {
            // Ball left the field on our side: the opponent scored.
            self.score_opponent += 1;
            self.reset_ball(Vec2::new(BALL_SPEED, 2.0));
        }
    }

    fn reset_ball(&mut self, velocity: Vec2) {
        self.ball = Vec2::ZERO;
        self.ball_velocity = velocity;
    }
}

fn constrain_paddle(position: &mut Vec2) {
    // Paddles are trapped between the two gutters (mirrors
    // `main::constrain_paddle_position`).
    let gutter_y = FIELD_SIZE.y / 2.0 - EDGE;
    let gutter_half_y = GUTTER_HEIGHT / 2.0;
    let limit = gutter_y + gutter_half_y + PADDLE_SHAPE.half_size.y;
    position.y = position.y.clamp(-limit, limit);
}

fn collide_with_side(ball: Aabb2d, wall: Aabb2d) -> Option<CollisionSide> {
    if !ball.intersects(&wall) {
        return None;
    }

    let closest_point = wall.closest_point(ball.center());
    let offset = ball.center() - closest_point;

    let side = if offset.x.abs() > offset.y.abs() {
        if offset.x < 0.0 {
            CollisionSide::Left
        } else {
            CollisionSide::Right
        }
    } else if offset.y > 0.0 {
        CollisionSide::Top
    } else {
        CollisionSide::Bottom
    };

    Some(side)
}

fn apply(state: &mut SimState, command: SimCommand) {
    match command {
        SimCommand::SetPlayerVelocity(velocity) => state.player_velocity = velocity,
        SimCommand::RemotePaddle(y) => state.opponent_target_y = y,
    }
}

fn to_point(Vec2 { x, y }: Vec2) -> Point2 {
    Point2 { x, y }
}

fn build_snapshot(state: &SimState) -> GameSnapshot {
    GameSnapshot {
        seq: state.seq,
        is_host: true,
        ball: to_point(state.ball),
        ball_velocity: to_point(state.ball_velocity),
        player_paddle: to_point(state.player_paddle),
        opponent_paddle: to_point(state.opponent_paddle),
        player_score: state.score_player,
        opponent_score: state.score_opponent,
    }
}

fn publish(state: &SimState, output: &Arc<Mutex<SimOutput>>) {
    let mut output = output.lock().expect("sim output poisoned");
    *output = SimOutput {
        ball: state.ball,
        ball_velocity: state.ball_velocity,
        player_paddle: state.player_paddle,
        opponent_paddle: state.opponent_paddle,
        score_player: state.score_player,
        score_opponent: state.score_opponent,
        seq: state.seq,
    };
}

/// Builds a `GameSnapshot` from the live simulation output (M7/host leaving).
/// Lets a leaving host hand its authoritative state to the guest before it
/// disconnects.
pub fn build_host_snapshot(sim: &MatchSim) -> GameSnapshot {
    let output = sim.output.lock().expect("sim output poisoned");
    GameSnapshot {
        seq: output.seq,
        is_host: true,
        ball: to_point(output.ball),
        ball_velocity: to_point(output.ball_velocity),
        player_paddle: to_point(output.player_paddle),
        opponent_paddle: to_point(output.opponent_paddle),
        player_score: output.score_player,
        opponent_score: output.score_opponent,
    }
}

fn run_sim(
    receiver: Receiver<SimCommand>,
    peer: PeerId,
    net_commands: tokio::sync::mpsc::UnboundedSender<NetCommand>,
    output: Arc<Mutex<SimOutput>>,
    seed: Option<GameSnapshot>,
) {
    let mut state = SimState::new(seed);
    let mut accumulator = Duration::ZERO;
    let mut last_tick = Instant::now();
    let mut last_snapshot = Instant::now();

    loop {
        // `recv_timeout` paces the loop (so we don't spin) while still waking
        // promptly for new commands. A disconnected channel means the match is
        // over: the only senders lived in `MatchSim` and the paddle sink.
        match receiver.recv_timeout(FIXED_DT / 2) {
            Ok(command) => apply(&mut state, command),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }

        let now = Instant::now();
        accumulator = accumulator.saturating_add(now.saturating_duration_since(last_tick));
        last_tick = now;
        // Don't spiral after a long stall (sleep, heavy load): run at most
        // a handful of steps to catch up.
        if accumulator > FIXED_DT * 8 {
            accumulator = FIXED_DT * 8;
        }

        while accumulator >= FIXED_DT {
            state.step();
            publish(&state, &output);
            accumulator -= FIXED_DT;
        }

        if now.saturating_duration_since(last_snapshot) >= SNAPSHOT_DT {
            let snapshot = build_snapshot(&state);
            let _ = net_commands.send(NetCommand::SendRequest {
                peer,
                request: Request::State(snapshot),
            });
            state.seq += 1;
            last_snapshot = now;
        }
    }
}