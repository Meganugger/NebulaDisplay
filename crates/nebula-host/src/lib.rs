//! NebulaDisplay host service library.
//!
//! Exposed as a library so integration tests can spin up a real server
//! in-process, and so a future Windows service wrapper / tray app can embed
//! it.

pub mod adaptive;
pub mod audio;
pub mod capture;
pub mod config;
pub mod discovery;
pub mod encode;
pub mod input;
pub mod pairing;
pub mod pipeline;
pub mod server;
pub mod state;
#[cfg(feature = "tls")]
pub mod tls;
