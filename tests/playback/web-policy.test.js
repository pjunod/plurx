"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const policy = require("../../crates/plurxd/src/web/playback-policy.js");

// The pure policy module is only half of the Auto counter: index.html decides
// which instant identifies a stall episode before it ever reaches
// `recordStallEpisode`. Calling the policy helper directly cannot see that
// wiring, so the counter regressions below run the shipped UI's own function.
const SHIPPED_UI = fs.readFileSync(
  path.join(__dirname, "../../crates/plurxd/src/web/index.html"),
  "utf8",
);

// Every function these tests borrow is declared at column zero in one inline
// <script>, so the next top-level `function` is a reliable terminator and no
// brace/string parsing is needed. A rename fails loudly rather than silently
// testing nothing.
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

// Deliberately NOT `shippedSource`. The rescue-collision regression has to be
// able to run against a build with no guard at all, or reverting the correction
// would fail it on a missing declaration instead of on the two sessions it
// opens — a name check dressed up as a behaviour check. A build that names the
// guard differently but keeps one automatic session-open still passes, which is
// the contract that actually matters.
function shippedSourceIfPresent(name) {
  const declared = DECLARATIONS.some((kind) =>
    SHIPPED_UI.includes(`${kind}${name}(`),
  );
  return declared ? shippedSource(name) : "";
}

// Rescue paths are async and interleave, so they cannot be judged by a
// synchronous call. Registered here and drained in order at the end of the
// file; a rejection is left unhandled exactly like a synchronous failure, so
// the process still exits nonzero.
const ASYNC_TESTS = [];
function asyncTest(name, run) {
  ASYNC_TESTS.push([name, run]);
}

function test(name, run) {
  try {
    run();
    process.stdout.write(`PASS ${name}\n`);
  } catch (error) {
    error.message = `${name}: ${error.message}`;
    throw error;
  }
}

test("every server verdict reaches exactly one initial web transport", () => {
  const rows = [
    [{ method: "direct_play" }, "direct"],
    [
      { method: "direct_play", selectedAudioIndex: 2 },
      "progressive_remux",
    ],
    [{ method: "remux" }, "progressive_remux"],
    [{ method: "remux", nativeHls: true }, "copy_hls"],
    [{ method: "remux", segmentedRemux: true }, "copy_hls"],
    [
      { method: "remux", nativeHls: true, segmentedRemux: true },
      "copy_hls",
    ],
    [{ method: "transcode" }, "transcode_hls"],
  ];
  for (const [input, expected] of rows) {
    assert.equal(policy.initialRoute(input), expected, JSON.stringify(input));
  }
});

test("native HLS is Safari-only unless MSE is unavailable", () => {
  assert.equal(
    policy.nativeHlsAvailable({
      canPlayNativeHls: false,
      hasWebKitPlaybackTarget: true,
      hlsJsSupported: false,
    }),
    false,
  );
  assert.equal(
    policy.nativeHlsAvailable({
      canPlayNativeHls: true,
      hasWebKitPlaybackTarget: true,
      hlsJsSupported: true,
    }),
    true,
  );
  assert.equal(
    policy.nativeHlsAvailable({
      canPlayNativeHls: true,
      hasWebKitPlaybackTarget: false,
      hlsJsSupported: true,
    }),
    false,
  );
  assert.equal(
    policy.nativeHlsAvailable({
      canPlayNativeHls: true,
      hasWebKitPlaybackTarget: false,
      hlsJsSupported: false,
    }),
    true,
  );
  assert.equal(policy.hlsTransport({ nativeHls: true, hevcCopy: true }), "native");
  assert.equal(policy.hlsTransport({ nativeHls: true, hevcCopy: false }), "mse");
  assert.equal(
    policy.hlsTransport({
      nativeHls: true,
      hevcCopy: false,
      hlsJsSupported: false,
    }),
    "native",
  );
});

test("copy-HLS audio compatibility follows the newly selected track", () => {
  assert.equal(
    policy.copyAudioNeedsTranscode({ codec: "aac", msePairSupported: true }),
    false,
  );
  assert.equal(
    policy.copyAudioNeedsTranscode({ codec: "ac3", msePairSupported: false }),
    true,
  );
  assert.equal(
    policy.copyAudioNeedsTranscode({
      codec: "ac3",
      clientAudioCodecs: ["aac", "ac3"],
      nativeHls: true,
    }),
    false,
  );
  assert.equal(
    policy.copyAudioNeedsTranscode({
      codec: "truehd",
      clientAudioCodecs: ["aac", "ac3"],
      nativeHls: true,
    }),
    true,
  );
  assert.equal(policy.copyAudioNeedsTranscode({ codec: null }), true);
});

test("manual quality and rescue height preserve the viewer's promise", () => {
  const forces = {
    auto: "auto",
    original: "original",
    nomse: "original",
    "1080": "transcode",
    "720": "transcode",
    "480": "transcode",
    "360": "transcode",
  };
  for (const [quality, force] of Object.entries(forces)) {
    assert.equal(policy.qualityForce(quality), force);
  }
  for (const height of [1080, 720, 480, 360]) {
    assert.equal(policy.transcodeHeight(String(height)), height);
  }
  for (const quality of ["auto", "original", "nomse", "future-value"]) {
    assert.equal(policy.transcodeHeight(quality), null);
  }
  assert.equal(
    policy.sessionHeight({
      quality: "original",
      sourceHeight: 2160,
      decidedMethod: "remux",
    }),
    2160,
  );
  assert.equal(
    policy.sessionHeight({
      quality: "auto",
      sourceHeight: 2160,
      decidedMethod: "remux",
      burnedSubtitle: true,
    }),
    2160,
  );
  assert.equal(
    policy.sessionHeight({
      quality: "auto",
      sourceHeight: 2160,
      decidedMethod: "remux",
      refusedOriginal: true,
    }),
    2160,
  );
  assert.equal(
    policy.sessionHeight({
      quality: "auto",
      sourceHeight: 2160,
      decidedMethod: "transcode",
      burnedSubtitle: true,
    }),
    null,
  );
});

