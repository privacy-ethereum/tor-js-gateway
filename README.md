# tor-js-gateway

Gateway server for [tor-js](https://github.com/voltrevo/tor-js) — enabling real Tor circuits built locally in the browser using WebAssembly. All cryptography runs on the client. The gateway never sees your traffic or knows your destination — it just relays encrypted bytes to Tor relays.

Built with [Arti](https://gitlab.torproject.org/tpo/core/arti), the Rust Tor implementation.

## Features

- **Fast Bootstrap** — Serves the consensus, authority certificates, and microdescriptors as a single brotli-compressed archive. The browser decompresses it natively in one fetch — no multi-step directory protocol, no multiple round trips to authorities.
- **WebSocket Relay** — Bridges browser connections to raw TCP sockets on the Tor network. The client builds circuits and negotiates keys — the gateway only forwards opaque, encrypted data. Only consensus-advertised relay addresses are allowed.
- **WebRTC Relay** — Data channel transport as an alternative to WebSocket. Harder to fingerprint and block — looks like regular video call traffic. Uses a signaling channel for connection management, ping/pong, and error reporting.
- **Unified Client Library** — `torJsGateway.js` exports a `Gateway` class that auto-selects the best transport (direct TCP in Node/Deno, WebRTC, or WebSocket) with fallback.

## Quick start

Requires Rust 1.89+.

```
cargo install --path .
tor-js-gateway init
tor-js-gateway
```

This creates a config at `~/.config/tor-js-gateway/config.json5` with sensible defaults and starts the server. Data (consensus, bootstrap archives) is stored in `~/.local/share/tor-js-gateway/`.

### Install as a service

```
tor-js-gateway install
```

This writes a systemd user unit, enables it, and starts it. The service starts on boot and restarts on failure. Manage with standard systemd commands:

```
systemctl --user status tor-js-gateway
systemctl --user restart tor-js-gateway
journalctl --user -u tor-js-gateway -f
```

To remove:

```
tor-js-gateway uninstall
```

## Configuration

Config is stored as JSON5 (supports comments and trailing commas) at `~/.config/tor-js-gateway/config.json5`. All fields are required.

```json5
{
  // Directory for cached consensus data and bootstrap archives
  "data_dir": "~/.local/share/tor-js-gateway",

  // HTTP server port (0 to disable)
  "port": 42298,

  // Serve uncompressed /bootstrap.zip
  "allow_uncompressed": false,

  // Max concurrent WebSocket relay connections
  "ws_max_connections": 8192,

  // Max WebSocket relay connections per client IP
  "ws_per_ip_limit": 16,

  // WebSocket relay idle timeout in seconds
  "ws_idle_timeout": 300,

  // WebSocket relay max connection lifetime in seconds
  "ws_max_lifetime": 3600,

  // UDP port for WebRTC data channel relay (0 to disable)
  "webrtc_port": 42299,
}
```

Use `tor-js-gateway show-default-config` to print defaults, or `tor-js-gateway show-config` to print the current effective config. A custom config path can be specified with `-c`:

```
tor-js-gateway -c /path/to/config.json5
```

### Environment

Set `RUST_LOG` to control log verbosity (default: `info`):

```
RUST_LOG=debug tor-js-gateway
```

## CLI

```
tor-js-gateway [OPTIONS] [COMMAND]
```

| Command | Description |
|---|---|
| `run` | Run the server in the foreground (default) |
| `init` | Create a default config file |
| `show-config` | Print the current config from disk |
| `show-default-config` | Print the hardcoded default config |
| `install` | Install and start a systemd user service |
| `uninstall` | Stop and remove the systemd user service |

| Option | Description |
|---|---|
| `-c, --config <PATH>` | Config file path (default: `~/.config/tor-js-gateway/config.json5`) |
| `run --once` | Exit after the first successful sync |

## How sync works

The daemon connects to Tor directory authorities via BEGINDIR streams, following the relay-style sync schedule from [dir-spec §5.3](https://spec.torproject.org/dir-spec/directory-cache-operation.html#download-ns-from-auth).

Each sync cycle:

1. Opens a dedicated directory circuit (retired immediately so it's never reused)
2. Fetches the microdescriptor consensus (requesting a diff if a previous consensus is cached)
3. Fetches authority certificates (only if coverage is incomplete)
4. Verifies the consensus (timeliness + authority signatures)
5. Fetches only missing microdescriptors in batches of 500
6. Updates the relay allowlist for the WebSocket proxy
7. Writes all files atomically to the data directory
8. Builds `bootstrap.zip` with pre-compressed brotli and gzip variants

## HTTP endpoints

| Path | Description |
|---|---|
| `/` | Landing page |
| `/bootstrap` | Bootstrap inspector — download and explore the consensus interactively |
| `/connect` | Relay connection tester — manual testing of WebSocket and WebRTC transports |
| `/metadata.json` | Sync metadata (consensus lifetime, relay count, timestamps) |
| `/bootstrap.zip.br` | Brotli bootstrap archive (transparent decoding if client accepts `br`) |
| `/bootstrap.zip` | Uncompressed bootstrap archive (disabled by default) |
| `/torJsGateway.js` | Client library — bootstrap, `Gateway` class, relay sockets |
| `/socket/{ip}:{port}` | WebSocket-to-TCP relay (consensus relays only) |
| `/rtc/connect` | WebRTC signaling — POST SDP offer, receive SDP answer |
| `/relay/random` | Random relay address from the consensus (IPv4 only if no IPv6) |

Data endpoints return `503` before the first successful sync. The server negotiates `Accept-Encoding` and serves pre-compressed `.br` or `.gz` variants from disk. Bootstrap endpoints support `ETag`/`If-None-Match` for 304 responses.

## Relay transports

Both WebSocket and WebRTC relay connections are restricted to:

- Addresses advertised in the current Tor consensus (exact IP:port match)
- Non-local IPs (private/loopback/link-local rejected as defence-in-depth)
- IPv4 only on servers without IPv6 connectivity (auto-detected at startup)

Limits (shared across both transports, configurable via config file):

| Limit | Default | Description |
|---|---|---|
| `ws_max_connections` | 8192 | Global concurrent connection cap |
| `ws_per_ip_limit` | 16 | Per client IP |
| `ws_idle_timeout` | 300s | Closed if no data flows in either direction |
| `ws_max_lifetime` | 3600s | Hard cutoff per connection |

### WebSocket

`/socket/{ip}:{port}` upgrades to a WebSocket and relays binary messages bidirectionally to the target TCP address.

### WebRTC

`POST /rtc/connect` accepts an SDP offer and returns an SDP answer. The browser then opens data channels labeled with the target `ip:port`. Each data channel is bridged to a TCP connection on the server.

A `_signal` data channel provides connection management:
- Server sends `{"type":"hello"}` on open with server info
- Client can send `{"type":"ping","ts":...}`, server responds with `{"type":"pong","ts":...}`
- Server sends `{"type":"rejected","channel":"ip:port","reason":"..."}` when a relay channel is denied

WebRTC traffic uses a separate UDP port (`webrtc_port`, default 42299). A single peer connection multiplexes all relay channels for a client.

## Client library

`/torJsGateway.js` is an ES module that works in browsers, Node.js, and Deno.

```js
import { Gateway, bootstrap } from 'https://your-gateway/torJsGateway.js';

// Bootstrap
const { consensus, microdescs, authcerts } = await bootstrap(
  'https://your-gateway/bootstrap.zip.br',
);

// Connect to a relay (auto-selects best transport)
const gw = new Gateway('https://your-gateway');
const sock = await gw.connect('198.51.100.1:9001');
sock.send(new Uint8Array([0x00, 0x07, ...]));
sock.onmessage = (data) => { /* Uint8Array */ };
sock.onclose = () => {};

// Force a specific transport
const wsGw = new Gateway('https://your-gateway', {
  strategies: ['websocket'],
});
```

Transport strategies tried in order (configurable via `strategies` option):
- **`direct`** — Raw TCP (Node.js/Deno only, no gateway involved)
- **`webrtc`** — WebRTC data channels (browsers with `RTCPeerConnection`)
- **`websocket`** — WebSocket relay (universal fallback)

## Data files

After a successful sync, the data directory contains:

| File | Description |
|---|---|
| `consensus-microdesc.txt` | Current microdescriptor consensus |
| `authority-certs.txt` | Trusted authority certificates |
| `microdescs.txt` | Concatenated microdescriptors |
| `metadata.json` | Sync metadata |
| `bootstrap.zip` | Uncompressed zip of the above `.txt` files |
| `bootstrap.zip.br` | Brotli-compressed (quality 6) |
| `bootstrap.zip.gz` | Gzip-compressed |
| `bootstrap.etag` | SHA3-256 hash for ETag |

All files are written atomically via `.tmp` intermediates.

## Docker

```
docker build --network=host -t tor-js-gateway .
docker run --network=host tor-js-gateway
```

`--network=host` is needed at build time for fetching crates, and at run time for reaching Tor directory authorities. Use `-p 42298:42298 -p 42299:42299/udp` instead if your Docker bridge has working outbound connectivity.
