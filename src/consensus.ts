import { fetchFromAuthority } from "./fetch.ts";

function parseFreshUntil(consensus: string): Date {
  const match = consensus.match(
    /^fresh-until (\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2})$/m,
  );
  if (!match) throw new Error("Could not parse fresh-until from consensus");
  return new Date(match[1] + "Z");
}

const CHUNK_SIZE = 60_000; // under 64KB KV limit

async function loadCachedConsensus(kv: Deno.Kv): Promise<string | null> {
  const meta = await kv.get<number>(["consensus", "chunks"]);
  if (meta.value === null) return null;
  const count = meta.value;
  // getMany limited to 10 keys at a time
  const parts: string[] = [];
  for (let i = 0; i < count; i += 10) {
    const keys = Array.from(
      { length: Math.min(10, count - i) },
      (_, j) => ["consensus", i + j],
    );
    const entries = await kv.getMany<string[]>(keys);
    for (const e of entries) {
      if (e.value === null) return null;
      parts.push(e.value as string);
    }
  }
  return parts.join("");
}

async function cacheConsensus(kv: Deno.Kv, text: string, expireIn: number): Promise<void> {
  const chunks: string[] = [];
  for (let i = 0; i < text.length; i += CHUNK_SIZE) {
    chunks.push(text.slice(i, i + CHUNK_SIZE));
  }
  // Write chunks individually — atomic batch exceeds 800KB mutation limit
  await Promise.all(chunks.map((c, i) => kv.set(["consensus", i], c, { expireIn })));
  await kv.set(["consensus", "chunks"], chunks.length, { expireIn });
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
