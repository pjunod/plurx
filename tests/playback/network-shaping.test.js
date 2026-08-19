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
const { EventEmitter } = require("node:events");
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

test("a physical-device profile can descend through three exact link stages", () => {
  const profile = lab.parseNetworkProfile("8mbps-to-1.1mbps-to-350kbps@15");
  assert.deepEqual(profile.stages.map((stage) => stage.kbps), [8000, 1100, 350]);
  assert.deepEqual(profile.stages.map((stage) => stage.label), [
    "before-cliff", "after-cliff", "after-cliff-2",
  ]);
  assert.equal(profile.cliff_after_seconds, 15);
  assert.throws(
    () => lab.parseNetworkProfile("8mbps-to-1mbps-to-2mbps"),
    /must descend at every stage/,
  );
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

async function withOrigin(bytes, run, { firstResponseDelayMs = 0 } = {}) {
  let requests = 0;
  const origin = http.createServer((request, response) => {
    const reply = () => {
      response.writeHead(200, { "content-type": "application/octet-stream" });
      response.end(Buffer.alloc(request.url === "/healthz" ? 1 : bytes, 0x61));
    };
    if (requests++ === 0 && firstResponseDelayMs > 0) {
      setTimeout(reply, firstResponseDelayMs);
    } else {
      reply();
    }
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
  // Keep each stage well beyond the bucket's 250 ms starting credit and the
  // first recorded slice. A one-second transfer puts that deliberate credit
  // on the 1.25 telemetry boundary and turns scheduler jitter into a false
  // leak; this payload holds the same bound over a multi-second window.
  await withOrigin(256 * 1024, async (origin) => {
    const profile = lab.parseNetworkProfile("1mbps-to-0.5mbps");
    const shaper = new lab.ShapingProxy(profile, origin);
    const base = await shaper.start();
    try {
      // Make both timed transfers pay equivalent connection/JIT costs. The
      // injected first-response delay makes this fail deterministically if the
      // warmup is removed, modeling the compounded cold-fetch and scheduling
      // skew seen under CPU load.
      const warmup = await fetch(`${base}/healthz`);
      await warmup.arrayBuffer();
      shaper.beginEvidence();

      const before = Date.now();
      const first = await fetch(`${base}/api/v1/files/1/direct`);
      await first.arrayBuffer();
      const firstMs = Date.now() - before;
      assert.ok(firstMs > 1_500, `256 KB over a 1 Mb/s link cannot arrive in ${firstMs}ms`);

      const cliffAt = shaper.applyCliff();
      assert.ok(cliffAt > 0, "the cliff point is recorded");

      const afterStart = Date.now();
      const second = await fetch(`${base}/api/v1/files/1/direct`);
      await second.arrayBuffer();
      const secondMs = Date.now() - afterStart;
      // The configured rate halves. Wall-clock fetches also contain fixed HTTP
      // overhead, so require a clearly slower transfer without asserting an
      // impossible strictly-greater-than 2x boundary around timer rounding.
      assert.ok(secondMs > firstMs * 1.75, `the post-cliff fetch must be far slower (${firstMs}ms then ${secondMs}ms)`);

      const telemetry = shaper.telemetry();
      assert.equal(telemetry.stages.length, 2);
      assert.ok(telemetry.cliff_applied_at_ms > 0);
      for (const stage of telemetry.stages) {
        // The bound the harness scores, not the delivered rate: this runs against
        // real sockets, and a delivered rate has no ceiling the design can state.
        assert.ok(
          stage.admitted_bytes <= lab.shaperClaimBoundBytes(stage.kbps, stage.admitted_span_ms),
          `${stage.label} released ${stage.admitted_bytes} B in ${stage.admitted_span_ms} ms`
          + ` over a ${stage.kbps} kb/s cap`,
        );
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
  }, { firstResponseDelayMs: 750 });
});

test("the physical-device control API is authenticated and advances exact stages", async () => {
  await withOrigin(1024, async (origin) => {
    const shaper = new lab.ShapingProxy(
      lab.parseNetworkProfile("8mbps-to-1mbps-to-350kbps"),
      origin,
      { controlToken: "test-control-token" },
    );
    const base = await shaper.start();
    const authorized = { "x-playback-lab-control": "test-control-token" };
    try {
      const refused = await fetch(`${base}/__playback_lab/status`);
      assert.equal(refused.status, 403);

      const status = await fetch(`${base}/__playback_lab/status`, { headers: authorized });
      assert.equal(status.status, 200);
      assert.equal((await status.json()).current_kbps, 8000);

      const first = await fetch(`${base}/__playback_lab/cliff`, {
        method: "POST", headers: authorized,
      });
      assert.equal(first.status, 200);
      assert.equal((await first.json()).current_kbps, 1000);
      const second = await fetch(`${base}/__playback_lab/cliff`, {
        method: "POST", headers: authorized,
      });
      assert.equal(second.status, 200);
      assert.equal((await second.json()).current_kbps, 350);
      const exhausted = await fetch(`${base}/__playback_lab/cliff`, {
        method: "POST", headers: authorized,
      });
      assert.equal(exhausted.status, 409);
    } finally {
      await shaper.close();
    }
  });
});

test("client probes are captured without credentials and TTFF drives scheduled cliffs", async () => {
  await withOrigin(1, async (origin) => {
    const shaper = new lab.ShapingProxy(
      lab.parseNetworkProfile("8mbps-to-1mbps-to-350kbps@0.01"),
      origin,
      {
        controlToken: "test-control-token",
        autoAdvance: true,
        recoveryCliffAfterSeconds: 0.01,
      },
    );
    await shaper.start();
    try {
      shaper.captureClientLog({
        event: "playback_probe",
        file_id: 42,
        snapshot: { runway: 3.5 },
        token: "must-not-survive",
        url: "http://example.invalid/?token=must-not-survive",
      });
      shaper.captureClientLog({ event: "ttff", reason: "cold-start", attempt: "one" });
      assert.equal(shaper.controlSnapshot().scheduled_advance.stage_index, 1);
      await new Promise((resolve) => setTimeout(resolve, 30));
      assert.equal(shaper.stageIndex, 1);

      shaper.captureClientLog({ event: "ttff", reason: "stall-buffering", attempt: "two" });
      assert.equal(shaper.controlSnapshot().scheduled_advance.stage_index, 2);
      await new Promise((resolve) => setTimeout(resolve, 30));
      assert.equal(shaper.stageIndex, 2);

      const evidence = JSON.stringify(shaper.controlSnapshot());
      assert.match(evidence, /playback_probe/);
      assert.match(evidence, /"runway":3.5/);
      assert.doesNotMatch(evidence, /must-not-survive/);
      assert.deepEqual(shaper.transitions.map((entry) => entry.reason), [
        "initial-ttff", "recovery-ttff",
      ]);
    } finally {
      await shaper.close();
    }
  });
});

test("device-run launches with deterministic defaults, writes evidence, and restores the app", async () => {
  await withTempDir(async (directory) => {
    await withOrigin(1, async (origin) => {
      const launches = [];
      const json = path.join(directory, "device-evidence.json");
      const evidence = await lab.deviceRunCommand({
        device: "physical-device-17",
        target: origin,
        public_host: "127.0.0.1",
        file_id: "42",
        item_id: "17",
        height: "480",
        network_profile: "8mbps-to-1mbps-to-350kbps",
        json,
      }, {
        createShaper: (profile, target, options) => new lab.ShapingProxy(profile, target, {
          ...options, listenHost: "127.0.0.1",
        }),
        launchDevice: (args) => launches.push(["launch", ...args]),
        restoreDevice: (args) => launches.push(["restore", ...args]),
        waitForAcceptance: async (shaper) => {
          shaper.captureClientLog({
            event: "playback_probe", file_id: 42, snapshot: { runway: 4.75 },
          });
          return { reason: "test-complete", event: null };
        },
      });

      assert.equal(launches.length, 2);
      assert.ok(launches[0].includes("-plurx.origin"));
      assert.ok(launches[0].includes("-plurx.acceptance.fileId"));
      assert.ok(launches[0].includes("-plurx.acceptance.probe"));
      assert.deepEqual(launches[1].slice(0, 5), [
        "restore", "--device", "physical-device-17", "--terminate-existing", "--activate",
      ]);
      assert.equal(evidence.completion.reason, "test-complete");
      const artifact = JSON.parse(await fsp.readFile(json, "utf8"));
      assert.equal(artifact.client_events[0].snapshot.runway, 4.75);
      assert.doesNotMatch(JSON.stringify(artifact), /control_token|authorization|bearer/i);
    });
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
      // Eight connections is exactly the shape that groups drains, so this
      // asserts what the bucket released rather than when the bytes landed.
      assert.ok(
        after.admitted_bytes <= lab.shaperClaimBoundBytes(after.kbps, after.admitted_span_ms),
        `concurrent requests released ${after.admitted_bytes} B in ${after.admitted_span_ms} ms`
        + ` over a ${after.kbps} kb/s cap`,
      );
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

/**
 * A real leak both admits and delivers the bytes, so the dilution tests below
 * drive the ledger and the delivery meter together. Driving only `record` would
 * describe drain grouping, which is not a leak and is no longer scored.
 */
function leak(shaper, bytes, media) {
  shaper.meterAdmission(bytes, media);
  shaper.record(bytes, media);
}

test("browser startup dead time cannot dilute a pre-cliff shaper leak", async () => {
  const shaper = new lab.ShapingProxy(
    lab.parseNetworkProfile("8mbps-to-1.5mbps"), "http://127.0.0.1:1",
  );
  let now = 1_000;
  shaper.now = () => now;
  shaper.startedAt = 0;
  shaper.stages[0].entered_at_ms = 0;
  leak(shaper, 500_000, false); // page assets before the first frame
  now = 30_000;
  shaper.beginEvidence();
  // Twelve Mb/s admitted and delivered over the one active second after 30
  // seconds of browser setup. Measuring from proxy bind would dilute this to
  // 387 kb/s.
  leak(shaper, 750_000, true);
  now = 31_000;
  leak(shaper, 750_000, true);
  shaper.applyCliff();
  const telemetry = shaper.telemetry();
  assert.equal(telemetry.stages[0].measured_kbps, 6857.1,
    "the whole-stage estimator includes the first delivered slice's cap-time");
  assert.equal(telemetry.stages[0].peak_kbps, 12_000,
    "the rolling peak still exposes the burst without counting browser startup");
  assert.equal(telemetry.stages[0].admitted_peak_kbps, 12_000,
    "the scored ledger peak sees the same burst");
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
  leak(shaper, 3_000_000, true);
  now = 3_500;
  leak(shaper, 3_000_000, true);
  now = 11_500; // the player's buffer is full until the cliff
  shaper.applyCliff();
  const telemetry = shaper.telemetry();
  assert.equal(telemetry.stages[0].held_seconds, 3.5);
  assert.equal(telemetry.stages[0].measured_kbps, 7384.6);
  assert.equal(telemetry.stages[0].peak_kbps, 24_000,
    "the last-byte boundary cannot dilute the delivered burst");
  assert.equal(telemetry.stages[0].admitted_peak_kbps, 24_000,
    "nor the admitted one");
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
  leak(shaper, 850_000, true);
  now = 999;
  leak(shaper, 850_000, true);
  now = 5_000;
  leak(shaper, 1_000_000, true);
  shaper.applyCliff();
  const telemetry = shaper.telemetry();
  assert.equal(telemetry.stages[0].measured_kbps, 3692.3);
  assert.equal(telemetry.stages[0].peak_kbps, 13_600);
  assert.equal(telemetry.stages[0].admitted_peak_kbps, 13_600);
  assert.equal(lab.scoreRecovery(CRITERIA, observation({ shaping: telemetry })).outcome, "shaping");
  await shaper.close();
});

/**
 * A downstream sink that holds its slice until the test releases it, which is
 * what a browser socket does whenever the player stops reading. Reservations
 * stay serialized through the shared bucket; only the completion is deferred.
 */
function heldSink() {
  const sink = new EventEmitter();
  sink.destroyed = false;
  sink.writable = true;
  sink.written = 0;
  sink.holding = true;
  sink.write = (slice) => {
    sink.written += slice.length;
    return !sink.holding;
  };
  sink.release = () => {
    sink.holding = false;
    sink.emit("drain");
  };
  return sink;
}

/** Run the microtask/immediate queues until the shaper stops making progress. */
async function settle(rounds = 50) {
  for (let index = 0; index < rounds; index += 1) {
    await new Promise((resolve) => setImmediate(resolve));
  }
}

/** A shaper whose reservations are paid for by advancing the clock, not a timer. */
function deterministicShaper(spec) {
  const shaper = new lab.ShapingProxy(lab.parseNetworkProfile(spec), "http://127.0.0.1:1");
  const clock = { now: 0 };
  shaper.now = () => clock.now;
  shaper.startedAt = 0;
  shaper.stages[0].entered_at_ms = 0;
  // The bucket, not a real timer, decides when a reservation is granted. Paying
  // the debt by advancing the clock keeps the whole scenario deterministic.
  // Round up, because a real timer never fires early either.
  shaper.waitForLimiter = (delay) => {
    clock.now += Math.ceil(delay);
    return new Promise((resolve) => setImmediate(resolve));
  };
  return [shaper, clock];
}

/*
 * The scored whole-stage quantity is the ledger's, not the delivered stream's,
 * and this is why. Three connections each take one legally priced slice and
 * then stop reading; the stage delivers nothing else. Every byte arrives in the
 * same instant the players resume, so the delivered stage rate is 4500 kb/s on
 * a 1500 kb/s link — a rate no bound can be stated for, since the skew is one
 * undrained slice per connection and the proxy opens up to `max_sockets` of
 * them. Nothing pads this stage: there is no later slice diluting the average
 * and no burst to isolate. If the run is scored on what the bucket released, it
 * passes; if it is scored on delivery timing, it invents a shaping fault.
 */
test("a stage delivered entirely in one grouped drain is not a shaper leak", async () => {
  const [shaper, clock] = deterministicShaper("8mbps-to-1.5mbps");
  const slice = Buffer.alloc(16 * 1024, 0x61);
  const open = () => false;
  shaper.applyCliff();

  const held = [heldSink(), heldSink(), heldSink()];
  const pending = held.map((sink) => shaper.writeShaped(slice, sink, true, open));
  await settle();
  for (const sink of held) {
    assert.equal(sink.written, slice.length, "each held connection was granted exactly one slice");
  }
  assert.ok(clock.now > 250, `three 16 KiB slices cannot be granted in ${clock.now}ms at 1500 kb/s`);

  clock.now = 3_000;
  for (const sink of held) sink.release();
  await Promise.all(pending);

  const after = shaper.telemetry().stages[1];
  assert.equal(after.measured_kbps, 4500, "the delivered stage rate is the one the reviewer reproduced");
  assert.ok(after.measured_kbps > 1500 * 1.25, "and it sits past the flat delivered gate this replaced");
  assert.equal(after.admitted_bytes, 3 * slice.length, "the bucket released exactly three slices");
  assert.ok(
    after.admitted_bytes <= lab.shaperClaimBoundBytes(1500, after.admitted_span_ms),
    `and no more than the ${after.admitted_span_ms} ms window allows`,
  );

  const observed = observation();
  observed.shaping.stages[1] = after;
  const score = lab.scoreRecovery(CRITERIA, observed);
  assert.deepEqual(score.errors, [],
    "a stage whose every byte drained at once is not evidence that the shaper leaked");
  assert.equal(score.outcome, "passed");
  await shaper.close();
});

/*
 * There is one bucket, so a refund credits whichever stage is live when it
 * happens. Filing the withdrawal against the stage that made the claim would
 * hand the post-cliff bucket spendable credit its own ledger never saw, and the
 * claims that credit funds would then read as bytes the bucket could not have
 * released.
 */
test("a refund after the cliff is withdrawn from the stage whose bucket took it", async () => {
  const [shaper, clock] = deterministicShaper("8mbps-to-1.5mbps");
  const slice = Buffer.alloc(16 * 1024, 0x61);
  const open = () => false;

  // One pre-cliff slice is granted and then held by a player that stops reading.
  const held = heldSink();
  const abandoned = shaper.writeShaped(slice, held, true, open).catch(() => null);
  await settle();
  assert.equal(held.written, slice.length);
  assert.equal(shaper.admissionSamples[0].length, 1, "the claim is filed against the stage that made it");

  // The link idles until the 8 Mb/s bucket is full, then the cliff clamps it.
  clock.now = 5_000;
  shaper.applyCliff();

  // The post-cliff stage then claims continuously, so its rolling peak sits on
  // the bucket's ceiling and one unaccounted slice is enough to break it.
  const free = heldSink();
  free.holding = false;
  for (let index = 0; index < 4; index += 1) await shaper.writeShaped(slice, free, true, open);

  // The abandoned connection closes here. The bucket is in debt, so it takes
  // the whole slice back and immediately funds another claim with it.
  held.destroyed = true;
  held.emit("close");
  await abandoned;
  for (let index = 0; index < 16; index += 1) await shaper.writeShaped(slice, free, true, open);

  const refunds = shaper.admissionSamples[1].filter((entry) => entry.bytes < 0);
  assert.deepEqual(shaper.admissionSamples[0].filter((entry) => entry.bytes < 0), [],
    "the pre-cliff ledger keeps a claim its bucket really made");
  assert.deepEqual(refunds.map((entry) => entry.bytes), [-slice.length],
    "and the post-cliff ledger records the credit its own bucket received");

  const after = shaper.telemetry().stages[1];
  const bound = Number(lab.shaperBurstBoundKbps(1500).toFixed(1));
  // 1966.1 kb/s against a 2006.1 kb/s ceiling. Filed against the claiming stage
  // instead, the same run reads 2097.2 kb/s — one uncancelled slice over the
  // ceiling, and a healthy bucket reported as a leak.
  assert.equal(after.admitted_peak_kbps, 1966.1);
  assert.ok(after.admitted_peak_kbps <= bound,
    `a refunded slice cannot inflate the peak, got ${after.admitted_peak_kbps} against ${bound}`);
  assert.ok(after.admitted_peak_kbps + (lab.SHAPER_SLICE_BYTES * 8) / 1000 > bound,
    "one more unaccounted slice must break the ceiling, or the credit was never binding");

  // End to end the abandoned slice balances: it is claimed pre-cliff, refunded
  // post-cliff, and never delivered, so the shaper's own totals agree without
  // any allowance at all.
  const whole = shaper.telemetry();
  assert.equal(whole.stages.reduce((total, stage) => total + stage.bytes, 0), 327_680);
  assert.equal(whole.stages.reduce((total, stage) => total + stage.admitted_bytes, 0), 327_680);

  const observed = observation();
  observed.shaping.stages[1] = after;
  // `after` records a refund of a claim the pre-cliff bucket really made, so
  // the synthetic pre-cliff stage has to carry that claim for the two stages to
  // describe one run. Without it the pair is missing 16 KiB of admission that
  // the real telemetry above has.
  observed.shaping.stages[0].admitted_bytes += slice.length;
  const score = lab.scoreRecovery(CRITERIA, observed);
  assert.deepEqual(score.errors, [], "a canceled transfer across the cliff is not a shaper leak");
  await shaper.close();
});

/*
 * A slice the downstream has already taken is spent. The reservation used to
 * stay refundable until after the post-drain cancellation check, so a `close`
 * landing between `drain` and that continuation resuming handed the bucket back
 * credit it had spent on bytes the socket accepted, and dropped the delivery
 * from the artifact at the same time. Reproduced at 32 sinks: 524,288 B went
 * downstream against a 250,759 B one-second bound and still scored `passed`.
 */
test("a drain-then-close race can neither refund a taken slice nor erase it", async () => {
  const [shaper] = deterministicShaper("8mbps-to-1.5mbps");
  const slice = Buffer.alloc(16 * 1024, 0x61);
  let closing = false;
  const open = () => closing;

  const sinks = [];
  const writes = [];
  for (let index = 0; index < 32; index += 1) {
    const sink = heldSink();
    sinks.push(sink);
    writes.push(shaper.writeShaped(slice, sink, true, open).catch(() => null));
  }
  await settle();
  const accepted = sinks.reduce((total, sink) => total + sink.written, 0);
  assert.equal(accepted, 32 * slice.length, "every sink took its slice");

  // Each sink drains — the bytes are downstream's — and only then closes,
  // before the continuation that follows the drain can resume.
  for (const sink of sinks) sink.release();
  closing = true;
  for (const sink of sinks) {
    sink.destroyed = true;
    sink.emit("close");
  }
  await Promise.all(writes);

  const telemetry = shaper.telemetry();
  const delivered = telemetry.stages.reduce((total, stage) => total + stage.bytes, 0);
  const admitted = telemetry.stages.reduce((total, stage) => total + stage.admitted_bytes, 0);
  assert.equal(delivered, accepted, "a taken slice cannot vanish from the artifact");
  assert.equal(admitted, accepted, "and cannot be refunded back into the bucket");
  assert.deepEqual(
    telemetry.stages.flatMap((stage, index) => shaper.admissionSamples[index])
      .filter((entry) => entry.bytes < 0),
    [],
    "no refund is filed for a slice the downstream already took",
  );

  // The old behavior scored this clean while the link carried every byte.
  const observed = observation();
  observed.shaping.stages[1] = telemetry.stages[0];
  assert.equal(delivered > admitted, false, "delivery never exceeds admission after the race");
  await shaper.close();
});

/*
 * `beginEvidence` discards the ledger mid-stage, so a slice the bucket priced
 * before the first presented frame can be delivered after it with its claim in
 * the discarded series. The allowance for that seam used to be a blanket
 * `max_sockets x slice_bytes` — 512 KiB usable by delivery with no claim behind
 * it anywhere. It is now the measured bytes of the claims that actually crossed.
 */
test("the evidence seam exempts the claims that crossed it and nothing else", async () => {
  const [shaper] = deterministicShaper("8mbps-to-1.5mbps");
  const slice = Buffer.alloc(16 * 1024, 0x61);
  const open = () => false;

  // One slice is priced, then held by a sink that has not drained yet.
  const held = heldSink();
  const crossing = shaper.writeShaped(slice, held, true, open);
  await settle();
  assert.equal(shaper.pendingClaims.size, 1, "the claim is outstanding at the seam");

  // The first frame arrives and the ledger holding that claim is discarded.
  shaper.beginEvidence();
  assert.equal(shaper.telemetry().evidence_pending_claim_bytes, slice.length,
    "the artifact records what was actually in flight, not a socket-count ceiling");
  assert.equal(shaper.telemetry().carried_over_bytes, 0, "nothing has crossed yet");

  // It drains after the reset, so this completion is the genuine carry-over.
  held.release();
  await crossing;
  assert.equal(shaper.telemetry().carried_over_bytes, slice.length);

  const carried = shaper.telemetry();
  const delivered = carried.stages.reduce((total, stage) => total + stage.bytes, 0);
  const admitted = carried.stages.reduce((total, stage) => total + stage.admitted_bytes, 0);
  assert.equal(delivered - admitted, slice.length,
    "the delivery outruns the surviving ledger by exactly the slice that crossed");

  // Scored with its provenance the run is clean; one unclaimed byte past it is
  // not, where the blanket allowance would have excused 512 KiB of it.
  const observed = observation();
  observed.shaping.evidence_pending_claim_bytes = slice.length;
  observed.shaping.carried_over_bytes = slice.length;
  observed.shaping.stages[1].bytes += slice.length;
  assert.equal(lab.scoreRecovery(CRITERIA, observed).outcome, "passed");
  observed.shaping.stages[1].bytes += 1;
  assert.equal(lab.scoreRecovery(CRITERIA, observed).outcome, "shaping");
  await shaper.close();
});

test("a claim canceled before it is delivered never widens the seam allowance", async () => {
  const [shaper] = deterministicShaper("8mbps-to-1.5mbps");
  const slice = Buffer.alloc(16 * 1024, 0x61);
  let closing = false;

  const held = heldSink();
  const abandoned = shaper.writeShaped(slice, held, true, () => closing).catch(() => null);
  await settle();
  shaper.beginEvidence();
  assert.equal(shaper.telemetry().evidence_pending_claim_bytes, slice.length);

  // The player abandons the rung before the sink ever drains, so the slice
  // never reached the wire. It is refunded, and it is not a carry-over.
  closing = true;
  held.destroyed = true;
  held.emit("close");
  await abandoned;
  assert.equal(shaper.telemetry().carried_over_bytes, 0,
    "a refunded claim cannot be spent as delivery allowance");
  assert.equal(shaper.pendingClaims.size, 0, "and it is no longer outstanding");
  await shaper.close();
});

test("a pre-evidence claim refunded after the reset keeps the delivery proof balanced", async () => {
  const [shaper] = deterministicShaper("8mbps-to-1.5mbps");
  const slice = Buffer.alloc(16 * 1024, 0x61);
  let closing = false;

  // Price one slice before the first frame, then hold it until the evidence
  // reset has discarded the positive admission that belongs to the claim.
  const held = heldSink();
  const abandoned = shaper.writeShaped(slice, held, true, () => closing).catch(() => null);
  await settle();
  assert.equal(shaper.pendingClaims.size, 1, "the pre-window claim is still outstanding");
  shaper.beginEvidence();

  // The player changes rung before the sink drains. The retained ledger sees
  // the refund but not the discarded claim, which is the exact seam the final
  // delivery proof must reconcile.
  closing = true;
  held.destroyed = true;
  held.emit("close");
  await abandoned;

  // Ordinary delivery on both sides of the cliff makes this a complete scored
  // run rather than another bookkeeping-only assertion.
  const free = heldSink();
  free.holding = false;
  const open = () => false;
  await shaper.writeShaped(slice, free, true, open);
  shaper.applyCliff();
  await shaper.writeShaped(slice, free, true, open);

  const telemetry = shaper.telemetry();
  assert.equal(telemetry.evidence_pending_claim_bytes, slice.length);
  assert.equal(telemetry.carried_over_bytes, 0, "the canceled claim delivered no bytes");
  assert.equal(telemetry.evidence_refunded_claim_bytes, slice.length,
    "the artifact records the exact cross-seam credit the bucket accepted");
  assert.equal(
    telemetry.stages.reduce((total, stage) => total + stage.bytes, 0),
    2 * slice.length,
    "only the two ordinary slices reached the browser",
  );

  const observed = observation({ shaping: telemetry });
  assert.equal(lab.scoreRecovery(CRITERIA, observed).outcome, "passed",
    "a real cross-seam refund cannot turn healthy shaping into a false failure");

  // Reconciliation is exact provenance, not a new blanket allowance.
  observed.shaping.stages[1].bytes += 1;
  assert.equal(lab.scoreRecovery(CRITERIA, observed).outcome, "shaping",
    "one genuinely unclaimed byte still invalidates the run");
  await shaper.close();
});

test("a canceled post-evidence claim cannot authorize later unclaimed delivery", async () => {
  const [shaper, clock] = deterministicShaper("8mbps-to-1.5mbps");
  const slice = Buffer.alloc(16 * 1024, 0x61);
  let closing = false;
  shaper.beginEvidence();

  // Claim a slice inside the evidence window and hold it before drain. By the
  // time the player cancels, the idle bucket is full and accepts no refund, so
  // its conservation ledger legitimately retains the whole positive claim.
  const held = heldSink();
  const abandoned = shaper.writeShaped(slice, held, true, () => closing).catch(() => null);
  await settle();
  assert.equal(shaper.pendingClaims.size, 1, "the post-window claim is still outstanding");
  clock.now = 10_000;
  closing = true;
  held.destroyed = true;
  held.emit("close");
  await abandoned;
  assert.equal(shaper.bucket.refund(slice.length, clock.now), 0,
    "the full bucket has no room for any more cancellation credit");

  // One ordinary post-cliff slice still has a matching admission. The canceled
  // claim delivered nothing, so it must authorize no browser bytes even though
  // its zero-credit refund leaves that admission in the conservation ledger.
  shaper.applyCliff();
  const free = heldSink();
  free.holding = false;
  await shaper.writeShaped(slice, free, true, () => false);
  const clean = shaper.telemetry();
  assert.equal(clean.undelivered_admitted_bytes, slice.length,
    "the artifact names the stranded admission from the canceled claim");
  assert.equal(lab.scoreRecovery(CRITERIA, observation({ shaping: clean })).outcome, "passed",
    "a legitimate zero-credit cancellation remains clean");

  // Reproduce the reviewer's counterexample through the real proxy: an equal
  // unreserved delivery cannot spend the canceled claim's stranded admission.
  shaper.record(slice.length, true);
  const bypassed = shaper.telemetry();
  assert.equal(
    bypassed.stages.reduce((total, stage) => total + stage.bytes, 0),
    bypassed.stages.reduce((total, stage) => total + stage.admitted_bytes, 0),
    "the old aggregate comparison saw equal totals and returned a false pass",
  );
  const score = lab.scoreRecovery(CRITERIA, observation({ shaping: bypassed }));
  assert.equal(score.outcome, "shaping");
  assert.match(score.errors[0], /canceled claims left 16384 B of admission without delivery/);
  await shaper.close();
});

test("a refund is metered as the credit the bucket took, not the slice that was asked back", () => {
  const bucket = new lab.TokenBucket(1500, 0);
  bucket.refill(10_000);
  assert.equal(bucket.tokens, bucket.capacity, "the idle bucket is full");
  assert.equal(bucket.refund(16 * 1024, 10_000), 0, "a full bucket cannot take a slice back");
  assert.equal(bucket.tokens, bucket.capacity, "and does not overflow trying");
  bucket.reserve(16 * 1024, 10_000);
  assert.equal(bucket.refund(16 * 1024, 10_000), 16 * 1024, "a bucket with room takes the whole slice");
});

test("slices held by separate sinks and released together are not a shaper leak", async () => {
  const shaper = new lab.ShapingProxy(
    lab.parseNetworkProfile("8mbps-to-1.5mbps"), "http://127.0.0.1:1",
  );
  let now = 0;
  shaper.now = () => now;
  shaper.startedAt = 0;
  shaper.stages[0].entered_at_ms = 0;
  // The bucket, not a real timer, decides when a reservation is granted. Paying
  // the debt by advancing the clock keeps the whole scenario deterministic.
  // Round up, because a real timer never fires early either.
  shaper.waitForLimiter = (delay) => {
    now += Math.ceil(delay);
    return new Promise((resolve) => setImmediate(resolve));
  };
  const slice = Buffer.alloc(16 * 1024, 0x61);
  const open = () => false;
  shaper.applyCliff();

  // Three connections each take one legally priced slice out of the 1.5 Mb/s
  // bucket and then stop reading. Every reservation waited its full turn.
  const held = [heldSink(), heldSink(), heldSink()];
  const pending = held.map((sink) => shaper.writeShaped(slice, sink, true, open));
  await settle();
  for (const sink of held) {
    assert.equal(sink.written, slice.length, "each held connection was granted exactly one slice");
  }
  const grantedBy = now;
  assert.ok(grantedBy > 250, `three 16 KiB slices cannot be granted in ${grantedBy}ms at 1500 kb/s`);

  // The link then idles long enough to refill the bucket, and all three players
  // resume reading at the same instant.
  now = 3_000;
  for (const sink of held) sink.release();
  await Promise.all(pending);

  // Fourteen further slices are admitted normally, each one waiting for the
  // bucket exactly as designed.
  const free = heldSink();
  free.holding = false;
  for (let index = 0; index < 14; index += 1) {
    await shaper.writeShaped(slice, free, true, open);
  }
  // One late slice, so the whole-stage average stays far below its own gate and
  // cannot be what this test is measuring.
  now = 7_000;
  await shaper.writeShaped(slice, free, true, open);

  const after = shaper.telemetry().stages[1];
  const bound = lab.shaperBurstBoundKbps(1500);
  // 17 slices land inside one window although only 14 were ever priced into it.
  assert.equal(after.peak_kbps, 2228.2,
    "the delivered rolling peak reaches the shape that used to score as a leak");
  assert.ok(after.peak_kbps > bound,
    `the delivered peak must exceed the ceiling, got ${after.peak_kbps} against ${bound}`);
  assert.equal(after.measured_kbps, 577.2);
  assert.ok(after.measured_kbps < 1500 * 1.25,
    "the whole-stage average must stay nonbinding, or it is what this test measures");

  const observed = observation();
  observed.shaping.stages[1] = after;
  const score = lab.scoreRecovery(CRITERIA, observed);
  assert.deepEqual(score.errors, [],
    "deferred drains on separate connections are not evidence that the shaper leaked");
  assert.equal(score.outcome, "passed");
  assert.ok(after.admitted_peak_kbps <= bound,
    `no window admitted past the ceiling, got ${after.admitted_peak_kbps} against ${bound}`);
  await shaper.close();
});

test("whole-stage rate includes the first slice's earning time", async () => {
  const shaper = new lab.ShapingProxy(
    lab.parseNetworkProfile("1mbps-to-0.5mbps"), "http://127.0.0.1:1",
  );
  let now = 0;
  shaper.now = () => now;
  shaper.startedAt = 0;
  shaper.stages[0].entered_at_ms = 0;
  const sliceBytes = 16 * 1024;
  const sliceMs = sliceBytes * 8 / 1000;
  for (let index = 0; index < 8; index += 1) {
    shaper.record(sliceBytes, true);
    now += sliceMs;
  }
  shaper.applyCliff();
  const before = shaper.telemetry().stages[0];
  assert.equal(before.measured_kbps, 1000,
    "eight slices at the cap must not report the old 8/7 first-slice bias");
  assert.equal(before.measured_media_kbps, 1000);
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
    attempt_id: "a1",
    attempt_reason: "cold-start",
    player_generation: 1,
    play_started_at: null,
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

/**
 * A healthy shaped run. Both stages carry a ledger consistent with what they
 * delivered, because that is what the scored gates read: `admitted_bytes` over
 * `admitted_span_ms` for the sustained rate, the rolling admitted peak for the
 * burst, and delivered bytes only as a total against the ledger's own total.
 */
function shapedStage(label, kbps, { spanMs, rate, mediaRate }) {
  const bytes = Math.round((rate * spanMs) / 8);
  return {
    label,
    kbps,
    bytes,
    media_bytes: Math.round((mediaRate * spanMs) / 8),
    measured_kbps: rate,
    measured_media_kbps: mediaRate,
    peak_kbps: rate,
    admitted_bytes: bytes,
    admitted_span_ms: spanMs,
    admitted_kbps: rate,
    admitted_peak_kbps: rate,
  };
}

function observation(extra = {}) {
  return {
    shaping: {
      cliff_applied_at_ms: 12_000,
      slice_bytes: 16 * 1024,
      max_sockets: 32,
      evidence_pending_claim_bytes: 0,
      carried_over_bytes: 0,
      evidence_refunded_claim_bytes: 0,
      undelivered_admitted_bytes: 0,
      stages: [
        shapedStage("before-cliff", 8000, { spanMs: 12_000, rate: 4100, mediaRate: 4000 }),
        shapedStage("after-cliff", 1500, { spanMs: 45_000, rate: 1480, mediaRate: 1400 }),
      ],
    },
    baseline_clock_rate: 1.0,
    baseline_error: null,
    pre_cliff_runway_seconds: 10,
    timeline: healthyTimeline(),
    ...extra,
  };
}

// No shaping tolerance appears here, and none is accepted: both shaping gates
// are the bucket's own conservation identity, so a multiplier would only be an
// unproved band in which a real leak scores as healthy.
const CRITERIA = {
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
  const after = leaked.shaping.stages[1];
  // Four times the cap, admitted and delivered alike, over the whole stage.
  after.admitted_bytes = Math.round((6000 * after.admitted_span_ms) / 8);
  after.bytes = after.admitted_bytes;
  const score = lab.scoreRecovery(CRITERIA, leaked);
  assert.equal(score.outcome, "shaping");
  assert.match(score.errors[0], /leaked/);
  assert.match(score.errors[0], /6000 kb\/s/);
});

// The sustained gate is the same identity as the burst gate, read over the
// stage's own ledger span. The fixed burst and slice terms are amortized across
// that span, so a long stage is held far closer to its cap than the flat 1.25x
// this gate used to carry: 1537.9 kb/s rather than 1875 kb/s on a 45 s stage.
test("the sustained bound tightens as the stage it scores gets longer", () => {
  const rate = (spanMs) => (lab.shaperClaimBoundBytes(1500, spanMs) * 8) / spanMs;
  assert.ok(rate(1_000) > rate(10_000), "a one-second window may claim more than a ten-second one");
  assert.ok(rate(45_000) < 1500 * 1.05, `a 45s stage is held within 5% of its cap, got ${rate(45_000)}`);
  assert.ok(rate(45_000) < 1500 * 1.25, "and well inside the flat tolerance this replaced");

  const leaked = observation();
  const after = leaked.shaping.stages[1];
  // 1900 kb/s sustained: inside the old flat 1875 kb/s gate's rounding, inside
  // the one-second burst bound, and past what the bucket can release over 45 s.
  after.admitted_bytes = Math.round((1900 * after.admitted_span_ms) / 8);
  after.bytes = after.admitted_bytes;
  after.admitted_peak_kbps = 1870;
  assert.ok(1870 < lab.shaperBurstBoundKbps(1500),
    "the peak must clear the burst gate, or this does not isolate the sustained gate");
  const score = lab.scoreRecovery(CRITERIA, leaked);
  assert.equal(score.outcome, "shaping");
  assert.match(score.errors[0], /sustained|admitted \d+ B in/);
  assert.match(score.errors[0], /1900 kb\/s/);
});

// The bucket stores 250 ms of credit and serialization allows exactly one
// outstanding claim, so the most a one-second window can take out is 1.25x the
// cap plus one 16 KiB slice. That ceiling is a consequence of token
// conservation, not an estimate, so a peak sitting exactly on it is healthy and
// the tolerance above it covers only reporting quantization.
test("an admitted peak at the bucket's own conservation ceiling is not a leak", () => {
  const designed = observation();
  const bound = lab.shaperBurstBoundKbps(8000);
  assert.equal(bound, 8000 * (1 + 250 / 1000) + (16 * 1024 * 8) / 1000,
    "the ceiling is stored burst, one window of earned rate, and one in-flight slice");
  designed.shaping.stages[0].admitted_peak_kbps = Number(bound.toFixed(1));
  const score = lab.scoreRecovery(CRITERIA, designed);
  assert.deepEqual(score.errors, []);
  assert.equal(score.outcome, "passed");
});

test("an admitted peak past the conservation ceiling is a leak", () => {
  const leaked = observation();
  leaked.shaping.stages[0].admitted_peak_kbps = 12_000;
  const score = lab.scoreRecovery(CRITERIA, leaked);
  assert.equal(score.outcome, "shaping");
  assert.match(score.errors[0], /1s burst rate of 12000 kb\/s over a 8000 kb\/s cap/);
});

// The ceiling already carries the one in-flight slice serialization permits; a
// second one — the skew a per-connection drain could add — must still fail.
test("one slice past the conservation ceiling is already a leak", () => {
  const leaked = observation();
  const bound = lab.shaperBurstBoundKbps(1500);
  leaked.shaping.stages[1].admitted_peak_kbps = Number((bound + (16 * 1024 * 8) / 1000).toFixed(1));
  const score = lab.scoreRecovery(CRITERIA, leaked);
  assert.equal(score.outcome, "shaping");
  assert.match(score.errors[0], /1s burst rate/);
});

// The burst gate carries no tolerance multiplier at all, so the slack above the
// proven ceiling is exactly what rounding the report to 0.1 kb/s can add — one
// reporting quantum, not the ~100 kb/s a 1% multiplier granted on this stage.
test("the burst gate allows one reporting quantum above the ceiling and no more", () => {
  const bound = lab.shaperBurstBoundKbps(8000);
  const reported = Number(bound.toFixed(1));
  assert.ok(Math.abs(reported - bound) <= 0.05, "the ceiling is compared at the quantum it is reported in");

  const designed = observation();
  designed.shaping.stages[0].admitted_peak_kbps = reported;
  assert.equal(lab.scoreRecovery(CRITERIA, designed).outcome, "passed");

  const over = observation();
  over.shaping.stages[0].admitted_peak_kbps = Number((reported + 0.1).toFixed(1));
  const score = lab.scoreRecovery(CRITERIA, over);
  assert.equal(score.outcome, "shaping",
    "0.1 kb/s past the ceiling already fails, so no unproved band survives above it");
  assert.ok(reported + 0.1 < bound * 1.01,
    "and that failing rate sits inside the 1% band this replaced, which would have passed it");
});

test("a stage with no admission ledger invalidates shaping evidence", () => {
  const missing = observation();
  missing.shaping.stages[1].admitted_bytes = null;
  missing.shaping.stages[1].admitted_span_ms = null;
  missing.shaping.stages[1].admitted_peak_kbps = null;
  const score = lab.scoreRecovery(CRITERIA, missing);
  assert.equal(score.outcome, "shaping");
  assert.match(score.errors[0], /no admission ledger for after-cliff/);

  // A half-populated ledger is a broken artifact, not a healthy stage.
  const partial = observation();
  partial.shaping.stages[1].admitted_peak_kbps = null;
  assert.equal(lab.scoreRecovery(CRITERIA, partial).outcome, "shaping");
});

// The ledger is the shaper's own bookkeeping, so one thing it cannot establish
// is that the browser was fed from it. Delivered bytes are gated as a total —
// the one delivered quantity per-connection drain grouping cannot distort.
test("bytes the browser received but the bucket never released are a leak", () => {
  const bypassed = observation();
  bypassed.shaping.stages[1].bytes += (32 * 16 * 1024) + 1;
  const score = lab.scoreRecovery(CRITERIA, bypassed);
  assert.equal(score.outcome, "shaping");
  assert.match(score.errors[0], /the browser received \d+ B while the bucket released \d+ B/);
});

// The exemption is provenance, not a ceiling. A blanket `max_sockets x
// slice_bytes` allowance excused 512 KiB of delivery that no claim anywhere
// accounted for — at 1.5 Mb/s, 2.8 seconds of link capacity scoring `passed`.
test("unclaimed delivery is a leak at any size, down to a single slice", () => {
  // No claim crossed the seam, so nothing is exempt and one slice is enough.
  const oneSlice = observation();
  oneSlice.shaping.carried_over_bytes = 0;
  oneSlice.shaping.stages[1].bytes += 16 * 1024;
  const score = lab.scoreRecovery(CRITERIA, oneSlice);
  assert.equal(score.outcome, "shaping");
  assert.match(score.errors[0], /reconciling 0 B delivered and 0 B refunded/);

  // The old blanket allowance is not available to unclaimed bytes any more.
  const blanket = observation();
  blanket.shaping.carried_over_bytes = 0;
  blanket.shaping.stages[1].bytes += 32 * 16 * 1024;
  assert.equal(lab.scoreRecovery(CRITERIA, blanket).outcome, "shaping");

  // An artifact with no seam field at all exempts nothing rather than 512 KiB.
  const absent = observation();
  delete absent.shaping.carried_over_bytes;
  absent.shaping.stages[1].bytes += 16 * 1024;
  assert.equal(lab.scoreRecovery(CRITERIA, absent).outcome, "shaping");
});

test("a genuine pre-evidence in-flight completion is exempt for exactly its bytes", () => {
  // A slice the bucket priced before `beginEvidence` and delivered after it is
  // real traffic with a real claim; the run still scores.
  const seam = observation();
  seam.shaping.evidence_pending_claim_bytes = 3 * 16 * 1024;
  seam.shaping.carried_over_bytes = 3 * 16 * 1024;
  seam.shaping.stages[1].bytes += 3 * 16 * 1024;
  assert.equal(lab.scoreRecovery(CRITERIA, seam).outcome, "passed");

  // One byte past what actually crossed the seam is still a leak.
  const overrun = observation();
  overrun.shaping.evidence_pending_claim_bytes = 3 * 16 * 1024;
  overrun.shaping.carried_over_bytes = 3 * 16 * 1024;
  overrun.shaping.stages[1].bytes += (3 * 16 * 1024) + 1;
  const score = lab.scoreRecovery(CRITERIA, overrun);
  assert.equal(score.outcome, "shaping");
  assert.match(score.errors[0], /reconciling 49152 B delivered and 0 B refunded/);
});

test("evidence-seam adjustments cannot exceed the claims measured at the reset", () => {
  const forged = observation();
  forged.shaping.evidence_pending_claim_bytes = 16 * 1024;
  forged.shaping.carried_over_bytes = 16 * 1024;
  forged.shaping.evidence_refunded_claim_bytes = 1;
  const score = lab.scoreRecovery(CRITERIA, forged);
  assert.equal(score.outcome, "shaping");
  assert.match(score.errors[0], /invalid evidence-seam provenance/);
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
    attempt_id: index < 20 ? "a1" : "a2",
    attempt_reason: "stall-restart",
  }));
  const events = lab.rungHistory(restarted);
  assert.equal(events.length, 1,
    "the attempt identity is authoritative when presentation symptoms are unchanged");
  assert.equal(events[0].direction, "restart");
  assert.equal(events[0].attempt_reason, "stall-restart");
});

test("consecutive same-reason attempts cannot collapse below the restart budget", () => {
  const restarted = healthyTimeline().map((row, index) => ({
    ...row,
    height: 720,
    ttff_ms: 1570,
    attempt_id: index < 10 ? "a1" : index < 20 ? "a2" : "a3",
    attempt_reason: "stall-restart",
  }));
  const events = lab.rungHistory(restarted);
  assert.equal(events.length, 2);
  assert.deepEqual(events.map((event) => event.attempt_id), ["a2", "a3"]);
  const score = lab.scoreRecovery(CRITERIA, observation({ timeline: restarted }));
  assert.equal(score.metrics.restarts, 2);
  assert.equal(score.outcome, "recovery");
  assert.ok(score.errors.some((error) => /automatic restarts/.test(error)), score.errors.join("; "));
});

test("attempt sequence gaps retain restarts hidden inside one sample interval", () => {
  const restarted = healthyTimeline().map((row, index) => ({
    ...row,
    attempt_id: index < 20 ? "a1" : "a3",
    attempt_reason: "stall-restart",
  }));
  const events = lab.rungHistory(restarted);
  assert.equal(events.length, 1);
  assert.equal(events[0].restart_count, 2);
  assert.equal(lab.scoreRecovery(CRITERIA, observation({ timeline: restarted })).metrics.restarts, 2);
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
    attempt_id: index < 20 ? "a1" : "a2",
    attempt_reason: index < 20 ? "cold-start" : "quality",
    player_generation: index < 20 ? 1 : 2,
    stalls: index < 20 ? 5 : index < 30 ? 0 : index < 38 ? 1 : 2,
  }));
  assert.equal(lab.rungHistory(reset).length, 1, "the counter reset occurs at one permitted restart");
  assert.equal(lab.timeToSustained(reset, 10, 0.9), null,
    "two post-restart stalls leave too little clean runway to prove recovery");
  const score = lab.scoreRecovery(CRITERIA, observation({ timeline: reset }));
  assert.equal(score.outcome, "recovery");
  assert.match(score.errors[0], /never held/);
});

test("an in-place restart does not recount stalls carried by the same player", () => {
  const carried = healthyTimeline().map((row, index) => ({
    ...row,
    height: 720,
    attempt_id: index < 31 ? "a1" : "a2",
    attempt_reason: index < 31 ? "cold-start" : "stall-restart",
    stalls: index === 0 ? 0 : index === 1 ? 1 : 2,
  }));
  const events = lab.rungHistory(carried);
  assert.equal(events.length, 1);
  assert.equal(events[0].at_ms, 31_000);
  assert.equal(events[0].counter_rebase, false);
  assert.equal(lab.timeToSustained(carried, 10, 0.9), 2000,
    "the restart must not add the same two pre-restart stalls a second time");
  const score = lab.scoreRecovery(CRITERIA, observation({ timeline: carried }));
  assert.equal(score.outcome, "passed");
  assert.equal(score.metrics.recovered_after_ms, 2000);
  assert.equal(score.metrics.restarts, 1);
});

test("missing attempt identity or an unexplained counter reset invalidates the run", () => {
  const missing = healthyTimeline();
  missing[5] = { ...missing[5], attempt_id: null };
  const missingScore = lab.scoreRecovery(CRITERIA, observation({ timeline: missing }));
  assert.equal(missingScore.outcome, "harness");
  assert.match(missingScore.errors[0], /missing player attempt/);

  const reset = healthyTimeline().map((row, index) => ({
    ...row,
    stalls: index < 10 ? 3 : 0,
  }));
  const resetScore = lab.scoreRecovery(CRITERIA, observation({ timeline: reset }));
  assert.equal(resetScore.outcome, "harness");
  assert.match(resetScore.errors[0], /decreased without an object rebase/);
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
