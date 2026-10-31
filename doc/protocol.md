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
  IPv4 interface (e.g. `192.168.1.255`) and loopback.
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

Implemented packet types: `kdeconnect.identity`, `kdeconnect.pair`. Unsupported
types are ignored (debug log).

## Trust model

- Pairing (`kdeconnect.pair` with `pair: true`) is surfaced to the user as a
  D-Bus signal. The decision is sent back as `pair: true/false`; unanswered
  requests expire after 30 s and are rejected.
- `pair: false` from a peer unpairs the device.
- On acceptance the peer id, name and certificate SHA-256 fingerprint are stored
  in `trusted_devices.json`.
- Reconnecting paired devices must present the same certificate; a mismatch is
  rejected.
- Replies are written through the device's **current** control channel, because
  Android replaces its socket on every discovery broadcast.

## Android compatibility

- `deviceId` must match `^[a-zA-Z0-9_-]{32,38}$`; other ids are silently
  discarded by the phone. AriesConnect generates and persists a 32-character id.
- The phone must be on a network it trusts ("Trusted networks" in KDE Connect;
  all networks are allowed by default).
- The phone connects to the advertised `tcpPort` (1716 by default).
