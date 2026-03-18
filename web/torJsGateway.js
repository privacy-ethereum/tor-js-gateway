/**
 * tor-js-gateway client library.
 *
 * All functions accept an optional onEvent callback for instrumentation.
 * Events are plain objects with a `type` string and relevant data fields.
 */

/**
 * Download bootstrap.zip from a tor-js-gateway server.
 *
 * Events emitted:
 * - { type: "fetch-start" }
 * - { type: "fetch-progress", loaded: number, total: number | undefined }
 * - { type: "fetch-done", bytes: number }
 * - { type: "decompress-start" }  (only if manual brotli needed)
 * - { type: "decompress-progress", loaded: number, total: number | undefined }
 * - { type: "decompress-done", method: "transparent" | "wasm", bytes: number }
 *
 * The server sends an `X-Decompressed-Content-Length` header with the
 * uncompressed zip size. When the browser handles brotli transparently,
 * the stream delivers decompressed bytes, so we use this header as the
 * progress total instead of `Content-Length` (which reflects compressed size).
 *
 * When the browser does NOT handle brotli, we stream-decompress via WASM,
 * emitting both fetch-progress (compressed) and decompress-progress
 * (decompressed) events simultaneously.
 *
 * @param {string} url - The bootstrap.zip.br endpoint URL.
 * @param {function} [onEvent] - Optional event callback.
 * @returns {Promise<Uint8Array>} The decompressed zip bytes.
 */
export async function smartBootstrapDownload(url, onEvent) {
  onEvent?.({ type: "fetch-start" });

  const res = await fetch(url);
  if (!res.ok) {
    throw new Error(`fetch failed: ${res.status} ${res.statusText}`);
  }

  const contentType = res.headers.get("content-type") || "";
  const contentLen = res.headers.get("content-length");
  const decompressedLen = res.headers.get("x-decompressed-content-length");

  if (contentType.includes("application/zip")) {
    // Transparent path — browser decompressed brotli
    const total = decompressedLen
      ? parseInt(decompressedLen, 10)
      : contentLen
        ? parseInt(contentLen, 10)
        : undefined;
    const bytes = await readResponseWithProgress(res, total, onEvent, { transparent: true });
    const wireBytes = contentLen ? parseInt(contentLen, 10) : bytes.byteLength;
    onEvent?.({ type: "fetch-done", bytes: wireBytes, transparent: true });
    onEvent?.({
      type: "decompress-done",
      method: "transparent",
      bytes: bytes.byteLength,
    });
    return bytes;
  }

  // WASM streaming path
  return streamDecompress(res, contentLen, decompressedLen, onEvent);
}

/**
 * Parse a bootstrap zip archive into its constituent documents.
 *
 * The zip uses Stored compression (no deflate), so we parse the
 * local file headers directly without a decompression library.
 *
 * Events emitted:
 * - { type: "parse-done", consensus: string, microdescs: string[], authcerts: string[] }
 *
 * @param {Uint8Array} zip - The raw zip bytes.
 * @param {function} [onEvent] - Optional event callback.
 * @returns {{ consensus: string, microdescs: string[], authcerts: string[] }}
 */
export function parseBootstrapZip(zip, onEvent) {
  const view = new DataView(zip.buffer, zip.byteOffset, zip.byteLength);
  const decoder = new TextDecoder();
  const files = {};

  let offset = 0;
  while (offset + 30 <= zip.byteLength) {
    const sig = view.getUint32(offset, true);
    if (sig !== 0x04034b50) break;

    const method = view.getUint16(offset + 8, true);
    if (method !== 0) {
      throw new Error(
        `unsupported compression method ${method}, expected Stored (0)`,
      );
    }

    const compressedSize = view.getUint32(offset + 18, true);
    const nameLen = view.getUint16(offset + 26, true);
    const extraLen = view.getUint16(offset + 28, true);
    const name = decoder.decode(
      zip.subarray(offset + 30, offset + 30 + nameLen),
    );
    const dataStart = offset + 30 + nameLen + extraLen;
    const data = zip.subarray(dataStart, dataStart + compressedSize);

    files[name] = decoder.decode(data);
    offset = dataStart + compressedSize;
  }

  const consensus = files["bootstrap/consensus-microdesc.txt"];
  const microdescBlob = files["bootstrap/microdescs.txt"];
  const authcertBlob = files["bootstrap/authority-certs.txt"];

  if (!consensus) {
    throw new Error("missing bootstrap/consensus-microdesc.txt in zip");
  }

  const result = {
    consensus,
    microdescs: splitDocuments(microdescBlob || "", "onion-key\n"),
    authcerts: splitDocuments(authcertBlob || "", "dir-key-certificate-version "),
  };

  onEvent?.({ type: "parse-done", ...result });
  return result;
}

/**
 * Download, decompress, and parse a bootstrap archive in one call.
 *
 * Combines smartBootstrapDownload + parseBootstrapZip, forwarding all
 * events from both, plus a final { type: "done" } event.
 *
 * @param {string} url - The bootstrap.zip.br endpoint URL.
 * @param {function} [onEvent] - Optional event callback.
 * @returns {Promise<{ consensus: string, microdescs: string[], authcerts: string[] }>}
 */
