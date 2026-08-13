"use strict";

/*
 * The playback lab's shaping layer, tested without a browser.
 *
 * Everything here is the part of the stall-recovery harness that decides
 * whether a run means anything: the profile grammar, the rate limiter, the
 * cliff, and the scoring that separates "the link was shaped and the session
 * failed to adapt" from "the harness never shaped anything." A green
 * stall-recovery run whose shaper silently did nothing would be worse than no
 * harness at all, so those failure modes are asserted directly.
 */

const assert = require("node:assert/strict");
const fs = require("node:fs");
const fsp = fs.promises;
const http = require("node:http");
const net = require("node:net");
const os = require("node:os");
const path = require("node:path");
const { spawnSync } = require("node:child_process");

const lab = require("../../scripts/playback-lab");

const ROOT = path.resolve(__dirname, "..", "..");
const LAB = path.join(ROOT, "scripts", "playback-lab");

const failures = [];
const pending = [];

function test(name, run) {
  pending.push([name, run]);
}

async function runAll() {
  for (const [name, run] of pending) {
    try {
      await run();
      process.stdout.write(`PASS ${name}\n`);
    } catch (error) {
      failures.push(`${name}: ${error.message}`);
      process.stdout.write(`FAIL ${name}: ${error.message}\n`);
    }
  }
}

function cli(args) {
  return spawnSync(process.execPath, [LAB, ...args], { encoding: "utf8", cwd: ROOT });
}

// ---------------------------------------------------------------- profiles

test("a malformed network profile is refused, with the reason named", () => {
  const rejected = [
    ["8mbps", "no descent"],
    ["8mbps-to-8mbps", "flat is not a cliff"],
    ["1mbps-to-8mbps", "rising is not a cliff"],
    ["8mbps-to-0mbps", "zero rate"],
    ["-8mbps-to-1mbps", "negative rate"],
    ["8gbps-to-1gbps", "unknown unit"],
    ["eightmbps-to-1mbps", "not a number"],
    ["8mbps-to-1.5mbps@0", "cliff delay must be positive"],
    ["8mbps-to-1.5mbps@later", "cliff delay must be a number"],
    ["8mbps-to-1.5mbps@12@20", "multiple cliff delays"],
    ["fast-link", "unknown named profile"],
    [true, "flag with no value"],
  ];
  for (const [spec, why] of rejected) {
    assert.throws(() => lab.parseNetworkProfile(spec), Error, `${JSON.stringify(spec)} (${why}) should be refused`);
  }
});

test("the acceptance profile parses into a recorded, descending cliff", () => {
  const profile = lab.parseNetworkProfile("8mbps-to-1.5mbps");
  assert.equal(profile.id, "8mbps-to-1.5mbps");
  assert.equal(profile.stages[0].kbps, 8000);
  assert.equal(profile.stages[1].kbps, 1500);
  assert.ok(profile.cliff_after_seconds > 0, "the cliff point must be recorded");
  assert.ok(profile.description.includes("8 Mb/s"), "the named profile keeps its description");
  assert.equal(lab.parseNetworkProfile("8mbps-to-1.5mbps@20").cliff_after_seconds, 20);
  assert.equal(lab.parseNetworkProfile("1500kbps-to-500kbps").stages[0].kbps, 1500);
});

test("no profile means no shaping, so existing suites keep today's behavior", () => {
  for (const spec of [undefined, null, "", false, "none"]) {
    assert.equal(lab.parseNetworkProfile(spec), null, JSON.stringify(spec));
  }
});

// ------------------------------------------------------------ token bucket

test("the bucket enforces its rate over time and repays debt", () => {
  const bucket = new lab.TokenBucket(1000, 0); // 125 bytes/ms
  const first = bucket.reserve(125_000, 0); // 1000 kb of payload
  assert.ok(first > 900 && first < 1010, `a 1000 kb claim on a 1000 kb/s link waits ~1s, got ${first}ms`);
  const second = bucket.reserve(125_000, 1000);
  assert.ok(second > 900, "the next claim waits again rather than riding free");
});

test("a rate drop cannot be paid for with credit earned before the cliff", () => {
  const bucket = new lab.TokenBucket(8000, 0);
  bucket.refill(10_000); // sit idle and fill to capacity at the high rate
  const bankedHigh = bucket.tokens;
  bucket.setRate(1500, 10_000);
  assert.ok(bucket.tokens <= bucket.capacity, "credit is clamped to the new, smaller bucket");
  assert.ok(bucket.tokens < bankedHigh, "pre-cliff credit does not survive the cliff intact");
});

test("a non-positive rate is refused rather than dividing by zero", () => {
  assert.throws(() => new lab.TokenBucket(0, 0), /positive/);
  assert.throws(() => new lab.TokenBucket(Number.NaN, 0), /positive/);
});

// ------------------------------------------------------------- media paths

test("media supply is told apart from the control plane", () => {
  for (const route of [
    "/hls/abc/index.m3u8", "/hls/abc/000001.ts", "/api/v1/files/12/direct",
    "/api/v1/files/12/stream.mp4", "/api/v1/offline/media/tok/master.m3u8",
  ]) assert.equal(lab.isMediaPath(route), true, route);
  for (const route of ["/", "/api/v1/system", "/api/v1/files/12/decision?caps=x", "/assets/hls.min.js"]) {
    assert.equal(lab.isMediaPath(route), false, route);
  }
});

test("proxy diagnostics strip bearer-token query strings", () => {
  const route = lab.diagnosticPath("/api/v1/files/12/direct?token=secret-lab-token&part=1");
  assert.equal(route, "/api/v1/files/12/direct");
  assert.doesNotMatch(route, /secret-lab-token|token=/);
});

