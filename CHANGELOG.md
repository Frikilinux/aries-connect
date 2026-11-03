# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Battery plugin (`kdeconnect.battery`): `BatteryLevel` and `IsCharging`
  properties on the device object, updated from the status packets pushed by
  the phone, announced with `PropertiesChanged` and cleared when the control
  channel closes.

### Fixed

- Detect control channels that die without a FIN (Wi-Fi off, phone out of
  range) with TCP keepalive, so `Connected` no longer stays `true` and the
  battery status is not stuck at the last reported value.

## [0.3.0] - 2026-11-02

### Added

- Per-device D-Bus objects (`dev.zotta.AriesConnect.Device`) served under
  `/dev/zotta/AriesConnect/Device/<id>` with read-only `DeviceId`, `DeviceName`,
  `DeviceType`, `Paired`, `Connected` and `Plugins` properties, announced with
  `PropertiesChanged`.
- `org.freedesktop.DBus.ObjectManager` on the service object: clients can
  enumerate devices and their properties with `GetManagedObjects` and watch
  `InterfacesAdded`.
- Ping plugin (`kdeconnect.ping`): `Ping(message)` on the device object pings a
  paired device and fails with a D-Bus error when it cannot be delivered;
  incoming pings are announced with the per-device `PingReceived` signal.
  `kdeconnect.ping` is advertised in the identity capabilities and can be
  toggled per device like any other plugin.

### Changed

- **Breaking:** per-device methods and signals moved from the service object to
  the device objects. `PingDevice`, `Unpair(deviceId)`,
  `DismissNotification(deviceId, id)`, `PluginEnabled` and
  `SetPluginEnabled(deviceId, ...)` are gone, as are the `DeviceSeen`,
  `Connected`, `Disconnected`, `PairingAccepted`, `PairingRemoved`,
  `NotificationReceived`, `NotificationDismissed`, `PingReceived` and
  `PluginStateChanged` signals. Use the device properties and its `Unpair`,
  `Ping`, `DismissNotification` and `SetPluginEnabled` methods instead.
- `Plugins` replaces `PluginEnabled` and `PluginStateChanged`;
  `PropertiesChanged` replaces the state signals; `InterfacesAdded` replaces
  `DeviceSeen`.
- `SetPluginEnabled` now fails for unpaired devices and for plugins the daemon
  does not implement.
- Binary release archives now ship the `contrib/` directory.
- CI skips documentation-only pushes: changes limited to `doc/`, `README.md`,
  `CHANGELOG.md`, `LICENSE`, `contrib/` or `.gitignore` do not start a run.

## [0.2.0] - 2026-11-01

### Added

- Notification mirroring (`kdeconnect.notification`): active notifications from
  paired devices are exposed over D-Bus with `ActiveNotifications`,
  `NotificationReceived` and `NotificationDismissed`.
- `DismissNotification` forwards dismissals to the phone through
  `kdeconnect.notification.request`.
- Unicast discovery announcements to the last known address of each peer, so
  Android devices reconnect while the screen is off and broadcasts are filtered.
- The last known IPv4 address of paired devices is persisted per device
  (`devices/<id>/device.json`) and used to seed unicast discovery after a
  restart.
- Example systemd user unit in `contrib/aries-connect.service`.
- Per-device plugin config: `PluginEnabled`, `SetPluginEnabled` and the
  `PluginStateChanged` signal; changes are persisted per device and disabling
  notifications drops their mirror.
- New D-Bus method `Unpair` and signal `PairingRemoved`: the daemon notifies the
  peer with `kdeconnect.pair: false`, deletes the device's state directory,
  clears its notifications and announces the removal. Peer-initiated unpair
  removes the same local state without echoing the packet back.
- Per-device state directories (`devices/<id>/device.json`) with atomic writes,
  replacing the monolithic trust store (migrated and removed on first start).

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

[Unreleased]: https://github.com/Frikilinux/aries-connect/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/Frikilinux/aries-connect/releases/tag/v0.3.0
[0.2.0]: https://github.com/Frikilinux/aries-connect/releases/tag/v0.2.0
[0.1.0]: https://github.com/Frikilinux/aries-connect/releases/tag/v0.1.0
