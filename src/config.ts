export const AUTHORITIES = [
  { host: "193.23.244.244", port: 80, name: "dannenberg" },
  { host: "131.188.40.189", port: 80, name: "gabelmoo" },
  { host: "204.13.164.118", port: 80, name: "bastet" },
  { host: "199.58.81.140", port: 80, name: "longclaw" },
  { host: "171.25.193.9", port: 443, name: "maatuska" },
  { host: "86.59.21.38", port: 80, name: "tor26" },
];

export const BATCH_SIZE = 92;
export const CONCURRENCY = 5;
export const CONNECT_TIMEOUT_MS = 2_000;
export const BODY_TIMEOUT_MS = 60_000;
export const INITIAL_BACKOFF_MS = 3_000;

export const WEEK_MS = 7 * 24 * 60 * 60 * 1000;
