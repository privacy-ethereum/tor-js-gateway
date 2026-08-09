# tor-js-gateway — archived

This repository is no longer developed. The gateway it held now lives in the
[tor-js](https://github.com/privacy-ethereum/tor-js) repository:

- **Source:** `crates/tor-js-gateway/`
- **Wire protocol (KPS-HTTP/1):** `PROTOCOL.md` at that repository's root

Development stopped here on 2026-07-15. The final state was consolidated into
tor-js on 2026-07-16 as a subtree add of `c58b8e4`, and everything since has
happened there. Nothing in this repository is newer than what tor-js has.

Please file issues and pull requests against
[privacy-ethereum/tor-js](https://github.com/privacy-ethereum/tor-js).

## Branches kept for history

| Branch | What it is |
|---|---|
| `kps` | The final state of this repository — the KPS-only gateway, at the exact commit consolidated into tor-js. |
| `archive/main` | The tip of `main` immediately before this notice replaced it. Pre-KPS: direct-TCP and WebSocket transports, `/rtc/connect` signalling. An ancestor of `kps`. |
| `archive/deno` | An earlier Deno implementation. |
