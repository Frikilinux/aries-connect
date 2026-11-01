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
| `Devices()`               | `-`       | Lists known devices as `(id, name, paired, connected)` — `a(ssbb)` |
| `ActiveNotifications()`   | `-`       | Lists active notifications as `(deviceId, id, appName, title, text)` — `a(sssss)` |
| `DismissNotification(deviceId, notificationId)` | `ss` | Dismisses a notification locally and on the phone |

## Signals

Every signal carries two strings; the second is context and is empty when
unused. Notification signals carry their own payloads.

| Signal                  | Payload                              | Meaning                                     |
| ----------------------- | ------------------------------------ | ------------------------------------------- |
| `DeviceSeen`            | `(id, name)`                         | A device appeared over UDP                  |
| `Connected`             | `(id, "")`                           | Control channel established and identified  |
| `Disconnected`          | `(id, "")`                           | Control channel closed                      |
| `PairingRequested`      | `(id, name)`                         | A pairing decision is required              |
| `PairingAccepted`       | `(id, "")`                           | Pairing completed                           |
| `PairingRejected`       | `(id, reason)`                       | Pairing rejected, cancelled or timed out    |
| `NotificationReceived`  | `(deviceId, id, appName, title, text)` | The phone posted a notification           |
| `NotificationDismissed` | `(deviceId, id)`                     | A notification was dismissed                |

Notifications are kept in memory across reconnects (Android replaces its socket
frequently); they are cleared when the device is unpaired.

## Pairing flow

1. The phone requests pairing over the control channel.
2. The daemon emits `PairingRequested(id, name)`.
3. The client calls `AcceptPairing(id)` or `RejectPairing(id)` within 30 s.
4. The daemon replies to the phone and emits `PairingAccepted` or
   `PairingRejected`.

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
```