export async function bootstrap(url, onEvent) {
  const zipBytes = await smartBootstrapDownload(url, onEvent);
  const result = parseBootstrapZip(zipBytes, onEvent);
  onEvent?.({ type: "done" });
  return result;
}

/**
 * Stream-download and decompress brotli via WASM, emitting both
 * fetch-progress (compressed) and decompress-progress (decompressed)
 * events simultaneously as chunks arrive.
 */
async function streamDecompress(res, contentLen, decompressedLen, onEvent) {
  const { default: init } = await import(
    "https://cdn.jsdelivr.net/npm/brotli-wasm@3.0.1/index.web.js",
  );
  const brotli = await init;

  const compressedTotal = contentLen ? parseInt(contentLen, 10) : undefined;
  const decompressedTotal = decompressedLen
    ? parseInt(decompressedLen, 10)
    : undefined;

  if (!res.body) {
    // Fallback: no ReadableStream
    const buf = await res.arrayBuffer();
    const bytes = new Uint8Array(buf);
    onEvent?.({
      type: "fetch-progress",
      loaded: buf.byteLength,
      total: compressedTotal,
    });
    onEvent?.({ type: "fetch-done", bytes: buf.byteLength });
    onEvent?.({ type: "decompress-start" });
    const decompressed = brotli.decompress(bytes);
    onEvent?.({
      type: "decompress-done",
      method: "wasm",
      bytes: decompressed.byteLength,
    });
    return decompressed;
  }

  // Try simultaneous download + decompress via DecompressStream
  const canStream = !!brotli.DecompressStream;
  const stream = canStream ? new brotli.DecompressStream() : null;
  const outChunks = [];
  const inChunks = [];
  let compressedLoaded = 0;
  let decompressStarted = false;
  let streamFailed = false;

  const reader = res.body.getReader();
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;

    const chunk = new Uint8Array(value); // copy for clean buffer
    inChunks.push(chunk);
    compressedLoaded += chunk.byteLength;
    onEvent?.({
      type: "fetch-progress",
      loaded: compressedLoaded,
      total: compressedTotal,
    });

    if (stream && !streamFailed) {
      if (!decompressStarted) {
        decompressStarted = true;
        onEvent?.({ type: "decompress-start" });
      }
      try {
        let consumed = 0;
        for (;;) {
          const result = stream.decompress(chunk.subarray(consumed), 65536);
          if (result.buf.length > 0) outChunks.push(result.buf);
          consumed += result.input_offset;
          if (result.code === 1 || result.code === 2) break;
        }
        onEvent?.({
          type: "decompress-progress",
          loaded: stream.total_out(),
          total: decompressedTotal,
        });
      } catch (e) {
        console.warn("DecompressStream failed, falling back to one-shot:", e);
        streamFailed = true;
        outChunks.length = 0;
      }
    }
  }

  onEvent?.({ type: "fetch-done", bytes: compressedLoaded });

  let decompressed;
  if (stream && !streamFailed) {
    // Streaming succeeded — concatenate decompressed chunks
    const totalOut = stream.total_out();
    decompressed = new Uint8Array(totalOut);
    let off = 0;
    for (const chunk of outChunks) {
      decompressed.set(chunk, off);
      off += chunk.byteLength;
    }
  } else {
    // Fallback: concatenate compressed chunks, one-shot decompress
    if (!decompressStarted) {
      onEvent?.({ type: "decompress-start" });
    }
    const compressed = new Uint8Array(compressedLoaded);
    let cOff = 0;
    for (const chunk of inChunks) {
      compressed.set(chunk, cOff);
      cOff += chunk.byteLength;
    }
    decompressed = brotli.decompress(compressed);
  }

  onEvent?.({
    type: "decompress-done",
    method: "wasm",
    bytes: decompressed.byteLength,
  });
  return decompressed;
}

/**
 * Read a fetch Response body with progress events via ReadableStream.
 * @param {Response} res - The fetch response.
 * @param {number|undefined} total - Expected total bytes for progress display.
 * @param {function} [onEvent] - Optional event callback.
 */
async function readResponseWithProgress(res, total, onEvent, extra) {
  if (!res.body) {
    // Fallback for environments without ReadableStream
    const buf = await res.arrayBuffer();
    onEvent?.({ type: "fetch-progress", loaded: buf.byteLength, total, ...extra });
    return new Uint8Array(buf);
  }

  const reader = res.body.getReader();
  const chunks = [];
  let loaded = 0;

  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    loaded += value.byteLength;
    onEvent?.({ type: "fetch-progress", loaded, total, ...extra });
  }

  const result = new Uint8Array(loaded);
  let offset = 0;
  for (const chunk of chunks) {
    result.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return result;
}

function splitDocuments(blob, marker) {
  if (!blob) return [];
  const docs = [];
  let pos = 0;
  while (pos < blob.length) {
    let next = blob.indexOf(`\n${marker}`, pos);
    if (next === -1) {
      docs.push(blob.slice(pos));
      break;
    }
    docs.push(blob.slice(pos, next + 1));
    pos = next + 1;
  }
  return docs.filter((d) => d.trim().length > 0);
}