const serverLadder = [
  { height: 1080, total_kbps: 8160, peak_kbps: 12160 },
  { height: 720, total_kbps: 4160, peak_kbps: 6160 },
  { height: 480, total_kbps: 2160, peak_kbps: 3160 },
  { height: 360, total_kbps: 1360, peak_kbps: 1960 },
];

const demandingDolbyVision = {
  container: "mkv",
  video_codec: "hevc",
  video_profile: "Main 10",
  width: 3840,
  height: 2160,
  bit_depth: 10,
  hdr: "dolby_vision",
  hdr_format: "Dolby Vision \u00b7 Profile 7 (HDR10-compatible)",
  bitrate: 90_892_368,
};

const playableHdr10 = {
  container: "mkv",
  video_codec: "hevc",
  video_profile: "Main 10",
  width: 3840,
  height: 2160,
  bit_depth: 10,
  hdr: "hdr10",
  hdr_format: "HDR10",
  bitrate: 42_000_000,
};

test("Auto starts from the server ladder, prior, and persisted last-good rung", () => {
  assert.deepEqual(
    policy.normalizedLadder(serverLadder, 700).map((rung) => rung.height),
    [360, 480],
  );
  assert.equal(
    policy.initialAutoRung({ ladder: serverLadder, persistedHeight: 480 }),
    480,
  );
  assert.equal(policy.initialAutoRung({ ladder: serverLadder }), null);
  assert.equal(
    policy.initialAutoRung({
      ladder: serverLadder,
      persistedHeight: 720,
      priorKbps: 1500,
    }),
    360,
  );
  assert.equal(
    policy.initialAutoRung({
      ladder: serverLadder,
      persistedHeight: 1080,
      playerHeight: 700,
    }),
    480,
  );
  assert.equal(
    policy.initialAutoRung({
      ladder: serverLadder,
      persistedHeight: 1080,
      playerHeight: 300,
    }),
    360,
    "a player below the ladder floor still gets the lowest rung",
  );
});

test("the player-height ceiling uses backing pixels rather than CSS pixels", () => {
  assert.equal(
    policy.playerPixelHeight({ layoutHeight: 469, devicePixelRatio: 2 }),
    938,
  );
  assert.equal(
    policy.playerPixelHeight({ layoutHeight: 469, devicePixelRatio: 1 }),
    469,
  );
  assert.equal(
    policy.playerPixelHeight({ intrinsicHeight: 720 }),
    720,
  );
});

test("an outgoing hls.js estimate survives a restart and outranks the cold prior", () => {
  assert.equal(
    policy.bandwidthSeedBps({
      outgoingEstimateBps: 3_200_000,
      priorKbps: 1500,
    }),
    3_200_000,
  );
  assert.equal(policy.bandwidthSeedBps({ priorKbps: 1500 }), 1_500_000);
  assert.equal(policy.bandwidthSeedBps({}), null);
});

test("a completed fragment provides a fresh severe-pressure estimate", () => {
  assert.equal(
    policy.transferSampleKbps({
      loadedBytes: 250_000,
      loadingStartMs: 1_000,
      loadingEndMs: 2_000,
    }),
    2_000,
  );
  assert.equal(policy.transferSampleKbps({ loadedBytes: 250_000 }), null);
});

test("learned decode identity is versioned, deterministic, and bitrate-bucketed", () => {
  const reordered = {
    bitrate: demandingDolbyVision.bitrate,
    hdr_format: demandingDolbyVision.hdr_format,
    height: demandingDolbyVision.height,
    width: demandingDolbyVision.width,
    video_profile: "  MAIN   10 ",
    video_codec: " HEVC ",
    bit_depth: demandingDolbyVision.bit_depth,
    hdr: " DOLBY_VISION ",
    ignored_future_field: "does not change v2",
  };
  const dolbyKey = policy.decodeLimitIdentity(demandingDolbyVision);
  assert.match(dolbyKey, /^decode-v2:/);
  assert.equal(policy.decodeLimitIdentity(reordered), dolbyKey);
  assert.notEqual(policy.decodeLimitIdentity(playableHdr10), dolbyKey);
  assert.equal(policy.decodeLimitIdentity({}), policy.decodeLimitIdentity({}));

  assert.equal(policy.bitrateBucket(9_999_999).index, 0);
  assert.equal(policy.bitrateBucket(10_000_000).index, 1);
  assert.equal(policy.bitrateBucket(19_999_999).index, 1);
  assert.equal(policy.bitrateBucket(20_000_000).index, 2);
  assert.equal(policy.bitrateBucket(null), null);
});

test("a learned limit applies only to the exact media load and Auto", () => {
  const nowMs = 2_000_000_000_000;
  const dolbyKey = policy.decodeLimitIdentity(demandingDolbyVision);
  const limits = {
    "hevc@2160": { lost: 20, rate: 8, secs: 150, at: nowMs - 1_000 },
    "decode-v2:not-json": { lost: 20, rate: 8, secs: 150, at: nowMs - 1_000 },
    [dolbyKey]: { lost: 20, rate: 8, secs: 150, at: nowMs - 1_000 },
  };
  const pruned = policy.pruneDecodeLimits(limits, { nowMs });
  assert.equal(pruned.changed, true);
  assert.deepEqual(Object.keys(pruned.limits), [dolbyKey]);

  assert.equal(
    policy.learnedDecodeLimitAction({
      quality: "auto",
      source: demandingDolbyVision,
      limits,
      nowMs,
    }).action,
    "apply",
  );
  assert.equal(
    policy.learnedDecodeLimitAction({
      quality: "auto",
      source: playableHdr10,
      limits,
      nowMs,
    }).action,
    "none",
  );
  for (const quality of ["original", "nomse", "1080"]) {
    assert.equal(
      policy.learnedDecodeLimitAction({
        quality,
        source: demandingDolbyVision,
        limits,
        nowMs,
      }).action,
      "bypass",
      quality,
    );
  }
});

