//! slipstream-core — headless CAN log analysis engine.
//!
//! # Architecture (the migration-cheap boundary)
//!
//! This crate is **UI-agnostic**. It NEVER depends on egui (or Tauri). Its public
//! surface is a set of *view-driven queries* over [`Session`]: the GUI asks for
//! exactly what fits on screen (a decimated series, a window of rows, some stats),
//! and never pulls the whole multi-GB dataset across any boundary.
//!
//! Today `gui-egui` calls these methods directly, in-process. Tomorrow a
//! `gui-tauri` crate wraps the same methods as `#[tauri::command]` — the request /
//! response types below are already `serde`-(de)serializable, so they double as
//! the IPC contract. That is the whole point of keeping core headless.
//!
//! # The three rules
//! 1. No GUI types in here (no `egui::Color32`, no widget handles).
//! 2. The public API is RPC-shaped: take a serde request, return a serde value.
//! 3. Loaded session/index/cache state lives in [`Session`], not in widgets.

pub mod dbc;
pub mod error;
pub mod health;
pub mod ingest;
pub mod model;
pub mod query;
pub mod store;

pub use error::{Error, Result};
pub use query::Session;
