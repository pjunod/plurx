#!/usr/bin/env node
"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");

const ROOT = path.resolve(__dirname, "../..");
const Core = require(path.join(ROOT, "crates/plurxd/src/web/reader.js"));
const INDEX = fs.readFileSync(path.join(ROOT, "crates/plurxd/src/web/index.html"), "utf8");
const SERVER = fs.readFileSync(path.join(ROOT, "crates/plurxd/src/http/web.rs"), "utf8");

function test(name, fn) {
  try { fn(); process.stdout.write(`ok - ${name}\n`); }
  catch (error) { process.stderr.write(`not ok - ${name}\n${error.stack}\n`); process.exitCode = 1; }
}

test("presentation preferences are local, finite, and bounded", () => {
  assert.deepEqual(Core.normalizePrefs(null), Core.DEFAULT_PREFS);
  assert.deepEqual(Core.normalizePrefs({
    font: "bogus", fontSize: 10000, lineHeight: -4, margin: "x", theme: "sepia", flow: "scrolled",
  }), {
    font: "publisher", fontSize: 220, lineHeight: 1.1, margin: 7, theme: "sepia", flow: "scrolled",
  });
});

test("publication links resolve within the archive and refuse remote or escaping targets", () => {
  assert.equal(Core.resolveHref("OEBPS/Text/one.xhtml", "../Images/cover.jpg"), "OEBPS/Images/cover.jpg");
  assert.equal(Core.resolveHref("OEBPS/Text/one.xhtml", "#middle"), "OEBPS/Text/one.xhtml#middle");
  assert.equal(Core.resolveHref("OEBPS/Text/one.xhtml", "../../../secret"), null);
  assert.equal(Core.resolveHref("OEBPS/Text/one.xhtml", "https://attacker.invalid/leak"), null);
  assert.equal(Core.resolveHref("OEBPS/Text/one.xhtml", "//attacker.invalid/leak"), null);
});

test("capability URLs encode path components and never add the account bearer", () => {
  const url = Core.resourceUrl("/api/v1/publication/session-cap/", "OEBPS/Text/chapter one.xhtml#part 2");
  assert.equal(url, "/api/v1/publication/session-cap/OEBPS/Text/chapter%20one.xhtml#part%202");
  assert.ok(!url.includes("token="));
});

test("locators keep paragraph identity separate from mutable pagination", () => {
  const body = { children: [] };
  const section = { children: [], parentElement: body };
  const paragraph = {
    id: "paragraph-42", textContent: "  The same paragraph survives a font change.  ",
    children: [], parentElement: section,
  };
  body.children.push(section); section.children.push(paragraph);
  paragraph.ownerDocument = { body };
  const locator = Core.makeLocator("Text/chapter.xhtml", 2, 4, 0.25, paragraph);
  assert.equal(locator.href, "Text/chapter.xhtml#paragraph-42");
  assert.equal(locator.locations.progression, 0.25);
  assert.equal(locator.locations.totalProgression, 0.5625);
  assert.deepEqual(locator.cinema.path, [0, 0]);
  assert.equal(Core.elementAtPath(body, locator.cinema.path), paragraph);
});

test("reader CSS changes presentation without changing locator inputs", () => {
  const paginated = Core.readerCss({ font: "serif", fontSize: 135, theme: "dark", flow: "paginated" });
  const scrolled = Core.readerCss({ font: "serif", fontSize: 135, theme: "dark", flow: "scrolled" });
  assert.match(paginated, /column-width/);
  assert.match(paginated, /font-size:135%/);
  assert.match(scrolled, /max-width:52rem/);
  assert.doesNotMatch(scrolled, /column-width/);
});

test("search snippets are bounded and publication-local inputs remain plain text", () => {
  const result = Core.snippet("Before ".repeat(20) + "needle" + " after".repeat(30), "needle");
  assert.ok(result.startsWith("…"));
  assert.ok(result.endsWith("…"));
  assert.ok(result.length < 170);
  assert.equal(Core.snippet("nothing here", "needle"), null);
});

test("the shipped frame allows parent inspection but no authored execution or navigation", () => {
  const frame = INDEX.match(/<iframe class="reader-frame"[^>]+>/);
  assert.ok(frame, "reader frame exists");
  assert.match(frame[0], /sandbox="allow-same-origin"/);
  for (const forbidden of ["allow-scripts", "allow-forms", "allow-popups", "allow-top-navigation", "allow-downloads"])
    assert.ok(!frame[0].includes(forbidden), `frame must omit ${forbidden}`);
  assert.match(INDEX, /ReaderCore\.resourceUrl\(READER\.open\.resource_base/);
  assert.ok(!/tok\(READER\.open\.resource_base/.test(INDEX), "resource capability must not carry account token");
});

test("reader assets are embedded in the single binary and loaded before the route", () => {
  assert.match(SERVER, /include_str!\("\.\.\/web\/reader\.js"\)/);
  assert.match(SERVER, /include_str!\("\.\.\/web\/reader\.css"\)/);
  assert.ok(INDEX.indexOf("/assets/reader.js") < INDEX.indexOf("async function viewReader"));
});

process.on("exit", () => {
  if (!process.exitCode) process.stdout.write("reader contracts passed\n");
});
