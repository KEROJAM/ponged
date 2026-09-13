//! Local match history / ranking (M9).
//!
//! Purely client-side persistence backed by SQLite (rusqlite). The result of
//! every finished match is written when we leave the `Playing` state, and the
//! lobby (M5) reads it back to show a win/loss record and recent games.

use std::sync::Mutex;

use bevy::prelude::*;
use rusqlite::{params, Connection};

use crate::menu::Opponent;
use crate::networking::{short_peer, GatewayState, NetChannels, NetCommand};
use crate::networking_demo::{IsHost, RemoteWorld};
use crate::Score;
use ponged::protocol::GatewayRequest;

/// How many past matches the ranking panel shows.
const HISTORY_LIMIT: usize = 10;

/// One stored match.
#[derive(Debug, Clone)]
pub struct MatchRecord {
    #[allow(dead_code)]
    pub id: i64,
    /// Local timestamp of when the match finished.
    #[allow(dead_code)]
    pub happened_at: String,
    pub rival: String,
    pub my_score: i32,
    pub opp_score: i32,
    pub was_host: bool,
}

/// Connection + cached ranking. Wrapped in a `Mutex` because `rusqlite`'s
/// `Connection` is not `Sync`.
#[derive(Resource)]
pub struct MatchHistory {
    db: Option<Mutex<Connection>>,
    /// Most recent matches, newest first.
    pub records: Vec<MatchRecord>,
    /// Bumped on every write; the lobby rebuilds its panel on change.
    pub rev: u32,
    /// True between entering `Playing` and recording the result.
    armed: bool,
}

impl Default for MatchHistory {
    fn default() -> Self {
        MatchHistory {
            db: None,
            records: Vec::new(),
            rev: 0,
            armed: false,
        }
    }
}

impl MatchHistory {
    fn ensure_open(&mut self) {
        if self.db.is_some() {
            return;
        }
        let Ok(conn) = Connection::open("pong_history.sqlite") else {
            warn!("Could not open local match history database");
            return;
        };
        if conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS matches (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT,
                    happened_at TEXT    NOT NULL,
                    rival       TEXT    NOT NULL,
                    my_score    INTEGER NOT NULL,
                    opp_score   INTEGER NOT NULL,
                    was_host    INTEGER NOT NULL
                );",
            )
            .is_ok()
        {
            self.db = Some(Mutex::new(conn));
            self.reload();
        }
    }

    fn reload(&mut self) {
        self.records.clear();
        let Some(db) = &self.db else { return };
        let Ok(conn) = db.lock() else { return };
        let mut stmt = match conn.prepare(
            "SELECT id, happened_at, rival, my_score, opp_score, was_host
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
            Ok(MatchRecord {
                id: row.get(0)?,
                happened_at: row.get(1)?,
                rival: row.get(2)?,
                my_score: row.get(3)?,
                opp_score: row.get(4)?,
                was_host: was_host != 0,
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

    /// Wins, losses (and draws) from the cached record.
    pub fn wins_losses(&self) -> (u32, u32, u32) {
        self.records.iter().fold((0, 0, 0), |(w, l, d), rec| {
            if rec.my_score > rec.opp_score {
                (w + 1, l, d)
            } else if rec.my_score < rec.opp_score {
                (w, l + 1, d)
            } else {
                (w, l, d + 1)
            }
        })
    }
}

/// Arms recording: the next exit from `Playing` saves the match.
pub fn arm_record(mut history: ResMut<MatchHistory>) {
    history.ensure_open();
    history.armed = true;
}

/// Records the finished match on exit from `Playing` (M9) and reports the
/// result to the gateway for ELO updates.
pub fn record_match(
    mut history: ResMut<MatchHistory>,
    host: Res<IsHost>,
    score: Res<Score>,
    world: Res<RemoteWorld>,
    opponent: Res<Opponent>,
    gateway: Res<GatewayState>,
    channels: Res<NetChannels>,
) {
    if !history.armed {
        return;
    }
    history.armed = false;
    history.ensure_open();
    let Some(db) = &history.db else { return };
    let Some(rival) = opponent.0 else { return };

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

    let result = {
        let Ok(conn) = db.lock() else { return };
        conn.execute(
            "INSERT INTO matches (happened_at, rival, my_score, opp_score, was_host)
             VALUES (datetime('now'), ?1, ?2, ?3, ?4)",
            params![short_peer(rival), my_score, opp_score, was_host],
        )
    };
    if result.is_ok() {
        history.rev += 1;
        history.reload();
    }

    // Report the result to the gateway for ELO rating updates (M6).
    if let Some(gw_peer) = gateway.peer {
        let _ = channels.commands.send(NetCommand::SendGatewayRequest {
            peer: gw_peer,
            request: GatewayRequest::ReportResult {
                opponent: rival.to_base58(),
                my_score: my_score as u32,
                opponent_score: opp_score as u32,
            },
        });
        info!("Reported match result to gateway: {my_score}-{opp_score} vs {rival}");
    }
}

/// (Re)loads the history once at lobby entry so the ranking panel is fresh.
pub fn refresh_on_menu(mut history: ResMut<MatchHistory>) {
    history.ensure_open();
    history.reload();
}