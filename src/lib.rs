//! Lib target exposing the daemon internals to the binary target and to
//! integration tests.
//!
//! AriesConnect: a KDE Connect-compatible daemon without KDE/Qt dependencies.

pub mod app;
pub mod core {
    pub mod discovery;
    pub mod network;
    pub mod pairing;
}
pub mod dbus;
pub mod identity;
pub mod plugins;
pub mod tls;
pub mod trusted;
