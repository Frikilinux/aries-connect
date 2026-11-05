# Protocol

AriesConnect implements the KDE Connect wire protocol (v8) directly on top of
UDP, TCP, TLS and JSON. No KDE/Qt libraries are involved.

## Transport

| Transport | Port | Purpose                              |
| --------- | ---- | ------------------------------------ |
| UDP       | 1716 | Device discovery (identity datagrams) |
| TCP       | 1716 | TLS control channel                  |

Both sockets bind `0.0.0.0` with `SO_REUSEADDR`.

## Discovery (UDP)

- Every device periodically broadcasts its identity as a plain JSON packet.
- AriesConnect broadcasts to `255.255.255.255`, the directed broadcast of each
  IPv4 interface (e.g. `192.168.1.255`) and loopback. It also remembers the last
  address of every peer and announces to it **unicast**, because Android may
  filter broadcast/multicast traffic while the screen is off.
- The last address of every **paired** device is persisted in its
  `devices/<id>/device.json`, so unicast announcements can resume right after a
  daemon restart.
- On receiving an identity it answers the exact source address **and** the
  source IP on port 1716, in both plain JSON and the legacy
  `kdeconnect.`-prefixed form. Android sends discovery datagrams from ephemeral
  UDP ports, so replying only to `ip:1716` is not reliable.
- Identity datagrams are parsed from the first `{`; the legacy prefix is
  tolerated because the JSON itself contains `"kdeconnect.identity"`.
- The daemon ignores its own identity (loopback/LAN echo).

## Control channel (TCP, protocol v8)

The phone is always the TCP initiator; AriesConnect only accepts connections.

1. The connecting peer sends its identity as one newline-terminated JSON line
   in **plaintext**.
2. AriesConnect starts the TLS handshake as the **client** (the TCP server is
   always the TLS client in KDE Connect), presenting its persisted
   self-signed RSA-2048 certificate. Peer certificates are accepted at the TLS
   layer; trust is established by pairing and pinning.
3. Both sides exchange their identities again over TLS. Device id and protocol
   version must match the plaintext identity, otherwise the connection is
   dropped.
4. From then on, packets are newline-delimited JSON. A packet is
   `{"id": <ms>, "type": "...", "body": {...}}`.

Implemented packet types: `kdeconnect.identity`, `kdeconnect.pair`,
`kdeconnect.notification`, `kdeconnect.ping`, `kdeconnect.battery`,
`kdeconnect.clipboard`, `kdeconnect.clipboard.connect` and
`kdeconnect.connectivity_report`; `kdeconnect.notification.request` is sent to
dismiss notifications on the phone. Unsupported types are ignored (debug log).

## Notifications

