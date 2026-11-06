# AriesConnect

AriesConnect is a KDE Connect-compatible daemon written in Rust, with no KDE/Qt
dependencies. It discovers paired devices on the LAN, runs the protocol v8
control channel and exposes devices, pairing and events over D-Bus
(`dev.zotta.AriesConnect`) so any frontend can consume them.

## Current status

### Core

- [x] Discovery (UDP 1716)
- [x] Control channel (TCP 1716, TLS, protocol v8)
- [x] Pairing and unpairing with certificate pinning
- [x] D-Bus API (`dev.zotta.AriesConnect`, per-device objects)
- [x] Per-device plugin configuration (`Plugins`, `SetPluginEnabled`)
- [x] Configurable device name (`config.json`, `DeviceName`, `SetDeviceName`)

### Plugins

- [x] Notifications (`kdeconnect.notification`)
- [x] Ping (`kdeconnect.ping`)
- [x] Battery (`kdeconnect.battery`)
- [x] Clipboard (`kdeconnect.clipboard`)
- [x] Connectivity report (`kdeconnect.connectivity_report`)
- [x] Find my phone (`kdeconnect.findmyphone.request`)
- [ ] Share (`kdeconnect.share.request`)

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

Other devices see the host name by default. To announce a different name:

```sh
busctl --user call dev.zotta.AriesConnect /dev/zotta/AriesConnect \
  dev.zotta.AriesConnect SetDeviceName s "My Desktop"
```

The value is stored in `$AC_DATA_DIR/config.json`; an empty name resets it to
the host name. Connected phones receive the new identity immediately.

## Systemd user unit

The folder `contrib/` contains an example systemd user unit.

## Tests

```sh
cargo test
```

The suite covers the v8 handshake, pairing acceptance, unpairing, the trust
store, notification mirroring and the ping plugin.

## Documentation

- [Protocol](doc/protocol.md) — discovery, TLS control channel, trust model.
- [D-Bus interface](doc/dbus.md) — methods, signals and pairing flow.
- [Usage](doc/usage.md) — configuration, state files, troubleshooting.

## License

GPL-2.0-or-later. See [LICENSE](LICENSE).
