//! Configuration persistence (key bindings, sensitivity, window settings).
//!
//! Stored in the platform's application data directory alongside the
//! encrypted match history database. Settings survive working directory
//! changes and are loaded at startup.

use bevy::prelude::*;
use dirs::data_local_dir;
use rusqlite::{Connection, params};

/// Path to the config database in the application data directory.
fn config_path() -> String {
    let dir = data_local_dir()
        .map(|p| p.join("ponged"))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    std::fs::create_dir_all(&dir).unwrap_or_default();
    dir.join("config.sqlite")
        .to_string_lossy()
        .to_string()
}

/// Player configuration loaded from the local database.
#[derive(Resource, Clone)]
pub struct Config {
    /// Key binding for moving the paddle up.
    pub key_up: KeyCode,
    /// Key binding for moving the paddle down.
    pub key_down: KeyCode,
    /// Paddle movement speed.
    pub paddle_speed: f32,
    /// Window scale factor.
    pub window_scale: f32,
    /// Whether vsync is enabled.
    pub vsync: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            key_up: KeyCode::ArrowUp,
            key_down: KeyCode::ArrowDown,
            paddle_speed: 5.0,
            window_scale: 1.0,
            vsync: true,
        }
    }
}

impl Config {
    /// Load the configuration from the database, falling back to defaults.
    pub fn load() -> Self {
        let conn = match Connection::open(config_path()) {
            Ok(c) => c,
            Err(_) => return Config::default(),
        };
        if conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS config (
                    key        TEXT PRIMARY KEY,
                    value      TEXT NOT NULL
                );",
            )
            .is_err()
        {
            return Config::default();
        }

        let key_up = Self::get_val(&conn, "key_up").unwrap_or_else(|| "ArrowUp".to_string());
        let key_down = Self::get_val(&conn, "key_down").unwrap_or_else(|| "ArrowDown".to_string());
        let paddle_speed: f32 = Self::get_val(&conn, "paddle_speed")
            .and_then(|v| v.parse().ok())
            .unwrap_or(5.0);
        let window_scale: f32 = Self::get_val(&conn, "window_scale")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1.0);
        let vsync: bool = Self::get_val(&conn, "vsync")
            .map(|v| v == "true")
            .unwrap_or(true);

        Config {
            key_up: key_code_from_str(&key_up),
            key_down: key_code_from_str(&key_down),
            paddle_speed,
            window_scale,
            vsync,
        }
    }

    /// Persist the current configuration to the database.
    pub fn save(&self) {
        let conn = match Connection::open(config_path()) {
            Ok(c) => c,
            Err(_) => return,
        };
        let _ = conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS config (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        );
        let _ = self.set_val(&conn, "key_up", &key_code_to_str(self.key_up));
        let _ = self.set_val(&conn, "key_down", &key_code_to_str(self.key_down));
        let _ = self.set_val(&conn, "paddle_speed", &self.paddle_speed.to_string());
        let _ = self.set_val(&conn, "window_scale", &self.window_scale.to_string());
        let _ = self.set_val(&conn, "vsync", &self.vsync.to_string());
    }

    fn get_val(conn: &Connection, key: &str) -> Option<String> {
        conn.query_row(
            "SELECT value FROM config WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .ok()
    }

    fn set_val(&self, conn: &Connection, key: &str, value: &str) {
        let _ = conn.execute(
            "INSERT INTO config (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        );
    }
}

fn key_code_from_str(s: &str) -> KeyCode {
    match s {
        "ArrowUp" => KeyCode::ArrowUp,
        "ArrowDown" => KeyCode::ArrowDown,
        "KeyW" => KeyCode::KeyW,
        "KeyS" => KeyCode::KeyS,
        _ => KeyCode::ArrowUp,
    }
}

fn key_code_to_str(kc: KeyCode) -> String {
    match kc {
        KeyCode::ArrowUp => "ArrowUp".to_string(),
        KeyCode::ArrowDown => "ArrowDown".to_string(),
        KeyCode::KeyW => "KeyW".to_string(),
        KeyCode::KeyS => "KeyS".to_string(),
        _ => "ArrowUp".to_string(),
    }
}

/// System that loads config at startup.
pub fn load_config(mut commands: Commands) {
    let config = Config::load();
    commands.insert_resource(config);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_code_roundtrip() {
        assert_eq!(key_code_to_str(KeyCode::ArrowUp), "ArrowUp");
        assert_eq!(key_code_to_str(KeyCode::KeyW), "KeyW");
        assert_eq!(key_code_from_str("ArrowUp"), KeyCode::ArrowUp);
        assert_eq!(key_code_from_str("KeyW"), KeyCode::KeyW);
    }

    #[test]
    fn test_default_config() {
        let cfg = Config::default();
        assert_eq!(cfg.key_up, KeyCode::ArrowUp);
        assert_eq!(cfg.key_down, KeyCode::ArrowDown);
        assert_eq!(cfg.paddle_speed, 5.0);
        assert_eq!(cfg.vsync, true);
    }
}