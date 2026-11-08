// Reading one link that points at many files: encrypted pastes, mirror
// pages, folder indexes. Shared source -- the app window serves this file
// over its own protocol, the extension popup loads it from the folder, and
// tests/scrape.mjs imports it. Keep it dependency free so all three can.
const SCRAPE = (() => {
  const safe = (s) => { try { return decodeURIComponent(s); } catch { return s; } };
  const b64 = (s) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));

  // PrivateBin carries the key in the URL fragment, base58 encoded, so the
  // server holding the paste never sees it. Neither does ours: Rust strips the
  // fragment before fetching, and the decryption happens here in the page.
  const B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
  function base58(text) {
    const bytes = [0];
    for (const ch of text) {
      let carry = B58.indexOf(ch);
      if (carry < 0) throw new Error("not a base58 key");
      for (let i = 0; i < bytes.length; i++) {
        carry += bytes[i] * 58;
        bytes[i] = carry & 0xff;
        carry >>= 8;
      }
      while (carry) { bytes.push(carry & 0xff); carry >>= 8; }
    }
    for (let i = 0; i < text.length && text[i] === "1"; i++) bytes.push(0);
    return new Uint8Array(bytes.reverse());
  }

  async function inflate(bytes) {
    const stream = new DecompressionStream("deflate-raw");
    const writer = stream.writable.getWriter();
    writer.write(bytes);
    writer.close();
    return new Uint8Array(await new Response(stream.readable).arrayBuffer());
  }

  /** The note inside an encrypted paste, or null when this is not one. */
  async function decrypt(body, fragment) {
    let paste;
    try { paste = JSON.parse(body); } catch { return null; }
    if (!paste || !paste.ct || !paste.adata) return null;
    const [iv, salt, iterations, length, tagLength, , , compression] = paste.adata[0];
    const material = await crypto.subtle.importKey(
      "raw", base58(fragment), "PBKDF2", false, ["deriveKey"]);
    const key = await crypto.subtle.deriveKey(
      { name: "PBKDF2", salt: b64(salt), iterations, hash: "SHA-256" },
      material, { name: "AES-GCM", length }, false, ["decrypt"]);
    // the adata block is the additional data, exactly as it was serialised
    const plain = new Uint8Array(await crypto.subtle.decrypt(
      { name: "AES-GCM", iv: b64(iv), tagLength,
        additionalData: new TextEncoder().encode(JSON.stringify(paste.adata)) },
      key, b64(paste.ct)));
    const text = new TextDecoder().decode(
      compression === "zlib" ? await inflate(plain) : plain);
    // format 2 wraps the note in an envelope; older pastes are the note itself
    try {
      const inner = JSON.parse(text);
      return typeof inner.paste === "string" ? inner.paste : text;
    } catch { return text; }
  }

  const NAMED = /\.[a-z0-9]{2,5}$/i;
  /** A link that already names a file needs no scraping. */
  function looksLikeFile(url) {
    try { return NAMED.test(new URL(url).pathname); } catch { return false; }
  }

  /** Every distinct http link in a page or a note, in the order written. */
  function harvest(text, base) {
    const out = [];
    const seen = new Set();
    const take = (raw) => {
      let link;
      try { link = new URL(raw, base); } catch { return; }
      if (link.protocol !== "http:" && link.protocol !== "https:") return;
      // a file host puts the real name in the fragment; it is never requested
      const hint = safe(link.hash.slice(1));
      const url = link.origin + link.pathname + link.search;
      if (seen.has(url)) return;
      seen.add(url);
      const tail = safe(link.pathname.split("/").filter(Boolean).pop() || "");
      out.push({ url: url, name: NAMED.test(hint) ? hint : NAMED.test(tail) ? tail : "" });
    };
    for (const m of text.matchAll(/(?:href|src)\s*=\s*["']([^"']+)["']/gi)) take(m[1]);
    for (const m of text.matchAll(/https?:\/\/[^\s"'<>()\[\]{}\\]+/gi)) {
      take(m[0].replace(/[.,;:!]+$/, ""));
    }
    return out;
  }

  /** Everything downloadable that one pasted link points at. */
  async function links(body, pageUrl) {
    const fragment = pageUrl.split("#")[1] || "";
    const page = pageUrl.split("#")[0];
    let text = body;
    if (fragment) {
      try {
        const note = await decrypt(body, fragment);
        if (note) text = note;
      } catch { /* not a paste we hold the key for; scrape it as a page */ }
    }
    return harvest(text, page).filter((link) => link.url !== page);
  }

  return { base58, decrypt, looksLikeFile, harvest, links };
})();