test("clearing an exact learned limit restores the ordinary HDR remux", () => {
  const nowMs = 2_000_000_000_000;
  const dolbyKey = policy.decodeLimitIdentity(demandingDolbyVision);
  const hdrKey = policy.decodeLimitIdentity(playableHdr10);
  const limits = {
    [dolbyKey]: { lost: 20, rate: 8, secs: 150, at: nowMs - 8 * 86_400_000 },
    [hdrKey]: { lost: 16, rate: 6.4, secs: 150, at: nowMs - 1_000 },
  };
  assert.equal(
    policy.learnedDecodeLimitAction({
      quality: "auto",
      source: demandingDolbyVision,
      limits,
      nowMs,
    }).action,
    "retest",
  );

  const cleared = policy.withoutLearnedDecodeLimit(
    limits,
    demandingDolbyVision,
  );
  assert.equal(cleared.removed, true);
  assert.equal(cleared.limits[dolbyKey], undefined);
  assert.deepEqual(cleared.limits[hdrKey], limits[hdrKey]);
  assert.equal(
    policy.learnedDecodeLimitAction({
      quality: "auto",
      source: demandingDolbyVision,
      limits: cleared.limits,
      nowMs,
    }).action,
    "none",
  );
  assert.equal(
    policy.initialRoute({ method: "remux", nativeHls: true }),
    "copy_hls",
    "without the exact learned verdict, the ordinary HDR10 remux route wins",
  );
});

test("learned HDR fallback names client performance and the range loss", () => {
  const view = policy.learnedDecodeLimitView({
    source: demandingDolbyVision,
    limit: { lost: 20, rate: 8, secs: 150 },
    ordinaryRange: "hdr10",
    deliveredRange: "sdr",
  });
  assert.match(view.loadLabel, /Dolby Vision .* Profile 7/);
  assert.equal(view.rangeConsequence, "HDR10 \u2192 SDR");
  assert.match(view.reason, /learned client-performance limit/);
  assert.match(view.reason, /HDR10 \u2192 SDR/);
});

test("a bandwidth cliff drops from 1080p to the sustainable rung in one move", () => {
  const decision = policy.decideRung({
    ladder: serverLadder,
    currentHeight: 1080,
    estimateKbps: 1500,
    runwaySeconds: 8,
    nowMs: 10_000,
    lastSwitchAtMs: 9_000,
  });
  assert.deepEqual(decision, {
    height: 360,
    reason: "bandwidth cliff",
    emergency: true,
    mildSamples: 0,
    upgradeSinceMs: null,
  });
});

test("an emergency downgrade ignores cooldown, dwell, and restart cost", () => {
  const decision = policy.decideRung({
    ladder: serverLadder,
    currentHeight: 720,
    estimateKbps: 5000,
    activeSupplyStall: true,
    nowMs: 20_000,
    lastSwitchAtMs: 19_999,
  });
  assert.equal(decision.height, 480);
  assert.equal(decision.reason, "supply stalls");
  assert.equal(decision.emergency, true);
});

test("an emergency downgrade cannot be stranded by the player-height ceiling", () => {
  const input = {
    ladder: serverLadder,
    currentHeight: 720,
    estimateKbps: 1541,
    runwaySeconds: 0.1,
    activeSupplyStall: true,
    supplyStalls: 3,
  };
  for (const playerHeight of [469, 300]) {
    const decision = policy.decideRung({ ...input, playerHeight });
    assert.equal(decision.height, 360, `player height ${playerHeight}`);
    assert.equal(decision.reason, "supply stalls", `player height ${playerHeight}`);
    assert.equal(decision.emergency, true, `player height ${playerHeight}`);
  }
});

test("one cliff episode uses fresh throughput and cannot restart twice", () => {
  const first = policy.decideRung({
    ladder: serverLadder,
    currentHeight: 720,
    // The pre-cliff EWMA still admits 480p; the completed fragment measures
    // the shaped link closely enough that only 360p is sustainable.
    estimateKbps: 2_800,
    recentEstimateKbps: 2_000,
    recentEstimateAtMs: 5_000,
    runwaySeconds: 8,
    nowMs: 10_000,
  });
  assert.equal(first.height, 360);
  assert.equal(first.reason, "bandwidth cliff");
  assert.equal(first.emergency, true);

  const duplicateSupplySignal = policy.decideRung({
    ladder: serverLadder,
    currentHeight: first.height,
    estimateKbps: 2_800,
    recentEstimateKbps: 2_000,
    recentEstimateAtMs: 5_000,
    runwaySeconds: 0.1,
    activeSupplyStall: true,
    supplyStalls: 3,
    nowMs: 10_000,
  });
  assert.equal(duplicateSupplySignal.height, 360);
  assert.equal(duplicateSupplySignal.reason, null);
  assert.equal(duplicateSupplySignal.emergency, false);

  const staleSample = policy.decideRung({
    ladder: serverLadder,
    currentHeight: 720,
    estimateKbps: 2_800,
    recentEstimateKbps: 2_000,
    recentEstimateAtMs: 5_000,
    runwaySeconds: 8,
    nowMs: 20_001,
  });
  assert.equal(staleSample.height, 480, "a stale transfer cannot steer a switch");
});

