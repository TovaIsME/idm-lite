const HOST = "com.idmlite.host";

async function enabled() {
  const { on = true } = await chrome.storage.local.get("on");
  return on;
}

async function paintBadge(problem) {
  if (problem) {
    await chrome.action.setBadgeText({ text: "!" });
    await chrome.action.setBadgeBackgroundColor({ color: "#c0392b" });
    await chrome.action.setTitle({ title: "IDM-lite — " + problem });
    return;
  }
  const on = await enabled();
  await chrome.action.setBadgeText({ text: on ? "" : "off" });
  await chrome.action.setBadgeBackgroundColor({ color: "#888888" });
  await chrome.action.setTitle({
    title: on ? "IDM-lite — capturing downloads" : "IDM-lite — capture paused"
  });
}

/**
 * Offer a download to the app. Resolves true only once the app has accepted it.
 * One-shot messaging on purpose: a long-lived port would keep both this service
 * worker and a host process alive, and go stale every time the worker restarts.
 */
async function handOff(url, { referrer = "", filename = "" } = {}) {
  let cookie = "";
  try {
    const jar = await chrome.cookies.getAll({ url });
    cookie = jar.map((c) => `${c.name}=${c.value}`).join("; ");
  } catch (e) {
    /* no cookie access for this URL; send it without */
  }
  try {
    const reply = await chrome.runtime.sendNativeMessage(HOST, {
      url,
      referrer,
      filename,
      cookie,
      ua: navigator.userAgent
    });
    if (reply && reply.accepted) {
      paintBadge(null);
      return true;
    }
    // the app declines what it cannot fetch itself, so that the browser --
    // which can clear a check the app cannot -- keeps the download
    paintBadge("left this one to the browser");
    return false;
  } catch (e) {
    // Almost always: the native host is not registered for this browser.
    paintBadge("app not found — run Install.bat, then reload this extension");
    console.error("idmlite: native host unreachable:", e);
    return false;
  }
}

/** A download Chrome is actually fetching right now, not a history row. */
function isLive(item) {
  // Chrome fires onCreated for EVERY row of the download history when it opens
  // the downloads database -- which it does on browser startup. Those rows are
  // old, finished downloads, and capturing them re-fetches the user's whole
  // Downloads folder on every sign-in. Only take one that is running now.
  if (item.state !== "in_progress") return false;
  const age = Date.now() - Date.parse(item.startTime);
  return !(age > 60000); // NaN (no startTime) counts as fresh
}

/** The last downloads the browser started, so a page can be matched to its own. */
const recent = [];

chrome.downloads.onCreated.addListener(async (item) => {
  if (!isLive(item)) return;
  recent.push({ referrer: item.referrer || "", at: Date.now() });
  if (recent.length > 64) recent.shift();
  const url = item.finalUrl || item.url;
  // blob:/data: URLs exist only inside the page, so another process cannot
  // refetch them. Leave those to the browser.
  if (!/^https?:/i.test(url) || !(await enabled())) return;

  // Hand off BEFORE cancelling. If the app is unreachable we must leave the
  // download with Chrome rather than cancel it into nothing.
  const taken = await handOff(url, {
    referrer: item.referrer,
    filename: item.filename ? item.filename.split(/[\\/]/).pop() : ""
  });
  if (!taken) return;

  try {
    await chrome.downloads.cancel(item.id);
  } catch (e) {
    return; // already finished; the app has it too, which is harmless
  }
  chrome.downloads.erase({ id: item.id }).catch(() => {});
});

// Capture is toggled in the popup; an action with a popup never fires onClicked.

// ----------------------------------------------------------------- the queue
//
// Opening a mirror link in a background tab lets the browser be the browser:
// it runs the page's scripts and clears whatever check the site puts up, which
// a plain HTTP client cannot do. Whatever download the page then starts is
// caught by the listener above and handed to IDM-lite like any other.
//
// This lives in the service worker, not the popup, so closing the popup does
// not abandon a half-opened queue.
const queue = { items: [], running: false, batch: 3, inFlight: 0, windowId: null };

/**
 * Queued pages open in a window of their own, minimized and unfocused.
 *
 * They have to be real tabs in a real browser -- that is the whole point, it
 * is what clears the checks these hosts put up -- but they do not have to be
 * in the user's face. Nothing lands in the window being used.
 */
async function parkingWindow() {
  if (queue.windowId !== null) {
    try {
      await chrome.windows.get(queue.windowId);
      return queue.windowId;
    } catch (e) {
      queue.windowId = null; // the user closed it
    }
  }
  // Several pages open at once and would each make their own window, so the
  // first one to ask does the making and the rest wait on that same promise.
  if (!parking) {
    parking = chrome.windows
      .create({ url: "about:blank", state: "minimized" })
      .then((win) => {
        queue.windowId = win.id;
        parking = null;
        return win.id;
      })
      .catch((e) => {
        parking = null;
        throw e;
      });
  }
  return parking;
}
let parking = null;

