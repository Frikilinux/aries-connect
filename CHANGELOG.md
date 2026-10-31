# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
