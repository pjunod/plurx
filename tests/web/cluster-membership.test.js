"use strict";

// The admin cluster-membership panel, tested against the shipped index.html
// rather than against a copy of it.
//
// What this file is actually protecting: the WORDS. The membership API already
// has its own Rust gate for lifecycle, admin gating, and refusal codes. The
// thing only this surface can get wrong is telling an operator that two voters
// are redundancy — docs/CLUSTERING-PLAN.md §7.2 makes two-node HA a stated
// non-goal precisely because two voters need both machines for every write and
// therefore survive no failure. A settings screen that renders that as a green
// "highly available" is the single most damaging bug this panel can ship, and
// no assertion on the API field would catch it. So the assertions below read
// the rendered sentences.

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");

const INDEX = path.join(__dirname, "../../crates/plurxd/src/web/index.html");
const SHIPPED_UI = fs.readFileSync(INDEX, "utf8");

// Same extraction contract as tests/playback/web-policy.test.js: every function
// borrowed here is declared at column zero in one inline <script>, so the next
// top-level declaration terminates it and no brace parsing is needed. A rename
// fails loudly rather than silently testing nothing.
const DECLARATIONS = ["\nfunction ", "\nasync function "];
function shippedSource(name) {
  const start = DECLARATIONS.map((kind) =>
    SHIPPED_UI.indexOf(`${kind}${name}(`),
  ).find((at) => at !== -1);
  assert.notEqual(start, undefined, `index.html no longer declares ${name}`);
  const rest = SHIPPED_UI.slice(start + 1);
  const ends = DECLARATIONS.map((kind) => rest.indexOf(kind, 1)).filter(
    (at) => at !== -1,
  );
  const end = ends.length ? Math.min(...ends) : -1;
  return (end === -1 ? rest : rest.slice(0, end)).trimEnd();
}

// The panel functions call a handful of the app's own helpers. Borrowing the
// shipped ones rather than stubbing them keeps the escaping and the "3m ago"
// formatting under test too.
const BORROWED = [
  "esc",
  "fmtAgo",
  "replicationText",
  "clusterQuorum",
  "clusterStateView",
  "membershipRefusalText",
  "clusterNodeRow",
  "clusterRefusalHtml",
  "joinPanel",
  "joinTokenHtml",
  "clusterPanel",
];

// One sandbox per test so a mutation of ME or CLUSTER_REFUSAL cannot leak into
// the next assertion.
function sandbox({ isAdmin = true, refusal = null, token = null } = {}) {
  const source = `
    let ME = ${JSON.stringify({ is_admin: isAdmin })};
    let CLUSTER_REFUSAL = ${JSON.stringify(refusal)};
    let CLUSTER_TOKEN = ${JSON.stringify(token)};
    ${BORROWED.map(shippedSource).join("\n")}
    return { ${BORROWED.join(", ")} };
  `;
  return new Function(source)();
}

let failures = 0;
function test(name, run) {
  try {
    run();
    process.stdout.write(`PASS ${name}\n`);
  } catch (error) {
    failures += 1;
    process.stdout.write(`FAIL ${name}\n${error && error.stack}\n`);
  }
}

// Words that assert redundancy or fault tolerance. Any of these in the
// two-voter banner is the exact failure this panel exists to avoid.
const REDUNDANCY_CLAIMS = [
  "highly available",
  "high availability",
  "high-availability",
  " ha ",
  "fault tolerant",
  "fault-tolerant",
  "failover",
  "fully replicated",
  "healthy",
];

function assertNoRedundancyClaim(text, where) {
  const haystack = ` ${text.toLowerCase().replace(/[^a-z]+/g, " ")} `;
  for (const claim of REDUNDANCY_CLAIMS) {
    assert.equal(
      haystack.includes(claim),
      false,
      `${where} claims redundancy with ${JSON.stringify(claim)}: ${text}`,
    );
  }
}

const REPLICATION = {
  backend: "hiqlite",
  health: "healthy",
  clustered: true,
  last_applied_term: 4,
  last_applied_index: 912,
  checked_at: 1_760_000_000,
  explanation: "This node has applied every known entry.",
};

function node(id, raftId, role, extra = {}) {
  return {
    node_id: id,
    raft_id: raftId,
    role,
    reachable: true,
    last_seen_at: Date.now(),
    ...extra,
  };
}

