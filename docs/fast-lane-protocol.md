# Zap fast-lane wire protocol (v1)

> The custom native-to-native "fast lane" transport. This is **additive**: the
> HTTP(S) browser path is the default and is never removed or gated. A browser
> cannot speak this protocol, so the fast lane only runs app-to-app and always
> falls back to HTTP. See `docs/custom-transport-brief.md` for the full design.

## 1. Discovery (HTTP bootstraps the fast lane)

HTTP stays the control channel. A native client that has a Zap share link first
calls, over the existing HTTP server (authenticating with the pairing token as a
`zap_session` cookie):

```
GET /api/capabilities
-> 200 {"fast":{"port":<u16>,"tls":<bool>,"version":1}}     fast lane available
-> 200 {"fast":null}                                         no fast lane (use HTTP)
```

- `port` - the TCP port of the fast-lane listener, bound to the same interface
  as the HTTP server. It is **OS-assigned** (bind port 0), so it must be read
  from here, never assumed.
- `tls` - whether the fast-lane listener itself speaks TLS (see §5). In v1 (this
  phase) the fast lane is plain TCP, so the server advertises the fast lane
  **only when the HTTP server is plain HTTP**; when HTTP is HTTPS, it returns
  `fast:null` and native clients stay on HTTPS. No silent downgrade. Fast-lane
  TLS lands in a later phase and will set `tls:true` under the shared cert.
- `version` - protocol version the server speaks (currently `1`).

Browsers never call this endpoint - they just render the page - so they are
unaffected. Older servers (no such endpoint / `fast:null`) make the client use
the HTTP path.

## 2. Framing

All multi-byte integers are **little-endian**. Strings are length-prefixed
UTF-8, not null-terminated. A "connection" is one TCP connection carrying one
handshake, one reply, then (on success) the requested byte range.

## 3. Handshake (client -> server)

```
offset  size  field
0       4     magic          = b"ZAPX" (0x5A 0x41 0x50 0x58)
4       2     version        u16  (client speaks; server rejects if unsupported)
6       1     op             u8   (1 = GET/download, 2 = PUT/upload)
7       2     token_len      u16
9       N     token          UTF-8, the pairing/session token; empty if the
                             server is open (no auth)
9+N     4     path_len       u32
..      M     path           UTF-8, the file path relative to the share root
                             (same value the HTTP path uses as ?path=)
..      8     offset         u64  resume point; first byte of the range to send
..      8     range_len      u64  bytes to send from `offset`; 0 = to EOF
```

`op = 2` (PUT/upload) is reserved for a later phase; v1 servers reply with an
error for it.

## 4. Handshake reply (server -> client)

```
offset  size  field
0       1     status         u8  (0 = OK, non-zero = error, see below)
```

On **OK** (`status = 0`):

```
1       8     total_size     u64  whole-file size in bytes
9       1     has_crc        u8   (1 = a whole-file CRC follows, 0 = none)
10      4     crc32          u32  IEEE/zlib CRC-32 of the WHOLE file
                             (present only if has_crc = 1)
```

Then the server writes the raw bytes of `[offset, offset + len)` where
`len = range_len` (or `total_size - offset` when `range_len = 0`), clamped to the
file. No per-byte framing inside the range - TCP is a reliable ordered stream, so
the client reads exactly `len` bytes and writes them at `offset` in its file.

On **error** (`status != 0`):

```
1       2     msg_len        u16
3       K     msg            UTF-8 human-readable error
```

### Status codes

| code | meaning |
| --- | --- |
| 0 | OK |
| 1 | bad request (bad magic / unsupported version / malformed handshake) |
| 2 | unauthorized (token required and did not match) |
| 3 | not found (path does not resolve to a readable file) |
| 4 | unsupported op (e.g. PUT in v1) |
| 5 | server error |

## 5. Auth + TLS

- **Auth:** when the server is secured (a pairing token exists), the handshake
  `token` must equal the server's session token, byte for byte. Mismatch or empty
  -> status 2 and the connection closes. When the server is open, `token` is
  ignored. This is the same token the HTTP `?k=` / `zap_session` cookie uses.
- **TLS:** in v1 the listener is plain TCP and is only advertised when the HTTP
  server is plain (see §1). A later phase runs the listener under the same
  self-signed rustls cert as HTTPS; native clients pin the fingerprint they
  already learned from the QR/pairing (`&fp=` in the share URL). `tls` in
  `/api/capabilities` reflects which mode is active.

## 6. Integrity + resume (mirror the HTTP path)

- **Downloads:** the client writes bytes into a temp file `.zap-part-<name>` at
  the correct offsets, and on completion verifies assembled size == `total_size`
  and (if `has_crc`) the whole-file CRC-32 before an atomic rename into place. A
  partial file is never exposed as complete - the same rule the HTTP path
  enforces.
- **Resume across drops:** the temp file's size on disk is the checkpoint. On a
  dropped connection the client re-handshakes with `offset` = current temp-file
  size and continues. Identical model to the HTTP resumable download/upload.
- **Fallback:** if the fast lane is not advertised, not reachable, or errors
  beyond retries, the client finishes the file over the HTTP path, resuming by
  offset from the bytes already on disk. A fast-lane failure never fails a
  transfer that HTTP could complete.

## 7. Multi-stream (later phase - noted, not in v1)

v1 uses a single connection / single range. The throughput win on lossy Wi-Fi
comes from opening **N connections**, each requesting a distinct `[offset,
range_len)` slice of the same file and writing it at the right position, plus
adapting the connection count and range size to measured throughput/RTT. The
framing above already carries everything needed for that (per-connection offset +
range_len); only the client's scheduling changes.