// ------------------------------------------------------------- the shaper

async function withOrigin(bytes, run) {
  const origin = http.createServer((request, response) => {
    response.writeHead(200, { "content-type": "application/octet-stream" });
    response.end(Buffer.alloc(bytes, 0x61));
  });
  await new Promise((resolve) => origin.listen(0, "127.0.0.1", resolve));
  const url = `http://127.0.0.1:${origin.address().port}`;
  try {
    return await run(url);
  } finally {
    await new Promise((resolve) => origin.close(resolve));
  }
}

test("the shaper holds the link, applies the cliff, and records both stages", async () => {
  await withOrigin(128 * 1024, async (origin) => {
    const profile = lab.parseNetworkProfile("1mbps-to-0.25mbps");
    const shaper = new lab.ShapingProxy(profile, origin);
    const base = await shaper.start();
    try {
      const before = Date.now();
      const first = await fetch(`${base}/api/v1/files/1/direct`);
      await first.arrayBuffer();
      const firstMs = Date.now() - before;
      assert.ok(firstMs > 500, `128 KB over a 1 Mb/s link cannot arrive in ${firstMs}ms`);

      const cliffAt = shaper.applyCliff();
      assert.ok(cliffAt > 0, "the cliff point is recorded");

      const afterStart = Date.now();
      const second = await fetch(`${base}/api/v1/files/1/direct`);
      await second.arrayBuffer();
      const secondMs = Date.now() - afterStart;
      assert.ok(secondMs > firstMs * 2, `the post-cliff fetch must be far slower (${firstMs}ms then ${secondMs}ms)`);

      const telemetry = shaper.telemetry();
      assert.equal(telemetry.stages.length, 2);
      assert.ok(telemetry.cliff_applied_at_ms > 0);
      for (const stage of telemetry.stages) {
        assert.ok(stage.measured_kbps <= stage.kbps * 1.25,
          `${stage.label} delivered ${stage.measured_kbps} kb/s over a ${stage.kbps} kb/s cap`);
        assert.ok(stage.media_bytes > 0, `${stage.label} attributed its bytes to media`);
      }
      assert.deepEqual(telemetry.transport_errors, []);
    } finally {
      await shaper.close();
    }
    await assert.rejects(
      () => fetch(`${base}/api/v1/files/1/direct`, { signal: AbortSignal.timeout(1000) }),
      /fetch failed|aborted/i,
      "closing the shaper releases its listening socket",
    );
  });
});

test("concurrent reservations are rescheduled at the cliff instead of bursting", async () => {
  await withOrigin(16 * 1024, async (origin) => {
    const profile = lab.parseNetworkProfile("8mbps-to-1mbps");
    const shaper = new lab.ShapingProxy(profile, origin);
    const base = await shaper.start();
    try {
      const requests = Array.from({ length: 8 }, () =>
        fetch(`${base}/api/v1/files/1/direct`).then((response) => response.arrayBuffer()));
      await new Promise((resolve) => setTimeout(resolve, 5));
      shaper.applyCliff();
      await Promise.all(requests);
      const after = shaper.telemetry().stages[1];
      assert.ok(after.media_bytes > 0, "concurrent media crossed the post-cliff stage");
      assert.ok(after.measured_kbps <= after.kbps * 1.25,
        `concurrent requests delivered ${after.measured_kbps} kb/s over a ${after.kbps} kb/s cap`);
    } finally {
      await shaper.close();
    }
  });
});

test("one non-reading response cannot block another shaped connection", async () => {
  const origin = http.createServer((request, response) => {
    response.writeHead(200, { "content-type": "application/octet-stream" });
    response.end(Buffer.alloc(request.url.includes("/files/1/") ? 64 * 1024 * 1024 : 16 * 1024, 0x61));
  });
  await new Promise((resolve) => origin.listen(0, "127.0.0.1", resolve));
  const target = `http://127.0.0.1:${origin.address().port}`;
  const shaper = new lab.ShapingProxy(lab.parseNetworkProfile("400mbps-to-200mbps"), target);
  let blockedDrainResolve;
  let blockedDrainSeen = false;
  const blockedDrain = new Promise((resolve) => { blockedDrainResolve = resolve; });
  const waitForDrain = shaper.waitForDrain.bind(shaper);
  shaper.waitForDrain = (sink, cancelled) => {
    const waiting = waitForDrain(sink, cancelled);
    const timer = setTimeout(() => {
      if (blockedDrainSeen) return;
      blockedDrainSeen = true;
      blockedDrainResolve();
    }, 100);
    return waiting.finally(() => clearTimeout(timer));
  };
  const base = await shaper.start();
  const stuck = net.connect(shaper.port, "127.0.0.1");
  try {
    await new Promise((resolve, reject) => {
      stuck.once("connect", resolve);
      stuck.once("error", reject);
    });
    stuck.write(
      "GET /api/v1/files/1/direct HTTP/1.1\r\n" +
      "Host: 127.0.0.1\r\nConnection: keep-alive\r\n\r\n",
    );
    stuck.pause();
    await Promise.race([
      blockedDrain,
      new Promise((_, reject) => setTimeout(() => reject(new Error("non-reading client never backpressured")), 3000)),
    ]);

    const started = Date.now();
    const healthy = await fetch(`${base}/hls/healthy/seg0.ts`, { signal: AbortSignal.timeout(1000) });
    assert.equal((await healthy.arrayBuffer()).byteLength, 16 * 1024);
    assert.ok(Date.now() - started < 1000, "a healthy connection must not wait for another socket to drain");
    assert.deepEqual(shaper.telemetry().transport_errors, []);
  } finally {
    stuck.destroy();
    await shaper.close();
    origin.closeAllConnections?.();
    await new Promise((resolve) => origin.close(resolve));
  }
});