function status(availability, nodes) {
  return { availability, nodes, replication: REPLICATION };
}

// ---- the two-voter state is the point -------------------------------------

test("two voters render as a reconfiguration in progress, never as redundancy", () => {
  const ui = sandbox();
  const view = ui.clusterStateView(
    status("degraded_reconfiguration", [
      node("node-a", 1, "voter"),
      node("node-b", 2, "voter"),
    ]),
  );
  assert.equal(view.tone, "warn");
  assertNoRedundancyClaim(`${view.title} ${view.body}`, "the two-voter banner");
  // It must say what the state is, not merely avoid saying the wrong thing.
  assert.match(view.title, /reconfiguration/i);
  assert.match(view.body, /survives no failure/i);
  assert.match(view.body, /third node/i);
});

test("the two-voter state reaches the rendered panel as those words", () => {
  const ui = sandbox();
  const html = ui.clusterPanel({
    cluster: status("degraded_reconfiguration", [
      node("node-a", 1, "voter"),
      node("node-b", 2, "voter"),
    ]),
    sys: { replication: REPLICATION },
  });
  // Asserting on the HTML, not on the projection: the issue's acceptance is the
  // text an operator reads, and a banner computed correctly but dropped from
  // the template would pass a view-only assertion.
  assert.match(html, /Reconfiguration in progress/);
  assert.match(html, /survives no failure/);
  assert.equal(html.includes("clstate warn"), true);
  assertNoRedundancyClaim(html.replace(/<[^>]*>/g, " "), "the two-voter panel");
});

test("three voters may say redundant, and count the loss they survive", () => {
  const ui = sandbox();
  const view = ui.clusterStateView(
    status("high_availability", [
      node("node-a", 1, "voter"),
      node("node-b", 2, "voter"),
      node("node-c", 3, "voter"),
    ]),
  );
  assert.equal(view.tone, "good");
  assert.match(view.title, /3 voters/);
  assert.match(view.body, /2 of the 3 voters/);
  assert.match(view.body, /one node is down/i);
});

test("quorum arithmetic matches Raft majorities", () => {
  const ui = sandbox();
  assert.deepEqual(ui.clusterQuorum(1), { majority: 1, tolerates: 0 });
  assert.deepEqual(ui.clusterQuorum(3), { majority: 2, tolerates: 1 });
  assert.deepEqual(ui.clusterQuorum(4), { majority: 3, tolerates: 1 });
  assert.deepEqual(ui.clusterQuorum(5), { majority: 3, tolerates: 2 });
});

// ---- the never-joined install is the common one ---------------------------

test("a never-joined SQLite install reads as normal, not as something missing", () => {
  const ui = sandbox();
  const view = ui.clusterStateView({
    unavailable: true,
    code: "membership_unavailable",
    message: "cluster membership is unavailable on this backend",
  });
  assert.equal(view.tone, "calm");
  assert.match(view.body, /nothing is missing/i);
  assert.match(view.body, /opt-in/i);
  // Nothing on this panel may read as a defect on a healthy single box.
  for (const alarm of ["error", "failed", "degraded", "warning", "problem"]) {
    assert.equal(
      view.body.toLowerCase().includes(alarm),
      false,
      `the never-joined body raises ${JSON.stringify(alarm)}: ${view.body}`,
    );
  }
});

test("a never-joined install is offered no roster and no join control", () => {
  const ui = sandbox();
  const html = ui.clusterPanel({
    cluster: { unavailable: true, code: "membership_unavailable" },
    sys: {
      replication: {
        backend: "sqlite",
        health: "healthy",
        clustered: false,
        explanation: "Watch state is stored on this server only.",
      },
    },
  });
  assert.match(html, /Not clustered/);
  // A "Create a join token" button here would mint nothing and refuse — the
  // dead end this panel is supposed to not have.
  assert.equal(html.includes("Create a join token"), false);
  assert.equal(html.includes("<table"), false);
  // The one lag answer is still the shared #233 projection and its renderer.
  assert.match(html, /SQLite single-node/);
});

test("a replicated one-node install says one node is a complete configuration", () => {
  const ui = sandbox();
  const view = ui.clusterStateView(
    status("single_node", [node("node-a", 1, "voter")]),
  );
  assert.equal(view.tone, "calm");
  assert.match(view.body, /complete, supported configuration/i);
});

