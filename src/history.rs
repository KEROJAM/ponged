//! Local match history / ranking (M9).
//!
//! Purely client-side persistence backed by encrypted SQLite
//! (SQLCipher via rusqlite). The result of every finished match is
//! written when we leave the `Playing` state, and the lobby (M5) reads
//! it back to show a win/loss record and recent games.
//! The database is stored in the platform's application data directory
//! (~/.local/share/ponged/ on Linux, %APPDATA%/ponged/ on Windows) so it
//! persists regardless of the working directory and is encrypted at rest.

use std::sync::Mutex;

use bevy::prelude::*;
use dirs::data_local_dir;
use rusqlite::{Connection, OptionalExtension, params};

/// Encryption key for the SQLite database. Stored here to keep the
/// database encrypted at rest; this is the only place it is stored.
const DB_ENCRYPTION_KEY: &str = "ponged-v1-encrypted-history";

use crate::Score;
use crate::menu::{ActiveMatch, Opponent, PeerNames};
use crate::networking::{GatewayState, NetChannels, NetCommand};
use crate::networking_demo::{IsHost, RemoteWorld};
use ponged::protocol::{GatewayRequest, RatingProof, DEFAULT_RATING};

/// How many past matches the ranking panel shows.
pub const HISTORY_LIMIT: usize = 10;

/// One stored match.
#[derive(Debug, Clone)]
pub struct MatchRecord {
    #[allow(dead_code)]
    pub id: i64,
    /// Local timestamp of when the match finished.
    #[allow(dead_code)]
    pub happened_at: String,
    #[allow(dead_code)]
    pub rival: String,
    pub my_score: i32,
    pub opp_score: i32,
    #[allow(dead_code)]
    pub was_host: bool,
    /// Gateway-assigned id of this match (0 for LAN/local matches). Used to
    /// apply a moderation revocation to the right record.
    #[allow(dead_code)]
    pub gateway_match_id: i64,
    /// True once a moderator revoked this match: it no longer counts toward
    /// the win/loss record (and the ELO it earned was rolled back).
    pub revoked: bool,
}

/// Connection + cached ranking. Wrapped in a `Mutex` because `rusqlite`'s
/// `Connection` is not `Sync`.
#[derive(Resource)]
#[derive(Default)]
pub struct MatchHistory {
    db: Option<Mutex<Connection>>,
    /// Most recent matches, newest first.
    pub records: Vec<MatchRecord>,
    /// Bumped on every write; the lobby rebuilds its panel on change.
    pub rev: u32,
    /// True between entering `Playing` and recording the result.
    armed: bool,
}


impl MatchHistory {
    fn db_path() -> String {
        let dir = data_local_dir()
            .map(|p| p.join("ponged"))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        std::fs::create_dir_all(&dir).unwrap_or_default();
        dir.join("pong_history.sqlite")
            .to_string_lossy()
            .to_string()
    }

    pub fn ensure_open(&mut self) {
        if self.db.is_some() {
            return;
        }
        let db_path = Self::db_path();
        let Ok(conn) = Connection::open(&db_path) else {
            warn!("Could not open local match history database");
            return;
        };
        if conn
            .execute_batch(&format!("PRAGMA key = '{}';", DB_ENCRYPTION_KEY))
            .is_err()
        {
            warn!("Could not set database encryption key");
            return;
        }
        if conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS matches (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT,
                    happened_at TEXT    NOT NULL,
                    rival       TEXT    NOT NULL,
                    my_score    INTEGER NOT NULL,
                    opp_score   INTEGER NOT NULL,
                    was_host    INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS settings (
                    key   TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );",
            )
            .is_ok()
        {
            // Migrate databases created before the revocation fields existed.
            let has_match_id = conn
                .prepare("SELECT 1 FROM pragma_table_info('matches') WHERE name = 'gateway_match_id'")
                .and_then(|mut stmt| stmt.query_row([], |_| Ok(())))
                .optional()
                .ok()
                .flatten()
                .is_some();
            if !has_match_id {
                conn.execute(
                    "ALTER TABLE matches ADD COLUMN gateway_match_id INTEGER NOT NULL DEFAULT 0",
                    [],
                )
                .ok();
            }
            let has_revoked = conn
                .prepare("SELECT 1 FROM pragma_table_info('matches') WHERE name = 'revoked'")
                .and_then(|mut stmt| stmt.query_row([], |_| Ok(())))
                .optional()
                .ok()
                .flatten()
                .is_some();
            if !has_revoked {
                conn.execute(
                    "ALTER TABLE matches ADD COLUMN revoked INTEGER NOT NULL DEFAULT 0",
                    [],
                )
                .ok();
            }
            self.db = Some(Mutex::new(conn));
            self.reload();
        }
    }

