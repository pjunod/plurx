/* Token-free shell shared by Cinema's native offline EPUB readers. */
(function () {
  "use strict";
  const Core = window.PlurxReaderCore;
  let reader = null;

  function element(id) { return document.getElementById(id); }
  function clone(value) { return JSON.parse(JSON.stringify(value)); }
  function post(event, fields) {
    const payload = Object.assign({ event: event }, fields || {});
    if (window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.cinemaOfflineReader) {
      window.webkit.messageHandlers.cinemaOfflineReader.postMessage(payload);
    } else if (window.CinemaOffline && typeof window.CinemaOffline.postMessage === "function") {
      window.CinemaOffline.postMessage(JSON.stringify(payload));
    }
    if (Array.isArray(window.__cinemaOfflineEvents)) window.__cinemaOfflineEvents.push(payload);
  }
  function fail(message) {
    element("loading").classList.add("hidden");
    element("error").textContent = message || "Cinema could not open this EPUB.";
    element("error").classList.remove("hidden");
    post("error", { message: element("error").textContent });
  }
  function orderIndex(href) {
    if (!reader) return -1;
    const path = Core.splitHref(href).path;
    return reader.publication.readingOrder.findIndex(function (link) {
      return Core.splitHref(link.href).path === path;
    });
  }
  function progressionLabel(value) { return Math.round(Math.max(0, Math.min(1, value || 0)) * 100) + "%"; }
  function updateStatus() {
    if (!reader) return;
    element("progress").textContent = progressionLabel(reader.progression);
    element("location").textContent = (reader.index + 1) + " of " + reader.publication.readingOrder.length;
    element("finished").textContent = reader.completed ? "Unfinish" : "Finish";
    element("finished").setAttribute("aria-pressed", String(reader.completed));
    document.querySelectorAll("[data-href]").forEach(function (button) {
      button.setAttribute("aria-current", String(Core.splitHref(button.dataset.href).path === Core.splitHref(reader.href).path));
    });
  }
  function snapshot(event) {
    if (!reader || !reader.locator) return;
    reader.dirty = false;
    post(event || "progress", {
      locator: clone(reader.locator),
      progression: reader.progression || 0,
      completed: !!reader.completed,
      recorded_at: Math.floor(Date.now() / 1000),
      preferences: clone(reader.preferences),
    });
  }
  function scheduleSave() {
    if (!reader) return;
    reader.dirty = true;
    clearTimeout(reader.saveTimer);
    reader.saveTimer = setTimeout(function () { snapshot("progress"); }, 1500);
  }
  function located(locator) {
    if (!reader || !locator) return;
    reader.locator = locator;
    reader.progression = locator.locations && locator.locations.totalProgression || 0;
    updateStatus(); scheduleSave();
  }
  async function navigate(href, locator) {
    if (!reader) return;
    const resolved = orderIndex(href) >= 0 ? href : Core.resolveHref(reader.href || href, href);
    const index = orderIndex(resolved);
    if (index < 0) { post("status", { message: "That link is outside this publication." }); return; }
    const generation = ++reader.generation;
    const link = reader.publication.readingOrder[index];
    const destination = locator || (Core.splitHref(resolved).fragment ? { version: 1, href: resolved } : null);
    element("loading").textContent = "Opening " + (link.title || ("section " + (index + 1))) + "…";
    element("loading").classList.remove("hidden");
    try {
      const url = Core.resourceUrl(reader.resourceBase, resolved);
      if (!url) throw new Error("The chapter link is invalid.");
      reader.index = index; reader.href = resolved;
      await reader.navigator.load(url, resolved, index, reader.publication.readingOrder.length, reader.preferences, destination);
      if (!reader || generation !== reader.generation) return;
      element("loading").classList.add("hidden"); updateStatus();
    } catch (error) { if (reader && generation === reader.generation) fail(error.message); }
  }
  async function step(direction) {
    if (!reader || !reader.navigator) return;
    if (reader.navigator.move(direction)) return;
    const next = reader.index + direction;
    if (next < 0 || next >= reader.publication.readingOrder.length) return;
    await navigate(reader.publication.readingOrder[next].href);
  }
  function tocRows(entries, depth) {
    (entries || []).forEach(function (entry) {
      const button = document.createElement("button");
      button.dataset.href = entry.href; button.style.paddingLeft = (10 + depth * 16) + "px";
      button.textContent = entry.title || "Untitled";
      button.addEventListener("click", function () { element("toc-dialog").close(); navigate(entry.href); });
      element("toc-list").appendChild(button);
      tocRows(entry.children, depth + 1);
    });
  }
  async function updatePreference(key, value) {
    if (!reader) return;
    reader.preferences = Core.normalizePrefs(Object.assign({}, reader.preferences, { [key]: value }));
    await reader.navigator.applyPrefs(reader.preferences); scheduleSave();
  }
  function wireControls() {
    element("close").addEventListener("click", function () { snapshot("progress"); post("close"); });
    element("previous").addEventListener("click", function () { step(-1); });
    element("next").addEventListener("click", function () { step(1); });
    element("toc").addEventListener("click", function () { element("toc-dialog").showModal(); });
    element("settings").addEventListener("click", function () { element("settings-dialog").showModal(); });
    element("finished").addEventListener("click", function () { if (reader) { reader.completed = !reader.completed; updateStatus(); snapshot("progress"); } });
    document.querySelectorAll("[data-close]").forEach(function (button) { button.addEventListener("click", function () { element(button.dataset.close).close(); }); });
    [["font", "font"], ["font-size", "fontSize"], ["line-height", "lineHeight"], ["margin", "margin"], ["theme", "theme"], ["flow", "flow"]].forEach(function (pair) {
      element(pair[0]).addEventListener("change", function (event) {
        const numeric = ["fontSize", "lineHeight", "margin"].includes(pair[1]);
        updatePreference(pair[1], numeric ? Number(event.target.value) : event.target.value);
      });
    });
  }
  function fillPreferences(preferences) {
    element("font").value = preferences.font; element("font-size").value = preferences.fontSize;
    element("line-height").value = preferences.lineHeight; element("margin").value = preferences.margin;
    element("theme").value = preferences.theme; element("flow").value = preferences.flow;
  }
  function showSearchResults(results) {
    const output = element("search-results");
    output.replaceChildren();
    if (!results.length) {
      const empty = document.createElement("div");
      empty.className = "muted"; empty.textContent = "No matches in this publication.";
      output.appendChild(empty); return;
    }
    results.forEach(function (result) {
      const button = document.createElement("button");
      const title = document.createElement("span");
      const snippet = document.createElement("span");
      title.className = "result-title"; title.textContent = result.title;
      snippet.className = "result-snippet"; snippet.textContent = result.snippet;
      button.append(title, snippet);
      button.addEventListener("click", function () {
        element("search-dialog").close(); navigate(result.locator.href, result.locator);
      });
      output.appendChild(button);
    });
  }
  async function searchPublication(event) {
    if (event) event.preventDefault();
    if (!reader) return;
    const query = element("search-query").value.trim();
    if (query.length < 2) {
      element("search-results").innerHTML = '<div class="muted">Enter at least two characters.</div>';
      return;
    }
    const generation = ++reader.searchGeneration;
    element("search-results").innerHTML = '<div class="muted">Searching this publication…</div>';
    const results = [];
    const order = reader.publication.readingOrder;
    const limit = Math.max(1, Number(reader.limits && (reader.limits.markupBytes || reader.limits.markup_bytes)) || 8388608);
    for (let index = 0; index < order.length && results.length < 100; index += 1) {
      if (!reader || generation !== reader.searchGeneration) return;
      const link = order[index];
      const url = Core.resourceUrl(reader.resourceBase, link.href);
      if (!url || !/html|xhtml|xml/i.test(link.type || "")) continue;
      try {
        const response = await fetch(url, { headers: { accept: "application/xhtml+xml,text/html;q=.9" }, referrerPolicy: "no-referrer" });
        const declared = Number(response.headers.get("content-length") || 0);
        const type = response.headers.get("content-type") || link.type || "";
        if (!response.ok || declared > limit || !/html|xhtml|xml/i.test(type)) {
          if (response.body) response.body.cancel();
          continue;
        }
        const bytes = await response.arrayBuffer();
        if (bytes.byteLength > limit) continue;
        const source = new TextDecoder("utf-8").decode(bytes);
        let doc = new DOMParser().parseFromString(source, /xhtml|xml/i.test(type) ? "application/xhtml+xml" : "text/html");
        if (doc.querySelector("parsererror")) doc = new DOMParser().parseFromString(source, "text/html");
        const blocks = Array.from(doc.body ? doc.body.querySelectorAll("h1,h2,h3,h4,h5,h6,p,li,blockquote,pre,figcaption") : []);
        blocks.forEach(function (block, position) {
          if (results.length >= 100) return;
          const excerpt = Core.snippet(block.textContent, query);
          if (!excerpt) return;
          const within = blocks.length > 1 ? position / (blocks.length - 1) : 0;
          results.push({
            title: link.title || "Section " + (index + 1),
            snippet: excerpt,
            locator: Core.makeLocator(link.href, index, order.length, within, block),
          });
        });
      } catch (_) { /* A missing optional spine document does not end search. */ }
    }
    if (reader && generation === reader.searchGeneration) showSearchResults(results);
  }
  window.startOfflineReader = async function (payload) {
    if (reader || !payload || !payload.publication || !payload.resourceBase) return false;
    const publication = payload.publication;
    if (!Array.isArray(publication.readingOrder) || !publication.readingOrder.length) { fail("This EPUB has no readable sections."); return false; }
    const preferences = Core.normalizePrefs(payload.preferences);
    reader = {
      publication: publication, limits: payload.limits || null,
      resourceBase: payload.resourceBase, preferences: preferences,
      locator: payload.locator || null, progression: Number(payload.progression) || 0,
      completed: !!payload.completed, index: 0, href: publication.readingOrder[0].href,
      generation: 0, searchGeneration: 0, dirty: false,
    };
    element("title").textContent = publication.metadata && publication.metadata.title || "Book";
    fillPreferences(preferences); tocRows(publication.toc, 0);
    reader.navigator = new Core.FrameNavigator(element("page"), {
      onLocation: located, onNavigate: navigate,
      onBlockedLink: function () { post("status", { message: "External publication links are blocked." }); },
      onBoundary: step,
    });
    const saved = reader.locator && orderIndex(reader.locator.href) >= 0 ? reader.locator : null;
    await navigate(saved ? saved.href : reader.href, saved);
    if (!reader) return false;
    reader.heartbeat = setInterval(function () { if (reader && reader.dirty) snapshot("progress"); }, 30000);
    post("ready"); return true;
  };
  window.addEventListener("pagehide", function () { if (reader) snapshot("progress"); });
  element("search").addEventListener("click", function () { element("search-dialog").showModal(); element("search-query").focus(); });
  element("search-form").addEventListener("submit", searchPublication);
  wireControls(); post("shell-ready");
})();
