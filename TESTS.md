# Tests

Se ejecutan con:

```bash
nix develop -c cargo test
```

Unit tests inline (`#[cfg(test)]`) en 7 módulos. Total: **105 tests**
(19 en la librería + 48 en el binario del cliente + 38 en el binario del
gateway).

## `src/protocol.rs` — protocolo de red (19 tests)

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
| + 4 tests de enrutado de peticiones/scripts del protocolo | Variantes de los mensajes del game protocol entre host/guest. |

## `src/sim.rs` — simulación / condición de victoria (22 tests)

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
| (clamp en migración) | Valores fuera de rango en un snapshot se clampan (99→max, 0→1.0). |
| `constrain_paddle_clamps_between_gutters` | Las paletas quedan atrapadas entre ambas bandas (clamp ±315). |
| `paddle_bounce_flips_horizontal_velocity` | Toque de la paleta izquierda devuelve la pelota a la derecha. |
| `opponent_paddle_bounce_sends_ball_back_left` | Toque de la paleta derecha (del oponente) la devuelve a la izquierda. |
| `dead_center_hit_rebounces_toward_side_it_came` | Golpe centrado (dx=0) devuelve la pelota hacia su lado de origen. |
| `top_gutter_flips_vertical_velocity` | La banda superior invierte la velocidad vertical. |
| `bottom_gutter_flips_vertical_velocity` | La banda inferior invierte la velocidad vertical. |
| `goal_resets_ball_position_and_rally_speed` | Tras un gol la pelota vuelve al centro con velocidad normal. |
| `apply_sets_player_velocity_and_opponent_target` | `SetPlayerVelocity` / `RemotePaddle` actualizan el estado. |
| `opponent_paddle_eases_toward_target` | La paleta remota se suaviza hacia su objetivo (factor 0.8). |
| `collision_side_detection_returns_all_four_sides` | `collide_with_side` devuelve Left/Right/Top/Bottom y `None`. |
| `snapshot_and_publish_reflect_authoritative_state` | `build_snapshot` y `publish` vuelcan el estado correctamente. |
| `migration_swaps_paddles_and_scores_from_senders_frame` | Host migration espeja paletas y puntuaciones del remitente. |
| `run_sim_applies_commands_publishes_and_exits_on_disconnect` | El hilo procesa comandos, publica snapshots y sale al cerrar el canal. |

## `src/config.rs` — configuración (8 tests)

| Test | Qué verifica |
|------|--------------|
| `test_key_code_roundtrip` | Convertir `KeyCode` ↔ string es biyectivo (`ArrowUp`, `KeyW`, …). |
| `test_default_config` | Los valores por defecto de la config (teclas, `paddle_speed`, vsync). |
| `unknown_key_code_falls_back_to_arrow_up` | Códigos desconocidos caen a `ArrowUp` en ambas direcciones. |
| `load_from_missing_db_returns_defaults` | Sin base de datos se devuelven los defaults. |
| `missing_values_fall_back_to_defaults` | Base vacía → defaults (vsync en `true`). |
| `save_then_load_roundtrips_all_fields` | Persistir y recargar conserva todas las opciones. |
| `load_uses_stored_values_over_defaults` | Los valores almacenados ganan a los defaults. |
| `unparseable_stored_values_fallback_to_defaults` | Valores corruptos (`"not-a-number"`) caen al default. |

## `src/bin/gateway.rs` — servidor / matchmaking / moderación (38 tests)

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
| `two_complementary_reports_store_one_match_and_move_elo_once` | Dos reportes del mismo `match_id` mueven el ELO una sola vez. |
| `revoked_match_restores_before_ratings` | Revocar una partida restaura los ratings previos. |
| `revoking_unknown_match_fails` | Revocar un `match_id` inexistente no cambia nada. |
| `edit_match_score_recomputes_elo_and_updates_storage` | Corregir marcador recalcula ELO y persiste. |
| `legacy_reports_without_match_id_merge_canonically` | Reportes viejos sin `match_id` se fusionan canónicamente. |
| `hash_password_is_salted` | El hash PBKDF-like depende de la sal. |
| `moderator_password_roundtrip` | `hash_password` + `ct_eq` verifican la contraseña. |
| `moderator_roles_roundtrip` | `ModRole::parse`/`Display` son consistentes. |
| `role_migration_adds_default_column` | La migración agrega la columna `role`. |
| `auth_session_carries_role` | La sesión recuerda el rol (admin/user) y expira. |
| `list_matches_json_is_valid` | `GET /api/matches` devuelve JSON bien formado. |
| + tests `http_tests` / `addresses_tests` | Login/logout por cookie, 401/403 por rol, página saludable y construcción de direcciones directas/de relay. |

## `src/history.rs` — historial local (9 tests)

| Test | Qué verifica |
|------|--------------|
| `hex_roundtrips` | `encode_hex`/`decode_hex` son inversos; entradas inválidas → `None`. |
| `wins_losses_and_draws_ignore_revoked` | El saldo W/L/D ignora partidas revocadas. |
| `rating_defaults_when_unset` | Sin base, `load_rating`/`load_username`/`load_rating_proof` caen al default. |
| `open_creates_schema_and_migrates_columns` | Abrir crea el esquema y migra `gateway_match_id`/`revoked`/`rival_peer`. |
| `recording_and_reloading_builds_records_newest_first` | Los registros salen más recientes primero y el saldo es correcto. |
| `mark_match_revoked_updates_records_and_rev` | Revocar una partida la oculta del saldo y no toca ids desconocidos. |
| `correct_score_overwrites_and_recomputes_tally` | Corregir marcador recalcula el W/L. |
| `username_and_rating_persist_roundtrip` | Nombre y ELO sobreviven re-aperturas de la BD cifrada. |
| `rating_proof_roundtrips_through_settings` | Un `RatingProof` firmado viaja hex→CBOR→hex intacto. |

## `src/networking.rs` — capa libp2p (8 tests)

| Test | Qué verifica |
|------|--------------|
| `circuit_address_is_detected` | Una multiaddr con `/p2p-circuit` se detecta como relayed. |
| `direct_address_is_not_a_circuit` | Una multiaddr directa no es un circuito. |
| `base_addr_drops_trailing_peer_protocol` | `base_addr` quita el `/p2p/<peer>` final del gateway. |
| `base_addr_without_peer_is_unchanged` | Sin `/p2p` la dirección base es idéntica. |
| `base_addr_is_none_without_address` | Sin gateway dialed, `base_addr` es `None`. |
| `gateway_agent_is_detected_by_prefix` | `is_gateway_agent` reconoce al gateway por su agente. |
| `client_key_persists_across_loads` | La ed25519 de identidad persiste entre cargas. |
| `invalid_key_file_is_replaced` | Una clave corrupta se regenera y persiste (32 B). |

## `src/menu.rs` — lobby (1 test)

| Test | Qué verifica |
|------|--------------|
| (limpieza de chat) | Al salir del menú se limpia el estado del chat reabierto. |