test("three supply stalls act while decode stalls never choose a rung", () => {
  const supply = policy.decideRung({
    ladder: serverLadder,
    currentHeight: 720,
    estimateKbps: 7000,
    supplyStalls: 3,
  });
  assert.equal(supply.height, 480);
  assert.equal(supply.emergency, true);

  const decode = policy.decideRung({
    ladder: serverLadder,
    currentHeight: 720,
    estimateKbps: 7000,
    decodeStalls: 9,
    runwaySeconds: 20,
  });
  assert.equal(decode.height, 720);
  assert.equal(decode.reason, null);
});

test("two long supply-stall episodes do not satisfy the three-stall rescue", () => {
  let events = [];
  for (const stall of [
    { start: 5_000, duration: 3_000 },
    { start: 25_000, duration: 3_000 },
  ]) {
    events = policy.recordStallEpisode({
      events,
      episodeAtMs: stall.start,
      nowMs: stall.start + 100,
    });
    events = policy.recordStallEpisode({
      events,
      episodeAtMs: stall.start,
      nowMs: stall.start + stall.duration,
    });
  }
  assert.deepEqual(events, [5_000, 25_000]);
  assert.equal(events.length >= 3, false);

  events = policy.recordStallEpisode({
    events,
    episodeAtMs: 45_000,
    nowMs: 48_000,
  });
  assert.equal(events.length, 3, "a third real episode reaches the threshold");
});

test("the shipped counter records one event per pause reported at both edges", () => {
  let nowMs = 0;
  const noteAutoStall = new Function(
    "PlaybackPolicy",
    "performance",
    `${shippedSource("noteAutoStall")}\nreturn noteAutoStall;`,
  )(policy, { now: () => nowMs });

  const player = { abr: { stallEvents: { supply: [], decode: [] } } };
  // Two real supply pauses. hls.js raises bufferStalledError just after each
  // one begins; the video element reports the same pause at its end, three
  // seconds later. Keying on the report instant instead of the wait's start
  // turns these two pauses into four events and fires the three-stall rescue.
  for (const startedAt of [5_000, 25_000]) {
    player.waitAt = startedAt;
    nowMs = startedAt + 100;
    noteAutoStall(player, "supply", player.waitAt);
    nowMs = startedAt + 3_000;
    noteAutoStall(player, "supply", startedAt);
    player.waitAt = null;
  }

  const supplyStalls = player.abr.stallEvents.supply.length;
  assert.equal(supplyStalls, 2, "two pauses must not become four stall events");
  assert.equal(
    policy.decideRung({
      ladder: serverLadder,
      currentHeight: 720,
      estimateKbps: 7000,
      supplyStalls,
    }).emergency,
    false,
    "two supply pauses must not reach the three-episode rescue",
  );

  // A third distinct pause still reaches it, so the correction narrows the
  // counter without disarming the rescue.
  player.waitAt = 45_000;
  nowMs = 45_100;
  noteAutoStall(player, "supply", player.waitAt);
  assert.equal(player.abr.stallEvents.supply.length, 3);
  assert.equal(
    policy.decideRung({
      ladder: serverLadder,
      currentHeight: 720,
      estimateKbps: 7000,
      supplyStalls: player.abr.stallEvents.supply.length,
    }).emergency,
    true,
  );
});

test("every shipped stall report carries the wait's start as its identity", () => {
  // The counter above is only correct if its callers hand it the episode
  // instant. These are the three reporting paths review finding 1 named.
  assert.match(
    SHIPPED_UI,
    /noteAutoStall\(p,[^;]*p\.waitAt\)/,
    "the hls.js bufferStalledError report must key on the open wait",
  );
  for (const caller of ["endWait", "persistentWait"]) {
    assert.match(
      shippedSource(caller),
      /recordWaitStall\(p,kind,ms,runway,[^;]*,began\)/,
      `${caller} must report the stall against the instant the wait began`,
    );
  }
});

// Both automatic rescues open a replacement session and both yield at that
// request before anything marks the player, so only the shipped wiring can
// answer whether two of them can be in flight at once. This runs the real
// `maybeDecodeRescue`, `rescueAutoSupply` and `autoControllerTick` against a
// session-open that stays pending until the test releases it — the ordering the
// browser actually produces, where the cheap health GET returns before the
// session-create POST.
function autoRescueHarness(player) {
  const opened = [];
  let releaseOpen = null;
  let openFails = false;
  const video = { currentTime: 12, paused: false, videoHeight: 720 };

  async function startTranscodeFallback(reason) {
    opened.push(reason);
    await new Promise((resolve) => {
      releaseOpen = resolve;
    });
    if (openFails) return; // a 5xx leaves the outgoing stream untouched
    player.method = "transcode";
  }

  const noop = () => {};
  const build = new Function(
    "PLAYER",
    "document",
    "performance",
    "PlaybackPolicy",
    "qualityForce",
    "playedSecs",
    "lostFrameRate",
    "MARGIN_CLEAR_SECS",
    "MARGIN_LOST_PER_MIN",
    "SUPPLY_RUNWAY_SECS",
    "clearDecodeLimit",
    "decodeLimitLabel",
    "decodeMarginVerdict",
    "rememberDecodeLimit",
    "clientLog",
    "toast",
    "playbackContext",
    "setLoading",
    "recordAutoSwitch",
    "startTranscodeFallback",
    "pollSessionHealth",
    "bufferRunway",
    "playerPixelHeight",
    "switchAutoRung",
    "rememberAutoRung",
    [
      shippedSourceIfPresent("claimAutoFallback"),
      shippedSourceIfPresent("releaseAutoFallback"),
      shippedSource("maybeDecodeRescue"),
      shippedSource("rescueAutoSupply"),
      shippedSource("autoControllerTick"),
      "return {maybeDecodeRescue, rescueAutoSupply, autoControllerTick};",
    ].join("\n"),
  );

  const shipped = build(
    player,
    { getElementById: () => video },
    { now: () => 90_000 },
    policy,
    () => "auto",
    () => 150,
    () => ({ lost: 0, rate: 0 }),
    60,
    2,
    6,
    () => false,
    () => "HEVC 2160p",
    () => ({ lost: 15, rate: 6, secs: 150, decodeMs: 91, budgetMs: 41.7 }),
    noop,
    noop,
    noop,
    () => ({}),
    noop,
    noop,
    startTranscodeFallback,
    async () => {}, // the health poll resolves before the session-create request
    () => 30,
    () => 720,
    async () => {},
    noop,
  );

  return {
    ...shipped,
    opened,
    video,
    failNextOpen() {
      openFails = true;
    },
    // Let every pending continuation run without completing the session-open.
    async settle() {
      for (let i = 0; i < 8; i += 1) await Promise.resolve();
    },
    async finishOpen() {
      assert.notEqual(releaseOpen, null, "no session-open was in flight");
      const resolve = releaseOpen;
      releaseOpen = null;
      resolve();
      await this.settle();
    },
  };
}

