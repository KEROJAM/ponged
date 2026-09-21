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
    /// Ask the other side for a match ("let's play"). The receiver confirms
    /// through the pre-match dialog and answers with [`Request::AcceptMatch`].
    InviteToPlay,
    /// Tell the other side we accept the paired match. Once both players have
    /// exchanged [`Request::AcceptMatch`], each side runs a short countdown
    /// and the match starts.
    AcceptMatch,
    /// Tell the other side we will not play: the pairing is cancelled and both
    /// sides go back to searching for another opponent.
    DeclineMatch,
    /// Confirms the match to the challenger and tells them to start playing.
    MatchStart,
    /// The peer ended the match (ESC): both sides leave the game and return to
    /// the menu instead of migrating the host or starting a rematch.
    MatchAbort,
    /// Push a fresh authoritative snapshot to the other side.
    State(GameSnapshot),
    /// The host hands authority (and its last authoritative snapshot) to the
    /// guest before leaving. The guest becomes host and keeps the match going.
    MigrateHost(GameSnapshot),
    /// The former guest acknowledges it has taken over as the host (used with
    /// [`Request::MigrateHost`]).
    HostMigrated,
    /// A chat message sent between peers in the lobby.
    Chat {
        #[serde(default)]
        text: String,
    },
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

/// Rating a brand-new player starts with (inside the Brick tier, < 1000).
///
/// The client, not the gateway, is the source of truth for a player's ELO: it
/// persists the rating locally, sends it at `Register`, and stores whatever the
/// gateway computes back after each match. The gateway keeps a cached copy in
/// SQLite but never outlives a client restart (its key is the ephemeral
/// `PeerId`), so it is never authoritative.
pub const DEFAULT_RATING: i32 = 800;

fn default_rating() -> i32 {
    DEFAULT_RATING
}

/// A rating certified by a gateway.
///
/// When a gateway accepts — or computes — a rating, it signs
/// `rating ‖ seq ‖ <client PeerId>` with the matchmaking keypair. Players
/// keep the newest proof they received in their local history, so a later
/// gateway (or the same one after losing its SQLite) can trust the claim
/// without its own database. Gateways in a deployment that share the same
/// key file can all verify each other's proofs.
///
/// `seq` is a per-player counter that increases every time a proof is issued;
/// seeing a higher `seq` never regresses a rating, which prevents players from
/// replaying an old, higher proof after losing matches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RatingProof {
    pub rating: i32,
    /// Monotonic counter; the client always keeps (and presents) its highest.
    pub seq: u64,
    /// Base58 `PeerId` of the gateway that issued it (informational — the
    /// verifying gateway checks the signature with its own keypair).
    pub gateway: String,
    /// ed25519 signature over `rating ‖ seq ‖ client PeerId` (see gateway).
    pub signature: Vec<u8>,
}

fn default_no_proof() -> Option<RatingProof> {
    None
}

/// RPC requests the game (client) sends to the gateway over
/// [`GATEWAY_PROTOCOL`], plus the one the gateway *pushes* back to announce a
/// reserved match.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GatewayRequest {
    /// Tell the gateway who we are and hand over our current ELO (which the
    /// client keeps locally). Creates the rating entry on first contact.
    /// Older clients omit `rating` (falls back to [`DEFAULT_RATING`]); the
    /// optional `proof` is a gateway-signed certificate that lets a new —
    /// or freshly-started — gateway trust the claimed rating.
    Register {
        username: String,
        #[serde(default = "default_rating")]
        rating: i32,
        #[serde(default = "default_no_proof")]
        proof: Option<RatingProof>,
    },
    /// Put us in the matchmaking queue (matched against peers of similar ELO).
    QueueMatch,
    /// Leave the matchmaking queue.
    LeaveQueue,
    /// Report a finished match so the gateway updates both ratings (ELO).
    /// `match_id` is the gateway-assigned id received in [`GatewayRequest::MatchFound`]
    /// (0 for matches that started outside the gateway's queue, e.g. LAN).
    ReportResult {
        #[serde(default)]
        match_id: u64,
        opponent: String,
        my_score: u32,
        opponent_score: u32,
    },
    /// Cheap keepalive / status query.
    Ping,
    /// **Gateway → client.** A match was reserved: `match_id` uniquely
    /// identifies this pairing for later result reports and moderation;
    /// `opponent` is the matched player's base58 `PeerId`; `addresses` are
    /// dialable relay/direct multiaddrs to reach them. Delivered to both sides.
    MatchFound {
        #[serde(default)]
        match_id: u64,
        opponent: String,
        addresses: Vec<Addr>,
    },
    /// **Gateway → client.** A moderator revoked match `match_id` for not being
    /// played fairly. The gateway rolled back the ELO both players gained from
    /// it and certifies the restored `rating` with a fresh `proof`; clients
    /// must adopt it (and drop the revoked match from their local record).
    MatchRevoked {
        match_id: u64,
        rating: i32,
        #[serde(default = "default_no_proof")]
        proof: Option<RatingProof>,
    },
    /// **Gateway → client.** A moderator corrected the final score of match
    /// `match_id` (each number is that player's own score from their point of
    /// view). ELO was recomputed from the corrected outcome; `rating` is the
    /// certifying proof's value. Clients must adopt it and update their local
    /// record so the win/loss tally stays honest.
    MatchCorrected {
        match_id: u64,
        my_score: u32,
        opponent_score: u32,
        rating: i32,
        #[serde(default = "default_no_proof")]
        proof: Option<RatingProof>,
    },
}