/** Close the parking window once nothing in it still wants the user. */
async function tidyUp() {
  if (queue.windowId === null) return;
  const waiting = queue.items.some((i) => i.state === "waiting");
  if (waiting) return; // those tabs are the user's now; leave them be
  const id = queue.windowId;
  queue.windowId = null;
  try { await chrome.windows.remove(id); } catch (e) { /* already closed */ }
}

// How long a page gets to start its own download before we move on. Long
// enough for a check to clear and a redirect to fire, short enough that fifty
// links do not take an afternoon.
// When to look at a queued page: after it loads, after a check would have
// cleared, and once more before giving up. (Tests shorten these.)
const ATTEMPTS = globalThis.__IDM_ATTEMPTS || [1200, 2600, 4000];
const SETTLE_MS = globalThis.__IDM_SETTLE_MS || 1500;
const sleep = (ms) => new Promise((done) => setTimeout(done, ms));

// MV3 stops a worker that looks idle, and awaiting a timer looks idle. Touching
// an extension API resets that clock, so the queue keeps itself awake.
let heartbeat = null;
function keepAwake(on) {
  if (on && !heartbeat) heartbeat = setInterval(() => chrome.runtime.getPlatformInfo(), 20000);
  if (!on && heartbeat) { clearInterval(heartbeat); heartbeat = null; }
}


/**
 * Runs inside a queued page: press the thing that starts the download.
 *
 * This is the click the user would make. It is not how the page's check is
 * passed -- the browser does that by being a browser, before this ever runs.
 *
 * Deliberately generic. A real file link wins, then the biggest visible
 * control that says some form of "download". No per-site selectors: those
 * rot the week after they are written.
 */
function pressTheDownload() {
  const FILE = /\.(zip|rar|7z|tar|gz|iso|exe|msi|apk|bin|dmg|pkg|mp4|mkv|mp3|flac|pdf|epub)(\?|$)/i;
  const VERB = /^\s*(download|get file|start download|free download|descargar|t[eé]l[eé]charger|herunterladen|baixar|scarica|unduh|скачать|下载|ダウンロード|تحميل)/i;
  // whole words only: "download-btn" must not read as an ad
  const JUNK = /(^|[^a-z])(ads?|adsbygoogle|advert\w*|sponsor\w*|banner|promo|popunder|taboola|outbrain)([^a-z]|$)/i;

  const onScreen = (el) => {
    const box = el.getBoundingClientRect();
    const style = getComputedStyle(el);
    return box.width > 40 && box.height > 18 &&
      style.visibility !== "hidden" && style.display !== "none" && style.opacity !== "0";
  };
  const area = (el) => {
    const box = el.getBoundingClientRect();
    return box.width * box.height;
  };

  // An ad unit dressed as a download button is the one thing that could make
  // this press something the user never wanted. Three cheap defences: anything
  // sitting under ad-ish markup is out, an off-site link that is not plainly a
  // file is out, and this only ever runs in the page's top frame, so the
  // iframes ads usually live in are out of reach to begin with.
  const adBait = (el) => {
    for (let node = el; node && node !== document.body; node = node.parentElement) {
      const marks = [node.id, String(node.className || ""), node.getAttribute("data-ad") || ""].join(" ");
      if (JUNK.test(marks)) return true;
    }
    return false;
  };
  const site = (host) => host.split(".").slice(-2).join(".");
  const ourSite = (el) => {
    try {
      return site(new URL(el.href, location.href).hostname) === site(location.hostname);
    } catch (e) {
      return false;
    }
  };

  const anchors = [...document.querySelectorAll("a[href]")].filter((a) => onScreen(a) && !adBait(a));

  // an anchor that is already the file needs no guessing
  const direct = anchors.find((a) => a.hasAttribute("download") || FILE.test(a.getAttribute("href") || ""));
  if (direct) {
    direct.click();
    return "link";
  }

  // otherwise the page's own download control, biggest first: these pages put
  // the real one front and centre and the decoys in the margins
  const controls = [...document.querySelectorAll("a, button, input[type=submit], [role=button]")]
    .filter((el) => onScreen(el) && !adBait(el))
    .filter((el) => VERB.test(el.innerText || el.value || el.getAttribute("aria-label") || ""))
    // a button stays on the page; a link that leaves it had better be a file
    .filter((el) => el.tagName !== "A" || ourSite(el) || FILE.test(el.getAttribute("href") || ""))
    .sort((a, b) => area(b) - area(a));
  if (controls.length) {
    controls[0].click();
    return "button";
  }
  return "none";
}

