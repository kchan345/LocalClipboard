# Architecture

This document explains how LocalClipboard is built and why. It covers the
decisions and trade-offs made while porting
[MoKhajavi75/local-clipboard](https://github.com/MoKhajavi75/local-clipboard)
(Go) to a single Rust executable.

## 1. Goals and constraints

| Requirement | Consequence |
|---|---|
| Feature parity with the Go app (chat, files, device count, QR, auto-clear, update banner, Go-style flags) | Same HTTP routes, same WS JSON field names, same CLI behaviour |
| Single self-contained executable | Web UI and the WASM codec are embedded with `include_str!`/`include_bytes!`. Static musl/CRT builds |
| **No server persistence**: chat and attachments exist only in client memory | Server keeps metadata only. An attachment is available only while its owner is connected |
| **Folder attachments** | Manifest + concatenated stream on the wire, streaming ZIP for the receiver |
| **On-the-fly lz4** on the peer-to-peer transport, **in WebAssembly**, **chunked** | One Rust codec crate compiled natively (server) and to `wasm32` (browser). 256 KiB chunks, constant memory |
| Build and test only in GitHub Actions | CI builds the WASM first and then runs fmt/clippy/tests on 3 OSes, release builds, a JS↔WASM compat test and Playwright e2e |

## 2. Component overview

```
 sending browser                       local-clipboard (Rust)                   receiving browser
┌───────────────────────┐            ┌──────────────────────────────┐          ┌────────────────────────┐
│ script.js (UI)        │  /ws JSON  │ http.rs  (axum routes)       │  /ws     │ script.js (UI)         │
│  owned: id → File(s)  │◀──────────▶│ hub.rs   (clients, registry, │◀────────▶│                        │
│                       │            │           timers, tokens)    │          │                        │
│ worker.js (Worker)    │ /relay/tok │ relay.rs (frame pump, limits)│ /file/id │ native <a download>    │
│  lcf.wasm  (lz4)      │═══LCF1════▶│   ├ decode lz4 → HTTP body   │═════════▶│   (streams to disk)    │
│  File.slice(256 KiB)  │            │   ├ folder → zip.rs stream   │          │                        │
│                       │            │   └ passthrough frames ──────│═LCF1════▶│ worker.js + lcf.wasm   │
└───────────────────────┘            └──────────────────────────────┘ /pull/id └────────────────────────┘
```

| Module | Responsibility |
|---|---|
| `crates/lcf` | LCF1 frame format + lz4 block codec (`lz4_flex`, `#![forbid(unsafe_code)]`) |
| `crates/lcf-wasm` | ~100 lines of C-ABI exports around `lcf` for the browser |
| `src/hub.rs` | Connected clients, unique-IP device count, chat broadcast, attachment registry, transfer tokens, auto-clear timer |
| `src/relay.rs` | Pumps one transfer: validates frames, enforces sizes, decodes or forwards, drives the zip writer |
| `src/zip.rs` | Streaming STORE/ZIP64 writer with an exact up-front length, and path sanitisation |
| `src/http.rs` | Routes, static assets, WS sessions, download/pull/relay handlers |
| `src/config.rs`, `src/main.rs` | clap CLI + Go-style argv normalisation, banner, terminal QR, browser opening |
| `web/` | Original vanilla JS UI (`script.js`) and transfer worker (`worker.js`) |

## 3. No persistence: where the data lives

The Go reference keeps uploaded files in a server-side map until the next clear.
Here the server holds **no content at all**:

- **Chat text** is relayed over `/ws` and rendered by each browser. The server
  keeps no history, so a device that joins later sees only new messages. That
  is the same as the reference (it also never replays chat).
- **Attachments** stay as `File` handles in the sharing tab (`owned: id → File`
  or folder entry list). The server registry maps `id → {owner connection,
  name, size, kind, count}`.
- **When the owner disconnects**, its ids move to a `gone` set. Everyone gets
  `{"type":"unavailable","ids":[…]}`, and later requests return `410 Gone`
  instead of `404`, so the UI can say "sender offline". Pending tokens of that
  owner are dropped, which fails waiting downloads immediately.
- **Clear** (manual or auto) wipes the registry and the `gone` set and
  broadcasts `clear`, then `config`, the same order as the reference.

Trade-off: a download needs the sender's tab to be open and awake. On a phone
the tab must stay in the foreground, because background tabs are throttled. We
accept this in exchange for zero server memory growth and "nothing is stored
on the host" privacy. Section 10 describes when to revisit it.

## 4. Transfer path: brokered relay instead of WebRTC

"Peer to peer" here means sender device → receiver device. It is implemented as
a **relay through the binary**, not as WebRTC data channels:

1. On plain `http://192.168.x.x` pages, browsers are **not in a secure context**. That
   rules out Service Workers and `showSaveFilePicker`, so a WebRTC receiver would have
   to collect the whole file in a JS `Blob` before saving. With a relay, the
   receiver uses a native `<a download>` that streams to disk with the browser's
   own progress UI and no size limit.
2. There is no ICE/STUN/TURN, no mDNS-candidate issues, and it works on Wi-Fi
   with client isolation, as long as clients can reach the server, which they
   already need to.
3. There is one code path for text and files, and it is easy to test in Rust.

Flow for `GET /file/{id}`:

1. The hub checks the registry, takes a per-owner concurrency slot
   (`--max-transfers-per-sender`, default 4, `429` when exhausted), creates a
   128-bit random **single-use token** bound to the owner's connection, and sends
   the owner only `{"type":"fileRequest","id","token","mode":"plain"}`.
2. The owner's worker opens **`WS /relay/{token}`** and streams LCF1 frames. It is
   a WebSocket and not a streaming `fetch` upload because request-body
   streaming needs HTTP/2 in Chrome, which plain-HTTP LAN servers don't offer,
   and Safari/Firefox don't support it at all. Without streaming uploads,
   on-the-fly compression of arbitrarily large files is impossible, while WS
   binary messages work everywhere.
3. `relay.rs` validates and decodes each frame and pushes the plain bytes into
   a **bounded channel (4 chunks)** that is the HTTP response body.
4. Guards:
   - The owner doesn't connect within `--transfer-timeout` → `504`, and the token is revoked.
   - The sender reports an error or disconnects mid-stream → the body errors, so the
     browser marks the download failed instead of saving a truncated file.
   - More bytes than announced, or fewer at END → abort.
   - The sender is silent for 60 s → abort.
   - A token that is unknown, reused or belongs to a disconnected owner → `404`.

### Backpressure chain

```
receiver disk/network slow → hyper stops polling body → bounded channel (4) full
→ relay stops reading /relay socket → TCP window closes → sender ws.bufferedAmount grows
→ worker waits while bufferedAmount > 4 MiB → stops calling File.slice().arrayBuffer()
```

Memory per transfer is therefore bounded:

- **Server:** about 4 × 1 MiB in the channel, plus socket buffers.
- **Sender:** about 4 MiB of socket buffer, plus two 256 KiB chunks (the one being sent and the prefetched one).
- **Receiver:** nothing, because the browser writes straight to disk.

## 5. The LCF1 frame format

Every WebSocket binary message on `/relay` and `/pull` is exactly one frame:

```
+---------+-----------------+---------------------+-----------+
| kind u8 | raw_len u32 LE  | payload_len u32 LE  | payload   |
+---------+-----------------+---------------------+-----------+
kind: 0 RAW · 1 LZ4 (lz4 block) · 2 MANIFEST (JSON) · 3 END · 4 ERROR (UTF-8 reason)
```

- **Each chunk is compressed independently** with the lz4 *block* format and
  no shared dictionary. A receiver never needs more than one chunk, any
  frame can be decoded on its own, and a bad frame affects only its chunk.
  Independent chunks cost a little ratio compared with one continuous lz4
  frame stream (the 64 KiB window restarts every 256 KiB), typically about 1–3 %.
- `raw_len` is capped at **1 MiB** (`MAX_CHUNK`) for data frames and 16 MiB for
  control frames. Decoding writes into a buffer of exactly `raw_len` bytes, and
  `lz4_flex` fails if the block would overflow it. This makes the format immune
  to decompression bombs, because the worst-case expansion per message is known before decoding.
- `payload_len` is checked against the lz4 worst-case bound for `raw_len`.
- A 9-byte header per 256 KiB chunk is 0.003 % overhead.
- We did not use the standard lz4 *frame* format (magic, block checksums,
  content size). It adds header parsing and xxHash code to the WASM module and
  gives nothing over TCP+WS, which already guarantee integrity in transit.
  Its linked-block mode would also prevent the "any frame decodes on its own"
  property.

### Adaptive compression

Compression is attempted only when it is likely to help:

| Rule | Where |
|---|---|
| Chunks < 64 bytes are never compressed | `lcf` |
| If lz4 saves < 10 %, the chunk is sent RAW (the receiver then pays nothing) | `lcf` |
| After 3 consecutive RAW results the rest of that file skips compression (reset per file) | `worker.js` |
| Known-compressed types are sent RAW from the start (jpg/png/webp/heic, video, most audio, zip/gz/7z/xz/zst, pdf, docx…) | `script.js` |
| Sender page on loopback and "plain" mode: no compression (the link is memory-speed) | `script.js` |
| `--transfer-compression off` disables it globally (advertised in `hello`) | server |

## 6. WebAssembly codec

The browser side of the codec is **the same Rust crate** (`crates/lcf`)
compiled to `wasm32-unknown-unknown`. It is wrapped by `crates/lcf-wasm`, which
exports a tiny C ABI:

```
lcf_alloc(len) → ptr            lcf_free(ptr, len)
lcf_max_chunk() → 1 MiB          lcf_max_frame_len(n)
lcf_encode(in, n, out, cap, try_compress) → frame len | <0
lcf_decode(in, n, out, cap)               → raw len   | <0
lcf_frame_kind(in, n)                     → kind      | <0
```

Design choices:

- **No wasm-bindgen and no generated JS glue.** The module has **zero imports** and is
  loaded with `WebAssembly.instantiate(bytes, {})`. That removes a CLI tool
  (`wasm-bindgen`/`wasm-pack`) from the build and keeps the build a single `cargo build`.
- **One source of truth.** Encoder and decoder bugs cannot diverge between
  browser and server. CI also runs `tests/wasm_compat.rs`, which runs the *real*
  `web/worker.js` under Node, has it decode Rust-encoded frames, and has Rust
  decode worker-encoded frames. It also checks that the worker's control frames
  are byte-identical to Rust's.
- **Constant memory.** The worker allocates one input and one output buffer
  (sized for `MAX_CHUNK`) inside WASM memory once and reuses them for every chunk.
  WASM memory therefore does not grow with file size. Views over
  `memory.buffer` are recreated after each call in case memory grew.
- **Off the UI thread.** All reading, encoding, decoding and socket I/O
  happens in a dedicated Web Worker. Workers are available on insecure origins.
- **Pipelining.** While chunk *n* is encoded and sent, `File.slice()` for chunk
  *n+1* is already being read, which hides disk and IPC latency.
- **Chunk size is 256 KiB.** It is large enough that per-call and per-message overheads
  (JS↔WASM copies, WS framing, syscalls) are negligible and lz4 has enough
  context. It is small enough to keep the UI progress smooth, keep backpressure
  granular and keep peak memory low. It is well under the 1 MiB protocol maximum.
- The module is embedded in the binary and served from `/lcf.wasm` with
  `application/wasm`. `build.rs` refuses to build the server if the WASM hasn't
  been built, so a release can never ship with a stale or empty codec.

### Where decoding happens

| Receiver | Path | Decoded by |
|---|---|---|
| Any browser, folders, files > `--browser-decode-max-mb` (64 MB), loopback pages | `GET /file/{id}` | Server (`lz4_flex`), then plain streamed HTTP to disk |
| LAN browser, file ≤ 64 MB | `WS /pull/{id}` | Receiver's worker (WASM). Frames pass through the server still compressed |

A browser's native download path cannot decode lz4, because there is no lz4
`Content-Encoding`. JS-side decoding needs the data collected in a `Blob`, since
Service Workers need a secure context. So we do end-to-end compression (both LAN
hops compressed, the server only validates frame headers) for files small
enough to hold in memory. For everything else we use server-side decode with
true streaming to disk. If WASM decoding fails for any reason other than the
sender being offline, the UI falls back to the native download.

## 7. Compression algorithm: lz4 vs zstd

The question was which codec balances CPU overhead, compression ratio and
throughput for chat messages and attachments.

### The numbers that matter

Typical single-core figures (lzbench-class measurements on a modern x86
desktop. Phones are about 2–4× slower, WASM about 1.3–2× slower than native):

| Codec | Compress | Decompress | Ratio (mixed text/binary) |
|---|---|---|---|
| lz4 (fast/block) | ~600–800 MB/s | ~3–4 GB/s | ~2.1× |
| zstd -1 | ~450–500 MB/s | ~1.3–1.6 GB/s | ~2.8× |
| zstd -3 | ~300–350 MB/s | ~1.2–1.5 GB/s | ~3.1× |
| zstd -9 | ~60–90 MB/s | ~1.3 GB/s | ~3.4× |
| gzip -6 | ~30–50 MB/s | ~300–400 MB/s | ~3.0× |

For already-compressed media (JPEG, HEIC, MP4, ZIP), which is most of what people
share from phones, every codec gets about 1.0×. The only question there is how
cheaply you *give up*.

### Where the CPU runs

- **Compression runs on the sender**, which is often a phone, inside WASM, in a
  browser worker. At WASM-on-phone speeds, lz4 still manages about 100–250 MB/s,
  comfortably above what phone Wi-Fi uploads sustain (roughly 20–80 MB/s). zstd-3
  drops to about 40–100 MB/s there, which is close to or *below* link speed, so it
  would become the bottleneck and the transfer would get *slower* than sending
  uncompressed.
- **Decompression runs on the server or the receiver.** Both codecs are fast
  there, but lz4 is 2–3× faster and uses a fixed, tiny amount of state.

### Why lz4 wins here

1. **Throughput dominates.** Nothing is stored anywhere, so a better ratio saves
   no memory or disk. It only saves link time, and only when the codec is
   faster than the link. lz4 is faster than any realistic LAN uplink on every
   client. zstd is not.
2. **Incompressible data is cheap.** lz4 finds out that a JPEG chunk won't
   compress at several hundred MB/s. The 10 % rule plus the "3 RAW chunks → stop"
   rule keep the cost of trying close to zero.
3. **Toolchain and size.** `lz4_flex` is pure Rust. It compiles to a small
   WASM module with the stock `wasm32-unknown-unknown` target and nothing else.
   zstd in WASM means the C library (emscripten or wasi-sdk/clang in CI, a
   much larger module) or a pure-Rust encoder that is slower and less mature
   than the C one. Pure Rust also keeps the static musl/Windows builds trivial.
4. **Chat text is not compressed at all.** Messages are at most a few KB. WS framing
   and TCP dominate, and neither codec would pay for its own overhead.

### When zstd would be the right choice

- **If attachments were stored on the server again** (the first design in this
  project kept files in RAM for the auto-clear period). Then compression
  happens once on a desktop-class CPU, memory is the scarce resource, and
  **zstd -3** gives about 30 % smaller RAM use at acceptable CPU cost. That was the
  recommendation for that design, and it was dropped when persistence was.
- **Slow links** (remote access over a VPN or the internet). There ratio matters more
  than CPU. A future `--transfer-compression zstd` could use a zstd WASM build,
  and the LCF1 `kind` byte leaves room for it.

## 8. Folders

- **Selection:** `<input webkitdirectory>` (📁 button), or drag and drop through
  `DataTransferItem.webkitGetAsEntry()`, which walks directories recursively
  (reading `readEntries` until it returns an empty batch) and records
  empty directories. Each top-level folder becomes one attachment
  (`kind:"dir"`, total size, file count).
- **Wire:** the first frame is `MANIFEST`, a JSON list of
  `{path,size,mtime}` / `{path,dir:true}`. It is followed by one continuous data
  stream of all files in manifest order, where chunks may straddle file
  boundaries. The server splits the stream using the declared sizes. Each file is
  still read in 256 KiB slices, so memory stays constant for folders of any size.
- **Output:** a streaming ZIP (`src/zip.rs`, about 400 lines, written in-house because
  no crate combines streaming, exact up-front length and ZIP64 with a pull-based API):
  - **STORE** method. The data already crossed the network lz4-compressed, and
    the server→receiver hop is LAN. Deflate would spend server CPU (around 50 MB/s)
    to shrink data that is often already compressed.
  - Because STORE output size is determined by names and sizes alone, the
    **exact `Content-Length` is known before the first byte**. Browsers then show
    real progress and ETA, and truncation is detectable.
  - The CRC-32 (`crc32fast`, SIMD) is computed on the fly and written in a data
    descriptor, like Go's `archive/zip` streaming writer.
  - ZIP64 extra fields and the ZIP64 end records are emitted only when needed
    (a file ≥ 4 GiB, an offset ≥ 4 GiB, or ≥ 65 535 entries). The unit tests check that
    the planned length equals the bytes written, including forced ZIP64 layouts.
  - UTF-8 name flag and DOS timestamps from `mtime`.
- **Safety:** paths are normalised (`\` → `/`). Absolute paths, drive letters,
  `..`, empty or NUL-containing components, duplicates, over-long names,
  more than 100 000 entries and data sent for a directory entry are all rejected.
  A malicious sender cannot produce a zip-slip archive.

## 9. Hub and concurrency

- The Go reference uses one goroutine that owns all state and talks to it over
  channels. Here the state is a **`std::sync::Mutex<Inner>` that is never held
  across an `.await`**. Every operation is short and synchronous, which is simpler
  than an actor, has no queueing latency and has no possibility of a stalled actor.
- **Every client has a bounded outbound queue** (256 messages), filled with
  `try_send`. A client that stops reading is disconnected instead of stalling
  broadcasts to everyone else. The reference has an equivalent buffered-channel
  drop.
- **Device count** is the number of unique client IPs, as in the reference. The IP is
  taken from the first `X-Forwarded-For` entry, then `X-Real-IP`, then the peer address.
- **Auto-clear timer:** a task sleeps until `deadline` or until a `Notify` wakes it.
  Every reschedule bumps a generation counter, so a stale wake-up can never clear
  early. The wall-clock `nextClearTime` (RFC 3339, UTC) is sent to clients for
  the countdown. Tests shorten the "minute" unit to make timer tests fast.
- **Message ids** are assigned by the server (hex millis + counter). The client
  sends a `ref`, and the server echoes it, so the sender can mark its own messages
  and bind the id to its local `File` handle. That is required because the server,
  not the sender, must name the attachment it will later request.

## 10. Differences from the reference

| Area | Reference (Go) | This port |
|---|---|---|
| File storage | Server RAM until clear | None. Streamed from the sender on demand |
| Upload | `POST /upload` multipart | Removed (no server storage). `WS /relay/{token}` instead |
| Folders | No | Yes, delivered as a streamed ZIP |
| Compression | None | lz4 per 256 KiB chunk, WASM in the browser |
| QR endpoint | PNG | SVG (no image crate, crisper on HiDPI screens) |
| `Content-Disposition` | Raw filename | RFC 5987 (`filename*=UTF-8''…`) plus an ASCII fallback |
| Message ids | Client-generated | Server-generated, `ref` echoed |
| Slow clients | Buffered channel | Bounded queue, slow client dropped |
| CLI | `-port`, `-open` | Same (Go syntax is accepted) plus `--bind`, `--auto-clear`, transfer tuning |
| Web UI | Reference HTML/CSS/JS | Original implementation (the reference has no license) |

## 11. Testing strategy

Everything runs in GitHub Actions (`.github/workflows/ci.yml`). No local toolchain
is required.

| Layer | What | Where |
|---|---|---|
| Codec | Frame parse/validation, bomb guards, RAW fallback, round trips | `crates/lcf` unit tests |
| WASM ABI | Export round trip | `crates/lcf-wasm` |
| Server units | argv normalisation, IP precedence, Content-Disposition, RFC 3339/DOS time, ZIP exact length, ZIP64, path sanitising, hub limits/tokens/timer | `src/*` unit tests |
| Integration | Real server on an ephemeral port. tokio-tungstenite clients act as browsers: chat, device count, config/clear/pause, 405/400/404/410/429/504, single-file relay (mixed RAW/LZ4), `/pull` passthrough, folder → ZIP verified with the `zip` crate, mid-transfer abort, oversize stream | `tests/api.rs`, `tests/relay.rs` |
| Browser codec | Real `worker.js` + `lcf.wasm` under Node vs the Rust `lcf` crate, in both directions | `tests/wasm_compat.rs` + `tests/js/wasm_compat.mjs` |
| End to end | Chromium via Playwright against the release Linux binary: text, files over LAN (WASM decode) and loopback (streamed), folder → zip checked with `unzip`, sender leaves → *Unavailable*/410 | `e2e/` |
| Platforms | fmt, clippy `-D warnings`, tests on Linux/Windows/macOS. Release builds for 4 targets | CI matrix |

## 12. Known limitations and future work

- The sender tab must stay open and in the foreground on mobile. A future
  opt-in `--store-attachments` mode could bring back server-side buffering,
  where zstd-3 would be the right codec (section 7).
- iOS Safari has no folder picker (`webkitdirectory`).
- No authentication or TLS, like the reference. The target is trusted LANs. A
  `--token` query parameter and a self-signed TLS mode would also unlock secure-context APIs
  (Service Worker streaming decode, `showSaveFilePicker`) for the receiving side.
- Receiver-leg compression for large files could use standard
  `Content-Encoding: zstd`/`gzip` negotiated with the browser, at the cost of
  losing the exact `Content-Length`.
