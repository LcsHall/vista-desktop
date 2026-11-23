//! Runs the Phone-tab recovery rules' unit tests (src/phone_recovery/logic.rs,
//! the `#[cfg(test)] mod tests` at its bottom) in a test binary of their own.
//! The file is included by path and uses only std and serde_json, so this
//! binary links nothing of Tauri or WebView2 (no app manifest, no window):
//!
//!     cargo test --locked --manifest-path src-tauri/Cargo.toml --test phone_recovery_logic
//!
//! The CI test build runs exactly that before the release build.
#![allow(dead_code)]

#[path = "../src/phone_recovery/logic.rs"]
mod logic;