test("closing interrupts an active throttled response", async () => {
  await withOrigin(1024 * 1024, async (origin) => {
    const shaper = new lab.ShapingProxy(lab.parseNetworkProfile("8kbps-to-4kbps"), origin);
    const base = await shaper.start();
    const request = fetch(`${base}/api/v1/files/1/direct`)
      .then((response) => response.arrayBuffer())
      .catch(() => null);
    await new Promise((resolve) => setTimeout(resolve, 50));
    await Promise.race([
      shaper.close(),
      new Promise((_, reject) => setTimeout(() => reject(new Error("shaper close timed out")), 1000)),
    ]);
    await request;
  });
});

test("browser restarts cancel upstream streams without phantom bytes or link debt", async () => {
  let largeClosedResolve;
  let largeClosedCount = 0;
  const largeClosed = new Promise((resolve) => { largeClosedResolve = resolve; });
  const origin = http.createServer((request, response) => {
    const large = request.url.includes("/files/1/");
    response.writeHead(200, { "content-type": "application/octet-stream" });
    if (!large) {
      response.end(Buffer.alloc(1024, 0x61));
      return;
    }
    response.flushHeaders();
    const timer = setInterval(() => response.write(Buffer.alloc(16 * 1024, 0x61)), 10);
    response.once("close", () => {
      clearInterval(timer);
      largeClosedCount += 1;
      if (largeClosedCount === 6) largeClosedResolve();
    });
  });
  await new Promise((resolve) => origin.listen(0, "127.0.0.1", resolve));
  const target = `http://127.0.0.1:${origin.address().port}`;
  const shaper = new lab.ShapingProxy(lab.parseNetworkProfile("128kbps-to-64kbps"), target);
  const base = await shaper.start();
  try {
    const controllers = Array.from({ length: 6 }, () => new AbortController());
    const requests = controllers.map((controller) =>
      fetch(`${base}/api/v1/files/1/direct`, { signal: controller.signal })
        .then((response) => response.arrayBuffer())
        .catch(() => null));
    await new Promise((resolve) => setTimeout(resolve, 30));
    for (const controller of controllers) controller.abort();
    await Promise.all(requests);
    await Promise.race([
      largeClosed,
      new Promise((_, reject) => setTimeout(() => reject(new Error("canceled upstream stayed open")), 750)),
    ]);
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(Object.values(shaper.agent.sockets).flat().length, 0,
      "canceled transfers release every active upstream Agent socket");

    const smallStarted = Date.now();
    const small = await fetch(`${base}/api/v1/files/2/direct`);
    assert.equal((await small.arrayBuffer()).byteLength, 1024);
    assert.ok(Date.now() - smallStarted < 500,
      "the canceled 16 KiB reservation must not delay the replacement request");

    const telemetry = shaper.telemetry();
    assert.deepEqual(telemetry.transport_errors, [], "a deliberate player restart is not a link fault");
    assert.equal(telemetry.stages[0].media_bytes, 1024,
      "only the replacement response, not the canceled slice, is counted as delivered");
  } finally {
    await shaper.close();
    await new Promise((resolve) => origin.close(resolve));
  }
});

test("browser startup dead time cannot dilute a pre-cliff shaper leak", async () => {
  const shaper = new lab.ShapingProxy(
    lab.parseNetworkProfile("8mbps-to-1.5mbps"), "http://127.0.0.1:1",
  );
  let now = 1_000;
  shaper.now = () => now;
  shaper.startedAt = 0;
  shaper.stages[0].entered_at_ms = 0;
  shaper.record(500_000, false); // page assets before the first frame
  now = 30_000;
  shaper.beginEvidence();
  // Twelve Mb/s delivered over the one active second after 30 seconds of
  // browser setup. Measuring from proxy bind would dilute this to 387 kb/s.
  shaper.record(750_000, true);
  now = 31_000;
  shaper.record(750_000, true);
  shaper.applyCliff();
  const telemetry = shaper.telemetry();
  assert.equal(telemetry.stages[0].measured_kbps, 12_000);
  assert.equal(lab.scoreRecovery(CRITERIA, observation({ shaping: telemetry })).outcome, "shaping");
  await shaper.close();
});

test("idle time after the last byte cannot dilute a pre-cliff shaper leak", async () => {
  const shaper = new lab.ShapingProxy(
    lab.parseNetworkProfile("8mbps-to-1.5mbps"), "http://127.0.0.1:1",
  );
  let now = 0;
  shaper.now = () => now;
  shaper.startedAt = 0;
  shaper.stages[0].entered_at_ms = 0;
  shaper.record(3_000_000, true);
  now = 3_500;
  shaper.record(3_000_000, true);
  now = 11_500; // the player's buffer is full until the cliff
  shaper.applyCliff();
  const telemetry = shaper.telemetry();
  assert.equal(telemetry.stages[0].held_seconds, 3.5);
  assert.equal(telemetry.stages[0].measured_kbps, 13_714.3);
  assert.equal(lab.scoreRecovery(CRITERIA, observation({ shaping: telemetry })).outcome, "shaping");
  await shaper.close();
});

