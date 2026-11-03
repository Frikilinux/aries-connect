//! KDE Connect plugins. Each plugin is an independent module that parses its
//! own packet types and feeds the shared state.

pub mod notifications;
pub mod ping;

/// Plugins implemented by this daemon.
///
/// Single source of truth for the `Plugins` property exposed on every device
/// object; the identity advertises the same packet types as capabilities.
pub const SUPPORTED_PLUGINS: &[&str] = &[notifications::NOTIFICATION_PACKET, ping::PING_PACKET];

/// Plugins implemented by this daemon.
pub fn supported_plugins() -> &'static [&'static str] {
    SUPPORTED_PLUGINS
}