test("a learner is named as not voting yet", () => {
  const ui = sandbox();
  const view = ui.clusterStateView(
    status("single_node", [
      node("node-a", 1, "voter"),
      node("node-b", 2, "learner"),
    ]),
  );
  assert.match(view.body, /catching up as a learner/i);
  assert.match(view.body, /do(es)? not vote yet/i);
});

// ---- every refusal is a sentence with a next step -------------------------

// All four exist in crates/plurx-core/src/cluster/membership.rs today.
const REFUSAL_CODES = [
  "removal_would_lose_quorum",
  "node_owns_offline_work",
  "cluster_leader_removal_refused",
  "cluster_node_not_found",
];

test("each removal refusal renders as an actionable sentence, not a code", () => {
  const ui = sandbox();
  for (const code of REFUSAL_CODES) {
    const text = ui.membershipRefusalText(code, "raw wire message");
    assert.equal(
      text.includes(code),
      false,
      `${code} leaked its raw code into the operator's sentence`,
    );
    assert.equal(
      text.includes("raw wire message"),
      false,
      `${code} fell through to the raw wire message`,
    );
    assert.ok(text.length > 80, `${code} has no real explanation: ${text}`);
    assert.match(text, /\.$/, `${code} is not a sentence: ${text}`);
  }
});

test("node_owns_offline_work tells the operator what to do next", () => {
  const ui = sandbox();
  const text = ui.membershipRefusalText("node_owns_offline_work", "");
  // Expected to be the common refusal until offline-package resolution lands,
  // so "this node owns offline work" alone would be a dead end.
  assert.match(text, /offline download/i);
  assert.match(text, /delete them or let them expire/i);
  assert.match(text, /then remove this node again/i);
});

test("an unknown refusal still says something true", () => {
  const ui = sandbox();
  assert.match(
    ui.membershipRefusalText("some_future_code", "the server said no"),
    /the server said no/,
  );
  assert.match(
    ui.membershipRefusalText(null, ""),
    /refused this removal and gave no reason/,
  );
});

test("a refusal is rendered into the panel against the node it names", () => {
  const ui = sandbox({
    refusal: { node_id: "node-b", code: "node_owns_offline_work", message: "" },
  });
  const html = ui.clusterRefusalHtml();
  assert.match(html, /node-b was not removed/);
  assert.match(html, /offline download/i);
});

// ---- roster rendering -----------------------------------------------------

test("last_seen_at is read as milliseconds, not seconds", () => {
  const ui = sandbox();
  // The API documents Unix milliseconds. Feeding fmtAgo() — which takes seconds
  // — the raw field prints a node seen a minute ago as decades stale.
  const html = ui.clusterNodeRow(
    node("node-a", 1, "voter", { last_seen_at: Date.now() - 120_000 }),
  );
  assert.match(html, /2m ago/);
  assert.equal(/\d{2,}[yd] ago/.test(html), false, `stale-looking row: ${html}`);
});

test("a roster row shows only the privacy-safe fields the API exposes", () => {
  const ui = sandbox();
  const row = ui.clusterNodeRow(node("node-a", 7, "voter"));
  assert.match(row, /node-a/);
  assert.match(row, />7</);
  assert.match(row, /voter/);
  assert.match(row, /reachable/);
  // Addresses and token material are not in the payload and must not be invented.
  for (const leak of ["addr", "http://", "raft_address", "api_address"]) {
    assert.equal(row.includes(leak), false, `roster row exposes ${leak}`);
  }
});

test("an unreachable node says so rather than showing a blank", () => {
  const ui = sandbox();
  const row = ui.clusterNodeRow(node("node-c", 3, "voter", { reachable: false }));
  assert.match(row, /not reachable/);
});

// ---- admin gating ---------------------------------------------------------

test("a non-admin gets no membership panel at all", () => {
  const ui = sandbox({ isAdmin: false });
  const html = ui.clusterPanel({
    cluster: status("high_availability", [
      node("node-a", 1, "voter"),
      node("node-b", 2, "voter"),
      node("node-c", 3, "voter"),
    ]),
    sys: { replication: REPLICATION },
  });
  // Not "an empty table" — nothing, including no node ids.
  assert.equal(html, "");
});

