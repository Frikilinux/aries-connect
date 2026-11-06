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
| `config.json`            | Daemon configuration (display name)             |
| `device_id`              | 32-character KDE Connect device id              |
| `devices/<id>/device.json` | One paired device: name, certificate SHA-256 fingerprint, last known IPv4 and per-plugin overrides |

`config.json` only stores an override for the name announced to other devices:

```json
{
  "deviceName": "My Desktop"
}
```

When `deviceName` is absent, `null` or blank, the host name is used. The file is
optional: the daemon starts with the host name when it does not exist.

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

## Device name

The name other devices see comes from `config.json`, falling back to the host
name. Change it at runtime (the daemon persists it and re-announces its
identity immediately):

```sh
busctl --user call dev.zotta.AriesConnect /dev/zotta/AriesConnect \
  dev.zotta.AriesConnect SetDeviceName s "My Desktop"
```

Read the current value:

```sh
busctl --user get-property dev.zotta.AriesConnect /dev/zotta/AriesConnect \
  dev.zotta.AriesConnect DeviceName
```

An empty string clears the override and returns to the host name:

```sh
busctl --user call dev.zotta.AriesConnect /dev/zotta/AriesConnect \
  dev.zotta.AriesConnect SetDeviceName s ""
```

The maximum is 128 characters. Renaming does not affect pairing: the device id
and certificate stay the same, so the phone keeps its trust.

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

Each known device is also a D-Bus object under
`/dev/zotta/AriesConnect/Device/<id>`. To unpair from the computer:

```sh
busctl --user call dev.zotta.AriesConnect \
  /dev/zotta/AriesConnect/Device/<id> \
  dev.zotta.AriesConnect.Device Unpair
```

The daemon sends `kdeconnect.pair: false` to the phone so it drops its side
too, then deletes the whole `devices/<id>/` directory and flips the device's
`Paired` property to `false`. The phone must be connected at that moment to
receive the notification; otherwise only the local state is cleared and the
phone keeps its side until it pairs again.

Unpairing from the phone (`kdeconnect.pair: false`) removes the same local
state (`devices/<id>/`) and also leaves `Paired` at `false`.

## Ping

Once a device is paired and connected, the desktop can ping it:

```sh
busctl --user call dev.zotta.AriesConnect \
  /dev/zotta/AriesConnect/Device/<id> \
  dev.zotta.AriesConnect.Device Ping s "Hello from the desktop"
```

The message may be empty. The phone shows a notification; it does not reply.
`Ping` fails with a D-Bus error when the device is not paired, the
`kdeconnect.ping` plugin is disabled for it or it is not connected.

Pings sent by the phone appear as the `PingReceived(message)` signal on the
device object; they are ignored when the plugin is disabled for that device.
The plugin can be toggled with `SetPluginEnabled` like any other. Toggling is
local only (the phone is not notified) and is announced with `PropertiesChanged`
on the `Plugins` property, which always lists the current state.

The phone learns the available plugins from the identity it receives when it
connects, so restart the phone app (or wait for it to reconnect) after upgrading
the daemon for the ping action to appear.

## Find My Phone

Ask a paired, connected phone to announce itself (ring):

```sh
busctl --user call dev.zotta.AriesConnect \
  /dev/zotta/AriesConnect/Device/<id> \
  dev.zotta.AriesConnect.Device FindMyPhone
```

The request has no argument. By convention, calling it a second time cancels
the previous request. `FindMyPhone` fails with a D-Bus error when the device
is not paired, the `kdeconnect.findmyphone.request` plugin is disabled for it
or it is not connected.

The daemon has no audio output, so requests received from a phone are consumed
without ringing; a frontend can implement that direction on top of the
protocol. The plugin can be toggled with `SetPluginEnabled` like any other.

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
- **The phone does not show the ping action**: restart the phone app so it
  fetches the daemon identity again, and check the device's `Plugins` property
  to confirm `kdeconnect.ping` is enabled.
- **`Ping` fails**: the error says whether the device is not paired, the plugin
  is disabled or the phone is not connected. The `Connected` and `Paired`
  properties report the device state.
- **Notifications stop when the phone screen is off**: Android is suspending the
  KDE Connect app or filtering its network traffic. Set the app's battery usage
  to unrestricted, enable its persistent notification and keep Wi-Fi active
  during sleep. The daemon also announces unicast to the peer's last address to
  help it reconnect, but it cannot keep the app alive.
