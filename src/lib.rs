//! Shared code for the Pong P2P project: the game client (`ponged-cliente`)
//! and the gateway server (`ponged-gateway`).
//!
//! Only protocol types live here; everything game-specific stays inside the
//! `main` binary so the gateway never pulls in Bevy.

pub mod protocol;
