//! `bread.lock.*` event integration — optional, non-blocking. See
//! `EVENTS.md` at the repo root for the full contract. breadlock works
//! identically with or without breadd running; every call here is
//! fire-and-forget (`BreadClient::emit` never blocks or errors this
//! process) so a missing or restarting breadd never affects locking
//! itself.

use bread_utils::bread_client::BreadClient;

/// This app's id in bread's sibling-app namespace registry
/// (`bread_shared::apps::KNOWN_APPS`) — events publish as `bread.lock.*`.
pub const APP_ID: &str = "lock";

pub fn emit_locked() {
    BreadClient::connect(APP_ID).emit("bread.lock.locked", serde_json::json!({}));
}

pub fn emit_unlocked() {
    BreadClient::connect(APP_ID).emit("bread.lock.unlocked", serde_json::json!({}));
}