test("slower delivery after a mid-stage burst cannot dilute a shaper leak", async () => {
  const shaper = new lab.ShapingProxy(
    lab.parseNetworkProfile("8mbps-to-1.5mbps"), "http://127.0.0.1:1",
  );
  let now = 0;
  shaper.now = () => now;
  shaper.startedAt = 0;
  shaper.stages[0].entered_at_ms = 0;
  // A 13.6 Mb/s one-second burst followed by slower delivery averages to only
  // 4.32 Mb/s over the whole active stage. The burst must remain observable.
  shaper.record(850_000, true);
  now = 999;
  shaper.record(850_000, true);
  now = 5_000;
  shaper.record(1_000_000, true);
  shaper.applyCliff();
  const telemetry = shaper.telemetry();
  assert.equal(telemetry.stages[0].measured_kbps, 4_320);
  assert.equal(telemetry.stages[0].peak_kbps, 13_600);
  assert.equal(lab.scoreRecovery(CRITERIA, observation({ shaping: telemetry })).outcome, "shaping");
  await shaper.close();
});

test("one immutable shaping snapshot is reused after the evidence window closes", async () => {
  const shaper = new lab.ShapingProxy(
    lab.parseNetworkProfile("8mbps-to-1.5mbps"), "http://127.0.0.1:1",
  );
  let now = 1_000;
  shaper.now = () => now;
  shaper.startedAt = 0;
  shaper.stages[0].entered_at_ms = 0;
  shaper.record(1_000, true);
  const frozen = shaper.freezeTelemetry();
  now = 60_000;
  shaper.record(5_000, true);
  assert.strictEqual(shaper.telemetry(), frozen);
  assert.equal(shaper.telemetry().stages[0].bytes, 1_000,
    "later cleanup activity cannot rewrite the retained throughput account");
  await shaper.close();
});

test("a shaper that cannot bind says so, and leaves no listening socket behind", async () => {
  const profile = lab.parseNetworkProfile("8mbps-to-1.5mbps");
  const shaper = new lab.ShapingProxy(profile, "http://127.0.0.1:1");
  await assert.rejects(
    () => shaper.start(() => Promise.reject(new Error("EACCES"))),
    /network shaping is unavailable/,
  );
  await shaper.close();
  const silent = new lab.ShapingProxy(profile, "http://127.0.0.1:1");
  await assert.rejects(() => silent.start(() => Promise.resolve(0)), /bound no port/);
  await silent.close();
});

// ------------------------------------------------------------- the scoring

function sample(atMs, overrides = {}) {
  return {
    at_ms: atMs,
    absolute_time: atMs / 1000,
    height: 360,
    ttff_ms: 500,
    stalls: 0,
    paused: false,
    seeking: false,
    ready_state: 4,
    media_error: null,
    ...overrides,
  };
}

function healthyTimeline(overrides = {}) {
  return Array.from({ length: 46 }, (_, index) => sample(index * 1000, overrides));
}

function observation(extra = {}) {
  return {
    shaping: {
      cliff_applied_at_ms: 12_000,
      stages: [
        { label: "before-cliff", kbps: 8000, measured_kbps: 4100, measured_media_kbps: 4000 },
        { label: "after-cliff", kbps: 1500, measured_kbps: 1480, measured_media_kbps: 1400 },
      ],
    },
    baseline_clock_rate: 1.0,
    baseline_error: null,
    pre_cliff_runway_seconds: 10,
    timeline: healthyTimeline(),
    ...extra,
  };
}

const CRITERIA = {
  shaping_tolerance: 1.25,
  baseline_minimum_clock_rate: 0.9,
  maximum_automatic_restarts: 1,
  maximum_upgrades_per_60s: 1,
  recovery_deadline_seconds: 30,
  sustained_seconds: 10,
  sustained_minimum_clock_rate: 0.9,
};

test("a clean recovery passes", () => {
  const score = lab.scoreRecovery(CRITERIA, observation());
  assert.deepEqual(score.errors, []);
  assert.equal(score.outcome, "passed");
});

test("a cliff that was never applied fails as a shaping fault, not a player fault", () => {
  const score = lab.scoreRecovery(CRITERIA, observation({
    shaping: { cliff_applied_at_ms: null, stages: observation().shaping.stages },
  }));
  assert.equal(score.outcome, "shaping");
  assert.match(score.errors[0], /never applied the cliff/);
});

test("a shaper that leaked more than its cap fails as a shaping fault", () => {
  const leaked = observation();
  leaked.shaping.stages[1].measured_kbps = 6000;
  const score = lab.scoreRecovery(CRITERIA, leaked);
  assert.equal(score.outcome, "shaping");
  assert.match(score.errors[0], /leaked/);
});

test("a shaping transport error invalidates the recovery verdict", () => {
  const broken = observation();
  broken.shaping.transport_errors = ["GET /hls/session/000001.ts: connection reset"];
  const score = lab.scoreRecovery(CRITERIA, broken);
  assert.equal(score.outcome, "shaping");
  assert.match(score.errors[0], /transport error/);
});

test("a baseline that was never sustainable cannot be read as a recovery verdict", () => {
  const score = lab.scoreRecovery(CRITERIA, observation({ baseline_clock_rate: 0.2 }));
  assert.equal(score.outcome, "browser_playback");
  assert.match(score.errors[0], /not sustainable/);

  const errored = lab.scoreRecovery(CRITERIA, observation({ baseline_error: "media error 3" }));
  assert.equal(errored.outcome, "browser_playback");
  assert.match(errored.errors[0], /before the cliff/);
});

