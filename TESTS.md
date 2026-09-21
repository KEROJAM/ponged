# Tests

Se ejecutan con:

```bash
nix develop -c cargo test
```

Unit tests inline (`#[cfg(test)]`) en 4 módulos. Total: **36 tests**.

## `src/config.rs` — configuración (2 tests)

| Test | Qué verifica |
|------|--------------|
| `test_key_code_roundtrip` | Convertir `KeyCode` ↔ string es biyectivo (`ArrowUp`, `KeyW`, …). |
| `test_default_config` | Los valores por defecto de la config (teclas, `paddle_speed`, vsync). |

## `src/sim.rs` — simulación / condición de victoria (9 tests)

| Test | Qué verifica |
|------|--------------|
| `win_at_five_points_for_host` | Llegar a 5 goles (host) termina el partido con 5-0. |
| `win_at_five_points_for_guest` | Ídem para el guest. |
| `no_win_below_five_points` | Con menos de 5 goles el partido no termina. |
| `match_over_freezes_ball_and_paddles` | Al terminar, la pelota y las paletas se congelan. |
| `goal_then_win_stops_playing` | El gol que define el partido detiene la simulación por completo. |
| `ball_speeds_up_after_paddle_hit` | Cada toque de paleta acelera la pelota según `RALLY_ACCELERATION`. |
| `rally_speed_caps_at_max` | La aceleración no supera `MAX_BALL_SPEED_MULT`. |
| `serve_resets_rally_speed` | Un gol reinicia la velocidad del rally a la normal. |
| `migration_preserves_ball_speed` | Al migrar host/estado se conserva la velocidad del rally. |
| — (clamp en migración) | Valores fuera de rango en un snapshot se clampan (99→max, 0→1.0). |

## `src/protocol.rs` — protocolo de red (15 tests)

| Test | Qué verifica |
|------|--------------|
| `rank_below_1000_is_brick` | Rating < 1000 (incl. 0) es `Brick`. |
| `rank_at_boundaries` | Tabla de rangos en cada frontera (1000→Bronze … 2400→Pong Master). |
| `rank_top_tier` | Por encima de 2400 (hasta `i32::MAX`) es `Pong Legend`. |
| `negative_ratings_are_brick` | Ratings negativos son `Brick`. |
| `rank_progress_inside_tier` | Progreso dentro del rango (0.0→1.0 según el ELO). |
| `rank_progress_full_at_top_tier` | En el último rango el progreso siempre es 1.0. |
| `next_rank_names` | Nombre del siguiente rango (`None` en el tope). |
| `snapshot_roundtrips_through_cbor` | `GameSnapshot` sobrevive un roundtrip CBOR bit a bit. |
| `old_snapshot_without_match_over_deserializes` | Retrocompat: snapshots viejos (sin `match_over`/`ball_speed_mult`) deserializan con defaults. |
| `hello_carries_name` | `Request::Hello` hace roundtrip CBOR. |
| `gateway_request_roundtrips` | `MatchFound` (oponente + direcciones relay) hace roundtrip. |
| `register_carries_client_rating` | `Register` con rating hace roundtrip. |
| `register_carries_rating_proof` | `Register` con `RatingProof` hace roundtrip. |
| `rating_proof_roundtrips` | `RatingProof` hace roundtrip CBOR. |
| `register_without_rating_defaults_to_800` | Retrocompat: clientes viejos sin campo `rating` registran con el default (800). |

## `src/bin/gateway.rs` — servidor / matchmaking (10 tests)

| Test | Qué verifica |
|------|--------------|
| `equal_ratings_win_moves_expected_amount` | Emparejados iguales: ganador +16, perdedor −16 (K=32). |
| `equal_ratings_draw_keeps_both` | Empate no mueve ningún rating. |
| `huge_upset_moves_more` | Sorprendente (400 vs 2400) mueve el máximo (~32). |
| `favorite_win_moves_little` | El favorito ganando apenas mueve (~0). |
| `ratings_swap_symmetrically` | La función ELO es simétrica (A pierde/B gana). |
| `new_players_start_as_brick` | Rating inicial cae en `Brick`. |
| `proof_roundtrips_sign_and_verify` | Firmar `RatingProof` ed25519 y verificarlo. |
| `tampered_rating_is_rejected` | Un rating alterado en un proof firmado es rechazado. |
| `proof_bound_to_other_player_is_rejected` | Un proof firmado para otro jugador es rechazado. |
| `replayed_old_seq_is_beat_by_cached_higher` | Anti-replay: un proof viejo (seq menor) no sobreescribe el rating cacheado. |