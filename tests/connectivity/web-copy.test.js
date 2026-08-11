"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const connectivity = require("../../crates/plurxd/src/web/connectivity.js");

const CONTRACT = JSON.parse(
  fs.readFileSync(
    path.join(__dirname, "..", "contracts", "connectivity-copy.json"),
    "utf8",
  ),
);

function test(name, run) {
  try {
    run();
    process.stdout.write(`PASS ${name}\n`);
  } catch (error) {
    error.message = `${name}: ${error.message}`;
    throw error;
  }
}

// A DOMException is what a browser hands us when `AbortSignal.timeout` fires;
// Node has the same constructor, so the case table can use the real thing
// rather than an object that merely claims the name.
function abortError() {
  return new DOMException("The operation was aborted.", "AbortError");
}

test("every class in the contract exists in the shipped classifier", () => {
  const contractClasses = Object.keys(CONTRACT.classes).sort();
  const shippedClasses = Object.keys(connectivity.COPY).sort();
  assert.deepEqual(shippedClasses, contractClasses);
  assert.equal(connectivity.SERVER_FALLBACK, CONTRACT.server_fallback);
  assert.deepEqual({ ...connectivity.ACTIONS }, CONTRACT.actions);
});

test("the credentials sentence is the contract's, byte for byte", () => {
  // docs §4: 401/403 during sign-in says this and nothing else, and a client
  // that words it differently has reintroduced the split between "your
  // password is wrong" and "your server is off".
  assert.equal(typeof CONTRACT.credentials_message, "string");
  assert.ok(CONTRACT.credentials_message.length > 0);
  assert.equal(connectivity.CREDENTIALS_MESSAGE, CONTRACT.credentials_message);
  // It is not a class, and must never be produced by one: a transport failure
  // that reached the viewer wearing this sentence is the exact bug §4 closes.
  for (const id of Object.keys(CONTRACT.classes)) {
    for (const context of [{ server: "Living Room" }, {}]) {
      const got = connectivity.describe(id, context);
      for (const field of ["title", "detail", "short"]) {
        assert.notEqual(
          got[field],
          CONTRACT.credentials_message,
          `${id} ${field}`,
        );
      }
    }
  }
  assert.equal(connectivity.isClass("credentials"), false);
});

test("describe() is byte-identical to the contract for every class", () => {
  // Three interpolation contexts: a display name, an origin with no name, and
  // nothing at all — the three states the web client can actually be in.
  const contexts = [
    [{ server: "Living Room" }, "Living Room"],
    [{ origin: "http://192.168.4.14:32400" }, "http://192.168.4.14:32400"],
    [{ server: "  ", origin: "" }, CONTRACT.server_fallback],
    [{}, CONTRACT.server_fallback],
  ];
  for (const [id, expected] of Object.entries(CONTRACT.classes)) {
    for (const [context, name] of contexts) {
      const got = connectivity.describe(id, context);
      const where = `${id} with ${JSON.stringify(context)}`;
      assert.equal(got.id, id, where);
      for (const field of ["title", "detail", "short"]) {
        assert.equal(
          got[field],
          expected[field].split("{server}").join(name),
          `${where} ${field}`,
        );
        assert.ok(!got[field].includes("{server}"), `${where} ${field} placeholder`);
      }
      assert.deepEqual(
        got.actions.map((action) => action.id),
        expected.actions,
        `${where} actions`,
      );
      assert.deepEqual(
        got.actions.map((action) => action.label),
        expected.actions.map((action) => CONTRACT.actions[action]),
        `${where} action labels`,
      );
    }
  }
});

