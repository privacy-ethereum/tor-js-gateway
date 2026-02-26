import { zipSync } from "npm:fflate@0.8";
import { fetchConsensus, parseValidUntil, extractDigests } from "./consensus.ts";
import { fetchAllMicrodescs } from "./microdescs.ts";

export interface CachedBundle {
  raw: Uint8Array;
  gzip: Uint8Array;
  brotli: Uint8Array;
  validUntil: Date;
}

let cached: CachedBundle | null = null;
let building = false;

async function gzipCompress(data: Uint8Array): Promise<Uint8Array> {
  const cs = new CompressionStream("gzip");
  const writer = cs.writable.getWriter();
  writer.write(Uint8Array.from(data));
  writer.close();
  return new Uint8Array(await new Response(cs.readable).arrayBuffer());
}

async function brotliCompress(data: Uint8Array): Promise<Uint8Array> {
  const proc = new Deno.Command("brotli", {
    args: ["--quality=6", "-"],
    stdin: "piped",
    stdout: "piped",
  }).spawn();
  // Write stdin and read stdout concurrently to avoid pipe buffer deadlock
  const [, output] = await Promise.all([
    async function () {
      const writer = proc.stdin.getWriter();
      await writer.write(data);
      await writer.close();
    }(),
    proc.output(),
  ]);
  return output.stdout;
}

export async function buildZip(kv: Deno.Kv): Promise<CachedBundle> {
  console.log("Fetching consensus...");
  const consensus = await fetchConsensus(kv);
  const vu = parseValidUntil(consensus);
  console.log(`  Valid until: ${vu.toISOString()}`);

  const digests = extractDigests(consensus);
  console.log(`  ${digests.length} microdescriptor digests`);

  console.log("Fetching microdescriptors...");
  const microdescs = await fetchAllMicrodescs(kv, digests);
  const mdCount = (microdescs.match(/^onion-key$/gm) || []).length;
  console.log(`  ${mdCount} microdescriptors fetched`);

  const enc = new TextEncoder();
  const raw = zipSync({
    [`tor-bootstrap-data/consensus.txt`]: [enc.encode(consensus), { level: 0 }],
    "tor-bootstrap-data/microdescs.txt": [enc.encode(microdescs), { level: 0 }],
  });

  console.log("Compressing...");
  const [gzip, brotli] = await Promise.all([gzipCompress(raw), brotliCompress(raw)]);

  cached = { raw, gzip, brotli, validUntil: vu };
  return cached;
}

export async function getBundle(kv: Deno.Kv): Promise<CachedBundle> {
  if (cached && new Date() < cached.validUntil) {
    return cached;
  }
  if (!building) {
    building = true;
    try {
      cached = await buildZip(kv);
    } finally {
      building = false;
    }
  }
  return cached!;
}
