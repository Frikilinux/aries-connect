# D-Bus interface

Service: `dev.zotta.AriesConnect`
Object: `/dev/zotta/AriesConnect`
Interface: `dev.zotta.AriesConnect`

The object path is a D-Bus address, not a filesystem path: it is the
reverse-DNS form of the service name and does not refer to the system `/dev`
directory.

## Methods

| Method                    | Signature | Description                                       |
| ------------------------- | --------- | ------------------------------------------------- |
| `AcceptPairing(deviceId)` | `s`       | Accepts the pending pairing request               |
| `RejectPairing(deviceId)` | `s`       | Rejects the pending pairing request               |
| `Unpair(deviceId)`        | `s`       | Removes the device and its persisted state, notifying the phone |
| `Devices()`               | `-`       | Lists known devices as `(id, name, paired, connected)` — `a(ssbb)` |
| `ActiveNotifications()`   | `-`       | Lists active notifications as `(deviceId, id, appName, title, text)` — `a(sssss)` |
| `DismissNotification(deviceId, notificationId)` | `ss` | Dismisses a notification locally and on the phone |
| `PingDevice(deviceId, message)` | `ss` | Pings the device with an optional `message` (may be empty); fails if it cannot be delivered |
| `PluginEnabled(deviceId, plugin)` | `ss` | Whether `plugin` (e.g. `kdeconnect.notification`, `kdeconnect.ping`) is enabled for the device; unknown devices report `false` |
| `SetPluginEnabled(deviceId, plugin, bool)` | `ssb` | Enables or disables `plugin` for the device; persisted and announced with `PluginStateChanged` |

## Signals

Core signals carry two strings; the second is context and is empty when unused.
`NotificationReceived`, `NotificationDismissed` and `PingReceived` carry their
own payloads.

| Signal                  | Payload                              | Meaning                                     |
| ----------------------- | ------------------------------------ | ------------------------------------------- |
| `DeviceSeen`            | `(id, name)`                         | A device appeared over UDP                  |
| `Connected`             | `(id, "")`                           | Control channel established and identified  |
| `Disconnected`          | `(id, "")`                           | Control channel closed                      |
| `PairingRequested`      | `(id, name)`                         | A pairing decision is required              |
| `PairingAccepted`       | `(id, "")`                           | Pairing completed                           |
| `PairingRejected`       | `(id, reason)`                       | Pairing rejected, cancelled or timed out    |
| `PairingRemoved`        | `(id, "")`                           | The device was unpaired                      |
| `NotificationReceived`  | `(deviceId, id, appName, title, text)` | The phone posted a notification           |
| `NotificationDismissed` | `(deviceId, id)`                     | A notification was dismissed                |
| `PingReceived`          | `(deviceId, message)`                | A paired device pinged us                   |
| `PluginStateChanged`    | `(deviceId, plugin, enabled)`        | A per-device plugin was toggled             |

Notifications are kept in memory across reconnects (Android replaces its socket
frequently); they are cleared when the device is unpaired or when its
notifications plugin is disabled. Clients should refresh `ActiveNotifications()`
when they receive `PluginStateChanged`.

`PluginStateChanged` is only emitted when a value actually changes:
`SetPluginEnabled` is a silent no-op for unknown devices or when the requested
state already matches. There is no signal for the initial state, so clients
must call `PluginEnabled` per device (unknown devices report `false`).
Unpairing deletes `devices/<id>/` and is announced with `PairingRemoved`, not
with `PluginStateChanged`.

## Pairing flow

1. The phone requests pairing over the control channel.
2. The daemon emits `PairingRequested(id, name)`.
3. The client calls `AcceptPairing(id)` or `RejectPairing(id)` within 30 s.
4. The daemon replies to the phone and emits `PairingAccepted` or
   `PairingRejected`.

## Unpairing flow

1. The client calls `Unpair(id)`.
2. The daemon sends `kdeconnect.pair: false` through the device's active control
   channel, so the phone drops its side too. This only reaches the phone while
   it is connected.
3. The daemon deletes `devices/<id>/`, clears the device's notifications and
   emits `PairingRemoved(id)`.
4. The phone closes the control channel; the daemon then emits
   `Disconnected(id)`.

An unpair requested by the phone (`kdeconnect.pair: false`) follows the same
local cleanup and emits `PairingRemoved`, but no packet is echoed back.

## Ping

`PingDevice(deviceId, message)` asks the phone to show a ping notification. The
message is optional and may be empty; the phone falls back to its generic
"Ping!" text. There is no acknowledgement on the wire, but the call **fails**
with a D-Bus error (`org.freedesktop.DBus.Error.Failed`) when the ping cannot
be delivered:

- the device is unknown or not paired;
- the `kdeconnect.ping` plugin is disabled for it;
- it has no active control channel, i.e. the phone is offline.

A ping sent by the phone emits `PingReceived(deviceId, message)`. Incoming
pings from unpaired devices, or from devices with the plugin disabled, are
dropped with a debug log and no signal.

Plugin state is local to the daemon: disabling `kdeconnect.ping` does not notify
the phone, which keeps offering the action until it reconnects. Clients learn
about changes through `PluginStateChanged`, or by querying `PluginEnabled`; the
toggle is persisted in `devices/<id>/device.json`.

```sh
busctl --user call dev.zotta.AriesConnect /dev/zotta/AriesConnect \
  dev.zotta.AriesConnect PingDevice ss <deviceId> ""

busctl --user call dev.zotta.AriesConnect /dev/zotta/AriesConnect \
  dev.zotta.AriesConnect PluginEnabled ss <deviceId> kdeconnect.ping

busctl --user call dev.zotta.AriesConnect /dev/zotta/AriesConnect \
  dev.zotta.AriesConnect SetPluginEnabled ssb <deviceId> kdeconnect.ping false
```

## Examples

```sh
busctl --user introspect dev.zotta.AriesConnect /dev/zotta/AriesConnect
busctl --user call dev.zotta.AriesConnect /dev/zotta/AriesConnect \
  dev.zotta.AriesConnect Devices

# listen for signals
busctl --user monitor dev.zotta.AriesConnect

# accept a pending pairing request
busctl --user call dev.zotta.AriesConnect /dev/zotta/AriesConnect \
  dev.zotta.AriesConnect AcceptPairing s <deviceId>

# list active notifications
busctl --user call dev.zotta.AriesConnect /dev/zotta/AriesConnect \
  dev.zotta.AriesConnect ActiveNotifications

# dismiss a notification on the phone as well
busctl --user call dev.zotta.AriesConnect /dev/zotta/AriesConnect \
  dev.zotta.AriesConnect DismissNotification ss <deviceId> <notificationId>

# unpair a device (also removes its persisted state)
busctl --user call dev.zotta.AriesConnect /dev/zotta/AriesConnect \
  dev.zotta.AriesConnect Unpair s <deviceId>

# ping a device (the message may be empty)
busctl --user call dev.zotta.AriesConnect /dev/zotta/AriesConnect \
  dev.zotta.AriesConnect PingDevice ss <deviceId> "Hello from the desktop"

# check and toggle the notifications plugin of a device
busctl --user call dev.zotta.AriesConnect /dev/zotta/AriesConnect \
  dev.zotta.AriesConnect PluginEnabled ss <deviceId> kdeconnect.notification

busctl --user call dev.zotta.AriesConnect /dev/zotta/AriesConnect \
  dev.zotta.AriesConnect SetPluginEnabled ssb <deviceId> \
  kdeconnect.notification false
```