function pressuredRemuxPlayer() {
  return {
    method: "remux",
    started: true,
    offset: 0,
    decodeRescued: false,
    decodeRetest: false,
    hitches: { back: 5, drop: 5, gap: 5, fps: 24, decodeMs: 91 },
    source: { codec: "hevc", height: 2160 },
    bufferLimits: null,
    autoFallbackInFlight: false,
    // Three supply episodes inside the 60 s window: the rescue is armed.
    abr: {
      switching: false,
      supplyRescued: false,
      stallEvents: { supply: [40_000, 60_000, 80_000], decode: [] },
      lastStallAtMs: 80_000,
      lastSwitchAtMs: 0,
      stableSinceMs: 0,
      switches: [],
    },
  };
}

asyncTest(
  "a decode verdict and the third supply stall in one interval open one session",
  async () => {
    const player = pressuredRemuxPlayer();
    const h = autoRescueHarness(player);

    // Exactly the shipped 5-second interval: maybeDecodeRescue() is not awaited,
    // and autoControllerTick() runs straight after it.
    h.maybeDecodeRescue();
    const tick = h.autoControllerTick();
    await h.settle();

    assert.deepEqual(
      h.opened,
      ["decode-rescue"],
      "a supply rescue must not open a second session behind an in-flight decode rescue",
    );
    assert.equal(
      player.abr.supplyRescued,
      false,
      "the refused supply rescue must not consume its one-shot latch",
    );

    await h.finishOpen();
    await tick;
    assert.deepEqual(h.opened, ["decode-rescue"]);
    assert.equal(
      player.autoFallbackInFlight,
      false,
      "the claim must be released once the session-open settles",
    );
  },
);

asyncTest(
  "a decode verdict during an in-flight supply rescue opens no second session",
  async () => {
    const player = pressuredRemuxPlayer();
    const h = autoRescueHarness(player);

    // The other order: the supply rescue wins the interval, and the decode
    // verdict lands on the next sample while its session-open is still pending.
    const rescue = h.rescueAutoSupply();
    await h.settle();
    h.maybeDecodeRescue();
    await h.settle();

    assert.deepEqual(
      h.opened,
      ["auto-supply"],
      "the decode rescue must not open a session behind an in-flight supply rescue",
    );
    assert.equal(
      player.decodeRescued,
      false,
      "the refused decode rescue must not consume its one-shot latch",
    );

    await h.finishOpen();
    await rescue;
    assert.deepEqual(h.opened, ["auto-supply"]);
    assert.equal(player.autoFallbackInFlight, false);
  },
);

asyncTest("a failed automatic session-open releases the claim", async () => {
  const player = pressuredRemuxPlayer();
  const h = autoRescueHarness(player);
  h.failNextOpen();

  h.maybeDecodeRescue();
  await h.settle();
  assert.deepEqual(h.opened, ["decode-rescue"]);
  await h.finishOpen();

  assert.equal(player.method, "remux", "the failed open must leave the stream");
  assert.equal(
    player.autoFallbackInFlight,
    false,
    "a 5xx must not wedge every later automatic rescue",
  );

  // The supply rescue that was refused while the decode rescue was in flight
  // can now run, so the guard costs nothing once the failure is known.
  const rescue = h.rescueAutoSupply();
  await h.settle();
  assert.deepEqual(h.opened, ["decode-rescue", "auto-supply"]);
  await h.finishOpen();
  await rescue;
});

test("mild pressure needs two samples plus cooldown, dwell, and switch gain", () => {
  const first = policy.decideRung({
    ladder: serverLadder,
    currentHeight: 720,
    estimateKbps: 4500,
    nowMs: 60_000,
    lastSwitchAtMs: 0,
  });
  assert.equal(first.height, 720);
  assert.equal(first.mildSamples, 1);

  const cooling = policy.decideRung({
    ladder: serverLadder,
    currentHeight: 720,
    estimateKbps: 4500,
    mildSamples: 1,
    nowMs: 59_999,
    lastSwitchAtMs: 0,
  });
  assert.equal(cooling.height, 720);

  const second = policy.decideRung({
    ladder: serverLadder,
    currentHeight: 720,
    estimateKbps: 4500,
    mildSamples: 1,
    nowMs: 60_000,
    lastSwitchAtMs: 0,
  });
  assert.equal(second.height, 480);
  assert.equal(second.reason, "bandwidth pressure");

  const marginal = policy.decideRung({
    ladder: serverLadder,
    currentHeight: 720,
    estimateKbps: 5350,
    mildSamples: 1,
    nowMs: 60_000,
    lastSwitchAtMs: 0,
  });
  assert.equal(marginal.height, 720, "the restart costs more than the gain");
});