test("every class offers retry, and only the address-fixable ones offer a server change", () => {
  // The half this test used to skip: which classes earn `change_server`. A
  // timeout or a 500 does not, because changing the address when the server is
  // merely busy throws away a working configuration (docs §1).
  const ADDRESS_FIXABLE = ["unreachable", "unknown_host", "insecure"];
  for (const [id, expected] of Object.entries(CONTRACT.classes)) {
    assert.ok(expected.actions.includes("retry"), `${id} must offer retry`);
    assert.ok(
      connectivity.describe(id).actions.some((action) => action.id === "retry"),
      `${id} must render retry`,
    );
    const offersChange = connectivity
      .describe(id)
      .actions.some((action) => action.id === "change_server");
    assert.equal(
      offersChange,
      ADDRESS_FIXABLE.includes(id),
      `${id} change_server`,
    );
    assert.equal(
      expected.actions.includes("change_server"),
      ADDRESS_FIXABLE.includes(id),
      `${id} change_server in the contract`,
    );
  }
});

test("a surface draws every action it can honour, and always draws retry", () => {
  // renderableActions is the decision the render site used to make with an
  // `if` nobody could test. Web passes canChangeServer:false — the app is
  // served by the server it talks to, so there is no second address — and the
  // native clients, which hold a server list, pass true.
  for (const [id, expected] of Object.entries(CONTRACT.classes)) {
    const withPicker = connectivity
      .renderableActions(id, { canChangeServer: true })
      .map((action) => action.id);
    const webOnly = connectivity
      .renderableActions(id, { canChangeServer: false })
      .map((action) => action.id);
    assert.deepEqual(withPicker, expected.actions, `${id} with a server picker`);
    assert.deepEqual(
      webOnly,
      expected.actions.filter((action) => action !== "change_server"),
      `${id} without a server picker`,
    );
    // every_error_offers_retry, made executable: retry survives every
    // combination, so no capability flag can produce a dead end.
    assert.ok(webOnly.includes("retry"), `${id} retry without a picker`);
    assert.ok(withPicker.includes("retry"), `${id} retry with a picker`);
    for (const drawn of [withPicker, webOnly]) {
      assert.deepEqual(drawn, [...new Set(drawn)], `${id} draws no duplicates`);
    }
  }
  // Default is the conservative one: a caller that says nothing gets no
  // change-server button rather than a button that cannot work.
  assert.deepEqual(
    connectivity.renderableActions("unreachable").map((a) => a.id),
    ["retry"],
  );
  // Labels come from the contract on this path too.
  assert.deepEqual(
    connectivity
      .renderableActions("unreachable", { canChangeServer: true })
      .map((a) => a.label),
    [CONTRACT.actions.retry, CONTRACT.actions.change_server],
  );
  // And an unplaceable id still yields a drawable surface.
  assert.deepEqual(
    connectivity.renderableActions("no_such_class").map((a) => a.id),
    ["retry"],
  );
});

test("the long deadline covers exactly the endpoints that legitimately run long", () => {
  // docs §3. The budget is a property of what is being asked for, so it is
  // asserted here rather than trusted to whichever call site remembered.
  const { api: SHORT, long: LONG } = connectivity.DEADLINES;
  assert.equal(SHORT, 15000);
  assert.equal(LONG, 120000);
  const rows = [
    // Playback preparation: ffprobe with no timeout of its own behind it.
    ["/files/91/decision?caps=h264,hevc&force=auto", LONG],
    ["/files/91/hls/sessions", LONG],
    // The three the first pass missed, each slow for a documented reason.
    ["/system/storage", LONG],
    ["/items/7/reanalyze", LONG],
    ["/items/7/refresh-artwork", LONG],
    // Ordinary reads and writes keep the short budget.
    ["/server", SHORT],
    ["/me", SHORT],
    ["/activity/detail", SHORT],
    ["/activity/sessions/3", SHORT],
    ["/auth/login", SHORT],
    ["/libraries/2/items?limit=60&offset=0&sort=title", SHORT],
    ["/libraries/2/scan", SHORT],
    ["/items/7", SHORT],
    ["/items/7/photo?size=thumb", SHORT],
    ["/settings", SHORT],
    ["/system/logs?level=info&limit=300", SHORT],
    ["/trakt/sync", SHORT],
    // Near misses: the pattern is anchored, so neither a prefix nor a suffix
    // can smuggle an ordinary call into the long budget.
    ["/files/91/decisions", SHORT],
    ["/files/91/hls/sessions/22", SHORT],
    ["/system/storage/usage", SHORT],
    ["/items/7/reanalyze/all", SHORT],
    ["/xitems/7/reanalyze", SHORT],
    ["", SHORT],
  ];
  for (const [path, expected] of rows) {
    assert.equal(connectivity.deadlineFor(path), expected, path);
  }
  assert.equal(connectivity.deadlineFor(null), SHORT);
  assert.equal(connectivity.deadlineFor(undefined), SHORT);
  // A query string never decides the budget.
  assert.equal(
    connectivity.deadlineFor("/system/storage?probe=1"),
    connectivity.deadlineFor("/system/storage"),
  );
});

