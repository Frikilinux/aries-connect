# D-Bus interface

Service: `dev.zotta.AriesConnect`

The daemon exposes two kinds of objects:

| Object | Interface | Scope |
| --- | --- | --- |
| `/dev/zotta/AriesConnect` | `dev.zotta.AriesConnect` | Service-wide data and the pairing prompt |
| `/dev/zotta/AriesConnect/Device/<id>` | `dev.zotta.AriesConnect.Device` | One known device: state, actions and its signals |

`org.freedesktop.DBus.ObjectManager` is served on the service object, so
clients can enumerate device objects and their properties with
`GetManagedObjects()` instead of walking paths.

The object paths are D-Bus addresses, not filesystem paths: they are the
reverse-DNS form of the service name and do not refer to the system `/dev`
directory.

## Service object

`/dev/zotta/AriesConnect`, interface `dev.zotta.AriesConnect`.

| Method | Signature | Description |
| --- | --- | --- |
| `AcceptPairing(deviceId)` | `s` | Accepts the pending pairing request |
| `RejectPairing(deviceId)` | `s` | Rejects the pending pairing request |
| `Devices()` | `-` | Snapshot of known devices as `(id, name, paired, connected)` — `a(ssbb)` |
| `ActiveNotifications()` | `-` | Notifications of every device as `(deviceId, id, appName, title, text)` — `a(sssss)` |

| Signal | Payload | Meaning |
| --- | --- | --- |
| `PairingRequested` | `(id, name)` | A pairing decision is required |
| `PairingRejected` | `(id, reason)` | Pairing rejected, cancelled or timed out |

`Devices()` is a convenience snapshot; `GetManagedObjects()` is the canonical
way to read device state. Both report the same data.

## Device objects

### Path

```
/dev/zotta/AriesConnect/Device/<id-seguro>
```

D-Bus path elements only accept `[A-Za-z0-9_]`, while KDE Connect ids may
contain `-`, so every other character is replaced by `_`. The raw id is always
available as the `DeviceId` property: **clients must use `DeviceId`, not the
path, to identify a device**. Do not build paths by hand; use
`GetManagedObjects()` or the path announced by `InterfacesAdded`.

An object is created when the daemon learns about a device (discovery or the
persisted trust store) and stays while the daemon knows it. Unpairing keeps the
object and only flips `Paired` to `false`; the phone usually reappears over
discovery, unpaired.

### Properties

All properties are read-only.

| Property | Type | Description |
| --- | --- | --- |
| `DeviceId` | `s` | KDE Connect device id (authoritative) |
| `DeviceName` | `s` | Device name announced in the identity |
| `DeviceType` | `s` | `phone`, `tablet`, `desktop`, `unknown`… |
| `Paired` | `b` | Whether the device is paired with this daemon |
| `Connected` | `b` | Whether it has an active control channel (silent disconnects are detected in ~30 s via TCP keepalive) |
| `Plugins` | `a{sb}` | Every implemented plugin and whether it is enabled for this device |
| `BatteryLevel` | `i` | Last reported charge (`0..=100`), or `-1` when unknown |
| `IsCharging` | `b` | Whether the device reported that it is charging |

Changes are announced with `org.freedesktop.DBus.Properties.PropertiesChanged`
on the device object. `Plugins` always lists every plugin the daemon
implements; plugins absent from `devices/<id>/device.json` count as enabled.

### Methods

| Method | Signature | Description |
| --- | --- | --- |
| `Unpair()` | `-` | Forgets the pairing and notifies the phone when connected |
| `Ping(message)` | `s` | Pings the device; the message may be empty. Fails if it cannot be delivered |
| `DismissNotification(notificationId)` | `s` | Dismisses a notification locally and on the phone |
| `SetPluginEnabled(plugin, enabled)` | `sb` | Enables or disables a plugin; fails for unsupported plugins |

### Signals

| Signal | Payload | Meaning |
| --- | --- | --- |
| `PingReceived` | `(message)` | The device pinged us (the message may be empty) |
| `NotificationReceived` | `(id, appName, title, text)` | The phone posted a notification |
| `NotificationDismissed` | `(id)` | A notification was dismissed, locally or on the phone |

Client-side proxies are bound to a path, so a client that uses proxies creates
one per device. A raw message stream (or `busctl monitor`) can instead filter
by interface and member and receive the signals of every device without
knowing the paths in advance.

## ObjectManager

`/dev/zotta/AriesConnect`, interface `org.freedesktop.DBus.ObjectManager`.

| Element | Signature |
| --- | --- |
| `GetManagedObjects()` | `a{oa{sa{sv}}}` |
| `InterfacesAdded` | `(o, a{sa{sv}})` |
| `InterfacesRemoved` | `(o, as)` |

`GetManagedObjects()` returns one entry per device object with all its
properties, which is enough to build the initial view in a single call.
`InterfacesAdded` is emitted when a new device object is registered.

## Pairing flow

1. The phone requests pairing over the control channel.
2. The daemon emits `PairingRequested(id, name)` on the service object.
3. The client calls `AcceptPairing(id)` or `RejectPairing(id)` within 30 s.
4. On accept, the device object appears (if new) and `Paired` becomes `true`
   (`PropertiesChanged`). On reject, the daemon emits
   `PairingRejected(id, reason)`.

## Unpairing flow

1. The client calls `Unpair()` on the device object.
2. The daemon sends `kdeconnect.pair: false` through the active control
   channel, so the phone drops its side too. This only reaches the phone while
   it is connected.
3. The daemon deletes `devices/<id>/`, clears the device's notifications and
   sets `Paired` to `false` (`PropertiesChanged`). The object stays, because
   the device is still known.
