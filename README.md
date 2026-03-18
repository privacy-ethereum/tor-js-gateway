# tor-js-gateway

Long-running Tor directory cache daemon that syncs consensus documents, authority certificates, and microdescriptors directly from the Tor network using the directory protocol. Serves a pre-built bootstrap archive over HTTP for fast client bootstrapping.

## How it works

The daemon uses [Arti](https://gitlab.torproject.org/tpo/core/arti) to connect to Tor directory authorities via BEGINDIR streams. It follows the relay-style sync schedule from [dir-spec §5.3](https://spec.torproject.org/dir-spec/directory-cache-operation.html#download-ns-from-auth), fetching a new consensus shortly after the current one stops being fresh.

Each sync cycle:

1. Opens a dedicated directory circuit (retired immediately so it's never reused by other code)
2. Fetches the microdescriptor consensus (requesting a diff via `X-Or-Diff-From-Consensus` if a previous consensus is cached)
3. Fetches authority certificates (only if any trusted authority is missing a valid cert)
4. Verifies the consensus (timeliness + authority signatures)
5. Fetches only missing microdescriptors in batches of 500
6. Writes all files atomically to the output directory
7. Builds a `bootstrap.zip` archive with pre-compressed brotli and gzip variants

## Building

Requires Rust 1.89+. Arti dependencies are pulled from [crates.io](https://crates.io/crates/arti-client).

```
cargo build --release
```

### Docker

```
# needs a decent amount of memory
docker build --network=host -t tor-js-gateway .

# can run on a small machine, but requires transfer from build server
docker run --network=host tor-js-gateway
```

For production, run detached with restart policy and log retention:

```
docker run -d --restart unless-stopped \
  --log-opt max-size=10m --log-opt max-file=3 \
  --network=host --name tor-js-gateway \
  tor-js-gateway
```

`--network=host` is needed at build time so the builder can fetch crates, and at run time so the daemon can reach Tor directory authorities. If your Docker bridge network has working outbound connectivity, you can use `-p 42298:42298` instead of `--network=host` at run time.

## Usage

```
tor-js-gateway --output-dir ./data
```

### CLI flags

| Flag | Default | Description |
|---|---|---|
| `-o, --output-dir` | (required) | Directory for cached documents and bootstrap archive |
| `-p, --port` | `42298` | HTTP server port (`0` to disable) |
| `--once` | off | Exit after the first successful sync instead of looping |
| `--allow-uncompressed` | off | Serve uncompressed `/bootstrap.zip` (production should use `/bootstrap.zip.br`) |

### Environment

Set `RUST_LOG` to control log verbosity (default: `info`). Example:

```
RUST_LOG=debug tor-js-gateway -o ./data
```

## Web UI

Navigate to `http://localhost:42298/` to open the built-in web interface. Click **Bootstrap** to download and inspect the current bootstrap archive directly in your browser.

## HTTP endpoints

| Path | Content-Type | Description |
|---|---|---|
| `/` | `text/html` | Web UI |
| `/metadata.json` | `application/json` | Sync metadata (brotli/gzip/identity) |
| `/bootstrap.zip` | `application/zip` | Bootstrap archive (brotli/gzip/identity) |
| `/bootstrap.zip.br` | `application/zip` or `application/octet-stream` | Brotli archive — transparent decoding if client accepts `br`, raw bytes otherwise |
| `/torJsGateway.js` | `text/javascript` | ES module: download, decompress, and parse bootstrap archives |

Data endpoints return `503 Service Unavailable` before the first successful sync.

The server negotiates `Accept-Encoding` and serves pre-compressed `.br` or `.gz` variants from disk — no on-the-fly compression.

Bootstrap endpoints (`/bootstrap.zip` and `/bootstrap.zip.br`) return an `ETag` header derived from a SHA3-256 hash of the archive. Clients can send `If-None-Match` to receive a `304 Not Modified` response when the content hasn't changed.

## Output files

After a successful sync, the output directory contains:

| File | Description |
|---|---|
| `consensus-microdesc.txt` | Current microdescriptor consensus |
| `authority-certs.txt` | Trusted authority certificates |
| `microdescs.txt` | Concatenated microdescriptors for all relays in the consensus |
| `metadata.json` | Sync metadata (consensus lifetime, relay count, file sizes, sync timestamp) |
| `bootstrap.zip` | Uncompressed zip archive of the three `.txt` files (stored, no zip-level compression) |
| `bootstrap.zip.br` | Brotli-compressed bootstrap.zip (quality 6) |
| `bootstrap.zip.gz` | Gzip-compressed bootstrap.zip |

The zip archive uses `Stored` compression (no deflate) since the outer brotli/gzip layer handles compression. Files inside the archive are under a `bootstrap/` prefix.

All files are written atomically via a `.tmp` intermediate to avoid serving partial data.
