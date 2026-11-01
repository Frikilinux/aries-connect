# AriesConnect

AriesConnect is a KDE Connect-compatible daemon written in Rust, with no KDE/Qt
dependencies. It discovers paired devices on the LAN, runs the protocol v8
control channel and exposes devices, pairing and events over D-Bus
(`dev.zotta.AriesConnect`) so any frontend can consume them.

Current status: discovery, pairing with persistent certificate pinning, the
D-Bus control interface and notification mirroring are implemented. Battery,
clipboard, ping and other plugins are not implemented yet.
Distribution packaging is not included; `contrib/` only contains an example
systemd user unit.

## Requirements

- Linux with a D-Bus session bus (any desktop).
- Stable Rust toolchain.

## Build

```sh
cargo build --release
```

## Run

```sh
./target/release/aries-connect
```

| Variable      | Default                          | Description              |
| ------------- | -------------------------------- | ------------------------ |
| `AC_PORT`     | `1716`                           | UDP and TCP port         |
| `AC_DATA_DIR` | `$XDG_DATA_HOME/aries-connect`   | State directory          |
| `RUST_LOG`    | `info`                           | `tracing` filter         |

The identity certificate (RSA-2048) and the 32-character device id are created
on first run and reused afterwards.

## Tests

```sh
cargo test
```

The suite covers the v8 handshake, pairing acceptance, the trust store and
notification mirroring.

## Documentation

- [Protocol](doc/protocol.md) — discovery, TLS control channel, trust model.
- [D-Bus interface](doc/dbus.md) — methods, signals and pairing flow.
- [Usage](doc/usage.md) — configuration, state files, troubleshooting.

## License

GPL-2.0-or-later. See [LICENSE](LICENSE).