test("a session that never recovers fails", () => {
  const frozen = healthyTimeline().map((row, index) =>
    index < 5 ? row : { ...row, absolute_time: 5 });
  const score = lab.scoreRecovery(CRITERIA, observation({ timeline: frozen }));
  assert.ok(score.errors.some((error) => /never held/.test(error)), score.errors.join("; "));
  assert.notEqual(score.outcome, "passed");
});

test("a fatal post-cliff media error cannot score as recovery", () => {
  const failed = healthyTimeline();
  failed[failed.length - 1] = {
    ...failed[failed.length - 1], media_error: 3, paused: true, ready_state: 0,
  };
  const score = lab.scoreRecovery(CRITERIA, observation({ timeline: failed }));
  assert.equal(score.outcome, "browser_playback");
  assert.match(score.errors[0], /media error 3/);
});

test("a link with headroom that still starved is reported as a supply fault", () => {
  const frozen = healthyTimeline().map((row, index) => (index < 5 ? row : { ...row, absolute_time: 5 }));
  const starved = observation({ timeline: frozen });
  starved.shaping.stages[1].measured_media_kbps = 200; // the link was barely used
  const score = lab.scoreRecovery(CRITERIA, starved);
  assert.equal(score.outcome, "server_supply");
});

test("recovery later than the deadline fails", () => {
  const late = healthyTimeline().map((row, index) =>
    index < 34 ? { ...row, absolute_time: 0 } : { ...row, absolute_time: (index - 34) });
  const score = lab.scoreRecovery(CRITERIA, observation({ timeline: late }));
  assert.ok(score.errors.some((error) => /recovery took|never held/.test(error)), score.errors.join("; "));
  assert.equal(score.outcome, "recovery");
});

test("too many restarts and too many upgrades each fail on their own", () => {
  const churn = healthyTimeline().map((row, index) => ({
    ...row,
    height: [360, 480, 720][index % 3],
    ttff_ms: 500 + (index % 3),
  }));
  const score = lab.scoreRecovery(CRITERIA, observation({ timeline: churn }));
  assert.ok(score.errors.some((error) => /automatic restarts/.test(error)), score.errors.join("; "));
  assert.ok(score.errors.some((error) => /upgrades inside one 60s window/.test(error)), score.errors.join("; "));
  assert.equal(score.outcome, "recovery");
});

test("one downgrade is one restart, counted once even though TTFF also resets", () => {
  const stepped = healthyTimeline().map((row, index) =>
    index < 5 ? { ...row, height: 720, ttff_ms: 400 } : { ...row, height: 360, ttff_ms: 900 });
  const events = lab.rungHistory(stepped);
  assert.equal(events.length, 1, "a height change and its TTFF reset are one restart");
  assert.equal(events[0].direction, "down");
  assert.equal(events[0].from_height, 720);
  assert.equal(events[0].to_height, 360);
});

test("a new player attempt is a restart even when its rung and TTFF match", () => {
  const restarted = healthyTimeline().map((row, index) => ({
    ...row,
    height: 720,
    ttff_ms: 1570,
    attempt_reason: index < 20 ? "cold-start" : "stall-restart",
  }));
  const events = lab.rungHistory(restarted);
  assert.equal(events.length, 1,
    "the attempt identity is authoritative when presentation symptoms are unchanged");
  assert.equal(events[0].direction, "restart");
  assert.equal(events[0].attempt_reason, "stall-restart");
});

// Both of the following were real defects, found by running the harness for
// real against a shaped link rather than by reading the code: the first scored
// a session that plainly never recovered as a pass, and the second reported the
// recovery time as a negative 56-year interval.
test("draining the pre-cliff buffer is not recovery", () => {
  // Ten seconds of banked runway keep the clock at 1.0x, then it collapses —
  // the exact shape a real 8 -> 1.5 Mb/s run produced with no controller.
  const drained = Array.from({ length: 46 }, (_, index) => sample(index * 1000, {
    absolute_time: index <= 10 ? index : 10 + (index - 10) * 0.15,
    stalls: index <= 10 ? 0 : Math.floor((index - 10) / 8),
  }));
  const score = lab.scoreRecovery(CRITERIA, observation({ timeline: drained }));
  assert.ok(score.errors.some((error) => /never held/.test(error)), score.errors.join("; "));
  assert.notEqual(score.outcome, "passed");
  assert.equal(lab.timeToSustained(drained, 10, 0.9), null,
    "a sustained window must reach the end of the observation, not stop where the buffer ran out");
});

test("an observation too short to outlast the banked runway is refused as harness error", () => {
  const score = lab.scoreRecovery(
    { ...CRITERIA, recovery_observe_seconds: 12, sustained_seconds: 10 },
    observation({ pre_cliff_runway_seconds: 10 }),
  );
  assert.equal(score.outcome, "harness");
  assert.match(score.errors[0], /cannot outlast/);
});

test("timeline timestamps are relative to the cliff, so they read forward from zero", () => {
  const score = lab.scoreRecovery(CRITERIA, observation());
  assert.ok(score.metrics.recovered_after_ms >= 0,
    `recovery time must not be negative, got ${score.metrics.recovered_after_ms}`);
  assert.ok(score.metrics.recovered_after_ms < 60_000, "and must be within the observation window");
});

test("the upgrade rate is measured over a sliding 60s window", () => {
  const events = [
    { at_ms: 0, direction: "up" },
    { at_ms: 30_000, direction: "up" },
    { at_ms: 59_000, direction: "up" },
    { at_ms: 120_000, direction: "up" },
    { at_ms: 10_000, direction: "down" },
  ];
  assert.equal(lab.peakUpgradeRate(events), 3);
  assert.equal(lab.peakUpgradeRate([]), 0);
});

