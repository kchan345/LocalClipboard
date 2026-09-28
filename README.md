# LocalClipboard

Share text, files **and folders** between devices on your local network with a
single executable. Run it on any computer, open the page on your phone or laptop
(or scan the QR code), and everything you paste or drop shows up on every
connected device.

This is a Rust rewrite of
[MoKhajavi75/local-clipboard](https://github.com/MoKhajavi75/local-clipboard)
with the same features, plus:

- **Zero server storage.** Chat text lives only in the open browser tabs.
  Attachments never leave the sharing device until someone clicks
  *Download*. They are then streamed on demand from the sender's browser
  through the server to the receiver.
- **On-the-fly lz4 compression** of every transfer. The browser compresses
  and decompresses 256 KiB chunks in a **WebAssembly module** compiled from the
  same Rust codec the server uses. Chunking keeps memory flat for files of
  any size.
- **Folder attachments.** Use the 📁 button or drag and drop a folder. Receivers
  get a streamed `.zip` with an exact size, so the browser shows real
  progress.
- **One file, no runtime.** The web UI and the WASM codec are embedded in
  the binary.

## Download

Grab a binary from the
[Releases page](https://github.com/kchan345/LocalClipboard/releases), or the latest
CI build from the [Actions tab](https://github.com/kchan345/LocalClipboard/actions)
(artifacts of the *CI* workflow):

| Platform | File |
|---|---|
| Windows x64 | `local-clipboard-windows-x64.exe` (static CRT) |
| Linux x64 | `local-clipboard-linux-x64` (static musl, runs on any distro) |
| macOS Apple Silicon | `local-clipboard-macos-arm64` |
| macOS Intel | `local-clipboard-macos-x64` |

On macOS/Linux run `chmod +x local-clipboard-*` first. macOS may ask you to
allow the unsigned binary under *System Settings → Privacy & Security*.

## Usage

```sh
local-clipboard                 # listen on 0.0.0.0:8080 and open the browser
local-clipboard --port 3000     # another port (Go-style "-port 3000" also works)
local-clipboard --no-open       # don't open a browser (also: -open=false)
```

On startup it prints the addresses and a QR code in the terminal:

```
📋 Local Clipboard v0.1.0
Server listening on 0.0.0.0:8080
Open http://localhost:8080 on this computer
Open http://192.168.1.23:8080 on your phone, or scan:
 █▀▀▀▀▀█ ...
```

Open the LAN URL on the other devices. All devices must be on the same
network, and the OS firewall must allow incoming connections on the port.

### In the browser

- **Send text.** Type, then press **Enter** (**Shift+Enter** adds a new line). Every message has a
  **Copy** button. Right-to-left text is detected automatically.
- **Share files.** Click 📎, or drop files anywhere on the page. Several files can be queued
  at once.
- **Share folders.** Click 📁, or drop folders (mixed with files if you like). Empty
  sub-folders are kept.
- **Download.** Click **Download** on an attachment. Folders arrive as `<name>.zip`.
- **Sender offline.** When the sharing tab is closed or loses its connection, its attachments
  show **Unavailable** on every device.
- **Auto-clear.** Messages on all devices are wiped after 1 min to 2 hours, or never.
  You can pause the timer, or press **Clear** to wipe immediately.
- The status bar shows how many devices are connected (unique IP addresses).
  A banner appears when a newer release is available.

### Options

| Flag | Default | Description |
|---|---|---|
| `-p, --port <PORT>` | `8080` | Port to listen on |
| `--bind <ADDR>` | `0.0.0.0` | Address to bind (e.g. `127.0.0.1`, `::`) |
| `--open[=<bool>]` / `--no-open` | open | Open the default browser at startup |
| `--auto-clear <MIN>` | `10` | Initial auto-clear interval in minutes (`0` = never) |
| `--transfer-compression <auto\|off>` | `auto` | lz4 compression of transfers |
| `--browser-decode-max-mb <MB>` | `64` | Files up to this size are sent to LAN receivers still compressed and decoded in their browser (WASM). Larger files are decoded by the server and streamed straight to disk |
| `--max-transfers-per-sender <N>` | `4` | Concurrent downloads one sending device serves |
| `--transfer-timeout <SEC>` | `30` | How long a download waits for the sending tab to respond |
| `-V, --version` / `-h, --help` | | |

| Environment variable | Effect |
|---|---|
| `LOCAL_CLIPBOARD_NO_OPEN=1` | Never open a browser (useful for services and containers) |
| `LOCAL_CLIPBOARD_HOST=<host>` | Host name/IP advertised in the banner and QR code (e.g. behind NAT or a proxy) |

The browser is also not opened inside Docker (`/.dockerenv`) or on Linux
without `$DISPLAY`/`$WAYLAND_DISPLAY`.

### HTTP API

| Method | Path | Description |
|---|---|---|
| GET | `/` `/styles.css` `/script.js` `/worker.js` `/lcf.wasm` | Embedded web app |
| GET | `/api/version` | Version string |
| GET | `/qr` | QR code (SVG) for the LAN URL |
| GET (WS) | `/ws` | Chat and control channel |
| GET | `/file/{id}` | Download an attachment (the sender streams it on demand). Returns `404` if unknown, `410` if the sender is offline, `429` if the sender is busy, `504` if the sender didn't respond |
| GET (WS) | `/pull/{id}` | Same, but delivered as compressed LCF1 frames for in-browser decoding |
| GET (WS) | `/relay/{token}` | Used by the sending browser to upload a requested attachment |
| POST | `/clear` | Clear all messages now |
| POST | `/set-interval` | `{"interval": <minutes>}` (`0` = never) |
| POST | `/toggle-pause` | Pause or resume the auto-clear timer |

### Behind a reverse proxy

WebSocket upgrades must be forwarded on `/ws`, `/pull/` and `/relay/`. Response
buffering must be off for `/file/` so downloads stream (nginx:
`proxy_buffering off;`). The client IP is taken from `X-Forwarded-For`, then
`X-Real-IP`. Set `LOCAL_CLIPBOARD_HOST` to the public name for the QR code.

## Limitations

- **The sharing tab must stay open.** Attachments are served from the sender's
  browser. Mobile browsers may suspend a background tab, so on a phone keep the
  page in front while others download.
- **iOS Safari cannot pick folders.** Share files instead, or zip the folder first.
- **Files that change after being shared** abort the transfer (the byte count no
  longer matches). The receiver sees a failed download and can retry.
- **No authentication or TLS.** It is meant for trusted home and office networks,
  exactly like the original. Anyone who can reach the port can read and post.

## Building

You only need a Rust toolchain. The CI does all of this for you.

```sh
rustup target add wasm32-unknown-unknown
sh scripts/build-wasm.sh          # builds crates/lcf-wasm → web/lcf.wasm
cargo build --release             # embeds web/* into target/release/local-clipboard
cargo test --workspace            # unit + integration tests (node needed for the WASM compat test)
cd e2e && npm i && npx playwright install chromium && npx playwright test   # browser tests
```

`web/lcf.wasm` is a build output and is not committed. `build.rs` stops with
instructions if it is missing.

- **CI** (`.github/workflows/ci.yml`) builds the WASM codec. It then runs rustfmt, clippy and
  all tests on Linux, Windows and macOS. It builds the four release binaries
  (uploaded as artifacts) and runs the Playwright tests against the Linux binary.
- **Release** (`.github/workflows/release.yml`) runs when a `v*` tag is pushed. It builds the
  binaries with the tag as the version and attaches them to a GitHub release together with
  `SHA256SUMS.txt`.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the design, the transfer protocol and the
lz4-versus-zstd trade-offs.

## License

MIT. The web UI is an original implementation. No code or assets were copied from the
reference project.