    fn reload(&mut self) {
        self.records.clear();
        let Some(db) = &self.db else { return };
        let Ok(conn) = db.lock() else { return };
        let mut stmt = match conn.prepare(
            "SELECT id, happened_at, rival, my_score, opp_score, was_host, gateway_match_id, revoked
             FROM matches ORDER BY id DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(e) => {
                warn!("Could not read match history: {e}");
                return;
            }
        };
        let rows = stmt.query_map(params![HISTORY_LIMIT as i64], |row| {
            let was_host: i64 = row.get(5)?;
            let revoked: i64 = row.get(7)?;
            Ok(MatchRecord {
                id: row.get(0)?,
                happened_at: row.get(1)?,
                rival: row.get(2)?,
                my_score: row.get(3)?,
                opp_score: row.get(4)?,
                was_host: was_host != 0,
                gateway_match_id: row.get(6)?,
                revoked: revoked != 0,
            })
        });
        self.records = match rows {
            Ok(rows) => rows.filter_map(Result::ok).collect(),
            Err(e) => {
                warn!("Could not read match history: {e}");
                Vec::new()
            }
        };
    }

    /// Wins, losses (and draws) from the cached record. Revoked matches (a
    /// moderator decided they weren't played fairly) never count.
    pub fn wins_losses(&self) -> (u32, u32, u32) {
        self.records
            .iter()
            .filter(|rec| !rec.revoked)
            .fold((0, 0, 0), |(w, l, d), rec| {
                if rec.my_score > rec.opp_score {
                    (w + 1, l, d)
                } else if rec.my_score < rec.opp_score {
                    (w, l + 1, d)
                } else {
                    (w, l, d + 1)
                }
            })
    }

    /// Number of revoked matches shown in the record panel.
    pub fn revoked_count(&self) -> u32 {
        self.records.iter().filter(|r| r.revoked).count() as u32
    }

    /// Marks every local record of gateway match `gateway_match_id` as revoked
    /// (called when the moderator pushes `MatchRevoked`).
    pub fn mark_match_revoked(&mut self, gateway_match_id: u64) {
        let changed = {
            let Some(db) = &self.db else { return };
            let Ok(conn) = db.lock() else { return };
            conn.execute(
                "UPDATE matches SET revoked = 1 WHERE gateway_match_id = ?1",
                params![gateway_match_id as i64],
            )
            .unwrap_or(0)
        };
        if changed > 0 {
            self.rev += 1;
            self.reload();
        }
    }

    /// Overwrites the final score of every local record of gateway match
    /// `gateway_match_id` (called when the moderator/cached rating corrected
    /// it), so the win/loss tally reflects the honest result.
    pub fn correct_score(&mut self, gateway_match_id: u64, my_score: u32, opp_score: u32) {
        let changed = {
            let Some(db) = &self.db else { return };
            let Ok(conn) = db.lock() else { return };
            conn.execute(
                "UPDATE matches SET my_score = ?2, opp_score = ?3 WHERE gateway_match_id = ?1",
                params![gateway_match_id as i64, my_score, opp_score],
            )
            .unwrap_or(0)
        };
        if changed > 0 {
            self.rev += 1;
            self.reload();
        }
    }

    /// Reads the stored display name, if any.
    pub fn load_username(&self) -> Option<String> {
        self.settings_value("username")
    }

    /// Persists the display name.
    pub fn save_username(&self, name: &str) {
        if let Err(e) = self.write_setting("username", name) {
            warn!("Could not save username: {e}");
        }
    }

    /// Reads the locally-kept ELO rating. The client is the source of truth
    /// for ELO: it never depends on the gateway's SQLite for continuity.
    pub fn load_rating(&self) -> i32 {
        let stored = self.settings_value("rating");
        stored
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(DEFAULT_RATING)
    }

    /// Persists the ELO rating (updated after each reported match).
    pub fn save_rating(&self, rating: i32) {
        if let Err(e) = self.write_setting("rating", &rating.to_string()) {
            warn!("Could not save rating: {e}");
        }
    }

    /// Reads the newest gateway-signed rating proof kept locally, if any.
    /// Stored hex-encoded, since the settings table only holds text.
    pub fn load_rating_proof(&self) -> Option<RatingProof> {
        let stored = self.settings_value("rating_proof")?;
        let bytes = decode_hex(&stored)?;
        cbor4ii::serde::from_slice(&bytes).ok()
    }