test("an unplaceable class falls back to unknown rather than to a native string", () => {
  for (const id of [null, undefined, "", "no_such_class", 7]) {
    assert.equal(connectivity.describe(id).id, "unknown", String(id));
  }
});

test("the browser's observable failures map onto the documented classes", () => {
  const rows = [
    // The browser collapses refused / DNS / TLS into one opaque TypeError.
    ["TypeError while online", { error: new TypeError("Failed to fetch"), online: true }, "unreachable"],
    ["TypeError with no onLine reading", { error: new TypeError("Failed to fetch") }, "unreachable"],
    ["TypeError while offline", { error: new TypeError("Failed to fetch"), online: false }, "offline"],
    // Our own deadline firing outranks everything: it is the one cause we know.
    ["AbortError from our deadline", { error: abortError(), online: true }, "timeout"],
    ["AbortError while offline", { error: abortError(), online: false }, "timeout"],
    ["TimeoutError", { error: new DOMException("timed out", "TimeoutError"), online: true }, "timeout"],
    ["500 response", { response: { ok: false, status: 500 } }, "server_error"],
    ["503 response", { response: { ok: false, status: 503 } }, "server_error"],
    ["200 with an unusable body", { response: { ok: true, status: 200 }, error: new SyntaxError("bad json") }, "server_error"],
    // The server's own sentence survives — "library not found" keeps saying so.
    ["404 with a server message", { response: { ok: false, status: 404 } }, null],
    ["400 with a server message", { response: { ok: false, status: 400 } }, null],
    // Auth keeps its pre-existing sign-out path.
    ["401", { response: { ok: false, status: 401 } }, null],
    ["403", { response: { ok: false, status: 403 } }, null],
    ["a success", {}, null],
    ["something else entirely", { error: new RangeError("nope"), online: true }, "unknown"],
  ];
  for (const [label, input, expected] of rows) {
    assert.equal(connectivity.classify(input), expected, label);
  }
  assert.equal(connectivity.classify(), null, "no argument");
});

test("no rendered string leaks the transport text it was classified from", () => {
  // docs/CLIENT-CONNECTIVITY.md §7: a suite that only checks the happy strings
  // would pass a client that appended "(Failed to fetch)" to every one of them.
  const natives = [
    "Failed to fetch",
    "NetworkError when attempting to fetch resource.",
    "Load failed",
    "The operation was aborted.",
    "ERR_CONNECTION_REFUSED",
    "TypeError",
    "DOMException",
  ];
  const contexts = [{ server: "Living Room" }, { origin: "http://plurx.local" }, {}];
  for (const id of Object.keys(CONTRACT.classes)) {
    for (const context of contexts) {
      const got = connectivity.describe(id, context);
      const rendered = [got.title, got.detail, got.short]
        .concat(got.actions.map((action) => action.label))
        .join("\n");
      for (const native of natives) {
        assert.ok(
          !rendered.includes(native),
          `${id} leaked ${JSON.stringify(native)}`,
        );
      }
    }
  }
  // And the same for the classifier's inputs: classify() returns an id, never
  // any part of the error it was handed.
  const classified = connectivity.classify({
    error: new TypeError("Failed to fetch"),
    online: true,
  });
  assert.equal(classified, "unreachable");
  const described = connectivity.describe(classified, { server: "Living Room" });
  assert.ok(!JSON.stringify(described).includes("Failed to fetch"));
});
