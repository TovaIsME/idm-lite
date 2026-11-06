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
    paintBadge("app could not start the download");
    return false;
  } catch (e) {
    // Almost always: the native host is not registered for this browser.
    paintBadge("app not found — run Install.bat, then reload this extension");
    console.error("idmlite: native host unreachable:", e);
    return false;
  }
}

chrome.downloads.onCreated.addListener(async (item) => {
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

chrome.action.onClicked.addListener(async () => {
  await chrome.storage.local.set({ on: !(await enabled()) });
  paintBadge(null);
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
