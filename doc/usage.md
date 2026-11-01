# Usage

## Run

```sh
cargo build --release
./target/release/aries-connect
```

The daemon must run inside a user session with a D-Bus session bus, because it
registers `dev.zotta.AriesConnect` there. It has no graphical dependencies and
keeps running until interrupted.

An example systemd user unit is provided in `contrib/aries-connect.service`;
adjust `ExecStart` to the binary path if you use it.

## Configuration

| Variable      | Default                              | Description                     |
| ------------- | ------------------------------------ | ------------------------------- |
| `AC_PORT`     | `1716`                               | UDP and TCP port                |
| `AC_DATA_DIR` | `$XDG_DATA_HOME/aries-connect`       | State directory                 |
| `RUST_LOG`    | `info`                               | `tracing` filter (`debug`, ...) |

Both ports must be reachable from the phone's network. If the machine runs a
firewall, allow inbound UDP and TCP on 1716.

## Persisted files

State lives in `$AC_DATA_DIR` (default `~/.local/share/aries-connect/`):

| File                     | Contents                                        |
| ------------------------ | ----------------------------------------------- |
| `cert.pem` / `key.pem`   | Self-signed RSA-2048 identity certificate       |
| `device_id`              | 32-character KDE Connect device id              |
| `devices/<id>/device.json` | One paired device: name, certificate SHA-256 fingerprint, last known IPv4 and per-plugin overrides |

`device.json` example:

```json
{
  "id": "15cd154dd02546c6a89679bbdae8da88",
  "name": "motorola edge 40",
  "cert_sha256": "f5d6a9f0...",
  "last_ip": "172.16.1.134",
  "plugins": {}
}
```

`plugins` lists only toggled plugins (`{"kdeconnect.notification": false}`);
anything absent is enabled. A legacy monolithic `trusted_devices.json` is
migrated into `devices/` automatically on the first start and removed
afterwards. Unpairing deletes the whole `devices/<id>/` directory.

Deleting the state directory resets the identity: the phone will see a new
device and must pair again.

## Pairing and unpairing

1. Open KDE Connect on the phone and wait for the computer to appear.
2. Request pairing from the phone.
3. Accept or reject within 30 seconds:

```sh
busctl --user call dev.zotta.AriesConnect /dev/zotta/AriesConnect \
  dev.zotta.AriesConnect AcceptPairing s <deviceId>

busctl --user call dev.zotta.AriesConnect /dev/zotta/AriesConnect \
  dev.zotta.AriesConnect RejectPairing s <deviceId>
```

Unpairing from the phone removes the device from the store (the whole
`devices/<id>/` directory).

## Troubleshooting

- **The phone does not list the computer**: confirm both are on the same
  network, client isolation is disabled, and inbound UDP/TCP 1716 is allowed.
  On the phone, the current network must be trusted.
- **The phone sees the computer but pairing fails**: check that a request was
  emitted (`busctl --user monitor dev.zotta.AriesConnect`) and accept it before
  the 30-second timeout.
- **The device id changes on every start**: the state directory is not
  writable; the daemon then falls back to an ephemeral id.
- **The certificate warning appears again**: the state directory was deleted or
  recreated.
- **Notifications stop when the phone screen is off**: Android is suspending the
  KDE Connect app or filtering its network traffic. Set the app's battery usage
  to unrestricted, enable its persistent notification and keep Wi-Fi active
  during sleep. The daemon also announces unicast to the peer's last address to
  help it reconnect, but it cannot keep the app alive.