/**
 * Did this page start a download since we opened it?
 *
 * The browser tells us the referrer, which is the page that started it -- the
 * only honest way to tell three queued tabs apart. When nothing else is in
 * flight there is nothing to confuse it with, so a bare "something started"
 * is enough; when several are, an unattributable download counts for none of
 * them and that tab is left open rather than closed on a guess.
 */
function cameFrom(pageUrl, since, alone) {
  return recent.some((d) => {
    if (d.at < since) return false;
    // a download that names its page must match ours; one that names none can
    // only be ours when ours was the only tab we had open
    return d.referrer ? d.referrer.startsWith(pageUrl) : alone;
  });
}

async function autoClicks() {
  const { autoclick = true } = await chrome.storage.local.get("autoclick");
  return autoclick;
}

async function openOne(item) {
  item.state = "opening";
  const since = Date.now();
  let tab;
  queue.inFlight++;
  try {
    const windowId = await parkingWindow();
    tab = await chrome.tabs.create({ url: item.url, active: false, windowId });
  } catch (e) {
    queue.inFlight--;
    item.state = "waiting";
    return;
  }
  const press = await autoClicks();
  const mine = () => cameFrom(item.url, since, queue.inFlight <= 1);

  try {
    // A check page sits there a second or two and then becomes the real page,
    // so look more than once instead of giving up on the first miss.
    for (const wait of ATTEMPTS) {
      await sleep(wait);
      if (mine() || !queue.running) break;
      if (!press) continue;
      try {
        const [done] = await chrome.scripting.executeScript({
          target: { tabId: tab.id },
          func: pressTheDownload,
        });
        if (done && done.result && done.result !== "none") item.state = "clicked";
      } catch (e) {
        // still loading, or a page we are not allowed to script
      }
    }
    await sleep(SETTLE_MS);
    if (mine()) {
      item.state = "caught";
      try { await chrome.tabs.remove(tab.id); } catch (e) { /* already gone */ }
    } else {
      item.state = "waiting"; // left open: it is the user's tab now
    }
  } finally {
    queue.inFlight--;
  }
}

async function runQueue() {
  if (queue.running) return;
  queue.running = true;
  keepAwake(true);
  try {
    while (queue.running) {
      const next = queue.items.filter((i) => i.state === "queued").slice(0, queue.batch);
      if (!next.length) break;
      await Promise.all(next.map(openOne));
    }
  } finally {
    queue.running = false;
    keepAwake(false);
    await tidyUp();
  }
}

chrome.runtime.onMessage.addListener((msg, _sender, reply) => {
  if (msg.type === "start") {
    queue.batch = Math.max(1, Math.min(8, msg.batch || 3));
    queue.items = msg.links.map((l) => ({ url: l.url, name: l.name, state: "queued" }));
    runQueue();
    reply({ ok: true });
  } else if (msg.type === "stop") {
    // stops opening anything further; tabs already open stay open
    queue.running = false;
    queue.items.forEach((i) => { if (i.state === "queued") i.state = ""; });
    reply({ ok: true });
  } else if (msg.type === "state") {
    reply({
      running: queue.running,
      left: queue.items.filter((i) => i.state === "queued").length,
      waiting: queue.items.filter((i) => i.state === "waiting").length,
      items: queue.items.map((i) => ({ url: i.url, state: i.state })),
    });
  } else if (msg.type === "show-tabs") {
    // the only time that window is ever put in front of anyone: on request
    if (queue.windowId !== null) {
      chrome.windows.update(queue.windowId, { focused: true, state: "normal" });
    }
    reply({ ok: true });
  } else if (msg.type === "repaint") {
    paintBadge(null);
    reply({ ok: true });
  }
  return true; // the reply may be sent after this returns
});

chrome.contextMenus.onClicked.addListener((info, tab) => {
  const url = info.linkUrl || info.srcUrl;
  if (url) handOff(url, { referrer: info.pageUrl || (tab && tab.url) || "" });
});

function installMenu() {
  chrome.contextMenus.removeAll(() => {
    void chrome.runtime.lastError;
    chrome.contextMenus.create(
      {
        id: "idmlite",
        title: "Download with IDM-lite",
        contexts: ["link", "image", "video", "audio"]
      },
      () => void chrome.runtime.lastError
    );
  });
  paintBadge(null);
}

chrome.runtime.onInstalled.addListener(installMenu);
chrome.runtime.onStartup.addListener(installMenu);
