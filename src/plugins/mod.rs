//! KDE Connect plugins. Each plugin is an independent module that parses its
//! own packet types and feeds the shared state.

pub mod battery;
pub mod clipboard;
pub mod connectivity_report;
pub mod find_my_phone;
pub mod notifications;
pub mod ping;

/// Plugins implemented by this daemon.
///
/// Single source of truth for the `Plugins` property exposed on every device
/// object; the identity advertises the same packet types as capabilities.
pub const SUPPORTED_PLUGINS: &[&str] = &[
    battery::BATTERY_PACKET,
    clipboard::CLIPBOARD_PACKET,
    connectivity_report::CONNECTIVITY_REPORT_PACKET,
    find_my_phone::FIND_MY_PHONE_PACKET,
    notifications::NOTIFICATION_PACKET,
    ping::PING_PACKET,
];

/// Plugins implemented by this daemon.
pub fn supported_plugins() -> &'static [&'static str] {
    SUPPORTED_PLUGINS
}