test("sustained playback is only credited when the clock really advanced", () => {
  const good = Array.from({ length: 20 }, (_, index) => sample(index * 1000));
  assert.equal(lab.timeToSustained(good, 10, 0.9), 0);
  const stalled = good.map((row) => ({ ...row, absolute_time: 0 }));
  assert.equal(lab.timeToSustained(stalled, 10, 0.9), null);
  const restalled = good.map((row, index) => ({ ...row, stalls: index > 3 ? 1 : 0 }));
  assert.equal(lab.timeToSustained(restalled, 10, 0.9), 4000);
});

test("a restart cannot erase later stalls by resetting the player counter", () => {
  const reset = healthyTimeline().map((row, index) => ({
    ...row,
    height: index < 20 ? 720 : 480,
    attempt_reason: index < 20 ? "cold-start" : "quality",
    stalls: index < 20 ? 5 : index < 30 ? 0 : index < 38 ? 1 : 2,
  }));
  assert.equal(lab.rungHistory(reset).length, 1, "the counter reset occurs at one permitted restart");
  assert.equal(lab.timeToSustained(reset, 10, 0.9), null,
    "two post-restart stalls leave too little clean runway to prove recovery");
  const score = lab.scoreRecovery(CRITERIA, observation({ timeline: reset }));
  assert.equal(score.outcome, "recovery");
  assert.match(score.errors[0], /never held/);
});

test("an equal-count restart cannot credit recovery before post-restart stalls", () => {
  const reset = healthyTimeline().map((row, index) => ({
    ...row,
    height: 720,
    attempt_reason: index < 3 ? "cold-start" : "stall-restart",
    stalls: index === 0 ? 0 : index === 1 ? 1 : 2,
  }));
  const events = lab.rungHistory(reset);
  assert.equal(events.length, 1);
  assert.equal(events[0].at_ms, 3000);
  assert.equal(lab.timeToSustained(reset, 10, 0.9), 3000,
    "the first clean window starts after the equal-count restart, not before its two stalls");
  const score = lab.scoreRecovery(CRITERIA, observation({ timeline: reset }));
  assert.equal(score.outcome, "passed");
  assert.equal(score.metrics.recovered_after_ms, 3000);
});

// ------------------------------------------------------------ the artifact

const ARTIFACT = {
  schema_version: 1,
  generated_at: "2026-08-13T19:00:00.000Z",
  suite: "smoke",
  browser: { browser: "Chrome" },
  network_profile: null,
  summary: { total: 1, passed: 1, failed: 0 },
  results: [{
    name: "direct-h264-aac-1080 :: auto :: steady",
    status: "passed",
    operation: "steady",
    decision: { method: "direct_play", delivery: { mode: "file" } },
    metrics: { actual_method: "direct", copy_hls: false, encoder: null, decoded_dimensions: "1920x1080" },
    end: { tried_fallback: false },
    errors: [],
    warnings: [],
  }],
};

async function withTempDir(run) {
  const directory = await fsp.mkdtemp(path.join(os.tmpdir(), "plurx-shaping-test-"));
  try {
    return await run(directory);
  } finally {
    await fsp.rm(directory, { recursive: true, force: true });
  }
}

test("a missing, unparseable, or foreign artifact is refused rather than read as empty", async () => {
  await withTempDir(async (directory) => {
    await assert.rejects(() => lab.loadArtifact(path.join(directory, "absent.json")), /could not be read/);
    await assert.rejects(() => lab.loadArtifact(undefined), /--json PATH is required/);

    const broken = path.join(directory, "broken.json");
    await fsp.writeFile(broken, "{ not json", "utf8");
    await assert.rejects(() => lab.loadArtifact(broken), /not valid JSON/);

    const foreign = path.join(directory, "foreign.json");
    await fsp.writeFile(foreign, JSON.stringify({ hello: "world" }), "utf8");
    await assert.rejects(() => lab.loadArtifact(foreign), /not a playback-lab report/);

    const truncated = path.join(directory, "truncated.json");
    await fsp.writeFile(truncated, JSON.stringify({ schema_version: 1 }), "utf8");
    await assert.rejects(() => lab.loadArtifact(truncated), /not a playback-lab report/);

    const good = path.join(directory, "good.json");
    await fsp.writeFile(good, JSON.stringify(ARTIFACT), "utf8");
    assert.equal((await lab.loadArtifact(good)).suite, "smoke");
  });
});

test("fatal artifacts retain the failure without retaining credentials", async () => {
  await withTempDir(async (directory) => {
    const json = path.join(directory, "fatal.json");
    const junit = path.join(directory, "fatal.xml");
    const error = new Error("fetch /hls/1.m3u8?token=secret-lab-token with Bearer secret-bearer failed");
    await lab.writeFatalRunReport({ suite: "stall-recovery", json, junit }, error);
    const retained = `${await fsp.readFile(json, "utf8")}\n${await fsp.readFile(junit, "utf8")}`;
    assert.doesNotMatch(retained, /secret-lab-token|secret-bearer/);
    assert.match(retained, /<redacted>/);
  });
});

