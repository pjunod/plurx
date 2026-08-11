(function (root, factory) {
  const connectivity = factory();
  if (typeof module === "object" && module.exports) module.exports = connectivity;
  root.PlurxConnectivity = connectivity;
})(typeof globalThis !== "undefined" ? globalThis : this, function () {
  "use strict";

  // `tests/contracts/connectivity-copy.json` is the source of truth for every
  // string below; this table is a transcription of it. The four client suites
  // read the JSON, and `tests/connectivity/web-copy.test.js` proves this file
  // and that file still say the same thing — so a drift here is a test failure,
  // not a shipped regression.
  const SERVER_FALLBACK = "the server";

  // The one sentence a client says *instead of* a class. It lives in the
  // contract for the same reason the classes do: a client that words it
  // differently has quietly reintroduced the split the taxonomy closed
  // (docs/CLIENT-CONNECTIVITY.md §4).
  const CREDENTIALS_MESSAGE = "Wrong username or password";

  const ACTIONS = Object.freeze({
    retry: "Try again",
    change_server: "Change server",
  });

  const COPY = Object.freeze({
    offline: Object.freeze({
      title: "You're offline",
      detail: "This device isn't connected to a network.",
      short: "You're offline.",
      actions: Object.freeze(["retry"]),
    }),
    unreachable: Object.freeze({
      title: "Can't reach {server}",
      detail:
        "The network is working, but the server didn't answer. It may be powered off, restarting, or on another network.",
      short: "Can't reach {server}.",
      actions: Object.freeze(["retry", "change_server"]),
    }),
    unknown_host: Object.freeze({
      title: "Can't find {server}",
      detail:
        "Nothing on this network answers to that address. If the server moved, point Cinema at its new one.",
      short: "Can't find {server}.",
      actions: Object.freeze(["retry", "change_server"]),
    }),
    timeout: Object.freeze({
      title: "No answer from {server}",
      detail:
        "The server accepted the connection but didn't answer in time. It may be busy or still starting up.",
      short: "No answer from {server}.",
      actions: Object.freeze(["retry"]),
    }),
    insecure: Object.freeze({
      title: "Couldn't connect securely to {server}",
      detail:
        "The secure connection failed. The server's certificate may have changed or expired.",
      short: "Couldn't connect securely to {server}.",
      actions: Object.freeze(["retry", "change_server"]),
    }),
    server_error: Object.freeze({
      title: "Error from {server}",
      detail:
        "The server answered with an error. Nothing is wrong with this device or your network.",
      short: "Error from {server}.",
      actions: Object.freeze(["retry"]),
    }),
    unknown: Object.freeze({
      title: "Something went wrong",
      detail: "Cinema couldn't complete that request.",
      short: "Something went wrong.",
      actions: Object.freeze(["retry"]),
    }),
  });

  function isClass(id) {
    return typeof id === "string" && Object.hasOwn(COPY, id);
  }

  // 401 and 403 have their own path — sign out and re-authenticate — and are
  // deliberately absent from the taxonomy: an expired token dressed as a
  // network failure sends people to check their router. `null` means "not a
  // connectivity class", so the caller keeps whatever it did before.
  function isAuthStatus(status) {
    return status === 401 || status === 403;
  }

  function abortLike(error) {
    if (!error) return false;
    const name = error.name || "";
    return name === "AbortError" || name === "TimeoutError";
  }

  function typeErrorLike(error) {
    if (!error) return false;
    if (typeof TypeError === "function" && error instanceof TypeError) return true;
    return error.name === "TypeError";
  }

  // The decision tree in docs/CLIENT-CONNECTIVITY.md §2.1, in order. `online`
  // is the caller's `navigator.onLine` reading, passed in rather than read
  // here: this file touches no globals so the tree can be driven under Node.
  //
  // `unreachable` is the catch-all for a live-network TypeError on purpose.
  // fetch() collapses connection-refused, DNS failure, TLS failure and a CORS
  // rejection into one opaque error, and "the server may be powered off,
  // restarting, or on another network" stays honest for all of them. Claiming
  // `unknown_host` or `insecure` from evidence the browser never supplies
  // would be a confident lie — those classes exist for the native clients,
  // whose errors are typed.
  function classify({ error = null, response = null, online = true } = {}) {
    if (response) {
      if (isAuthStatus(response.status)) return null;
      // A body the client could not use is the server answering wrongly, which
      // is `server_error` even when the status said 200.
      if (response.status >= 500 || (response.ok && error)) return "server_error";
      // Every other 4xx carries the server's own sentence ("library not
      // found"). Replacing it with a class would be a downgrade.
      return null;
    }
    if (!error) return null;
    if (abortLike(error)) return "timeout";
    // Trustworthy in the negative direction only: `false` means there is no
    // interface at all. `true` means very little, which is why it is not used
    // to conclude anything.
    if (online === false) return "offline";
    if (typeErrorLike(error)) return "unreachable";
    return "unknown";
  }

  // Display name → origin → "the server". No title *begins* with {server}, so
  // a bare origin never has to carry a sentence's first capital.
  function serverLabel({ server = null, origin = null } = {}) {
    const named = typeof server === "string" ? server.trim() : "";
    if (named) return named;
    const at = typeof origin === "string" ? origin.trim() : "";
    if (at) return at;
    return SERVER_FALLBACK;
  }

  function interpolate(text, name) {
    return text.split("{server}").join(name);
  }

  // Anything the caller cannot place lands on `unknown` rather than falling
  // through to a native string; that is the whole reason the class exists.
  function describe(classId, context = {}) {
    const id = isClass(classId) ? classId : "unknown";
    const copy = COPY[id];
    const name = serverLabel(context);
    return {
      id,
      title: interpolate(copy.title, name),
      detail: interpolate(copy.detail, name),
      short: interpolate(copy.short, name),
      actions: copy.actions.map((action) => ({
        id: action,
        label: ACTIONS[action],
      })),
    };
  }

  // What a surface should actually draw, as opposed to what the class carries.
  // The contract's action list is the platform-independent answer; a surface
  // may subtract from it only what it genuinely cannot honour, which on web is
  // `change_server`: this app is served BY the server it talks to, so there is
  // no second address to point it at and the button would sign the viewer out
  // onto the same login form. The native clients hold a server list and pass
  // `canChangeServer: true`.
  //
  // It is a function here rather than an `if` at the render site so the rule is
  // a testable decision instead of a line someone can delete: `retry` survives
  // every combination, which is `every_error_offers_retry` made executable.
  function renderableActions(classId, { canChangeServer = false } = {}) {
    return describe(classId).actions.filter(
      (action) => action.id !== "change_server" || canChangeServer,
    );
  }

  // Request deadlines (docs §3). Stated once, here, because the budget is a
  // property of WHAT is being asked for and not of who is asking — as a literal
  // at each call site it is a rule only the sites that remembered obey, and the
  // ones that forget turn a slow success into "No answer from the server".
  const DEADLINES = Object.freeze({ api: 15000, long: 120000 });

  // Endpoints whose server-side work is legitimately slower than a JSON read.
  // Each is here because something behind it has no timeout of its own or a
  // much larger one; none of them is merely "usually a bit slow".
  const LONG_DEADLINE_ROUTES = Object.freeze([
    // Playback preparation. `/decision` reaches `markers_for`, which can fall
    // through to a live `ffprobe -show_chapters` subprocess with no timeout,
    // behind an availability stat that may sit on a spun-down NAS.
    /^\/files\/[^/]+\/decision$/,
    // Opening an HLS session waits on ffmpeg actually starting the encode.
    /^\/files\/[^/]+\/hls\/sessions$/,
    // The storage probe is capped at 120s server-side and may take as long as
    // a sleeping array takes to answer.
    /^\/system\/storage$/,
    // A sequential re-probe of every file on the item, deliberately so.
    /^\/items\/[^/]+\/reanalyze$/,
    // A forced enrich over the item and its ancestors: several sequential
    // metadata-provider round trips.
    /^\/items\/[^/]+\/refresh-artwork$/,
  ]);

  function deadlineFor(path) {
    const route = String(path == null ? "" : path).split("?")[0].split("#")[0];
    return LONG_DEADLINE_ROUTES.some((pattern) => pattern.test(route))
      ? DEADLINES.long
      : DEADLINES.api;
  }

  return Object.freeze({
    SERVER_FALLBACK,
    CREDENTIALS_MESSAGE,
    ACTIONS,
    COPY,
    DEADLINES,
    isClass,
    classify,
    serverLabel,
    describe,
    renderableActions,
    deadlineFor,
  });
});