4. The phone closes the control channel; `Connected` becomes `false`.

An unpair requested by the phone (`kdeconnect.pair: false`) follows the same
local cleanup and leaves `Paired` at `false`, without echoing a packet back.

## Ping

`Ping(message)` asks the phone to show a ping notification. The message is
optional and may be empty; the phone falls back to its generic "Ping!" text.
There is no acknowledgement on the wire, but the call **fails** with a D-Bus
error (`org.freedesktop.DBus.Error.Failed`) when the ping cannot be delivered:

- the device is not paired;
- the `kdeconnect.ping` plugin is disabled for it;
- it has no active control channel, i.e. the phone is offline.

A ping sent by the phone emits `PingReceived(message)` on the device object.
Incoming pings from unpaired devices, or from devices with the plugin
disabled, are dropped with a debug log and no signal.

## Battery

The phone pushes `kdeconnect.battery` when the control channel is established
and whenever the charge, the charging state or the low-battery threshold
changes. `BatteryLevel` and `IsCharging` mirror the last report and are
announced with `PropertiesChanged` on the device object.

`BatteryLevel` is `-1` until the first report, when the device has no battery,
when the plugin is disabled, after unpairing and when the control channel
closes. The cached value is dropped on disconnect, so `BatteryLevel` never
shows stale data while the phone is offline; it is repopulated when the phone
reconnects and pushes its status. The deprecated `kdeconnect.battery.request`
packet is not used: the phone sends its state on its own.

Disabling the plugin only hides the battery data (`BatteryLevel` becomes `-1`
and `IsCharging` becomes `false`); it does not disconnect the device, so
`Connected` stays `true`. Enabling it again starts from `-1` until the phone
sends a new status, which may not happen until the level changes.

Silent disconnects (Wi-Fi turned off, phone out of range) are detected with TCP
keepalive: the daemon probes the control channel after 15 s of inactivity and
gives up after three unanswered probes, so `Connected` flips to `false` and the
battery status is cleared within roughly 30 s.

## Notifications

`ActiveNotifications()` on the service object aggregates the notifications of
every device. `NotificationReceived` and `NotificationDismissed` are emitted on
the corresponding device object, and `DismissNotification(id)` is a device
method.

Notifications are kept in memory across reconnects (Android replaces its socket
frequently); they are cleared when the device is unpaired or when its
notifications plugin is disabled. Clients should refresh `ActiveNotifications()`
when they receive `PropertiesChanged` for `Plugins`.

## Plugins

`Plugins` lists every plugin implemented by the daemon (`kdeconnect.notification`
and `kdeconnect.ping`) with its state for that device. Toggling one:

```sh
busctl --user call dev.zotta.AriesConnect \
  /dev/zotta/AriesConnect/Device/<id> \
  dev.zotta.AriesConnect.Device SetPluginEnabled sb kdeconnect.ping false
```

`SetPluginEnabled` fails for plugins the daemon does not implement and for
unpaired devices. Setting the value it already has is a silent no-op. A real
change is persisted in `devices/<id>/device.json` and announced with
`PropertiesChanged` on `Plugins`.

Plugin state is **local**: toggling a plugin neither rewrites the identity nor
notifies the phone, so the phone keeps offering that plugin until it
reconnects. Disabling notifications drops that device's mirrored
notifications; reenabling starts empty.

## Examples

```sh
# list device objects and all their properties
busctl --user call dev.zotta.AriesConnect /dev/zotta/AriesConnect \
  org.freedesktop.DBus.ObjectManager GetManagedObjects

# inspect one device
busctl --user introspect dev.zotta.AriesConnect \
  /dev/zotta/AriesConnect/Device/<id>

busctl --user get-property dev.zotta.AriesConnect \
  /dev/zotta/AriesConnect/Device/<id> dev.zotta.AriesConnect.Device DeviceName

# listen for signals (device and service)
busctl --user monitor dev.zotta.AriesConnect

# accept a pending pairing request
busctl --user call dev.zotta.AriesConnect /dev/zotta/AriesConnect \
  dev.zotta.AriesConnect AcceptPairing s <deviceId>

# snapshot of known devices
busctl --user call dev.zotta.AriesConnect /dev/zotta/AriesConnect \
  dev.zotta.AriesConnect Devices

# list active notifications
busctl --user call dev.zotta.AriesConnect /dev/zotta/AriesConnect \
  dev.zotta.AriesConnect ActiveNotifications

# dismiss a notification on the phone as well
busctl --user call dev.zotta.AriesConnect \
  /dev/zotta/AriesConnect/Device/<id> \
  dev.zotta.AriesConnect.Device DismissNotification s <notificationId>

# unpair a device (also removes its persisted state)
busctl --user call dev.zotta.AriesConnect \
  /dev/zotta/AriesConnect/Device/<id> \
  dev.zotta.AriesConnect.Device Unpair

# ping a device (the message may be empty)
busctl --user call dev.zotta.AriesConnect \
  /dev/zotta/AriesConnect/Device/<id> \
  dev.zotta.AriesConnect.Device Ping s "Hello from the desktop"

# read the last reported battery state
busctl --user get-property dev.zotta.AriesConnect \
  /dev/zotta/AriesConnect/Device/<id> dev.zotta.AriesConnect.Device BatteryLevel

# toggle the ping plugin of a device
busctl --user call dev.zotta.AriesConnect \
  /dev/zotta/AriesConnect/Device/<id> \
  dev.zotta.AriesConnect.Device SetPluginEnabled sb kdeconnect.ping false
```