test("normalization removes every run-local value and keeps the behavioral shape", () => {
  const noisy = JSON.parse(JSON.stringify(ARTIFACT));
  noisy.results[0].errors = [
    "session 6f1c9d02-4b7a-4a1e-9f2b-1c3d4e5f6a7b failed at 127.0.0.1:52341",
    "runtime /var/folders/xy/T/plurx-playback-lab-Ab3d9/data went away after 1234 ms",
    "started at 2026-08-13T19:00:00.000Z",
  ];
  const normalized = lab.normalizeTrace(noisy);
  const text = JSON.stringify(normalized);
  assert.doesNotMatch(text, /6f1c9d02/, "UUIDs are scrubbed");
  assert.doesNotMatch(text, /52341/, "ports are scrubbed");
  assert.doesNotMatch(text, /plurx-playback-lab-Ab3d9/, "temp paths are scrubbed");
  assert.doesNotMatch(text, /2026-08-13T19/, "wall-clock is scrubbed");
  assert.doesNotMatch(text, /1234 ms/, "durations are scrubbed");
  assert.equal(normalized.results[0].decision_method, "direct_play");
  assert.equal(normalized.results[0].decoded_dimensions, "1920x1080");
  assert.equal(normalized.results[0].status, "passed");
});

test("two runs that differ only in run-local values normalize identically", () => {
  const first = JSON.parse(JSON.stringify(ARTIFACT));
  const second = JSON.parse(JSON.stringify(ARTIFACT));
  second.generated_at = "2027-01-01T05:06:07.000Z";
  second.results[0].duration_ms = 99_999;
  second.results[0].metrics.ttff_ms = 4242;
  assert.deepEqual(lab.normalizeTrace(first), lab.normalizeTrace(second));
});

// ------------------------------------------------------------------- CLI

test("the manifest keeps the stall-recovery suite reviewable and opt-in", () => {
  const manifest = lab.loadManifest();
  const suite = manifest.suites["stall-recovery"];
  assert.ok(suite, "the stall-recovery suite exists");
  assert.equal(suite.requires_network_profile, true);
  const cases = lab.expandCases(manifest, "stall-recovery");
  assert.equal(cases.length, 1);
  assert.equal(cases[0].operation, "shaped-cliff");
  assert.ok(cases[0].recovery.recovery_deadline_seconds > 0, "the criteria are in the manifest, not the code");

  // The shaping fixture must not widen the general matrix.
  const full = lab.expandCases(manifest, "full");
  assert.equal(full.some((testCase) => testCase.fixture === "shaping-mpeg4-mp3-720"), false);
  assert.equal(lab.expandCases(manifest, "smoke").length, 11, "the smoke suite is unchanged");
  assert.equal(full.length, 44, "the full suite is unchanged");

  const ordinaryCorpus = lab.fixturesForBuild(manifest);
  assert.equal(ordinaryCorpus.some((fixture) => fixture.id === "shaping-mpeg4-mp3-720"), false,
    "the general fixtures command does not pay for the 120-second opt-in source");
  const shapedCorpus = lab.fixturesForBuild(manifest, new Set(["shaping-mpeg4-mp3-720"]));
  assert.deepEqual(shapedCorpus.map((fixture) => fixture.id), ["shaping-mpeg4-mp3-720"]);
});

test("the run command retains JSON and JUnit when a bad profile exits nonzero", async () => {
  await withTempDir(async (directory) => {
    const json = path.join(directory, "bad-profile.json");
    const junit = path.join(directory, "bad-profile.xml");
    const result = cli([
      "run", "--suite", "stall-recovery", "--network-profile", "8mbps-to-9mbps",
      "--json", json, "--junit", junit,
    ]);
    assert.equal(result.status, 1);
    assert.match(result.stderr, /must descend/);
    assert.doesNotMatch(result.stdout, /BUILD|START/, "no corpus or server work happens first");
    const artifact = JSON.parse(await fsp.readFile(json, "utf8"));
    assert.equal(artifact.outcome, "harness");
    assert.deepEqual(artifact.summary, { total: 1, passed: 0, failed: 1 });
    assert.match(await fsp.readFile(junit, "utf8"), /failures="1"/);
  });
});

test("a shaped suite refuses to run unshaped and retains the harness failure", async () => {
  await withTempDir(async (directory) => {
    const artifact = path.join(directory, "unshaped.json");
    const result = cli(["run", "--suite", "stall-recovery", "--json", artifact]);
    assert.equal(result.status, 1);
    assert.match(result.stderr, /--network-profile/);
    assert.equal(JSON.parse(await fsp.readFile(artifact, "utf8")).outcome, "harness");
  });
});

function lifecycleDependencies(manifest, result, state, startError = null) {
  const fixture = manifest.fixtures.find((entry) => entry.id === "shaping-mpeg4-mp3-720");
  const server = {
    baseUrl: "http://127.0.0.1:41001",
    runtime: "/tmp/playback-lab-contract-runtime",
    token: "contract-token",
    files: new Map([[fixture.filename, { id: 1, filename: fixture.filename }]]),
    close: async () => { state.server_closed += 1; },
  };
  const shaper = {
    start: async () => {
      if (startError) throw startError;
      return "http://127.0.0.1:41002";
    },
    close: async () => { state.shaper_closed += 1; },
    telemetry: () => ({
      profile: "8mbps-to-1.5mbps",
      spec: "8mbps-to-1.5mbps",
      cliff_after_seconds: 12,
      cliff_applied_at_ms: result?.shaping?.cliff_applied_at_ms ?? null,
      transport_errors: [],
      stages: result?.shaping?.stages || [],
    }),
  };
  const driver = {
    start: async () => {},
    close: async () => { state.driver_closed += 1; },
    exec: async () => true,
  };
  return {
    buildFixtures: async () => ({
      directory: "/tmp/playback-lab-contract-fixtures",
      metadata: { [fixture.id]: { duration_ms: 120_000 } },
    }),
    startServer: async () => server,
    createShaper: () => shaper,
    createDriver: () => driver,
    prepareBrowser: async () => ({
      browser: "Contract Browser",
      caps: { vcodec: "h264", acodec: "aac", hdr: 0 },
      native_hls: false,
    }),
    api: async (_base, route) => route.startsWith("/system/logs") ? [] : { version: "contract" },
    runOneCase: async (_driver, _server, _manifest, _fixture, testCase) => ({
      name: testCase.name,
      fixture: testCase.fixture,
      quality: testCase.quality,
      operation: testCase.operation,
      status: result.status,
      outcome: result.outcome,
      duration_ms: 0,
      errors: result.errors,
      warnings: [],
      metrics: result.metrics,
      shaping: result.shaping,
    }),
  };
}