test("a slow server with draining runway is actionable before a stall", () => {
  const decision = policy.decideRung({
    ladder: serverLadder,
    currentHeight: 720,
    estimateKbps: 9000,
    recentSpeed: 0.8,
    runwaySeconds: 7,
    previousRunwaySeconds: 10,
    mildSamples: 1,
    nowMs: 60_000,
    lastSwitchAtMs: 0,
  });
  assert.equal(decision.height, 480);
  assert.equal(decision.reason, "server supply");
});

test("recovery holds for 45 seconds, moves up once, and respects pixel height", () => {
  const first = policy.decideRung({
    ladder: serverLadder,
    currentHeight: 720,
    estimateKbps: 16_000,
    runwaySeconds: 20,
    nowMs: 60_000,
    lastSwitchAtMs: 0,
  });
  assert.equal(first.height, 720);
  assert.equal(first.upgradeSinceMs, 60_000);

  const upgrade = policy.decideRung({
    ladder: serverLadder,
    currentHeight: 720,
    estimateKbps: 16_000,
    runwaySeconds: 20,
    nowMs: 105_000,
    lastSwitchAtMs: 0,
    upgradeSinceMs: first.upgradeSinceMs,
  });
  assert.equal(upgrade.height, 1080);
  assert.equal(upgrade.reason, "bandwidth recovered");

  const dwell = policy.decideRung({
    ladder: serverLadder,
    currentHeight: 720,
    estimateKbps: 16_000,
    runwaySeconds: 20,
    nowMs: 115_000,
    lastSwitchAtMs: 105_000,
    upgradeSinceMs: 70_000,
  });
  assert.equal(dwell.height, 720, "recovery cannot upgrade twice in 60 seconds");

  const capped = policy.decideRung({
    ladder: serverLadder,
    currentHeight: 480,
    estimateKbps: 16_000,
    runwaySeconds: 20,
    playerHeight: 700,
    nowMs: 120_000,
    lastSwitchAtMs: 0,
    upgradeSinceMs: 70_000,
  });
  assert.equal(capped.height, 480, "the 720p rung exceeds the player");
});

test("a rejected cheap stream gets one compatibility transcode", () => {
  assert.equal(policy.fallbackAction({ method: "direct_play" }), "transcode");
  assert.equal(policy.fallbackAction({ method: "remux" }), "transcode");
  assert.equal(
    policy.fallbackAction({ method: "remux", alreadyTried: true }),
    "fail",
  );
  assert.equal(policy.fallbackAction({ method: "transcode" }), "fail");
  assert.equal(
    policy.fallbackAction({ method: "remux", playbackIsReal: true }),
    "fail",
  );
  assert.equal(
    policy.fallbackAction({ method: "remux", mediaFailure: false }),
    "fail",
  );
});

test("a persistent stall gets one bounded method-aware recovery", () => {
  assert.equal(
    policy.stallRecoveryAction({ method: "remux", quality: "auto" }),
    "transcode",
  );
  for (const quality of ["original", "nomse", "1080"]) {
    assert.equal(
      policy.stallRecoveryAction({ method: "remux", quality }),
      "restart",
      quality,
    );
  }
  for (const method of ["direct_play", "transcode"]) {
    assert.equal(
      policy.stallRecoveryAction({ method, quality: "auto" }),
      "restart",
      method,
    );
  }
  assert.equal(
    policy.stallRecoveryAction({
      method: "remux",
      quality: "auto",
      alreadyRecovered: true,
    }),
    "prompt",
  );
  assert.equal(policy.stallRecoveryAction({ method: "unknown" }), "prompt");
});

test("fallback swaps preserve healthy playback until the replacement exists", () => {
  assert.equal(
    policy.fallbackResetBeforeOpen({ reason: "stall-recovery" }),
    true,
  );
  assert.equal(
    policy.fallbackResetBeforeOpen({ reason: "stall-manual" }),
    true,
  );
  assert.equal(
    policy.fallbackResetBeforeOpen({ reason: "auto-supply" }),
    true,
  );
  for (const reason of ["decode-rescue", "stream-rejected", null]) {
    assert.equal(
      policy.fallbackResetBeforeOpen({ reason }),
      false,
      String(reason),
    );
  }
});

test("a persistent-stall prompt survives later waiting events", () => {
  assert.equal(
    policy.waitingOverlayAction({ started: false, stallPrompt: false }),
    "ignore",
  );
  assert.equal(
    policy.waitingOverlayAction({ started: true, stallPrompt: false }),
    "buffer",
  );
  assert.equal(
    policy.waitingOverlayAction({ started: true, stallPrompt: true }),
    "preserve_prompt",
  );
});

test("HDR subtitle burns keep the current delivery instead", () => {
  for (const deliveredRange of ["dolby_vision", "hdr10", "hlg"]) {
    assert.equal(
      policy.subtitleBurnAction({ requiresBurn: true, deliveredRange }),
      "keep_hdr",
    );
  }
  assert.equal(
    policy.subtitleBurnAction({ requiresBurn: true, deliveredRange: "sdr" }),
    "burn",
  );
  assert.equal(
    policy.subtitleBurnAction({ requiresBurn: true, deliveredRange: null }),
    "burn",
  );
  assert.equal(
    policy.subtitleBurnAction({ requiresBurn: false, deliveredRange: "hdr10" }),
    "native",
  );
});

