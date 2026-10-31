# AriesConnect

AriesConnect is a KDE Connect-compatible daemon written in Rust, with no KDE/Qt
dependencies. It discovers paired devices on the LAN, runs the protocol v8
control channel and exposes devices, pairing and events over D-Bus
(`dev.zotta.AriesConnect`) so any frontend can consume them.

Current status: discovery, pairing with persistent certificate pinning and the
D-Bus control interface are implemented. Battery, notifications and other
plugins are not part of this release yet, and distribution packaging is not
included in the repository.

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

The suite covers the v8 handshake, pairing acceptance and the trust store.

## Releases

Releases are built by `.github/workflows/release.yml` when a `v*` tag is pushed.
The tag must match the `version` in `Cargo.toml`, otherwise the workflow fails.

Tag signing is done locally after configuring SSH signing:

```sh
git config gpg.format ssh
git config user.signingkey ~/.ssh/id_ed25519.pub

git tag -s v0.1.0 -m "AriesConnect 0.1.0"
git push origin v0.1.0
```

The workflow publishes:

- `aries-connect-<version>.tar.gz` — source archive without `.git`.
- `aries-connect-<version>-x86_64-unknown-linux-gnu.tar.gz` — binary with
  `README.md`, `LICENSE` and `doc/`.
- `SHA256SUMS` and a build-provenance attestation for both archives.

Release notes are taken from the matching section of
[CHANGELOG.md](CHANGELOG.md). Artifacts can be verified with:

```sh
sha256sum -c SHA256SUMS
gh attestation verify aries-connect-<version>.tar.gz \
  --repo Frikilinux/aries-connect
```

## Documentation

- [Protocol](doc/protocol.md) — discovery, TLS control channel, trust model.
- [D-Bus interface](doc/dbus.md) — methods, signals and pairing flow.
- [Usage](doc/usage.md) — configuration, service, state files, troubleshooting.

## License

GPL-2.0-or-later. See [LICENSE](LICENSE).
