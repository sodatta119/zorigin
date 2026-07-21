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

Common prefix (both ops):

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
```

For **GET** (`op = 1`), the prefix is followed by:

```
..      8     offset         u64  resume point; first byte of the range to send
..      8     range_len      u64  bytes to send from `offset`; 0 = to EOF
```

For **PUT** (`op = 2`), the prefix is followed by:

```
..      8     offset         u64  client's known offset (informational; the server
                             replies with the authoritative resume offset)
..      8     total          u64  whole-file size the client will upload
..      1     has_crc        u8   (1 = a whole-file CRC follows)
..      4     crc32          u32  IEEE/zlib CRC-32 of the whole file (if has_crc)
```

## 4. Handshake reply (server -> client)

```
offset  size  field
0       1     status         u8  (0 = OK, non-zero = error, see below)
```

On a **GET OK** (`status = 0`):

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

**Stat (zero-length) request.** A handshake whose effective `len` is 0 - the
client sends `offset >= total_size` (e.g. `offset = u64::MAX`) - is a "stat": the
server replies with the OK header (`total_size` + CRC) and **no data**, and does
not create a transfer row. The multi-stream client uses this once up front to
learn the file size before fanning out, which also warms the server's CRC cache
(below) so the parallel workers don't each recompute it.

**Server CRC cache (implementation note, not wire-visible).** The server caches
the whole-file CRC-32 per `(path, size, mtime)`. Without it, every one of a
multi-stream download's handshakes would re-read the whole file to compute the
CRC. The first request (normally the stat) computes it; the rest reuse it.

On a **PUT OK** (`status = 0`):

```
1       8     offset         u64  the authoritative resume offset - how many
                             bytes the server already holds. The client seeks its
                             local file here and streams [offset, total).
```

The client then writes the raw bytes `[offset, total)`. When the server has the
whole file it verifies the CRC, atomically renames the temp file into place, and
sends a single **final status** byte: `0` = ok/verified, `6` = integrity failure,
`5` = other server error. A dropped connection mid-upload leaves the temp file for
the client to resume (re-handshake -> new authoritative offset).

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
| 4 | unsupported op |
| 5 | server error |
| 6 | integrity check failed (upload CRC mismatch) |

## 5. Auth + TLS

- **Auth:** when the server is secured (a pairing token exists), the handshake
  `token` must equal the server's session token, byte for byte. Mismatch or empty
  -> status 2 and the connection closes. When the server is open, `token` is
  ignored. This is the same token the HTTP `?k=` / `zap_session` cookie uses.
- **TLS (implemented, feature `tls`):** when the HTTP server is HTTPS, the fast
  lane runs under the **same** self-signed rustls cert, and `/api/capabilities`
  reports `tls:true`. A native client pins the cert by the SHA-256 fingerprint it
  learned from the pairing link (`&fp=` in the share URL) - there is no CA on the
  LAN, so fingerprint pinning is the trust primitive; a wrong fingerprint fails
  the handshake and the transfer is refused (no downgrade). The control channel
  (`/api/capabilities`, HTTP fallback) uses the same pinned TLS. When the HTTP
  server is plain, the fast lane is plain TCP, token-authed, and `tls:false` - the
  fast lane is never run plain alongside an HTTPS server.

## 6. Integrity + resume (mirror the HTTP path)

- **Downloads:** the client writes bytes into a temp file `.zap-part-<name>` at
  the correct offsets, and on completion verifies assembled size == `total_size`
  and (if `has_crc`) the whole-file CRC-32 before an atomic rename into place. A
  partial file is never exposed as complete - the same rule the HTTP path
  enforces.
- **Uploads (PUT):** the server appends into `.zap-part-<name>` from the
  authoritative offset it reports, verifies the client-supplied whole-file CRC-32
  once the temp file reaches `total`, then atomically renames into place - byte
  for byte the same resumable-upload semantics as the HTTP `PUT /upload` path,
  reusing the same temp file and coalesced transfer row. The HTTP fallback for
  uploads is the browser's resumable upload (`HEAD` for the offset, then `PUT`
  with `X-Zap-Total` / `X-Zap-Crc32`).
- **Resume across drops:** the temp file's size on disk is the checkpoint. On a
  dropped connection the client re-handshakes with `offset` = current temp-file
  size and continues. Identical model to the HTTP resumable download/upload.
- **Fallback:** if the fast lane is not advertised, not reachable, or errors
  beyond retries, the client finishes the file over the HTTP path, resuming by
  offset from the bytes already on disk. A fast-lane failure never fails a
  transfer that HTTP could complete.

## 7. Multi-stream (implemented) + adaptation (later phase)

The client opens **N connections** (default 4), each requesting a distinct
`[offset, range_len)` slice of the same file (default 4 MiB chunks) and writing it
at the correct absolute offset via a positioned write, so the ranges reassemble
with no locking on the hot path. Workers pull chunk indices from a shared queue;
a failed chunk is requeued and retried by any worker, and if the pool exhausts its
retry budget the client truncates the temp file to its **contiguous prefix** and
falls back to HTTP from there. On success it verifies the whole-file size + CRC-32
before the atomic rename. The wire format above is unchanged - only the client's
scheduling differs from a single stream.

**Resume model with holes.** During a run the temp file is pre-sized to
`total_size` and filled at offsets, so it briefly contains gaps. The client
guarantees the file at rest is either complete or a valid contiguous prefix: on a
graceful failure/fallback it truncates to the contiguous prefix, so the next run
(or the HTTP fallback) resumes by size as usual. A hard process kill mid-run can
leave a full-size file with holes; the whole-file CRC check catches this and the
download restarts clean (never surfaced as complete). A future per-file chunk
manifest (sidecar) would make even a hard-kill resumable - noted, not built.

**Adaptation (implemented).** By default the client adapts live:

- It stats the file, then starts an elastic worker pool at `MIN_STREAMS` (4, the
  brief's "start ~4") and hands out ranges from a moving cursor at the current
  chunk size.
- A controller samples aggregate throughput every 100 ms. While throughput keeps
  improving and no ranges are failing it grows concurrency multiplicatively
  (slow-start, doubling toward the `--streams` cap, default 8); on a throughput
  drop it sheds one stream, and on any range failures it eases off one stream
  (AIMD). Chunk size for new ranges is sized from measured per-connection
  throughput and RTT (connect time), clamped to [1, 8] MiB - big enough to keep a
  few RTTs in flight, small enough that a dropped stream re-fetches little.
- Each decision is logged (`zap: fast-lane adapt - streams A->B, chunk N KiB, ~X
  MB/s, rtt Y ms, errs Z`) so the behavior is observable.
- `--fixed` turns adaptation off and uses exactly `--streams` connections at a
  constant `--chunk-mb`, for A/B experiments.

The adaptation lever is genuine: on a lossy-but-connected link, more streams fill
the pipe a single congestion-controlled stream cannot, and modest chunks bound the
cost of a dropped range. On a clean, low-latency link (e.g. loopback) the transfer
is already near line rate at a few streams, so the ramp simply settles quickly.

**Integrity cost note.** A download is verified with a whole-file CRC-32: the
server computes it once (cached), the client re-computes it over the assembled
file before the rename. For very large files this adds a read pass on each side;
a future optimization could fold per-range CRCs together (`crc32_combine`) to
verify without the extra pass. Correctness first: a partial or corrupt file is
never surfaced as complete.

**True loss-tolerance (out of scope):** independent streams with no head-of-line
blocking and real FEC is QUIC/UDP territory (`quinn`/`iroh`), a separate track.
