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

## Signals

Every signal carries two strings; the second is context and is empty when
unused.

| Signal             | Payload                | Meaning                                     |
| ------------------ | ---------------------- | ------------------------------------------- |
| `DeviceSeen`       | `(id, name)`           | A device appeared over UDP                  |
| `Connected`        | `(id, "")`             | Control channel established and identified  |
| `Disconnected`     | `(id, "")`             | Control channel closed                      |
| `PairingRequested` | `(id, name)`           | A pairing decision is required              |
| `PairingAccepted`  | `(id, "")`             | Pairing completed                           |
| `PairingRejected`  | `(id, reason)`         | Pairing rejected, cancelled or timed out    |

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
```
