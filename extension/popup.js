// The popup is a view. Every piece of state that has to outlive it -- the
// queue above all -- lives in the service worker, because a popup is destroyed
// the moment it loses focus and a half-opened queue would die with it.
const $ = (s) => document.querySelector(s);
const ask = (msg) => chrome.runtime.sendMessage(msg);

let links = [];

// ------------------------------------------------------------------ capture
async function paintCapture() {
  const { on = true } = await chrome.storage.local.get("on");
  $("#capture").setAttribute("aria-checked", String(on));
}
async function paintAutoClick() {
  const { autoclick = true } = await chrome.storage.local.get("autoclick");
  $("#autoclick").setAttribute("aria-checked", String(autoclick));
}
$("#autoclick").onclick = async () => {
  const on = $("#autoclick").getAttribute("aria-checked") === "true";
  await chrome.storage.local.set({ autoclick: !on });
  paintAutoClick();
};
$("#capture").onclick = async () => {
  const on = $("#capture").getAttribute("aria-checked") === "true";
  await chrome.storage.local.set({ on: !on });
  await ask({ type: "repaint" });
  paintCapture();
};

// --------------------------------------------------------------------- scan
function note(text, strongWord) {
  const el = $("#note");
  el.textContent = "";
  if (!text) return;
  if (strongWord) {
    const b = document.createElement("b");
    b.textContent = strongWord + " ";
    el.append(b);
  }
  el.append(document.createTextNode(text));
}

// Pressing a download button on a page nobody is looking at is the one thing
// here that can go wrong quietly, so it is said out loud once and recorded.
async function agreed() {
  const { adblockOk } = await chrome.storage.local.get("adblockOk");
  return !!(adblockOk && adblockOk.at);
}
$("#agree-box").onchange = (e) => ($("#consent-yes").disabled = !e.target.checked);
$("#consent-no").onclick = () => document.body.classList.remove("asking");
$("#consent-yes").onclick = async () => {
  await chrome.storage.local.set({ adblockOk: { at: new Date().toISOString() } });
  document.body.classList.remove("asking");
  scan();
};

async function startScan() {
  if (await agreed()) return scan();
  $("#agree-box").checked = false;
  $("#consent-yes").disabled = true;
  document.body.classList.add("asking");
}

$("#scan").onclick = startScan;
const scan = async () => {
  const url = $("#url").value.trim();
  if (!/^https?:\/\//i.test(url)) {
    $("#url").focus();
    return note("Paste a link starting with http:// or https:// first.");
  }
  $("#scan").disabled = true;
  $("#scan").textContent = "Scanning";
  note("");
  try {
    // the native host fetches it: no page can refuse us for being an extension,
    // and a paste's key in the fragment is never sent to its server
    const answer = await chrome.runtime.sendNativeMessage("com.idmlite.host", { scrape: url });
    if (!answer || answer.error) throw new Error(answer ? answer.error : "no answer");
    links = (await SCRAPE.links(answer.body, url)).map((l) => ({
      url: l.url,
      name: l.name,
      on: !!l.name || SCRAPE.looksLikeFile(l.url),
      state: "",
    }));
    if (!links.some((l) => l.on)) links.forEach((l) => (l.on = true));
    if (!links.length) return note("No links in that page.");
    paintList();
    $("#results").classList.remove("hidden");
    note(`${links.length} links found. Start opens them in background tabs; whatever downloads is captured into IDM-lite.`);
  } catch (e) {
    note(
      String(e.message || e).includes("host")
        ? "IDM-lite is not reachable. Run Install.bat, then reload this extension."
        : "Could not read that link: " + (e.message || e)
    );
  } finally {
    $("#scan").disabled = false;
    $("#scan").textContent = "Scan";
  }
};

// pre-fill with whatever tab is open, so a paste page is one click away
chrome.tabs.query({ active: true, currentWindow: true }).then(([tab]) => {
  if (tab && /^https?:/i.test(tab.url || "")) $("#url").value = tab.url;
});

// --------------------------------------------------------------------- list
function paintList() {
  const list = $("#list");
  list.textContent = "";
  links.forEach((link, i) => {
    const row = document.createElement("label");
    row.className = "item";
    row.dataset.state = link.state;
    const box = document.createElement("input");
    box.type = "checkbox";
    box.checked = link.on;
    box.onchange = () => { link.on = box.checked; paintStart(); };
    const txt = document.createElement("div");
    txt.className = "txt";
    const nm = document.createElement("span");
    nm.className = "nm";
    // textContent: this came off someone else's page
    nm.textContent = link.name || link.url;
    const st = document.createElement("span");
    st.className = "st";
    st.textContent = STATE[link.state] || link.url.replace(/^https?:\/\//, "");
    txt.append(nm, st);
    row.append(box, txt);
    list.append(row);
  });
  paintStart();
}

const STATE = {
  queued: "waiting its turn",
  opening: "open in a background tab",
  clicked: "pressed its download button",
  caught: "downloading -- handed to IDM-lite",
  waiting: "open in a tab, nothing to press",
  done: "finished with",
};

function paintStart() {
  const n = links.filter((l) => l.on).length;
  $("#start").textContent = n ? `Start (${n})` : "Start";
  $("#start").disabled = !n;
}
$("#all").onclick = () => { links.forEach((l) => (l.on = true)); paintList(); };
$("#none").onclick = () => { links.forEach((l) => (l.on = false)); paintList(); };

// -------------------------------------------------------------------- queue
$("#start").onclick = async () => {
  const chosen = links.filter((l) => l.on).map((l) => ({ url: l.url, name: l.name }));
  if (!chosen.length) return;
  await ask({ type: "start", links: chosen, batch: Number($("#batch").value) || 3 });
  poll();
};
$("#show").onclick = () => ask({ type: "show-tabs" });
$("#stop").onclick = async () => {
  await ask({ type: "stop" });
  poll();
};

// The queue reports its own progress; the popup just mirrors it, and stops
// asking the moment it is closed.
async function poll() {
  const state = await ask({ type: "state" });
  if (!state) return;
  const byUrl = new Map(state.items.map((i) => [i.url, i.state]));
  let changed = false;
  for (const link of links) {
    const now = byUrl.get(link.url) || link.state;
    if (now !== link.state) { link.state = now; changed = true; }
  }
  if (changed) paintList();
  $("#stop").classList.toggle("hidden", !state.running);
  $("#start").classList.toggle("hidden", state.running);
  $("#show").classList.toggle("hidden", !state.waiting);
  if (state.running) {
    $("#queue-hint").textContent =
      `${state.left} to go, out of sight in a minimized window. You can close this popup.`;
    setTimeout(poll, 700);
  } else if (state.waiting) {
    $("#queue-hint").textContent =
      `Done. ${state.waiting} page${state.waiting > 1 ? "s" : ""} had nothing to press -- Show tabs to deal with them.`;
  } else {
    $("#queue-hint").textContent = state.items.length
      ? "Done. Everything that started is downloading in IDM-lite."
      : "";
  }
}

paintCapture();
paintAutoClick();
poll();