test("Settings itself still turns a non-admin away before any of this loads", () => {
  // The panel guard above is belt and braces; this is the door. Asserted on the
  // source because the redirect needs a location and a DOM to execute.
  const viewSettings = shippedSource("viewSettings");
  assert.match(viewSettings, /if\(!ME\.is_admin\)\{\s*location\.hash="#\/";\s*return;\s*\}/);
});

// ---- the join token is bearer material ------------------------------------

test("the join token is never written to browser storage or a URL", () => {
  // An absence property, so it is asserted over the source of every function
  // that touches the token rather than by running one of them.
  const handlers = [
    "loadCluster",
    "joinPanel",
    "joinTokenHtml",
    "mintJoinToken",
    "clearJoinToken",
    "copyJoinToken",
    "forgetJoinToken",
    "clusterPanel",
  ].map((name) => `${name}:\n${shippedSource(name)}`);
  for (const source of handlers) {
    for (const sink of [
      "localStorage",
      "sessionStorage",
      "indexedDB",
      "document.cookie",
      "location.hash",
      "location.search",
      "console.log",
      "console.error",
    ]) {
      assert.equal(
        source.includes(sink),
        false,
        `a join-token handler reaches ${sink}:\n${source}`,
      );
    }
  }
  // And nothing anywhere in the app stores the token under any key.
  assert.equal(
    /(local|session)Storage\.setItem\([^)]*(TOKEN|token)[^)]*\)/.test(
      SHIPPED_UI.replace(/localStorage\.setItem\("plurx_token"/g, ""),
    ),
    false,
    "some code path persists a token through Storage.setItem",
  );
});

test("the token is shown once, with what it is and how long it lasts", () => {
  const ui = sandbox({
    token: {
      token: "plxjoin:v1:aaaa:bbbb",
      expires_at: Date.now() + 600_000,
      raft_id: 4,
    },
  });
  const html = ui.joinPanel();
  assert.match(html, /plxjoin:v1:aaaa:bbbb/);
  assert.match(html, /Shown once/);
  assert.match(html, /10 minutes/);
  assert.match(html, /Raft id 4/);
  // It must say what the holder of the token can do, not just that it is secret.
  assert.match(html, /complete authority\s+to join a node/i);
  assert.match(html, /Done — clear it/);
});

test("clearing the token removes it from the rendered panel", () => {
  const ui = sandbox({ token: null });
  const html = ui.joinPanel();
  assert.equal(html.includes("plxjoin:"), false);
  assert.match(html, /Create a join token/);
});

// ---- wiring ---------------------------------------------------------------

test("the Cluster tab is registered and dispatched", () => {
  assert.match(SHIPPED_UI, /\["cluster","Cluster"\]/);
  assert.match(SHIPPED_UI, /if\(tab==="cluster"\)\s*return clusterPanel\(d\)/);
  // Fetched on first open, not with the rest of Settings: the common install
  // refuses this endpoint, and asking every visit would spend a failed request.
  assert.match(SHIPPED_UI, /if\(tab==="cluster"\) loadCluster\(\)/);
  assert.equal(
    /Promise\.all\(\[[^\]]*cluster\/nodes/.test(SHIPPED_UI),
    false,
    "the roster is fetched eagerly with the rest of Settings",
  );
});

test("this panel adds no endpoint beyond the three the node API already ships", () => {
  const called = new Set();
  const CALL = /api\(\s*([`"'])(\/cluster[^`"']*)\1/g;
  for (const [, , route] of SHIPPED_UI.matchAll(CALL)) {
    // A template hole is a node id, and a node id is a fact about one cluster
    // rather than about the routes this panel speaks to.
    called.add(route.replace(/\$\{[^}]*\}/g, "<id>"));
  }
  assert.deepEqual(
    [...called].sort(),
    ["/cluster/join-tokens", "/cluster/nodes", "/cluster/nodes/<id>"],
  );
});

test("the typed refusal code survives the fetch helper", () => {
  // membershipRefusalText is keyed on the stable code, so an api() that drops it
  // would silently downgrade every refusal to the raw wire message.
  const api = shippedSource("api");
  assert.match(api, /code=b\.code\|\|null/);
  assert.match(api, /error\.code=code/);
});

process.exit(failures ? 1 : 0);