test("an unavailable shaper fails the full run, retains artifacts, and cleans owned state", async () => {
  await withTempDir(async (directory) => {
    const manifest = lab.loadManifest();
    const json = path.join(directory, "unavailable.json");
    const junit = path.join(directory, "unavailable.xml");
    const state = { server_closed: 0, shaper_closed: 0, driver_closed: 0 };
    const dependencies = lifecycleDependencies(
      manifest, null, state, new Error("network shaping is unavailable: EACCES"),
    );
    const outcome = await lab.executeRun(manifest, {
      suite: "stall-recovery", network_profile: "8mbps-to-1.5mbps", json, junit,
    }, dependencies);
    assert.equal(outcome.code, 1);
    assert.match(outcome.error.message, /shaping is unavailable/);
    assert.deepEqual(state, { server_closed: 1, shaper_closed: 1, driver_closed: 0 });
    assert.equal(JSON.parse(await fsp.readFile(json, "utf8")).outcome, "harness");
    assert.match(await fsp.readFile(junit, "utf8"), /failures="1"/);
  });
});

test("missed-cliff and failed-recovery verdicts fail the full run and clean every owner", async () => {
  const missed = lab.scoreRecovery(CRITERIA, observation({
    shaping: { cliff_applied_at_ms: null, stages: observation().shaping.stages },
  }));
  const frozen = healthyTimeline().map((row, index) =>
    index < 5 ? row : { ...row, absolute_time: 5 });
  const recovery = lab.scoreRecovery(CRITERIA, observation({ timeline: frozen }));
  for (const [name, score] of [["missed-cliff", missed], ["failed-recovery", recovery]]) {
    await withTempDir(async (directory) => {
      const manifest = lab.loadManifest();
      const json = path.join(directory, `${name}.json`);
      const junit = path.join(directory, `${name}.xml`);
      const state = { server_closed: 0, shaper_closed: 0, driver_closed: 0 };
      const result = { ...score, status: "failed", shaping: observation().shaping };
      if (name === "missed-cliff") result.shaping = { ...result.shaping, cliff_applied_at_ms: null };
      const outcome = await lab.executeRun(manifest, {
        suite: "stall-recovery", network_profile: "8mbps-to-1.5mbps", json, junit,
      }, lifecycleDependencies(manifest, result, state));
      assert.deepEqual(outcome, { code: 1, error: null });
      assert.deepEqual(state, { server_closed: 1, shaper_closed: 1, driver_closed: 1 });
      const artifact = JSON.parse(await fsp.readFile(json, "utf8"));
      assert.equal(artifact.outcome, score.outcome);
      assert.deepEqual(artifact.summary, { total: 1, passed: 0, failed: 1 });
      assert.match(await fsp.readFile(junit, "utf8"), /failures="1"/);
    });
  }
});

test("the normalize command exits nonzero on missing and invalid artifacts", async () => {
  await withTempDir(async (directory) => {
    const missing = cli(["normalize", "--json", path.join(directory, "absent.json")]);
    assert.equal(missing.status, 1);
    assert.match(missing.stderr, /could not be read/);

    const malformedPath = path.join(directory, "malformed.json");
    await fsp.writeFile(malformedPath, "{ not json", "utf8");
    const malformed = cli(["normalize", "--json", malformedPath]);
    assert.equal(malformed.status, 1);
    assert.match(malformed.stderr, /not valid JSON/);

    const foreignPath = path.join(directory, "foreign.json");
    await fsp.writeFile(foreignPath, JSON.stringify({ hello: "world" }), "utf8");
    const foreign = cli(["normalize", "--json", foreignPath]);
    assert.equal(foreign.status, 1);
    assert.match(foreign.stderr, /not a playback-lab report/);
  });
});

test("the normalize command emits a stable trace for a real artifact", async () => {
  await withTempDir(async (directory) => {
    const artifact = path.join(directory, "report.json");
    await fsp.writeFile(artifact, JSON.stringify(ARTIFACT), "utf8");
    const result = cli(["normalize", "--json", artifact]);
    assert.equal(result.status, 0, result.stderr);
    const parsed = JSON.parse(result.stdout);
    assert.equal(parsed.results[0].decision_method, "direct_play");
    assert.equal(parsed.network_profile, null);
  });
});

test("the documented acceptance command is the one the harness accepts", () => {
  const help = cli([]);
  assert.equal(help.status, 0, help.stderr);
  assert.match(help.stdout, /--network-profile/);
  assert.match(help.stdout, /8mbps-to-1\.5mbps/);
  assert.match(help.stdout, /stall-recovery/);
});

runAll().then(() => {
  if (failures.length) {
    process.stderr.write(`\n${failures.length} shaping contract failure(s)\n`);
    process.exitCode = 1;
    return;
  }
  process.stdout.write(`\n${pending.length} shaping contracts hold\n`);
});