/// RPC replies from the gateway.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GatewayResponse {
    /// `username`, current rating and rank after [`GatewayRequest::Register`].
    /// `proof` certifies the accepted rating; clients store it as their new
    /// ledger entry (absent when talking to a gateway too old to sign).
    Registered {
        username: String,
        rating: i32,
        rank: String,
        #[serde(default = "default_no_proof")]
        proof: Option<RatingProof>,
    },
    /// Currently queued; `position` is the number of players ahead of us.
    Queued {
        position: u32,
    },
    /// Left the queue.
    Dequeued,
    /// The gateway's view of our rating and rank after a report, plus the
    /// freshly-signed `proof` certifying it.
    Rating {
        rating: i32,
        rank: String,
        #[serde(default = "default_no_proof")]
        proof: Option<RatingProof>,
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

/// Index of the rank tier that `rating` currently belongs to.
fn rank_index(rating: i32) -> usize {
    let mut idx = 0;
    for (i, threshold) in RANK_THRESHOLDS.iter().enumerate() {
        if rating >= *threshold {
            idx = i;
        }
    }
    idx
}

/// Maps an ELO rating to one of the [`RANKS`].
pub fn rank_for_rating(rating: i32) -> &'static str {
    RANKS[rank_index(rating)]
}

/// The rank the player will be promoted to next, or `None` at the top tier.
pub fn next_rank_name(rating: i32) -> Option<&'static str> {
    RANKS.get(rank_index(rating) + 1).copied()
}

