//! The `yoink` CLI: pull a flake's cached dependencies (or given store
//! paths) into a local binary cache through a yoink service, and monitor the
//! pull.

pub mod auth;
pub mod config;
pub mod format;
pub mod plain;
pub mod pull;
pub mod state;
pub mod stream;
pub mod targets;
pub mod tui;
