// What the queue presses on a page nobody is looking at. The risk is an ad
// dressed as a download button, so that is what this pins down.
// The code under test is sliced out of the extension, not a copy of it.
// Run: node tests/clicker.mjs
import assert from "node:assert";
import { readFileSync } from "node:fs";

const source = readFileSync("extension/background.js", "utf8");
const from = source.indexOf("function pressTheDownload() {");
const to = source.indexOf("\n}\n", from) + 2;
assert.ok(from > 0 && to > from, "pressTheDownload must be findable in background.js");
const pressTheDownload = new Function("return " + source.slice(from, to))();

// ----------------------------------------------------------- a fake page
const BODY = { id: "", className: "", parentElement: null, getAttribute: () => null };

function el(tag, props = {}) {
  const node = {
    tagName: tag.toUpperCase(),
    id: "",
    className: "",
    innerText: "",
    value: "",
    href: "",
    attrs: {},
    width: 200,
    height: 40,
    clicked: 0,
    parentElement: BODY,
    ...props,
  };
  node.getBoundingClientRect = () => ({ width: node.width, height: node.height });
  node.getAttribute = (k) => (k === "href" ? node.href || null : node.attrs[k] ?? null);
  node.hasAttribute = (k) => k === "download" ? "download" in node.attrs : k in node.attrs;
  node.click = () => node.clicked++;
  return node;
}

/** Wrap a node in an ad container, the way a page embeds one. */
function inside(node, className) {
  node.parentElement = { id: "", className, parentElement: BODY, getAttribute: () => null };
  return node;
}

function page(nodes, url = "https://host.example/file/abc") {
  globalThis.document = {
    body: BODY,
    querySelectorAll: (sel) =>
      nodes.filter((n) => {
        if (sel === "a[href]") return n.tagName === "A" && n.href;
        return ["A", "BUTTON"].includes(n.tagName) ||
          (n.tagName === "INPUT" && n.attrs.type === "submit") ||
          n.attrs.role === "button";
      }),
  };
  globalThis.getComputedStyle = () => ({ visibility: "visible", display: "block", opacity: "1" });
  globalThis.location = new URL(url);
}

// ------------------------------------------------------------- the checks
const realFile = el("a", { href: "/get/Game.part01.rar", innerText: "Download" });
const adButton = inside(el("a", { href: "https://ad-network.example/x", innerText: "DOWNLOAD NOW", width: 700, height: 90 }), "ads banner");
page([adButton, realFile]);
assert.strictEqual(pressTheDownload(), "link", "a real file link is what gets pressed");
assert.strictEqual(realFile.clicked, 1);
assert.strictEqual(adButton.clicked, 0, "the ad is never pressed, even though it is far bigger");

// no file link on the page: the page's own button, not the ad's
const adBig = inside(el("button", { innerText: "Download", width: 800, height: 120 }), "adsbygoogle");
const sponsored = inside(el("button", { innerText: "Free Download", width: 900, height: 200 }), "sponsored-slot");
const realButton = el("button", { innerText: "Download", width: 180, height: 44 });
page([adBig, sponsored, realButton]);
assert.strictEqual(pressTheDownload(), "button");
assert.strictEqual(realButton.clicked, 1, "the page's own control wins over bigger ad bait");
assert.strictEqual(adBig.clicked + sponsored.clicked, 0);

// a link that leaves the site and is not plainly a file is not worth the risk
const offsite = el("a", { href: "https://elsewhere.example/landing", innerText: "Download" });
page([offsite]);
assert.strictEqual(pressTheDownload(), "none", "an off-site link that is not a file is left alone");
assert.strictEqual(offsite.clicked, 0);

// but the same link pointing at an actual file is fine: CDNs are another host
const cdn = el("a", { href: "https://cdn.elsewhere.example/Game.part02.rar", innerText: "Download" });
page([cdn]);
assert.strictEqual(pressTheDownload(), "link", "an off-site file link is still a file");

// "download-btn" is not an ad, however much it contains the letters a and d
const named = el("button", { innerText: "Download", className: "download-btn primary" });
page([named]);
assert.strictEqual(pressTheDownload(), "button", "ad matching is on whole words, not substrings");
assert.strictEqual(named.clicked, 1);

// where it is ambiguous it declines: "ad-" as its own word is treated as an ad
// even in "ad-free", because pressing nothing is the cheap mistake here
const ambiguous = el("button", { innerText: "Download", className: "ad-free-zone" });
page([ambiguous]);
assert.strictEqual(pressTheDownload(), "none", "anything ad-shaped is left for the user");

// nothing to press is a perfectly good answer
page([el("button", { innerText: "Report abuse" }), el("a", { href: "/about", innerText: "About" })]);
assert.strictEqual(pressTheDownload(), "none", "it presses nothing rather than something wrong");

console.log("ok: clicker");