/// How close `rating` is to the next rank: `0.0` at the floor of the current
/// tier, growing to `1.0` at the next threshold (full for the top tier).
pub fn rank_progress(rating: i32) -> f32 {
    let idx = rank_index(rating);
    if idx == RANK_THRESHOLDS.len() - 1 {
        return 1.0;
    }
    let floor = RANK_THRESHOLDS[idx];
    let next = RANK_THRESHOLDS[idx + 1];
    ((rating - floor) as f32 / (next - floor) as f32).clamp(0.0, 1.0)
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
    fn rank_progress_inside_tier() {
        assert_eq!(rank_progress(300), 0.3);
        assert_eq!(rank_progress(1000), 0.0);
        assert_eq!(rank_progress(1100), 0.5);
        assert_eq!(rank_progress(1199), 0.995);
        assert_eq!(rank_progress(2399), 0.995);
    }

    #[test]
    fn rank_progress_full_at_top_tier() {
        assert_eq!(rank_progress(2400), 1.0);
        assert_eq!(rank_progress(9999), 1.0);
    }

    #[test]
    fn next_rank_names() {
        assert_eq!(next_rank_name(300), Some("Bronze"));
        assert_eq!(next_rank_name(1100), Some("Silver"));
        assert_eq!(next_rank_name(2399), Some("Pong Legend"));
        assert_eq!(next_rank_name(2400), None);
        assert_eq!(next_rank_name(9999), None);
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
            match_id: 7,
            opponent: "12D3KooExample".into(),
            addresses: vec!["/ip4/1.2.3.4/tcp/4001/p2p/test".into()],
        };
        let bytes = cbor4ii::serde::to_vec(Vec::new(), &req).expect("serialize");
        let decoded: GatewayRequest = cbor4ii::serde::from_slice(&bytes).expect("deserialize");
        assert_eq!(decoded, req);
    }

    #[test]
    fn report_result_roundtrips() {
        let req = GatewayRequest::ReportResult {
            match_id: 42,
            opponent: "12D3KooOpponent".into(),
            my_score: 5,
            opponent_score: 2,
        };
        let bytes = cbor4ii::serde::to_vec(Vec::new(), &req).expect("serialize");
        let decoded: GatewayRequest = cbor4ii::serde::from_slice(&bytes).expect("deserialize");
        assert_eq!(decoded, req);
    }

    #[test]
    fn match_revoked_roundtrips() {
        let req = GatewayRequest::MatchRevoked {
            match_id: 42,
            rating: 816,
            proof: Some(RatingProof {
                rating: 816,
                seq: 5,
                gateway: "12D3KooRevoker".into(),
                signature: vec![9, 8, 7],
            }),
        };
        let bytes = cbor4ii::serde::to_vec(Vec::new(), &req).expect("serialize");
        let decoded: GatewayRequest = cbor4ii::serde::from_slice(&bytes).expect("deserialize");
        assert_eq!(decoded, req);
    }

    #[test]
    fn match_corrected_roundtrips() {
        let req = GatewayRequest::MatchCorrected {
            match_id: 7,
            my_score: 5,
            opponent_score: 4,
            rating: 832,
            proof: None,
        };
        let bytes = cbor4ii::serde::to_vec(Vec::new(), &req).expect("serialize");
        let decoded: GatewayRequest = cbor4ii::serde::from_slice(&bytes).expect("deserialize");
        assert_eq!(decoded, req);
    }

    #[test]
    fn old_matchfound_without_match_id_defaults_to_zero() {
        // An older gateway that predates `match_id` must deserialize with 0 so
        // a new client never mistakes an unrelated match for a revocable one.
        #[derive(Serialize)]
        enum OldGatewayRequest {
            MatchFound {
                opponent: String,
                addresses: Vec<String>,
            },
        }
        let old = OldGatewayRequest::MatchFound {
            opponent: "12D3KooOld".into(),
            addresses: vec!["/p2p/old".into()],
        };
        let bytes = cbor4ii::serde::to_vec(Vec::new(), &old).expect("serialize");
        let decoded: GatewayRequest = cbor4ii::serde::from_slice(&bytes).expect("deserialize");
        assert_eq!(
            decoded,
            GatewayRequest::MatchFound {
                match_id: 0,
                opponent: "12D3KooOld".into(),
                addresses: vec!["/p2p/old".into()],
            }
        );
    }

    #[test]
    fn register_carries_client_rating() {
        let req = GatewayRequest::Register {
            username: "pongster".into(),
            rating: 1234,
            proof: None,
        };
        let bytes = cbor4ii::serde::to_vec(Vec::new(), &req).expect("serialize");
        let decoded: GatewayRequest = cbor4ii::serde::from_slice(&bytes).expect("deserialize");
        assert_eq!(decoded, req);
    }

    #[test]
    fn register_carries_rating_proof() {
        let proof = RatingProof {
            rating: 1234,
            seq: 7,
            gateway: "12D3KooProof".into(),
            signature: vec![0xde, 0xad, 0xbe, 0xef],
        };
        let req = GatewayRequest::Register {
            username: "pongster".into(),
            rating: 1234,
            proof: Some(proof.clone()),
        };
        let bytes = cbor4ii::serde::to_vec(Vec::new(), &req).expect("serialize");
        let decoded: GatewayRequest = cbor4ii::serde::from_slice(&bytes).expect("deserialize");
        assert_eq!(
            decoded,
            GatewayRequest::Register {
                username: "pongster".into(),
                rating: 1234,
                proof: Some(proof),
            }
        );
    }

    #[test]
    fn rating_proof_roundtrips() {
        let proof = RatingProof {
            rating: 1500,
            seq: 3,
            gateway: "12D3KooProof".into(),
            signature: vec![1, 2, 3, 4, 5],
        };
        let bytes = cbor4ii::serde::to_vec(Vec::new(), &proof).expect("serialize");
        let decoded: RatingProof = cbor4ii::serde::from_slice(&bytes).expect("deserialize");
        assert_eq!(decoded, proof);
    }

    #[test]
    fn register_without_rating_defaults_to_800() {
        // Simulates an older client that predates the `rating` field: it must
        // deserialize with the local default so a fresh server never loses
        // the player's stored ELO.
        #[derive(Serialize)]
        enum OldGatewayRequest {
            Register { username: String },
        }
        let old = OldGatewayRequest::Register {
            username: "vintage".into(),
        };
        let bytes = cbor4ii::serde::to_vec(Vec::new(), &old).expect("serialize");
        let decoded: GatewayRequest = cbor4ii::serde::from_slice(&bytes).expect("deserialize");
        assert_eq!(
            decoded,
            GatewayRequest::Register {
                username: "vintage".into(),
                rating: DEFAULT_RATING,
                proof: None,
            }
        );
    }
}
