// The bug this guards: Chrome replays downloads.onCreated for every row of the
// download history when it opens the downloads DB at startup, so the extension
// re-downloaded the user's whole Downloads folder on every sign-in.
// Run: node tests/capture_guard.mjs
import assert from "node:assert";
import { readFileSync } from "node:fs";

globalThis.__IDM_ATTEMPTS = [1, 1, 1]; // the queue looks at a page this often
globalThis.__IDM_SETTLE_MS = 1;
let onMessage;
let quiet = false; // true: pretend the opened page starts no download

let onCreated;
const handed = [];
globalThis.chrome = {
  storage: {
    local: (() => {
      const kept = { on: true };
      return {
        get: async (k) => (typeof k === "string" ? { [k]: kept[k] } : { ...kept }),
        set: async (o) => Object.assign(kept, o),
      };
    })(),
  },
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
globalThis.chrome.runtime.onMessage = { addListener: (f) => (onMessage = f) };

await import("data:text/javascript," + encodeURIComponent(readFileSync("extension/background.js", "utf8")));

// a download the browser started, carrying the page that started it
const fakeDownload = (referrer) => onCreated(item({ id: Math.random(), referrer }));
const item = (over) => ({ id: 1, url: "https://h/f.zip", state: "in_progress", startTime: new Date().toISOString(), ...over });

await onCreated(item({ state: "complete" }));
await onCreated(item({ state: "interrupted" }));
await onCreated(item({ startTime: "2021-03-05T10:00:00.000Z" })); // stale history row
assert.deepStrictEqual(handed, [], "history replay must never be captured");

await onCreated(item({}));
await onCreated(item({ startTime: undefined })); // no startTime: still a real download
assert.deepStrictEqual(handed, ["https://h/f.zip", "https://h/f.zip"], "a live download must be captured");
console.log("ok: capture guard");

// ---------------------------------------------------------------- the queue
// Opening mirror links in background tabs is what saves the user clicking
// through fifty of them, so it gets checked: batching, stopping, and telling
// a page that started its own download from one that is waiting for a click.
const opened = [];
let removed = [];
let windows = [];
let windowsClosed = [];
globalThis.chrome.windows = {
  create: async (o) => {
    const win = { id: 900 + windows.length, ...o };
    windows.push(win);
    return win;
  },
  get: async (id) => {
    const win = windows.find((w) => w.id === id && !windowsClosed.includes(id));
    if (!win) throw new Error("no such window");
    return win;
  },
  remove: async (id) => void windowsClosed.push(id),
  update: async () => {},
};

const tabUrl = new Map();
globalThis.chrome.tabs = {
  // a real page either starts its download once it loads, or sits there
  create: async (o) => {
    const id = opened.push(o); // id now, not after the await: tabs open at once
    tabUrl.set(id, o.url);
    if (!quiet) await fakeDownload(o.url);
    return { id };
  },
  remove: async (id) => void removed.push(id),
};
globalThis.chrome.runtime.getPlatformInfo = async () => ({ os: "win" });
// pages that need the button pressed before anything downloads
let pressed = 0;
let pageHasButton = false;
globalThis.chrome.scripting = {
  executeScript: async ({ target }) => {
    pressed++;
    if (!pageHasButton) return [{ result: "none" }];
    await fakeDownload(tabUrl.get(target.tabId)); // the press starts it
    return [{ result: "button" }];
  },
};

const send = (msg) => new Promise((done) => onMessage(msg, {}, done));

// Wait for the queue to report itself finished rather than guessing at a
// sleep: the last batch lands whenever it lands.
async function settle(ms = 4000) {
  const deadline = Date.now() + ms;
  for (;;) {
    const state = await send({ type: "state" });
    if (!state.running) return state;
    if (Date.now() > deadline) throw new Error("the queue never finished");
    await new Promise((r) => setTimeout(r, 5));
  }
}
const links = Array.from({ length: 5 }, (_, i) => ({ url: `https://h/f${i}`, name: `f${i}.rar` }));

await send({ type: "start", links, batch: 2 });
const after = await settle();
assert.strictEqual(opened.length, 5, "every link opens, two at a time");
assert.ok(opened.every((o) => o.active === false), "tabs must open in the background");
assert.strictEqual(windows.length, 1, "all of them share one window");
assert.strictEqual(windows[0].state, "minimized",
  "which is minimized: queued tabs never appear in front of the user");
assert.ok(opened.every((o) => o.windowId === windows[0].id),
  "and nothing lands in the window the user is working in");
assert.deepStrictEqual(windowsClosed, [windows[0].id],
  "with nothing left waiting, the window closes itself");
assert.deepStrictEqual(removed.sort((a, b) => a - b), [1, 2, 3, 4, 5],
  "a tab whose page started a download is closed again");
assert.ok(after.items.every((i) => i.state === "caught"), "each one reports what happened");

// a page that only downloads once its button is pressed: the queue presses it
opened.length = 0;
removed = [];
pressed = 0;
quiet = true;          // nothing downloads on load
pageHasButton = true;  // but there is a button, and pressing it starts one
await send({ type: "start", links: links.slice(0, 3), batch: 3 });
const clicked = await settle();
assert.ok(pressed >= 3, `every queued page gets pressed (pressed ${pressed})`);
assert.ok(clicked.items.every((i) => i.state === "caught"),
  "pressing it counts as the download starting, so the tab closes");
assert.strictEqual(removed.length, 3, "and all three tabs close again");

// with the clicking turned off, nothing is pressed
pageHasButton = false;
await chrome.storage.local.set({ autoclick: false });
pressed = 0;
await send({ type: "start", links: links.slice(0, 1), batch: 1 });
await settle();
assert.strictEqual(pressed, 0, "the user can turn the pressing off");
await chrome.storage.local.set({ autoclick: true });

// an unattributable download must not be credited to every open tab
opened.length = 0;
removed = [];
quiet = true;
pageHasButton = false;
const race = send({ type: "start", links: links.slice(0, 3), batch: 3 });
await new Promise((r) => setTimeout(r, 2));
await fakeDownload("https://somewhere-else/page"); // a download of the user's own
await race;
const confused = await settle();
assert.deepStrictEqual(removed, [],
  "a download from another page closes nobody's tab");
assert.ok(confused.items.every((i) => i.state === "waiting"),
  "and credits nobody's link");

// a page that starts nothing is left open for the user, not silently dropped
opened.length = 0;
removed = [];
quiet = true;
await send({ type: "start", links: links.slice(0, 2), batch: 2 });
const idle = await settle();
assert.deepStrictEqual(removed, [], "a tab that downloaded nothing stays open");
assert.ok(idle.items.every((i) => i.state === "waiting"), "and says it needs a click");

// stop means stop: nothing further opens
quiet = false;
opened.length = 0;
await send({ type: "start", links, batch: 1 });
await send({ type: "stop" });
const stopped = await settle();
assert.ok(opened.length < 5, `stop halts the queue (opened ${opened.length} of 5)`);
assert.strictEqual(stopped.running, false, "and it reports stopped");
assert.strictEqual(stopped.left, 0, "nothing is left queued behind a stop");

console.log("ok: browser queue");
