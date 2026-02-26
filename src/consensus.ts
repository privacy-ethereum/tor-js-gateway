import { fetchFromAuthority } from "./fetch.ts";

function parseFreshUntil(consensus: string): Date {
  const match = consensus.match(
    /^fresh-until (\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2})$/m,
  );
  if (!match) throw new Error("Could not parse fresh-until from consensus");
  return new Date(match[1] + "Z");
}

const CHUNK_SIZE = 60_000; // under 64KB KV limit

function toHex(buf: ArrayBuffer): string {
  return [...new Uint8Array(buf)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

async function sha3(text: string): Promise<string> {
  const hash = await crypto.subtle.digest("SHA3-256", new TextEncoder().encode(text));
  return toHex(hash);
}

async function purgeConsensus(kv: Deno.Kv, hash: string, count: number): Promise<void> {
  await Promise.all(Array.from({ length: count }, (_, i) => kv.delete(["consensus", hash, i])));
  await kv.delete(["consensus", "ptr"]);
}

async function loadCachedConsensus(kv: Deno.Kv): Promise<string | null> {
  const ptr = await kv.get<{ hash: string; chunks: number }>(["consensus", "ptr"]);
  if (ptr.value === null) return null;
  const { hash, chunks: count } = ptr.value;
  // getMany limited to 10 keys at a time
  const parts: string[] = [];
  for (let i = 0; i < count; i += 10) {
    const keys = Array.from(
      { length: Math.min(10, count - i) },
      (_, j) => ["consensus", hash, i + j],
    );
    const entries = await kv.getMany<string[]>(keys);
    for (const e of entries) {
      if (e.value === null) return null;
      parts.push(e.value as string);
    }
  }
  const text = parts.join("");
  if (await sha3(text) !== hash) {
    console.warn("  Consensus cache integrity check failed, purging");
    await purgeConsensus(kv, hash, count);
    return null;
  }
  return text;
}

async function cacheConsensus(kv: Deno.Kv, text: string, expireIn: number): Promise<void> {
  const hash = await sha3(text);
  const chunks: string[] = [];
  for (let i = 0; i < text.length; i += CHUNK_SIZE) {
    chunks.push(text.slice(i, i + CHUNK_SIZE));
  }
  // Write chunks individually — atomic batch exceeds 800KB mutation limit
  await Promise.all(chunks.map((c, i) => kv.set(["consensus", hash, i], c, { expireIn })));
  // Atomic pointer swap — readers always see a consistent set of chunks
  await kv.set(["consensus", "ptr"], { hash, chunks: chunks.length }, { expireIn });
}

export async function fetchConsensus(kv: Deno.Kv): Promise<string> {
  const cached = await loadCachedConsensus(kv);
  if (cached) {
    console.log("  Using cached consensus");
    return cached;
  }

  const text = await fetchFromAuthority(
    "/tor/status-vote/current/consensus-microdesc",
    (body) => body.startsWith("network-status-version"),
  );

  const freshUntil = parseFreshUntil(text);
  const expireIn = Math.max(0, freshUntil.getTime() - Date.now());
  console.log(`  Fresh until: ${freshUntil.toISOString()} (${Math.round(expireIn / 60_000)}m)`);
  await cacheConsensus(kv, text, expireIn);

  return text;
}

export function parseValidUntil(consensus: string): Date {
  const match = consensus.match(
    /^valid-until (\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2})$/m,
  );
  if (!match) throw new Error("Could not parse valid-until from consensus");
  return new Date(match[1] + "Z");
}

export function extractDigests(consensus: string): string[] {
  const digests: string[] = [];
  for (const line of consensus.split("\n")) {
    if (line.startsWith("m ")) {
      for (const d of line.slice(2).trim().split(",")) {
        if (d) digests.push(d.trim());
      }
    }
  }
  return digests;
}