test("directional seeks use short horizontal and long vertical steps", () => {
  assert.equal(policy.seekDeltaSeconds("ArrowLeft"), -10);
  assert.equal(policy.seekDeltaSeconds("ArrowRight"), 10);
  assert.equal(policy.seekDeltaSeconds("ArrowDown"), -30);
  assert.equal(policy.seekDeltaSeconds("ArrowUp"), 30);
  assert.equal(policy.seekDeltaSeconds("Enter"), null);
});

test("decode rescue uses lost frames over a long window, not pipeline latency", () => {
  assert.equal(
    policy.decodeMarginVerdict(
      { back: 5, drop: 5, gap: 5, fps: 24, decodeMs: 1000 },
      149,
    ),
    null,
  );
  assert.equal(
    policy.decodeMarginVerdict(
      { back: 5, drop: 5, gap: 5, fps: 24, decodeMs: 1000 },
      180,
    ),
    null,
  );
  const verdict = policy.decodeMarginVerdict(
    { back: 5, drop: 5, gap: 5, fps: 24, decodeMs: 91.04 },
    150,
  );
  assert.deepEqual(verdict, {
    lost: 15,
    rate: 6,
    secs: 150,
    decodeMs: 91,
    budgetMs: 41.7,
  });
});

// Every reason the server can refuse a stream with, from the wire body it
// actually sends to the two lines the overlay shows. The generic sentence
// these replace — "the server couldn't build this stream — see Settings →
// Logs" — was shown for all of them alike, including for a session the server
// was still successfully starting.
test("each refusal the server names reaches the overlay as itself", () => {
  const rows = [
    // (status, body, expected title, expected fragment of the detail line)
    [
      404,
      { code: "session_gone", message: "this stream is no longer running" },
      "Playback failed to start.",
      "no longer running",
    ],
    [
      502,
      {
        code: "producer_failed",
        message: "the server's encoder exited before it produced any video (exit status: 1)",
      },
      "Playback failed to start.",
      "exit status: 1",
    ],
    [
      502,
      {
        code: "session_failed",
        message: "the server could not build this stream: the encoder never produced any video",
      },
      "Playback failed to start.",
      "never produced any video",
    ],
    [
      501,
      {
        code: "unsupported_build",
        message: "this server's ffmpeg build has no subtitles filter",
      },
      "Playback failed to start.",
      "no subtitles filter",
    ],
    // The one that is NOT a failure: the server is still inside its own
    // hardware->software recovery and said so.
    [
      503,
      {
        code: "startup_timeout",
        message: "the server is still preparing this stream after 45s",
      },
      "Still preparing this stream…",
      "still preparing",
    ],
  ];
  for (const [status, body, title, fragment] of rows) {
    const failure = policy.parseStreamFailure({
      status,
      body: JSON.stringify(body),
    });
    assert.deepEqual(
      failure,
      { status, code: body.code, message: body.message },
      body.code,
    );
    const overlay = policy.streamFailureOverlay(failure);
    assert.equal(overlay.title, title, body.code);
    assert.ok(
      overlay.detail.toLowerCase().includes(fragment.toLowerCase()),
      `${body.code}: "${overlay.detail}" should contain "${fragment}"`,
    );
    assert.equal(overlay.retryable, body.code === "startup_timeout", body.code);
  }
});

// Only a startup that ran out of budget is retryable, and it is retryable on
// either signal — a server that has not yet migrated the code still says 503.
test("a still-starting stream is never reported as a permanent failure", () => {
  for (const failure of [
    { status: 503, code: "startup_timeout", message: "still preparing" },
    { status: 503, code: null, message: "still preparing" },
    { status: 500, code: "startup_timeout", message: "still preparing" },
  ]) {
    const overlay = policy.streamFailureOverlay(failure);
    assert.equal(overlay.retryable, true, JSON.stringify(failure));
    assert.equal(overlay.title, "Still preparing this stream…");
  }
});

// A guess is worse than the generic sentence: it explains the wrong failure
// with complete confidence. Anything unreadable falls back rather than invents.
test("an illegible refusal explains nothing rather than guessing", () => {
  const nothing = [
    { status: 502, body: "<html>502 Bad Gateway</html>" },
    { status: 502, body: "" },
    { status: 502, body: "null" },
    { status: 502, body: JSON.stringify({ code: "producer_failed" }) },
    { status: 502, body: JSON.stringify({ message: "   " }) },
    // A 200 is not a refusal at all.
    { status: 200, body: JSON.stringify({ message: "fine" }) },
  ];
  for (const input of nothing) {
    assert.equal(policy.parseStreamFailure(input), null, JSON.stringify(input));
  }
  assert.equal(policy.streamFailureOverlay(null), null);
  assert.equal(policy.streamFailureOverlay({ status: 502 }), null);
});

// The legacy `{error}` body several routes still use carries a readable
// sentence too, and dropping it would silently un-explain those routes.
test("the legacy error body is still read", () => {
  assert.deepEqual(
    policy.parseStreamFailure({
      status: 404,
      body: JSON.stringify({ error: "transcode session not found" }),
    }),
    { status: 404, code: null, message: "transcode session not found" },
  );
});

// The stale-reason trap, in its new clothes. A segment refused early in a film
// must not be produced as the confident explanation for a fatal error forty
// minutes later; the generic sentence is the honest answer there.
test("a refusal only explains a failure it is contemporary with", () => {
  const failure = {
    status: 502,
    code: "producer_failed",
    message: "the server's encoder exited before it produced any video",
    at: 1_000_000,
  };
  assert.ok(policy.streamFailureOverlay(failure, 1_000_000 + 60_000));
  assert.equal(policy.streamFailureOverlay(failure, 1_000_000 + 600_000), null);
  // An unstamped failure, or a caller that does not stamp, is still read —
  // the check may not silently discard the only reason there is.
  assert.ok(policy.streamFailureOverlay(failure));
  assert.ok(
    policy.streamFailureOverlay({ ...failure, at: undefined }, 9_999_999),
  );
});