Notifications are mirrored only for **paired** devices with their
`kdeconnect.notification` plugin enabled (see [Per-device plugins](#per-device-plugins)).
The phone sends one `kdeconnect.notification` packet per posted notification and
another one with `isCancel: true` when it is dismissed.

Relevant body fields: `id` (string or number), `appName`, `title`, `text`,
`time` (milliseconds) and `isCancel`. The daemon stores up to 100 entries per
device and drops the oldest first, announcing the eviction with
`NotificationDismissed` so clients that mirror the set stay in sync. A
cancellation for an id the daemon does not know is still forwarded as
`NotificationDismissed`, which lets a client drop an entry it learned about
before the daemon evicted or restarted.

Dismissing from the desktop sends `kdeconnect.notification.request` with
`{"cancel": "<id>"}`. The local mirror is removed immediately; the phone may
ignore the request for non-clearable notifications.

## Ping

Both directions use the same `kdeconnect.ping` packet with an optional
`{"message": "<text>"}` body. The sender does not expect a reply: the receiver
only shows a notification, and the daemon surfaces incoming pings over D-Bus.

Pings are accepted only from **paired** devices with their `kdeconnect.ping`
plugin enabled; the same restriction applies when the desktop pings the phone.
The message may be missing or empty, in which case the phone shows its generic
"Ping!" notification.

AriesConnect advertises `kdeconnect.ping` in both `incomingCapabilities` and
`outgoingCapabilities` of its identity, which is what makes the phone offer the
ping action. The phone evaluates capabilities when it receives an identity, so
after changing the advertised set the daemon must reconnect (or announce again)
before the phone reflects it.

Incoming pings are dropped with a debug log when they come from an unpaired
device or when the plugin is disabled for it.

## Battery

The device that has a battery pushes `kdeconnect.battery` when the control
channel is established and again whenever the charge, the charging state or the
low-battery threshold changes:

```json
{ "currentCharge": 75, "isCharging": true, "thresholdEvent": 0 }
```

`currentCharge` is `-1` when the device has no battery, and `thresholdEvent` is
`1` while the device considers the battery low. AriesConnect stores the last
status per device while it is connected and exposes it as `BatteryLevel` and
`IsCharging` on the device object, announced with `PropertiesChanged`. The
cached status is dropped when the control channel closes, so the properties go
back to `-1`/`false` until the phone reconnects.

The old `kdeconnect.battery.request` packet is deprecated and current clients do
not answer it, so the daemon does not send it: status updates are pushed by the
device. Battery packets are accepted only from **paired** devices with their
`kdeconnect.battery` plugin enabled.

Note that KDE Connect Android advertises `kdeconnect.battery` in its identity
even when the plugin is disabled for a device, so turning the plugin off on the
phone is only visible to the daemon through the absence of updates, not through
a capability change.

## Connectivity report

The phone pushes `kdeconnect.connectivity_report` with the coarse signal
strength of each subscription (SIM):

```json
{
  "signalStrengths": {
    "6": { "networkType": "LTE", "signalStrength": 3 },
    "17": { "networkType": "HSPA", "signalStrength": 0 }
  }
}
```

The key is an arbitrary subscription id. `networkType` is the cellular
technology (`GSM`, `CDMA`, `UMTS`, `LTE`, `5G`, `HSPA`, ...) and
`signalStrength` is a level in `0..=4`, not dBm. AriesConnect stores the last
map per device while it is connected and exposes it as `SignalStrengths`
(`a{s(si)}`) on the device object, announced with `PropertiesChanged`. The map
is cleared when the control channel closes, when the device is unpaired or when
the plugin is disabled.

Reports are accepted only from **paired** devices with the
`kdeconnect.connectivity_report` plugin enabled. The daemon consumes the
current `signalStrengths` form; the older flat fields (`networkType`,
`wifiLinkSpeed`, `wifiFrequency`, coordinates, ...) are ignored.

## Clipboard

Clipboard sync is bidirectional and text-only. Every change is sent as:

```json
{ "content": "texto copiado" }
```

with type `kdeconnect.clipboard`. In addition, when the control channel is
established each side may send `kdeconnect.clipboard.connect` with the time the
clipboard was last updated:

```json
{ "content": "texto copiado", "timestamp": 1700000000000 }
```

`timestamp` is milliseconds since the epoch, may be a number or a string, and is
`0` when unknown. The receiver should ignore a `clipboard.connect` whose
timestamp is `0` or older than its own clipboard timestamp, so the newest
content wins when both devices connect.

AriesConnect does not touch the local clipboard: it relays both packet types
over D-Bus and leaves reading/writing to a session client. Empty text is
ignored, and text over 1 MiB is dropped. Packets are accepted only from
**paired** devices with the `kdeconnect.clipboard` plugin enabled.

There is no acknowledgement: a sent packet may be ignored by the peer, for
example when its clipboard plugin is disabled. On Android 10+ the phone cannot
read the clipboard in the background without extra permissions, so automatic
phone-to-desktop sync may require the manual “Send clipboard” action.

## Trust model

- Pairing (`kdeconnect.pair` with `pair: true`) is surfaced to the user as a
  D-Bus signal. The decision is sent back as `pair: true/false`; unanswered
  requests expire after 30 s and are rejected.
- `pair: false` from a peer unpairs the device.
- Unpairing from the desktop sends `pair: false` to the peer before forgetting
  the device, so both sides drop the pairing. If the peer is offline at that
  moment the packet is lost, so an untrusted device that later reconnects and
  sends any plugin packet is answered with `pair: false`: the phone then drops
  its stale pairing instead of showing the desktop as paired forever.
- On acceptance the peer id, name and certificate SHA-256 fingerprint are stored
  in the device's `devices/<id>/device.json`.
- Reconnecting paired devices must present the same certificate; a mismatch is
  rejected.
- Replies are written through the device's **current** control channel, because
  Android replaces its socket on every discovery broadcast.

## Per-device plugins

Each paired device has its own `devices/<id>/device.json` holding name,
certificate fingerprint, last known IPv4 address and plugin overrides. Plugins
listed in `plugins` carry an explicit `false`; anything absent is enabled, so
defaults keep working as plugins are added. Device files are written atomically
(temporary file plus rename) and only when a value actually changed.

- `SetPluginEnabled(plugin, bool)` on the device object toggles one plugin; the
  change is persisted and announced with `PropertiesChanged` on the device's
  `Plugins` property. Only real changes update it: repeated values are silent
  no-ops, and unsupported plugins are rejected with a D-Bus error.
- Plugin state is **local**: toggling a plugin neither rewrites the identity nor
  notifies the phone, so the phone keeps offering that plugin until it
  reconnects. Outgoing packets of a disabled plugin are not sent either.
- Disabling notifications drops that device's mirrored notifications;
  reenabling starts empty.
- Unpairing deletes the device directory, including its plugin state. Legacy
  monolithic `trusted_devices.json` files are migrated into `devices/` on load
  and removed afterwards.

## Android compatibility

- `deviceId` must match `^[a-zA-Z0-9_-]{32,38}$`; other ids are silently
  discarded by the phone. AriesConnect generates and persists a 32-character id.
- The phone must be on a network it trusts ("Trusted networks" in KDE Connect;
  all networks are allowed by default).
- The phone connects to the advertised `tcpPort` (1716 by default).
