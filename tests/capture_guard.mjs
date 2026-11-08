// The bug this guards: Chrome replays downloads.onCreated for every row of the
// download history when it opens the downloads DB at startup, so the extension
// re-downloaded the user's whole Downloads folder on every sign-in.
// Run: node tests/capture_guard.mjs
import assert from "node:assert";
import { readFileSync } from "node:fs";

let onCreated;
const handed = [];
globalThis.chrome = {
  storage: { local: { get: async () => ({ on: true }), set: async () => {} } },
  action: { setBadgeText: async () => {}, setBadgeBackgroundColor: async () => {}, setTitle: async () => {}, onClicked: { addListener() {} } },
  cookies: { getAll: async () => [] },
  runtime: {
    sendNativeMessage: async (_h, m) => (handed.push(m.url), { accepted: true }),
    onInstalled: { addListener() {} },
    onStartup: { addListener() {} },
  },
  downloads: { onCreated: { addListener: (f) => (onCreated = f) }, cancel: async () => {}, erase: async () => {} },
  contextMenus: { onClicked: { addListener() {} }, removeAll: () => {}, create: () => {} },
};

await import("data:text/javascript," + encodeURIComponent(readFileSync("extension/background.js", "utf8")));

const item = (over) => ({ id: 1, url: "https://h/f.zip", state: "in_progress", startTime: new Date().toISOString(), ...over });

await onCreated(item({ state: "complete" }));
await onCreated(item({ state: "interrupted" }));
await onCreated(item({ startTime: "2021-03-05T10:00:00.000Z" })); // stale history row
assert.deepStrictEqual(handed, [], "history replay must never be captured");

await onCreated(item({}));
await onCreated(item({ startTime: undefined })); // no startTime: still a real download
assert.deepStrictEqual(handed, ["https://h/f.zip", "https://h/f.zip"], "a live download must be captured");
console.log("ok");
