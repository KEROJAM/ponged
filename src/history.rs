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
    /// Opponent's display name at the moment the match was recorded (best
    /// effort: `"Jugador"` if we never heard a `Hello`, or a short peer id on
    /// records written by old builds).
    pub rival: String,
    /// Full base58 peer id of the opponent, so the lobby can resolve their
    /// latest display name at render time (e.g. when the name arrived after
    /// the match was recorded, or for legacy rows that only kept a peer id).
    pub rival_peer: Option<String>,
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
        self.open_path(&Self::db_path());
    }

    fn open_path(&mut self, db_path: &str) {
        let Ok(conn) = Connection::open(db_path) else {
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
            let has_rival_peer = conn
                .prepare("SELECT 1 FROM pragma_table_info('matches') WHERE name = 'rival_peer'")
                .and_then(|mut stmt| stmt.query_row([], |_| Ok(())))
                .optional()
                .ok()
                .flatten()
                .is_some();
            if !has_rival_peer {
                conn.execute(
                    "ALTER TABLE matches ADD COLUMN rival_peer TEXT",
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
            "SELECT id, happened_at, rival, my_score, opp_score, was_host, gateway_match_id, revoked, rival_peer
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
                rival_peer: row.get(8).ok(),
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
    // falling back to a generic label for peers that never said hello. The
    // full peer id is stored alongside so the lobby can re-resolve the latest
    // name for a record written while the name was still unknown.
    let rival_name = names.0.get(&rival).cloned().unwrap_or_else(|| "Jugador".to_string());
    let rival_peer = rival.to_base58();

    let result = {
        let Ok(conn) = db.lock() else { return };
        conn.execute(
            "INSERT INTO matches (happened_at, rival, my_score, opp_score, was_host, gateway_match_id, rival_peer)
             VALUES (datetime('now'), ?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                rival_name,
                my_score,
                opp_score,
                was_host,
                active.0 as i64,
                rival_peer,
            ],
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db_path(name: &str) -> String {
        std::env::temp_dir()
            .join(format!("ponged-history-test-{name}.sqlite"))
            .to_string_lossy()
            .to_string()
    }

    fn open_test_history(name: &str) -> MatchHistory {
        let path = temp_db_path(name);
        let _ = std::fs::remove_file(&path);
        let mut history = MatchHistory::default();
        history.open_path(&path);
        assert!(history.db.is_some(), "history DB should open");
        history
    }

    fn insert_match(history: &MatchHistory, id: i64, my: i32, opp: i32, gw_id: i64) {
        let conn = history.db.as_ref().unwrap().lock().unwrap();
        conn.execute(
            "INSERT INTO matches (id, happened_at, rival, my_score, opp_score, was_host, gateway_match_id, rival_peer)
             VALUES (?1, datetime('now'), 'rival', ?2, ?3, 1, ?4, NULL)",
            params![id, my, opp, gw_id],
        )
        .unwrap();
    }

    #[test]
    fn hex_roundtrips() {
        let bytes = [0x00u8, 0x01, 0xab, 0xff, 0x10];
        assert_eq!(decode_hex(&encode_hex(&bytes)).unwrap(), bytes);
        assert_eq!(decode_hex("0x"), None);
        assert_eq!(decode_hex("zz"), None);
        assert_eq!(decode_hex("a"), None);
    }

    #[test]
    fn wins_losses_and_draws_ignore_revoked() {
        let mut history = MatchHistory::default();
        history.records = vec![
            MatchRecord { id: 3, happened_at: String::new(), rival: "a".into(), rival_peer: None, my_score: 5, opp_score: 2, was_host: true, gateway_match_id: 3, revoked: false },
            MatchRecord { id: 2, happened_at: String::new(), rival: "b".into(), rival_peer: None, my_score: 1, opp_score: 5, was_host: true, gateway_match_id: 2, revoked: false },
            MatchRecord { id: 1, happened_at: String::new(), rival: "c".into(), rival_peer: None, my_score: 3, opp_score: 3, was_host: true, gateway_match_id: 1, revoked: false },
            MatchRecord { id: 0, happened_at: String::new(), rival: "d".into(), rival_peer: None, my_score: 5, opp_score: 0, was_host: true, gateway_match_id: 0, revoked: true },
        ];
        assert_eq!(history.wins_losses(), (1, 1, 1));
        assert_eq!(history.revoked_count(), 1);
    }

    #[test]
    fn rating_defaults_when_unset() {
        let history = MatchHistory::default();
        assert_eq!(history.load_rating(), DEFAULT_RATING);
        assert_eq!(history.load_username(), None);
        assert_eq!(history.load_rating_proof(), None);
    }

    #[test]
    fn open_creates_schema_and_migrates_columns() {
        let history = open_test_history("migrate");
        let conn = history.db.as_ref().unwrap().lock().unwrap();
        let has_full_columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('matches')
                 WHERE name IN ('gateway_match_id', 'revoked', 'rival_peer')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_full_columns, 3);
    }

    #[test]
    fn recording_and_reloading_builds_records_newest_first() {
        let mut history = open_test_history("reload");
        insert_match(&history, 1, 5, 2, 100);
        insert_match(&history, 2, 1, 5, 101);
        history.reload();
        assert_eq!(history.records.len(), 2);
        assert_eq!(history.records[0].id, 2);
        assert_eq!(history.records[0].rival, "rival");
        assert!(history.records[0].was_host);
        assert_eq!(history.records[0].gateway_match_id, 101);
        assert_eq!(history.wins_losses(), (1, 1, 0));
    }

    #[test]
    fn mark_match_revoked_updates_records_and_rev() {
        let mut history = open_test_history("revoke");
        insert_match(&history, 1, 5, 2, 100);
        history.reload();
        assert!(!history.records[0].revoked);

        history.mark_match_revoked(100);
        assert!(history.records[0].revoked);
        assert_eq!(history.revoked_count(), 1);
        assert_eq!(history.wins_losses(), (0, 0, 0));

        let rev_before = history.rev;
        history.mark_match_revoked(999);
        assert_eq!(history.rev, rev_before, "unknown match must not bump rev");
    }

    #[test]
    fn correct_score_overwrites_and_recomputes_tally() {
        let mut history = open_test_history("correct");
        insert_match(&history, 1, 5, 0, 100);
        history.reload();
        assert_eq!(history.wins_losses(), (1, 0, 0));

        history.correct_score(100, 0, 5);
        assert_eq!(history.records[0].my_score, 0);
        assert_eq!(history.records[0].opp_score, 5);
        assert_eq!(history.wins_losses(), (0, 1, 0));
    }

    #[test]
    fn username_and_rating_persist_roundtrip() {
        let history = open_test_history("settings");
        assert_eq!(history.load_username(), None);
        history.save_username("Jahil");
        history.save_rating(1234);
        assert_eq!(history.load_username().as_deref(), Some("Jahil"));
        assert_eq!(history.load_rating(), 1234);

        let mut reloaded = MatchHistory::default();
        reloaded.open_path(&temp_db_path("settings"));
        assert_eq!(reloaded.load_username().as_deref(), Some("Jahil"));
        assert_eq!(reloaded.load_rating(), 1234);
    }

    #[test]
    fn rating_proof_roundtrips_through_settings() {
        let history = open_test_history("proof");
        let proof = RatingProof {
            rating: 1500,
            seq: 7,
            gateway: "12D3KooWxxxx".into(),
            signature: vec![1, 2, 3, 4],
        };
        history.save_rating_proof(&proof);
        let loaded = history.load_rating_proof().expect("proof roundtrip");
        assert_eq!(loaded, proof);

        let mut reloaded = MatchHistory::default();
        reloaded.open_path(&temp_db_path("proof"));
        assert_eq!(reloaded.load_rating_proof(), Some(proof));
    }
}
