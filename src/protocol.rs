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
    /// Greet the other side after connecting, carrying our display name so the
    /// peer can label us in its player graph. `name` defaults to empty for
    /// peers that don't send one.
    Hello {
        #[serde(default)]
        name: String,
    },
    /// Ask the other side for a match ("let's play"). The receiver accepts by
    /// replying with [`Request::MatchStart`].
    InviteToPlay,
    /// Confirms the match to the challenger and tells them to start playing.
    MatchStart,
    /// Push a fresh authoritative snapshot to the other side.
    State(GameSnapshot),
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
    MatchFound {
        opponent: String,
        addresses: Vec<Addr>,
    },
}

/// RPC replies from the gateway.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GatewayResponse {
    /// `username`, current rating and rank after [`GatewayRequest::Register`].
    Registered {
        username: String,
        rating: i32,
        rank: String,
    },
    /// Currently queued; `position` is the number of players ahead of us.
    Queued {
        position: u32,
    },
    /// Left the queue.
    Dequeued,
    /// The gateway's view of our rating and rank after a report.
    Rating {
        rating: i32,
        rank: String,
    },
    Pong,
    Error(String),
}

/// Rank tiers, ordered from lowest to highest. Derived from the ELO rating.
pub const RANKS: [&str; 9] = [
    "Brick",
    "Bronze",
    "Silver",
    "Gold",
    "Platinum",
    "Diamond",
    "Obsidian",
    "Pong Master",
    "Pong Legend",
];

/// Lower bound (inclusive) of each rank's ELO range.
const RANK_THRESHOLDS: [i32; 9] = [0, 1000, 1200, 1400, 1600, 1800, 2000, 2200, 2400];

/// Maps an ELO rating to one of the [`RANKS`].
pub fn rank_for_rating(rating: i32) -> &'static str {
    let mut rank = RANKS[0];
    for (i, threshold) in RANK_THRESHOLDS.iter().enumerate() {
        if rating >= *threshold {
            rank = RANKS[i];
        }
    }
    rank
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[test]
    fn rank_below_1000_is_brick() {
        assert_eq!(rank_for_rating(0), "Brick");
        assert_eq!(rank_for_rating(999), "Brick");
    }

    #[test]
    fn rank_at_boundaries() {
        assert_eq!(rank_for_rating(1000), "Bronze");
        assert_eq!(rank_for_rating(1199), "Bronze");
        assert_eq!(rank_for_rating(1200), "Silver");
        assert_eq!(rank_for_rating(1399), "Silver");
        assert_eq!(rank_for_rating(1400), "Gold");
        assert_eq!(rank_for_rating(1599), "Gold");
        assert_eq!(rank_for_rating(1600), "Platinum");
        assert_eq!(rank_for_rating(1799), "Platinum");
        assert_eq!(rank_for_rating(1800), "Diamond");
        assert_eq!(rank_for_rating(1999), "Diamond");
        assert_eq!(rank_for_rating(2000), "Obsidian");
        assert_eq!(rank_for_rating(2199), "Obsidian");
        assert_eq!(rank_for_rating(2200), "Pong Master");
        assert_eq!(rank_for_rating(2399), "Pong Master");
    }

    #[test]
    fn rank_top_tier() {
        assert_eq!(rank_for_rating(2400), "Pong Legend");
        assert_eq!(rank_for_rating(9999), "Pong Legend");
        assert_eq!(rank_for_rating(i32::MAX), "Pong Legend");
    }

    #[test]
    fn negative_ratings_are_brick() {
        assert_eq!(rank_for_rating(-1), "Brick");
        assert_eq!(rank_for_rating(i32::MIN), "Brick");
    }

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

    #[test]
    fn hello_carries_name() {
        let req = Request::Hello {
            name: "test_player".into(),
        };
        let bytes = cbor4ii::serde::to_vec(Vec::new(), &req).expect("serialize");
        let decoded: Request = cbor4ii::serde::from_slice(&bytes).expect("deserialize");
        assert_eq!(
            decoded,
            Request::Hello {
                name: "test_player".into()
            }
        );
    }

    #[test]
    fn gateway_request_roundtrips() {
        let req = GatewayRequest::MatchFound {
            opponent: "12D3KooExample".into(),
            addresses: vec!["/ip4/1.2.3.4/tcp/4001/p2p/test".into()],
        };
        let bytes = cbor4ii::serde::to_vec(Vec::new(), &req).expect("serialize");
        let decoded: GatewayRequest = cbor4ii::serde::from_slice(&bytes).expect("deserialize");
        assert_eq!(decoded, req);
    }
}