// A 503 belongs to the playlist request that received it. hls.js may retry
// inside the same instance; once that retry loads a level, the refusal is
// stale immediately. This runs the shipped state helpers and also pins the
// LEVEL_LOADED wiring that calls them, so a later fatal cannot inherit the
// earlier "Still preparing" explanation.
test("a successful playlist retry immediately clears its 503 explanation", () => {
  const player = { hls: {} };
  let now = 1_000_000;
  const build = new Function(
    "PlaybackPolicy",
    "PLAYER",
    "Date",
    [
      "let STREAM_FAILURE=null;",
      shippedSource("clearStreamFailure"),
      shippedSource("noteStreamFailure"),
      shippedSource("clearStreamFailureFor"),
      shippedSource("currentStreamFailureOverlay"),
      "return {noteStreamFailure, clearStreamFailureFor, currentStreamFailureOverlay};",
    ].join("\n"),
  );
  const shipped = build(policy, player, { now: () => now });
  shipped.noteStreamFailure(
    503,
    JSON.stringify({
      code: "startup_timeout",
      message: "the server is still preparing this stream after 55s",
    }),
  );
  assert.equal(
    shipped.currentStreamFailureOverlay().title,
    "Still preparing this stream…",
  );

  // A destroyed predecessor's late success is not authority over this stream.
  shipped.clearStreamFailureFor({});
  assert.ok(shipped.currentStreamFailureOverlay());

  // The matching retry succeeded. A later fatal now has no stale refusal to
  // display, even though the old 90-second freshness window is still open.
  now += 1_000;
  shipped.clearStreamFailureFor(player.hls);
  assert.equal(shipped.currentStreamFailureOverlay(), null);

  // LEVEL_LOADED proves only that a retryable playlist request recovered. It
  // must not erase a terminal refusal captured from another HLS request.
  shipped.noteStreamFailure(
    502,
    JSON.stringify({
      code: "producer_failed",
      message: "the encoder exited before it produced video",
    }),
  );
  shipped.clearStreamFailureFor(player.hls);
  assert.equal(
    shipped.currentStreamFailureOverlay().detail,
    "the encoder exited before it produced video",
  );
  assert.match(
    shippedSource("attachHls"),
    /Hls\.Events\.LEVEL_LOADED[\s\S]*?clearStreamFailureFor\(hls\)/,
    "the shipped successful-level event must invalidate its own refusal",
  );
});

// `unsupported_build` is returned by the session-creation POST, before hls.js
// exists. Exercise the shipped api -> openSession -> burnSub catch path rather
// than composing policy helpers: the typed body must survive the rejected
// promise and become the persistent player overlay, not a 2.2-second toast.
asyncTest("a burn session-open refusal reaches the persistent overlay", async () => {
  const loading = [];
  const toasts = [];
  let request = null;
  const player = {
    fileId: 7,
    offset: 0,
    audio: [{ index: 0 }],
    curAudio: 0,
    subs: [{ index: 2, language: "eng" }],
    curSub: -1,
    burnedSub: null,
  };
  const video = { currentTime: 12 };
  const build = new Function(
    "PlaybackPolicy",
    "fetch",
    "document",
    "PLAYER",
    "endWait",
    "streamGeneration",
    "newAttempt",
    "teardownHls",
    "clearSubs",
    "pbSyncSubIcon",
    "setLoading",
    "subLabelFor",
    "transcodeOpts",
    "toast",
    "clientLog",
    "playbackContext",
    "armStall",
    "attachSession",
    "qualityForce",
    "sessionHeight",
    "newRequestId",
    "logout",
    [
      'const API="/api/v1"; let TOKEN="token";',
      'const PLAYBACK_ID="playback-1"; let STREAM_FAILURE=null;',
      shippedSource("api"),
      shippedSource("openSession"),
      shippedSource("currentStreamFailureOverlay"),
      shippedSource("showSessionOpenFailure"),
      shippedSource("burnSub"),
      "return {burnSub};",
    ].join("\n"),
  );
  const shipped = build(
    policy,
    async (url, init) => {
      request = { url, init };
      return {
        status: 501,
        ok: false,
        text: async () =>
          JSON.stringify({
            code: "unsupported_build",
            message: "this server's ffmpeg build has no overlay filter",
          }),
      };
    },
    { getElementById: (id) => (id === "video" ? video : null) },
    player,
    () => {},
    () => ({ me: player, live: () => true }),
    () => {},
    () => {},
    () => {},
    () => {},
    (...args) => loading.push(args),
    () => "English bitmap",
    (start, audio) => ({
      start,
      audio,
      subtitle_burn: player.burnedSub,
    }),
    (message) => toasts.push(message),
    () => {},
    () => ({}),
    () => {},
    () => {},
    () => "auto",
    () => null,
    () => "request-1",
    () => {},
  );

  await shipped.burnSub(2);
  assert.equal(request.url, "/api/v1/files/7/hls/sessions");
  assert.equal(request.init.method, "POST");
  assert.equal(JSON.parse(request.init.body).subtitle_burn, 2);
  assert.deepEqual(loading.at(-1), [
    true,
    "Playback failed to start.",
    "this server's ffmpeg build has no overlay filter",
  ]);
  assert.equal(
    loading.some(([on]) => on === false),
    false,
    "the persistent refusal must not be hidden after the rejected session open",
  );
  assert.equal(toasts.at(-1), "Playback failed");
});

// Drained last, in registration order, after every synchronous case has run.
(async () => {
  for (const [name, run] of ASYNC_TESTS) {
    try {
      await run();
    } catch (error) {
      error.message = `${name}: ${error.message}`;
      throw error;
    }
    process.stdout.write(`PASS ${name}\n`);
  }
})();
