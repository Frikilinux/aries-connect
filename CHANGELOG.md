# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Notification mirroring (`kdeconnect.notification`): active notifications from
  paired devices are exposed over D-Bus with `ActiveNotifications`,
  `NotificationReceived` and `NotificationDismissed`.
- `DismissNotification` forwards dismissals to the phone through
  `kdeconnect.notification.request`.
- Unicast discovery announcements to the last known address of each peer, so
  Android devices reconnect while the screen is off and broadcasts are filtered.
- The last known IPv4 address of paired devices is persisted in
  `trusted_devices.json` and used to seed unicast discovery after a restart.
- Example systemd user unit in `contrib/aries-connect.service`.

### Changed

- Link closures are logged at info level with the device id.

## [0.1.0] - 2026-10-31

### Added

- Device discovery over UDP 1716: limited broadcast, per-interface directed
  broadcast and unicast replies to received identities.
- KDE Connect protocol v8 control channel over TCP 1716 with TLS.
- Pairing (`kdeconnect.pair`) with accept/reject and a 30 s timeout.
- Persistent trust store with SHA-256 certificate pinning.
- D-Bus service `dev.zotta.AriesConnect` with `Devices`, `AcceptPairing` and
  `RejectPairing` methods plus device and pairing signals.
- Persisted RSA-2048 identity certificate and 32-character device id.

[Unreleased]: https://github.com/Frikilinux/aries-connect/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Frikilinux/aries-connect/releases/tag/v0.1.0
