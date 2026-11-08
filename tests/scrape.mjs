// One pasted link can point at many files. These are the pure parts of that:
// reading an encrypted paste, and pulling the links out of whatever came back.
// The code under test is the code that ships: extension/scrape.js, loaded by
// the app window, the extension popup and this test alike.
// Run: node tests/scrape.mjs
import assert from "node:assert";
import { readFileSync } from "node:fs";

const source = readFileSync("extension/scrape.js", "utf8") + "\nexport default SCRAPE;";
const SCRAPE = (await import("data:text/javascript," + encodeURIComponent(source))).default;

// --------------------------------------------------------------- base58
const B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
function encode58(bytes) {
  const digits = [0];
  for (const byte of bytes) {
    let carry = byte;
    for (let i = 0; i < digits.length; i++) {
      carry += digits[i] << 8;
      digits[i] = carry % 58;
      carry = (carry / 58) | 0;
    }
    while (carry) { digits.push(carry % 58); carry = (carry / 58) | 0; }
  }
  let out = "";
  for (const b of bytes) { if (b) break; out += "1"; }
  return out + digits.reverse().map((d) => B58[d]).join("");
}

const key = crypto.getRandomValues(new Uint8Array(32));
assert.deepStrictEqual([...SCRAPE.base58(encode58(key))], [...key], "base58 round trip");
assert.deepStrictEqual([...SCRAPE.base58(encode58(new Uint8Array([0, 0, 7])))], [0, 0, 7],
  "leading zero bytes survive base58");

// ------------------------------------------- build a paste, then read it back
const b64 = (bytes) => Buffer.from(bytes).toString("base64");

async function deflateRaw(bytes) {
  const cs = new CompressionStream("deflate-raw");
  const w = cs.writable.getWriter();
  w.write(bytes);
  w.close();
  return new Uint8Array(await new Response(cs.readable).arrayBuffer());
}

async function makePaste(note, compression) {
  const iv = crypto.getRandomValues(new Uint8Array(16)); // PrivateBin uses 16, not 12
  const salt = crypto.getRandomValues(new Uint8Array(8));
  const adata = [[b64(iv), b64(salt), 100000, 256, 128, "aes", "gcm", compression], "markdown", 0, 0];
  const material = await crypto.subtle.importKey("raw", key, "PBKDF2", false, ["deriveKey"]);
  const aes = await crypto.subtle.deriveKey(
    { name: "PBKDF2", salt, iterations: 100000, hash: "SHA-256" },
    material, { name: "AES-GCM", length: 256 }, false, ["encrypt"]);
  const envelope = new TextEncoder().encode(JSON.stringify({ paste: note }));
  const body = compression === "zlib" ? await deflateRaw(envelope) : envelope;
  const ct = await crypto.subtle.encrypt(
    { name: "AES-GCM", iv, tagLength: 128,
      additionalData: new TextEncoder().encode(JSON.stringify(adata)) },
    aes, body);
  return JSON.stringify({ status: 0, v: 2, adata, ct: b64(new Uint8Array(ct)) });
}

const note = [
  "## Download links",
  "- https://host.example/aaa111#Game_--_.part1.rar",
  "- https://host.example/bbb222#Game_--_.part2.rar",
  "- https://host.example/aaa111#Game_--_.part1.rar", // the same file, listed twice
].join("\n");

for (const compression of ["zlib", "none"]) {
  const paste = await makePaste(note, compression);
  const links = await SCRAPE.links(paste, "https://paste.example/?abc#" + encode58(key));
  assert.deepStrictEqual(links, [
    { url: "https://host.example/aaa111", name: "Game_--_.part1.rar" },
    { url: "https://host.example/bbb222", name: "Game_--_.part2.rar" },
  ], `a ${compression} paste yields each distinct file once, named by its fragment`);
}

// a wrong key must not look like an empty page: it must fail loudly enough to fall back
const paste = await makePaste(note, "zlib");
const wrong = await SCRAPE.links(paste, "https://paste.example/?abc#" + encode58(crypto.getRandomValues(new Uint8Array(32))));
assert.deepStrictEqual(wrong, [], "an undecryptable paste yields nothing, never ciphertext links");

// ------------------------------------------------------------- plain pages
const page = `<html><body>
  <a href="/files/one.zip">one</a>
  <a href="two.iso">two</a>
  <a href="/about">not a file</a>
  <script src="https://cdn.example/app.js"></script>
  bare: https://other.example/three.7z, and https://other.example/three.7z again
</body></html>`;
const harvested = await SCRAPE.links(page, "https://site.example/dir/index.html");
assert.deepStrictEqual(harvested.map((l) => l.url), [
  "https://site.example/files/one.zip",
  "https://site.example/dir/two.iso",
  "https://site.example/about",
  "https://cdn.example/app.js",
  "https://other.example/three.7z",
], "relative links resolve against the page and repeats collapse");
assert.strictEqual(harvested.find((l) => l.url.endsWith("/about")).name, "",
  "a link with no file name gets none invented for it");

// what the composer uses to decide between downloading and scanning
assert.ok(SCRAPE.looksLikeFile("https://h/x/file.part01.rar"));
assert.ok(SCRAPE.looksLikeFile("https://h/f.zip?token=9"));
assert.ok(!SCRAPE.looksLikeFile("https://paste.example/?e0e968c04a3df58e"));
assert.ok(!SCRAPE.looksLikeFile("https://h/downloads/"));

console.log("ok");
