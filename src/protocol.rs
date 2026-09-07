use serde::{Deserialize, Serialize};

/// A position on the playing field. Plain `f32` pairs so game state can
/// cross the wire without needing Bevy's serde support.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Point2 {
    pub x: f32,
    pub y: f32,
}

/// A point-in-time snapshot of the whole game, sent by the authoritative
/// host to the client (and echoed back for round-trip checks).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct GameSnapshot {
    pub seq: u64,
    pub ball: Point2,
    pub player_paddle: Point2,
    pub opponent_paddle: Point2,
    pub player_score: u32,
    pub opponent_score: u32,
}

/// Requests sent over the `/pong/state/1.0.0` protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    /// Greet the other side after connecting.
    Hello,
    /// Push a fresh authoritative snapshot to the other side.
    State(GameSnapshot),
    /// Tell the other side where this player's paddle is.
    Paddle { y: f32 },
}

/// Responses to the above.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Response {
    Welcome,
    State(GameSnapshot),
    Ack,
}