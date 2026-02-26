import { buildZip, getBundle } from "./src/zip.ts";

const kv = await Deno.openKv();

// Build on startup
console.log("Starting up...");
const initial = await buildZip(kv);
function fmtSize(bytes: number): string {
  return bytes < 1024 * 1024
    ? `${(bytes / 1024).toFixed(0)} KB`
    : `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}
console.log(`  raw: ${fmtSize(initial.raw.byteLength)}, gzip: ${fmtSize(initial.gzip.byteLength)}, brotli: ${fmtSize(initial.brotli.byteLength)}`);
console.log("Ready.");

const handler = async (req: Request) => {
  if (new URL(req.url).pathname === "/") {
    const bundle = await getBundle(kv);
    const ae = req.headers.get("Accept-Encoding") ?? "";
    let body: Uint8Array;
    let encoding: string | undefined;
    if (ae.includes("br")) {
      body = bundle.brotli;
      encoding = "br";
    } else if (ae.includes("gzip")) {
      body = bundle.gzip;
      encoding = "gzip";
    } else {
      body = bundle.raw;
    }
    const headers: Record<string, string> = {
      "Content-Type": "application/zip",
      "Content-Disposition": 'attachment; filename="tor-bootstrap.zip"',
      "Content-Length": String(body.byteLength),
    };
    if (encoding) {
      headers["Content-Encoding"] = encoding;
    }
    return new Response(Uint8Array.from(body), { headers });
  }
  return new Response("Not Found", { status: 404 });
};

function getLanAddress(): string | null {
  for (const iface of Deno.networkInterfaces()) {
    if (iface.family === "IPv4" && !iface.address.startsWith("127.")) {
      return iface.address;
    }
  }
  return null;
}

async function generateSelfSignedCert(): Promise<{ cert: string; key: string }> {
  const { stdout: key } = await new Deno.Command("openssl", {
    args: ["ecparam", "-genkey", "-name", "prime256v1", "-noout"],
    stdout: "piped",
    stderr: "null",
  }).output();

  const keyPem = new TextDecoder().decode(key);

  const proc = new Deno.Command("openssl", {
    args: [
      "req", "-new", "-x509",
      "-key", "/dev/stdin",
      "-days", "365",
      "-subj", "/CN=tor-bootstrap-helper",
      "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1",
    ],
    stdin: "piped",
    stdout: "piped",
    stderr: "null",
  }).spawn();

  const [, output] = await Promise.all([
    async function () {
      const writer = proc.stdin.getWriter();
      await writer.write(key);
      await writer.close();
    }(),
    proc.output(),
  ]);

  return { cert: new TextDecoder().decode(output.stdout), key: keyPem };
}

const lan = getLanAddress();

function tryServe(
  basePort: number,
  extra: Record<string, unknown>,
  scheme: string,
) {
  for (let port = basePort; ; port++) {
    try {
      Deno.serve({
        port,
        hostname: "0.0.0.0",
        onListen() {
          console.log(`  ${scheme}://${lan ?? "0.0.0.0"}:${port}`);
        },
        ...extra,
      }, handler);
      return;
    } catch (e) {
      if (e instanceof Deno.errors.AddrInUse) {
        console.warn(`Port ${port} in use, trying ${port + 1}...`);
        continue;
      }
      throw e;
    }
  }
}

// HTTP
tryServe(8080, {}, "http");

// HTTPS (self-signed)
console.log("Generating self-signed certificate...");
const tls = await generateSelfSignedCert();
tryServe(8443, { cert: tls.cert, key: tls.key }, "https");
