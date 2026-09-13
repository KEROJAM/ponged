use libp2p::StreamProtocol;
use serde::{Deserialize, Serialize};

/// The libp2p protocol id for the gateway matchmaking + lobby RPC
/// (`/pong/matchmaking/1.0.0`). Game state traffic keeps its own protocol
/// (`/pong/state/1.0.0`) so the two flows never compete for one codec.
pub const GATEWAY_PROTOCOL: StreamProtocol = StreamProtocol::new("/pong/matchmaking/1.0.0");

/// Agent version the gateway advertises over `identify`. The client uses it to
/// recognise the peer it dialed as the matchmaking server.
pub const GATEWAY_AGENT_VERSION: &str = "pong-gateway/1.0.0";

/// A position on the playing field. Plain `f32` pairs so game state can
/// cross the wire without needing Bevy's serde support.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Point2 {
    pub x: f32,
    pub y: f32,
}

/// A point-in-time snapshot of the game. The node with `is_host == true` runs
/// the authoritative simulation (ball, goals, score); the other side renders
/// this state and only sends back its own paddle through `player_paddle`.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct GameSnapshot {
    pub seq: u64,
    /// Whether the sender of this snapshot is the authoritative host.
    pub is_host: bool,
    pub ball: Point2,
    pub ball_velocity: Point2,
    /// The sender's own paddle position.
    pub player_paddle: Point2,
    /// Where the sender's opponent paddle currently sits.
    pub opponent_paddle: Point2,
    /// Score as seen by the sender: its own points and the opponent's.
    pub player_score: u32,
    pub opponent_score: u32,
    /// True once either player reached the win score. The ball stops moving
    /// but snapshots keep flowing so the guest can render the final state.
    #[serde(default)]
    pub match_over: bool,
    /// Current rally speed multiplier (`1.0` = base ball speed, grows with
    /// each paddle hit). Missing on snapshots from old peers ⇒ normal speed.
    #[serde(default = "default_ball_speed_mult")]
    pub ball_speed_mult: f32,
}

/// Old snapshots without `ball_speed_mult` must default to normal speed.
fn default_ball_speed_mult() -> f32 {
    1.0
}

/// Requests sent over the `/pong/state/1.0.0` protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    /// Greet the other side after connecting.
    Hello,
    /// Ask the other side for a match ("let's play"). The receiver accepts by
    /// replying with [`Request::MatchStart`].
    InviteToPlay,
    /// Confirms the match to the challenger and tells them to start playing.
    MatchStart,
    /// Push a fresh authoritative snapshot to the other side.
    State(GameSnapshot),
    /// Tell the other side where this player's paddle is.
    Paddle { y: f32 },
    /// The host hands authority (and its last authoritative snapshot) to the
    /// guest before leaving. The guest becomes host and keeps the match going.
    MigrateHost(GameSnapshot),
    /// The former guest acknowledges it has taken over as the host (used with
    /// [`Request::MigrateHost`]).
    HostMigrated,
}

/// Responses to the above.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Response {
    Welcome,
    State(GameSnapshot),
    Ack,
}

// --- Gateway matchmaking (M6) ---------------------------------------------

/// A dialable way to reach a player: a multiaddr as a string (usually either a
/// direct TCP address or a `/p2p-circuit` relayed address through the gateway).
pub type Addr = String;

/// RPC requests the game (client) sends to the gateway over
/// [`GATEWAY_PROTOCOL`], plus the one the gateway *pushes* back to announce a
/// reserved match.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GatewayRequest {
    /// Tell the gateway who we are. Creates the rating entry on first contact.
    Register { username: String },
    /// Put us in the matchmaking queue (matched against peers of similar ELO).
    QueueMatch,
    /// Leave the matchmaking queue.
    LeaveQueue,
    /// Report a finished match so the gateway updates both ratings (ELO).
    ReportResult {
        opponent: String,
        my_score: u32,
        opponent_score: u32,
    },
    /// Cheap keepalive / status query.
    Ping,
    /// **Gateway → client.** A match was reserved: `opponent` is the matched
    /// player's base58 `PeerId`; `addresses` are dialable relay/direct
    /// multiaddrs to reach them. Delivered to both sides (via M3).
    MatchFound { opponent: String, addresses: Vec<Addr> },
}

/// RPC replies from the gateway.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GatewayResponse {
    /// `username` and current rating after [`GatewayRequest::Register`].
    Registered { username: String, rating: i32 },
    /// Currently queued; `position` is the number of players ahead of us.
    Queued { position: u32 },
    /// Left the queue.
    Dequeued,
    /// The gateway's view of our rating after a report.
    Rating { rating: i32 },
    Pong,
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[test]
    fn snapshot_roundtrips_through_cbor() {
        let snap = GameSnapshot {
            seq: 42,
            is_host: true,
            ball: Point2 { x: 1.5, y: -2.5 },
            ball_velocity: Point2 { x: -0.5, y: 0.25 },
            player_paddle: Point2 { x: -380.0, y: 10.0 },
            opponent_paddle: Point2 { x: 380.0, y: -10.0 },
            player_score: 3,
            opponent_score: 2,
            match_over: false,
            ball_speed_mult: 1.5,
        };
        let bytes = cbor4ii::serde::to_vec(Vec::new(), &snap).expect("serialize");
        let decoded: GameSnapshot = cbor4ii::serde::from_slice(&bytes).expect("deserialize");
        assert_eq!(decoded, snap);
    }

    #[test]
    fn old_snapshot_without_match_over_deserializes() {
        // Simulates an older peer that doesn't know the `match_over` field:
        // it must deserialize with `match_over == false` (serde default).
        #[derive(Serialize)]
        struct OldSnapshot {
            seq: u64,
            is_host: bool,
            ball: Point2,
            ball_velocity: Point2,
            player_paddle: Point2,
            opponent_paddle: Point2,
            player_score: u32,
            opponent_score: u32,
        }
        let old = OldSnapshot {
            seq: 7,
            is_host: false,
            ball: Point2::default(),
            ball_velocity: Point2::default(),
            player_paddle: Point2::default(),
            opponent_paddle: Point2::default(),
            player_score: 1,
            opponent_score: 0,
        };
        let bytes = cbor4ii::serde::to_vec(Vec::new(), &old).expect("serialize");
        let decoded: GameSnapshot = cbor4ii::serde::from_slice(&bytes).expect("deserialize");
        assert!(!decoded.match_over);
        assert_eq!(decoded.player_score, 1);
        assert_eq!(decoded.ball_speed_mult, 1.0);
    }
}