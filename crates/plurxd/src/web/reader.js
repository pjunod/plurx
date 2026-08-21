/* Cinema's bounded EPUB navigator.
 *
 * The server owns package parsing and resource authorization. This file owns
 * presentation and locators only. Publication documents stay in an iframe
 * whose sandbox deliberately omits scripts, forms, popups, downloads, and
 * top navigation; the account bearer is never appended to a publication URL.
 */
(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  else root.PlurxReaderCore = api;
})(typeof globalThis !== "undefined" ? globalThis : this, function () {
  "use strict";

  const DEFAULT_PREFS = Object.freeze({
    font: "publisher",
    fontSize: 100,
    lineHeight: 1.55,
    margin: 7,
    theme: "light",
    flow: "paginated",
  });
  const THEMES = Object.freeze({
    light: { background: "#fffdf8", foreground: "#24211d", link: "#294f9b" },
    sepia: { background: "#f3ead7", foreground: "#392f24", link: "#704c1c" },
    dark: { background: "#171715", foreground: "#e9e3d8", link: "#9db2ff" },
  });
  const FONTS = Object.freeze({
    publisher: "inherit",
    serif: 'Charter, "Bitstream Charter", "Sitka Text", Georgia, serif',
    sans: 'system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif',
    accessible: 'Atkinson Hyperlegible, Verdana, Arial, sans-serif',
  });

  function clamp(value, low, high, fallback) {
    const number = Number(value);
    return Number.isFinite(number) ? Math.min(high, Math.max(low, number)) : fallback;
  }

  function normalizePrefs(value) {
    const raw = value && typeof value === "object" ? value : {};
    return {
      font: Object.prototype.hasOwnProperty.call(FONTS, raw.font) ? raw.font : DEFAULT_PREFS.font,
      fontSize: clamp(raw.fontSize, 70, 220, DEFAULT_PREFS.fontSize),
      lineHeight: clamp(raw.lineHeight, 1.1, 2.4, DEFAULT_PREFS.lineHeight),
      margin: clamp(raw.margin, 2, 18, DEFAULT_PREFS.margin),
      theme: Object.prototype.hasOwnProperty.call(THEMES, raw.theme) ? raw.theme : DEFAULT_PREFS.theme,
      flow: raw.flow === "scrolled" ? "scrolled" : DEFAULT_PREFS.flow,
    };
  }

  function splitHref(href) {
    const text = String(href || "");
    const hash = text.indexOf("#");
    return hash < 0
      ? { path: text, fragment: "" }
      : { path: text.slice(0, hash), fragment: text.slice(hash + 1) };
  }

  function safeDecode(value) {
    try { return decodeURIComponent(value); } catch (_) { return value; }
  }

  function normalizePath(path) {
    const out = [];
    for (const raw of String(path || "").split("/")) {
      const part = safeDecode(raw);
      if (!part || part === ".") continue;
      if (part === "..") {
        if (!out.length) return null;
        out.pop();
      } else {
        if (part.includes("\\") || part.includes(":")) return null;
        out.push(part);
      }
    }
    return out.length ? out.join("/") : null;
  }

  function resolveHref(current, target) {
    const raw = String(target || "").trim();
    if (!raw || /^[a-z][a-z0-9+.-]*:/i.test(raw) || raw.startsWith("//") || raw.startsWith("/")) return null;
    const to = splitHref(raw);
    const from = splitHref(current);
    const base = from.path.includes("/") ? from.path.slice(0, from.path.lastIndexOf("/") + 1) : "";
    const path = normalizePath(to.path ? base + to.path : from.path);
    if (!path) return null;
    return path + (to.fragment ? "#" + to.fragment : "");
  }

  function resourceUrl(base, href) {
    const split = splitHref(href);
    const path = normalizePath(split.path);
    if (!path) return null;
    const encoded = path.split("/").map(encodeURIComponent).join("/");
    return String(base || "") + encoded + (split.fragment ? "#" + encodeURIComponent(split.fragment) : "");
  }

  function totalProgression(index, count, progression) {
    const total = Math.max(1, Number(count) || 1);
    const position = Math.min(total - 1, Math.max(0, Number(index) || 0));
    return Math.min(1, Math.max(0, (position + clamp(progression, 0, 1, 0)) / total));
  }

  function elementPath(element, body) {
    const path = [];
    let node = element;
    while (node && node !== body) {
      const parent = node.parentElement;
      if (!parent) return null;
      path.unshift(Array.prototype.indexOf.call(parent.children, node));
      node = parent;
    }
    return node === body ? path : null;
  }

  function elementAtPath(body, path) {
    if (!body || !Array.isArray(path)) return null;
    let node = body;
    for (const index of path) {
      if (!Number.isInteger(index) || index < 0 || !node.children || index >= node.children.length) return null;
      node = node.children[index];
    }
    return node;
  }

  function textQuote(element) {
    return String(element && element.textContent || "").replace(/\s+/g, " ").trim().slice(0, 160);
  }

  function makeLocator(href, index, count, within, element) {
    const split = splitHref(href);
    const id = element && element.id ? String(element.id) : "";
    const path = element && element.ownerDocument && element.ownerDocument.body
      ? elementPath(element, element.ownerDocument.body) : null;
    const locator = {
      version: 1,
      href: split.path + (id ? "#" + encodeURIComponent(id) : ""),
      locations: {
        progression: clamp(within, 0, 1, 0),
        totalProgression: totalProgression(index, count, within),
      },
    };
    if (path) locator.cinema = { path: path, text: textQuote(element) };
    return locator;
  }

  function blockElements(doc) {
    if (!doc || !doc.body) return [];
    const selector = "h1,h2,h3,h4,h5,h6,p,li,blockquote,pre,figure,table,section,article,div";
    return Array.from(doc.body.querySelectorAll(selector)).filter(function (element) {
      const text = textQuote(element);
      if (!text) return false;
      return !Array.from(element.children || []).some(function (child) {
        return /^(H[1-6]|P|LI|BLOCKQUOTE|PRE|FIGURE|TABLE|SECTION|ARTICLE|DIV)$/i.test(child.tagName) && textQuote(child);
      });
    });
  }

  function visibleAnchor(doc, flow) {
    const elements = blockElements(doc);
    if (!elements.length) return doc && doc.body;
    const horizontal = flow === "paginated";
    let best = elements[0], score = Infinity;
    for (const element of elements) {
      const rects = Array.from(element.getClientRects ? element.getClientRects() : []);
      for (const rect of rects) {
        const visible = horizontal
          ? rect.right > 0 && rect.left < doc.documentElement.clientWidth
          : rect.bottom > 0 && rect.top < doc.documentElement.clientHeight;
        if (!visible) continue;
        const candidate = Math.abs(horizontal ? rect.left : rect.top);
        if (candidate < score) { score = candidate; best = element; }
      }
    }
    return best;
  }

  function progressOf(doc, flow) {
    if (!doc || !doc.body) return 0;
    const root = flow === "paginated" ? doc.body : doc.scrollingElement || doc.documentElement;
    const current = flow === "paginated" ? Math.abs(root.scrollLeft || 0) : root.scrollTop || 0;
    const extent = flow === "paginated"
      ? Math.max(0, root.scrollWidth - root.clientWidth)
      : Math.max(0, root.scrollHeight - root.clientHeight);
    return extent ? Math.min(1, current / extent) : 0;
  }

  function readerCss(value) {
    const prefs = normalizePrefs(value);
    const theme = THEMES[prefs.theme];
    const family = FONTS[prefs.font];
    const margin = prefs.margin;
    const common = `
      :root { color-scheme: ${prefs.theme === "dark" ? "dark" : "light"}; background:${theme.background}; color:${theme.foreground}; }
      html, body { box-sizing:border-box; background:${theme.background} !important; color:${theme.foreground} !important; }
      body { font-family:${family} !important; font-size:${prefs.fontSize}% !important; line-height:${prefs.lineHeight} !important; }
      body * { max-width:100%; }
      img, svg, video { max-width:100% !important; height:auto !important; }
      a { color:${theme.link} !important; }
      script, iframe, frame, object, embed, form { display:none !important; }
      :focus-visible { outline:3px solid ${theme.link} !important; outline-offset:3px; }
    `;
    if (prefs.flow === "scrolled") {
      return common + `
        html { overflow:auto !important; }
        body { min-height:100%; margin:0 auto !important; padding:${margin}vh ${margin}vw !important; max-width:52rem !important; overflow:visible !important; }
      `;
    }
    return common + `
      html { width:100%; height:100%; overflow:hidden !important; }
      body { position:absolute; inset:0; width:auto !important; height:100% !important; margin:0 !important;
        padding:${margin}vh ${margin}vw !important; max-width:none !important; overflow-x:auto !important; overflow-y:hidden !important;
        column-width:${Math.max(40, 100 - margin * 2)}vw; column-gap:${margin * 2}vw; column-fill:auto;
        scroll-snap-type:x mandatory; }
      body > * { scroll-snap-align:start; }
    `;
  }

  function findText(doc, quote) {
    const needle = String(quote || "").replace(/\s+/g, " ").trim();
    if (!needle) return null;
    return blockElements(doc).find(function (element) { return textQuote(element).includes(needle); }) || null;
  }

  function restoreElement(doc, locator) {
    if (!doc || !doc.body || !locator) return null;
    const fragment = safeDecode(splitHref(locator.href).fragment);
    if (fragment) {
      const byId = doc.getElementById(fragment);
      if (byId) return byId;
    }
    const cinema = locator.cinema && typeof locator.cinema === "object" ? locator.cinema : {};
    const byPath = elementAtPath(doc.body, cinema.path);
    if (byPath && (!cinema.text || textQuote(byPath).includes(String(cinema.text).slice(0, 60)))) return byPath;
    return findText(doc, cinema.text);
  }

  function scrollElement(doc, element, flow) {
    if (!doc || !element) return false;
    if (flow === "paginated") {
      const body = doc.body;
      const rect = element.getBoundingClientRect();
      const width = Math.max(1, body.clientWidth);
      body.scrollLeft += Math.floor(rect.left / width) * width;
    } else {
      element.scrollIntoView({ block: "start", inline: "nearest" });
    }
    return true;
  }

  function restoreLocator(doc, locator, flow) {
    const element = restoreElement(doc, locator);
    if (element) return scrollElement(doc, element, flow);
    const within = clamp(locator && locator.locations && locator.locations.progression, 0, 1, 0);
    const root = flow === "paginated" ? doc.body : doc.scrollingElement || doc.documentElement;
    if (flow === "paginated") root.scrollLeft = within * Math.max(0, root.scrollWidth - root.clientWidth);
    else root.scrollTop = within * Math.max(0, root.scrollHeight - root.clientHeight);
    return false;
  }

  function snippet(text, query) {
    const clean = String(text || "").replace(/\s+/g, " ").trim();
    const needle = String(query || "").trim().toLocaleLowerCase();
    const at = clean.toLocaleLowerCase().indexOf(needle);
    if (at < 0) return null;
    const start = Math.max(0, at - 56), end = Math.min(clean.length, at + needle.length + 88);
    return (start ? "…" : "") + clean.slice(start, end) + (end < clean.length ? "…" : "");
  }

  function stripExecutableMarkup(doc) {
    if (!doc || !doc.querySelectorAll) return;
    doc.querySelectorAll("script,iframe,frame,object,embed,form,meta[http-equiv],base").forEach(function (node) { node.remove(); });
    doc.querySelectorAll("*").forEach(function (element) {
      for (const attribute of Array.from(element.attributes || [])) {
        const name = attribute.name.toLowerCase();
        const value = String(attribute.value || "").trim();
        if (name.startsWith("on") || /^(javascript|vbscript):/i.test(value)) element.removeAttribute(attribute.name);
      }
    });
  }

  class FrameNavigator {
    constructor(frame, options) {
      this.frame = frame;
      this.options = options || {};
      this.href = "";
      this.index = 0;
      this.count = 1;
      this.prefs = normalizePrefs();
      this.locator = null;
      this.ready = false;
      this.onScroll = this.onScroll.bind(this);
      this.onClick = this.onClick.bind(this);
      this.onKey = this.onKey.bind(this);
      this.onResize = this.onResize.bind(this);
      if (typeof ResizeObserver !== "undefined") {
        this.resizeObserver = new ResizeObserver(this.onResize);
        this.resizeObserver.observe(frame);
      }
    }

    document() { return this.frame.contentDocument; }

    async load(url, href, index, count, prefs, locator) {
      this.detach();
      this.href = splitHref(href).path;
      this.index = index;
      this.count = count;
      this.prefs = normalizePrefs(prefs);
      this.locator = locator || null;
      this.ready = false;
      await new Promise((resolve, reject) => {
        const timer = setTimeout(function () { reject(new Error("The chapter took too long to open.")); }, 30000);
        this.frame.onload = function () { clearTimeout(timer); resolve(); };
        this.frame.onerror = function () { clearTimeout(timer); reject(new Error("Cinema could not load this chapter.")); };
        this.frame.src = url;
      });
      const doc = this.document();
      if (!doc || !doc.body) throw new Error("The EPUB chapter is not a readable document.");
      stripExecutableMarkup(doc);
      this.installStyle();
      doc.addEventListener("click", this.onClick, true);
      doc.addEventListener("keydown", this.onKey, true);
      const root = this.scrollRoot();
      root.addEventListener("scroll", this.onScroll, { passive: true });
      const destination = locator || (splitHref(href).fragment ? { href: href } : null);
      if (destination) {
        await new Promise(function (resolve) { requestAnimationFrame(function () { requestAnimationFrame(resolve); }); });
        restoreLocator(doc, destination, this.prefs.flow);
      }
      this.ready = true;
      this.emitLocation();
      if (this.options.onReady) this.options.onReady(doc);
    }

    scrollRoot() {
      const doc = this.document();
      return this.prefs.flow === "paginated" ? doc.body : doc.scrollingElement || doc.documentElement;
    }

    installStyle() {
      const doc = this.document();
      let style = doc.getElementById("cinema-reader-style");
      if (!style) {
        style = doc.createElement("style");
        style.id = "cinema-reader-style";
        (doc.head || doc.documentElement).appendChild(style);
      }
      style.textContent = readerCss(this.prefs);
    }

    currentLocator() {
      if (!this.ready) return this.locator;
      const doc = this.document();
      return makeLocator(this.href, this.index, this.count, progressOf(doc, this.prefs.flow), visibleAnchor(doc, this.prefs.flow));
    }

    async applyPrefs(value) {
      const generation = (this.prefGeneration || 0) + 1;
      this.prefGeneration = generation;
      // Range sliders can fire faster than two animation frames. Every member
      // of one burst must keep the anchor captured before the burst, and only
      // the newest layout may restore it; otherwise late older frames rewind
      // the reader to a different paragraph.
      const anchor = this.prefAnchor || this.currentLocator();
      this.prefAnchor = anchor;
      clearTimeout(this.resizeTimer);
      this.resizeAnchor = null;
      const oldRoot = this.scrollRoot();
      if (oldRoot) oldRoot.removeEventListener("scroll", this.onScroll);
      this.prefs = normalizePrefs(value);
      this.installStyle();
      const newRoot = this.scrollRoot();
      if (newRoot) newRoot.addEventListener("scroll", this.onScroll, { passive: true });
      await new Promise(function (resolve) { requestAnimationFrame(function () { requestAnimationFrame(resolve); }); });
      if (generation !== this.prefGeneration) return;
      restoreLocator(this.document(), anchor, this.prefs.flow);
      this.prefAnchor = null;
      this.emitLocation();
    }

    move(direction) {
      if (!this.ready) return false;
      const root = this.scrollRoot();
      const horizontal = this.prefs.flow === "paginated";
      const current = horizontal ? Math.abs(root.scrollLeft || 0) : root.scrollTop || 0;
      const extent = horizontal ? root.scrollWidth - root.clientWidth : root.scrollHeight - root.clientHeight;
      const step = (horizontal ? root.clientWidth : root.clientHeight) * 0.9;
      if ((direction < 0 && current <= 2) || (direction > 0 && current >= extent - 2)) return false;
      if (horizontal) root.scrollTo({ left: Math.max(0, current + direction * step), behavior: "smooth" });
      else root.scrollBy({ top: direction * step, behavior: "smooth" });
      return true;
    }

    onScroll() {
      // Reflow can dispatch scroll events before the durable anchor has been
      // restored. Publishing those intermediate positions would replace the
      // very locator the reflow is preserving.
      if (this.prefAnchor || this.resizeAnchor) return;
      clearTimeout(this.scrollTimer);
      this.scrollTimer = setTimeout(() => this.emitLocation(), 120);
    }

    onResize() {
      if (!this.ready) return;
      this.resizeAnchor = this.resizeAnchor || this.locator || this.currentLocator();
      clearTimeout(this.resizeTimer);
      this.resizeTimer = setTimeout(() => {
        const anchor = this.resizeAnchor;
        this.resizeAnchor = null;
        if (!this.ready || !anchor) return;
        restoreLocator(this.document(), anchor, this.prefs.flow);
        this.emitLocation();
      }, 100);
    }

    onClick(event) {
      const anchor = event.target && event.target.closest ? event.target.closest("a[href]") : null;
      if (!anchor) return;
      event.preventDefault();
      const href = resolveHref(this.href, anchor.getAttribute("href"));
      if (href && this.options.onNavigate) this.options.onNavigate(href);
      else if (this.options.onBlockedLink) this.options.onBlockedLink();
    }

    onKey(event) {
      if (event.key === "ArrowRight" || event.key === "PageDown") {
        event.preventDefault(); if (!this.move(1) && this.options.onBoundary) this.options.onBoundary(1);
      } else if (event.key === "ArrowLeft" || event.key === "PageUp") {
        event.preventDefault(); if (!this.move(-1) && this.options.onBoundary) this.options.onBoundary(-1);
      }
    }

    emitLocation() {
      const locator = this.currentLocator();
      this.locator = locator;
      if (locator && this.options.onLocation) this.options.onLocation(locator);
    }

    detach() {
      clearTimeout(this.scrollTimer);
      clearTimeout(this.resizeTimer);
      this.prefAnchor = null;
      this.resizeAnchor = null;
      const doc = this.document();
      if (!doc) return;
      doc.removeEventListener("click", this.onClick, true);
      doc.removeEventListener("keydown", this.onKey, true);
      const root = this.prefs.flow === "paginated" ? doc.body : doc.scrollingElement || doc.documentElement;
      if (root) root.removeEventListener("scroll", this.onScroll);
    }

    destroy() {
      this.detach();
      if (this.resizeObserver) this.resizeObserver.disconnect();
      this.frame.onload = null; this.frame.onerror = null; this.frame.src = "about:blank";
    }
  }

  return {
    DEFAULT_PREFS,
    FrameNavigator,
    elementAtPath,
    elementPath,
    makeLocator,
    normalizePath,
    normalizePrefs,
    readerCss,
    resolveHref,
    resourceUrl,
    snippet,
    splitHref,
    totalProgression,
  };
});