    /// Persists a gateway-signed rating proof (hex-encoded CBOR).
    pub fn save_rating_proof(&self, proof: &RatingProof) {
        let Ok(bytes) = cbor4ii::serde::to_vec(Vec::new(), proof) else {
            return;
        };
        if let Err(e) = self.write_setting("rating_proof", &encode_hex(&bytes)) {
            warn!("Could not save rating proof: {e}");
        }
    }

    fn settings_value(&self, key: &str) -> Option<String> {
        let db = self.db.as_ref()?;
        let conn = db.lock().ok()?;
        let mut stmt = conn
            .prepare("SELECT value FROM settings WHERE key = ?1")
            .ok()?;
        let mut rows = stmt.query_map([key], |row| row.get(0)).ok()?;
        rows.next().and_then(Result::ok)
    }

    fn write_setting(&self, key: &str, value: &str) -> Result<(), String> {
        let Some(db) = &self.db else {
            return Ok(());
        };
        let Ok(conn) = db.lock() else {
            return Err("settings lock poisoned".to_string());
        };
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
    }
}

/// Arms recording: the next exit from `Playing` saves the match.
pub fn arm_record(mut history: ResMut<MatchHistory>) {
    history.ensure_open();
    history.armed = true;
}

/// Records the finished match on exit from `Playing` (M9) and reports the
/// result to the gateway for ELO updates. Only matches that actually reached
/// the win threshold are recorded/reported: an abandoned match (opponent
/// disconnected, ESC before `WIN_SCORE`, `MatchAbort`) has no verifiable
/// outcome and must not move anyone's rating.
#[allow(clippy::too_many_arguments)]
pub fn record_match(
    mut history: ResMut<MatchHistory>,
    host: Res<IsHost>,
    score: Res<Score>,
    world: Res<RemoteWorld>,
    opponent: Res<Opponent>,
    names: Res<PeerNames>,
    gateway: Res<GatewayState>,
    active: Res<ActiveMatch>,
    match_over: Res<crate::sim::MatchOver>,
    channels: Res<NetChannels>,
) {
    if !history.armed {
        return;
    }
    history.armed = false;
    history.ensure_open();
    let Some(db) = &history.db else { return };
    let Some(rival) = opponent.0 else { return };

    if !match_over.0 {
        info!("Match ended without a winner; result discarded");
        return;
    }

    // The guest's Score resource stays 0/0 (it renders the host's state), so
    // its result is taken from the last authoritative snapshot. Scores there
    // are seen from the host's perspective: host's "player" is our opponent.
    let (my_score, opp_score, was_host) = if host.0 {
        (score.player as i64, score.opponent as i64, true)
    } else {
        match world.curr {
            Some(s) => (s.opponent_score as i64, s.player_score as i64, false),
            None => return,
        }
    };

    // Store the opponent's display name when we learned it (via `Hello`),
    // falling back to a generic label for peers that never said hello.
    let rival_name = names.0.get(&rival).cloned().unwrap_or_else(|| "Jugador".to_string());

    let result = {
        let Ok(conn) = db.lock() else { return };
        conn.execute(
            "INSERT INTO matches (happened_at, rival, my_score, opp_score, was_host, gateway_match_id)
             VALUES (datetime('now'), ?1, ?2, ?3, ?4, ?5)",
            params![rival_name, my_score, opp_score, was_host, active.0 as i64],
        )
    };
    if result.is_ok() {
        history.rev += 1;
        history.reload();
    }

    // Report the result to the gateway for ELO rating updates (M6). The
    // gateway-assigned `match_id` lets the gateway merge the two reports and,
    // if a moderator revokes the match, lets the client drop it too.
    if let Some(gw_peer) = gateway.peer {
        let _ = channels.commands.send(NetCommand::SendGatewayRequest {
            peer: gw_peer,
            request: GatewayRequest::ReportResult {
                match_id: active.0,
                opponent: rival.to_base58(),
                my_score: my_score as u32,
                opponent_score: opp_score as u32,
            },
        });
        info!("Reported match result (#{}) to gateway: {my_score}-{opp_score} vs {rival}", active.0);
    }
}

/// (Re)loads the history once at lobby entry so the ranking panel is fresh.
pub fn refresh_on_menu(mut history: ResMut<MatchHistory>) {
    history.ensure_open();
    history.reload();
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    let bytes: Vec<u8> = hex
        .as_bytes()
        .chunks(2)
        .map(|pair| std::str::from_utf8(pair).ok().and_then(|s| u8::from_str_radix(s, 16).ok()))
        .collect::<Option<_>>()?;
    (bytes.len() == hex.len() / 2).then_some(bytes)
}
