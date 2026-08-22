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
// A slice ends at the next top-level declaration of ANY kind, not only the next
// function. A `const` table sitting between two functions would otherwise be
// swallowed by whichever function precedes it, and a harness that also asks for
// that table by name gets "already declared" — a confusing failure about the
// slicer rather than about the code under test.
// `window.`/`document.` are not declarations, but they are the other thing that
// appears at column zero in this script: top-level listener registration. A
// function followed by one (closePlayer is) would otherwise be sliced together
// with every handler after it, and those RUN at build time — the harness fails
// on an undefined `window` instead of on the function under test.
const TERMINATORS = DECLARATIONS.concat([
  "\nconst ",
  "\nlet ",
  "\nwindow.",
  "\ndocument.",
]);
function sliceDeclaration(start) {
  const rest = SHIPPED_UI.slice(start + 1);
  const ends = TERMINATORS.map((kind) => rest.indexOf(kind, 1)).filter(
    (at) => at !== -1,
  );
  const end = ends.length ? Math.min(...ends) : -1;
  return (end === -1 ? rest : rest.slice(0, end)).trimEnd();
}
function shippedSource(name) {
  const start = DECLARATIONS.map((kind) =>
    SHIPPED_UI.indexOf(`${kind}${name}(`),
  ).find((at) => at !== -1);
  assert.notEqual(start, undefined, `index.html no longer declares ${name}`);
  return sliceDeclaration(start);
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

test("estimated skip markers are hedged without rebuilding each tick", () => {
  let writes = 0;
  const skip = {
    dataset: {},
    value: "",
    get innerHTML() {
      return this.value;
    },
    set innerHTML(value) {
      writes += 1;
      this.value = value;
    },
  };
  const renderSkip = new Function(
    "document",
    "esc",
    `${shippedSource("renderSkip")}
return renderSkip;`,
  )(
    { getElementById: (id) => (id === "pskip" ? skip : null) },
    (value) => value,
  );
  const exact = {
    kind: "credits",
    start_ms: 6_000,
    chapter: true,
  };
  renderSkip(exact);
  assert.equal(
    skip.innerHTML,
    '<button onclick="skipCurrent()">Skip Credits ›</button>',
  );
  renderSkip(exact);
  assert.equal(writes, 1, "the exact marker should not rebuild on timeupdate");

  const estimated = { ...exact, chapter: false };
  renderSkip(estimated);
  assert.equal(
    skip.innerHTML,
    '<button onclick="skipCurrent()">Skip Credits · Estimated ›</button>',
  );
  renderSkip(estimated);
  assert.equal(writes, 2, "the estimated marker should not rebuild on timeupdate");
});

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
function autoRescueHarness(player, autoAbr = true) {
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
    "SERVER",
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
    { playback_auto_abr: autoAbr },
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

async function autoRungTick(autoAbr) {
  const switches = [];
  let polls = 0;
  const player = {
    method: "transcode",
    started: true,
    offset: 0,
    ladder: serverLadder,
    autoHeight: 720,
    health: { target_height: 720, recent_speed: 2 },
    hls: { bandwidthEstimate: 1_000_000 },
    abr: {
      switching: false,
      stallEvents: { supply: [], decode: [] },
      recentEstimateKbps: null,
      recentEstimateAtMs: null,
      lastStallAtMs: null,
      lastSwitchAtMs: 0,
      mildSamples: 0,
      upgradeSinceMs: null,
      previousRunway: null,
      stableSinceMs: 0,
      failedHeights: new Set(),
    },
  };
  const tick = new Function(
    "PLAYER",
    "SERVER",
    "document",
    "performance",
    "PlaybackPolicy",
    "qualityForce",
    "SUPPLY_RUNWAY_SECS",
    "pollSessionHealth",
    "rescueAutoSupply",
    "bufferRunway",
    "playerPixelHeight",
    "switchAutoRung",
    "rememberAutoRung",
    `${shippedSource("autoControllerTick")}\nreturn autoControllerTick;`,
  )(
    player,
    { playback_auto_abr: autoAbr },
    { getElementById: () => ({ currentTime: 12, paused: false, videoHeight: 720 }) },
    { now: () => 90_000 },
    policy,
    () => "auto",
    6,
    async () => { polls += 1; },
    async () => {},
    () => 1,
    () => 720,
    async (from, decision) => { switches.push([from, decision.height]); },
    () => {},
  );

  await tick();
  return { polls, switches };
}

asyncTest("the Auto switch gates an automatic rung change", async () => {
  const enabled = await autoRungTick(true);
  assert.deepEqual(enabled.switches, [[720, 360]], "the enabled controller still acts");

  const disabled = await autoRungTick(false);
  assert.equal(disabled.polls, 0, "off must return before sampling controller health");
  assert.deepEqual(disabled.switches, [], "off must not change the playback rung");
});

test("the disabled Auto controller leaves every manual ladder rung available", () => {
  const build = new Function(
    "PLAYER",
    "SERVER",
    "PlaybackPolicy",
    [
      shippedBinding("const", "QUALITY_MODES"),
      shippedSource("qualityOptions"),
      "return qualityOptions();",
    ].join("\n"),
  );
  const options = build(
    { ladder: serverLadder },
    { playback_auto_abr: false },
    policy,
  );

  assert.deepEqual(
    options.map(([value]) => value),
    ["auto", "original", "nomse", "1080", "720", "480", "360"],
  );
});

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

// ---- detail-screen track facts + pre-play selection (issue #266) ----------
//
// Everything below runs the SHIPPED index.html functions. The detail screen is
// generated JavaScript, so a Rust suite can be green while the page has stopped
// naming a subtitle track or has started sending `audio=` on a request nobody
// touched — and that last one silently downgrades a direct play.

// A top-level data declaration, sliced the way shippedSource() slices a
// function. The tests then exercise the shipped table rather than a copy that
// drifts away from it.
function shippedBinding(keyword, name) {
  const at = SHIPPED_UI.indexOf(`\n${keyword} ${name}=`);
  assert.notEqual(at, -1, `index.html no longer declares ${keyword} ${name}`);
  return sliceDeclaration(at);
}

// The detail screen with no browser: format helpers that are not under test are
// stubbed, everything that decides what a viewer READS is shipped code.
function detailHarness({ decisions = {} } = {}) {
  const requested = [];
  const build = new Function(
    "document",
    "PlaybackPolicy",
    "qualityForce",
    "CAPS_Q",
    "api",
    "fmtSize",
    "fmtDur",
    "fmtMbps",
    [
      shippedSource("esc"),
      shippedSource("fmtChannels"),
      shippedSource("audioLabel"),
      shippedBinding("const", "LANGS"),
      shippedSource("langName"),
      shippedSource("subNeedsBurn"),
      shippedBinding("const", "SUB_FORMATS"),
      shippedSource("subFormat"),
      shippedSource("subFactLabel"),
      shippedSource("audioFactLabel"),
      shippedSource("trackChip"),
      shippedSource("trackFactRow"),
      shippedSource("preferredLanguageNote"),
      shippedSource("specBlock"),
      shippedBinding("let", "PREPLAY"),
      shippedSource("prePlaySelection"),
      shippedSource("clearPrePlay"),
      shippedSource("prePlaySelectionQuery"),
      shippedSource("decisionUrl"),
      shippedSource("setPrePlay"),
      shippedSource("prePlayPickers"),
      shippedBinding("const", "PREPLAY_SCOPE_NOTE"),
      shippedSource("prePlayBurnNeeded"),
      shippedSource("prePlayApplication"),
      shippedSource("prePlayPreview"),
      "return {specBlock, prePlayPickers, setPrePlay, clearPrePlay," +
        " prePlaySelection, decisionUrl, prePlayApplication, prePlayBurnNeeded," +
        " preferredLanguageNote};",
    ].join("\n"),
  );
  const shipped = build(
    // No note element: setPrePlay's preview early-outs, which is what a click
    // on a page this test never rendered would do.
    { getElementById: () => null },
    policy,
    () => "auto",
    "vcodec=h264&acodec=aac",
    async (url) => {
      requested.push(url);
      const answer = decisions[url];
      if (!answer) throw new Error(`no stubbed decision for ${url}`);
      return answer;
    },
    () => "3.4 GB",
    () => "1h 52m",
    () => "8.1 Mb/s",
  );
  return { ...shipped, requested };
}

function bookMetadataHarness() {
  const build = new Function(
    "grid",
    [
      shippedSource("esc"),
      shippedSource("bookByline"),
      shippedSource("bookEditionSection"),
      "return {bookByline, bookEditionSection};",
    ].join("\n"),
  );
  return build((items) => `<div data-editions="${items.length}"></div>`);
}

test("book bylines escape provider text and edition rows use only server relations", () => {
  const shipped = bookMetadataHarness();
  const byline = shipped.bookByline({
    kind: "book",
    author: 'A. Reader <script src="https://example.com/x.js"></script>',
  });
  assert.match(byline, /^<div class="muted"[^>]*>By A\. Reader /);
  assert.doesNotMatch(byline, /<script/);
  assert.match(byline, /&lt;script/);
  assert.equal(shipped.bookByline({ kind: "movie", author: "Wrong" }), "");
  assert.equal(shipped.bookEditionSection({ editions: [] }), "");
  assert.match(
    shipped.bookEditionSection({ editions: [{ id: 2, kind: "audiobook" }] }),
    /Other editions[\s\S]+data-editions="1"/,
  );
});

const MOVIE_FILE = {
  id: 42,
  filename: "Arrival.2016.mkv",
  available: true,
  container: "mkv",
  video_codec: "hevc",
  width: 3840,
  height: 2160,
  audio_streams: [
    { index: 0, codec: "truehd", channels: 8, language: "eng", default: true },
    { index: 1, codec: "ac3", channels: 6, language: "fra" },
  ],
  subtitle_streams: [
    {
      index: 0,
      codec: "subrip",
      language: "eng",
      forced: true,
      title: "Heptapod",
    },
    { index: 1, codec: "subrip", language: "eng", hearing_impaired: true },
    { index: 2, codec: "hdmv_pgs_subtitle", language: "fra" },
  ],
  playback_defaults: {
    audio: {
      selected_index: 0,
      preferred_language: "eng",
      preferred_language_status: "selected",
    },
    subtitle: {
      selected_index: 1,
      preferred_language: "eng",
      preferred_language_status: "selected",
    },
  },
};

test("the detail screen names every subtitle track, its format and its markers", () => {
  const html = detailHarness().specBlock(MOVIE_FILE);
  assert.match(html, /<dt>Subtitles<\/dt>/);
  // Language · format · forced/SDH — the three facts criterion 1 asks for, for
  // tracks that until now were invisible until playback started.
  assert.match(html, /English · SRT · forced · Heptapod/);
  assert.match(html, /English · SRT · SDH/);
  assert.match(html, /French · PGS/);
  // The server's chosen track, marked as such, from `playback_defaults` — not
  // re-derived here from `default` flags or admin settings.
  assert.match(
    html,
    /English · SRT · SDH · <span class="tdef">plays by default<\/span>/,
  );
  assert.equal(
    (html.match(/plays by default/g) || []).length,
    2,
    "exactly one audio and one subtitle track carry the marker",
  );
});

test("the detail screen keeps only the selected subtitle visible until expanded", () => {
  const html = detailHarness().specBlock(MOVIE_FILE);
  const disclosure = html.match(/<details class="trkfold">([\s\S]+?)<\/details>/)?.[1];
  assert.ok(disclosure, "multiple subtitle tracks use a native disclosure");
  assert.doesNotMatch(html, /<details class="trkfold" open>/);
  assert.match(
    disclosure,
    /^<summary><span class="trk on">English · SRT · SDH · <span class="tdef">plays by default<\/span><\/span><span class="trkmore">2 more<\/span><\/summary>/,
  );
  assert.match(disclosure, /English · SRT · forced · Heptapod/);
  assert.match(disclosure, /French · PGS/);
});

test("one subtitle track needs no expand control", () => {
  const html = detailHarness().specBlock({
    ...MOVIE_FILE,
    subtitle_streams: [MOVIE_FILE.subtitle_streams[0]],
    playback_defaults: {
      ...MOVIE_FILE.playback_defaults,
      subtitle: {
        ...MOVIE_FILE.playback_defaults.subtitle,
        selected_index: 0,
      },
    },
  });
  assert.equal(html.includes('class="trkfold"'), false);
  assert.match(html, /English · SRT · forced · Heptapod/);
});

test("a file with no subtitle tracks says so instead of showing an empty row", () => {
  const bare = {
    ...MOVIE_FILE,
    subtitle_streams: [],
    playback_defaults: {
      ...MOVIE_FILE.playback_defaults,
      subtitle: {
        selected_index: null,
        preferred_language: "eng",
        preferred_language_status: "no_tracks",
      },
    },
  };
  const html = detailHarness().specBlock(bare);
  assert.match(html, /<dt>Subtitles<\/dt><dd><div class="trks">None<\/div>/);
  // `no_tracks` must not also produce "no English subtitles": the row already
  // said it, and saying it twice reads as two different problems.
  assert.equal(html.includes("langnote"), false);
});

test("an audiobook part is not asked about subtitles it could never have", () => {
  const part = {
    id: 9,
    filename: "book-01.m4b",
    available: true,
    container: "m4b",
    audio_streams: [{ index: 0, codec: "aac", channels: 2, default: true }],
    subtitle_streams: [],
    playback_defaults: {
      audio: {
        selected_index: 0,
        preferred_language: "eng",
        preferred_language_status: "unknown",
      },
      subtitle: {
        selected_index: null,
        preferred_language: "eng",
        preferred_language_status: "no_tracks",
      },
    },
  };
  const html = detailHarness().specBlock(part);
  assert.equal(html.includes("<dt>Subtitles</dt>"), false);
  assert.equal(html.includes("preplay"), false, "nothing to choose between");
});

test("the preferred-language sentence keeps `unknown` distinct from `missing`", () => {
  const { preferredLanguageNote } = detailHarness();
  const note = (status) =>
    preferredLanguageNote("subtitles", {
      preferred_language: "eng",
      preferred_language_status: status,
    });
  assert.equal(note("missing"), "no English subtitles");
  // The whole point of the fifth state: an untagged track means the absence of
  // English cannot be claimed, so the page must not claim it.
  assert.notEqual(note("unknown"), note("missing"));
  assert.match(note("unknown"), /no language tag/);
  assert.match(note("available"), /English subtitles available/);
  assert.equal(note("selected"), "", "the marked chip already says this");
  assert.equal(note("no_tracks"), "", "the row already says this");
});

test("a scanning viewer learns 'English audio, no English subtitles'", () => {
  const html = detailHarness().specBlock({
    ...MOVIE_FILE,
    subtitle_streams: [{ index: 0, codec: "subrip", language: "fra" }],
    playback_defaults: {
      audio: {
        selected_index: 0,
        preferred_language: "eng",
        preferred_language_status: "selected",
      },
      subtitle: {
        selected_index: null,
        preferred_language: "eng",
        preferred_language_status: "missing",
      },
    },
  });
  assert.match(html, /English · TRUEHD/);
  assert.match(html, /no English subtitles/);
});

test("both pickers offer the server default, every track, and Off", () => {
  const html = detailHarness().prePlayPickers(MOVIE_FILE);
  assert.match(html, /<select id="pp-a-42"/);
  assert.match(html, /<select id="pp-s-42"/);
  // "Default" is its own option and is what an untouched picker sends: see the
  // request test below for why that distinction is load-bearing.
  assert.match(html, /<option value="" selected>Default · English · TRUEHD · 7\.1</);
  assert.match(html, /<option value="-1">Off<\/option>/);
  assert.match(html, /<option value="2">French · PGS<\/option>/);
  assert.match(html, /it doesn’t change your Playback defaults/);
});

test("an untouched picker changes the decision request by nothing at all", () => {
  const h = detailHarness();
  const legacy = "/files/42/decision?vcodec=h264&acodec=aac&force=auto";
  assert.equal(h.decisionUrl(42, "auto", h.prePlaySelection(42)), legacy);
  // Choosing, then choosing "Default" again, must return to the byte-for-byte
  // legacy request — otherwise reverting a choice leaves a remux behind.
  h.setPrePlay(42, "audio", "1");
  assert.equal(
    h.decisionUrl(42, "auto", h.prePlaySelection(42)),
    `${legacy}&audio=1`,
  );
  h.setPrePlay(42, "audio", "");
  assert.equal(h.decisionUrl(42, "auto", h.prePlaySelection(42)), legacy);
  // Off is an explicit choice, and -1 is how it is spelled.
  h.setPrePlay(42, "subtitle", "-1");
  assert.equal(
    h.decisionUrl(42, "auto", h.prePlaySelection(42)),
    `${legacy}&subtitle=-1`,
  );
});

test("a pre-play choice is per file and does not survive leaving the item", () => {
  const h = detailHarness();
  h.setPrePlay(42, "audio", "1");
  assert.deepEqual(h.prePlaySelection(42), { audio: 1, subtitle: null });
  // A multi-version item's other file is a different set of streams; one
  // item-wide index would point into the wrong file.
  assert.equal(h.prePlaySelection(43), null);
  h.clearPrePlay();
  assert.equal(
    h.prePlaySelection(42),
    null,
    "a choice made on one item must not apply to the next",
  );
});

// The cold-start half: what a selection does to the FIRST session open.
const PGS_DECISION = {
  method: "transcode",
  delivered_dynamic_range: "sdr",
  subtitles: [
    { index: 0, codec: "subrip", text: true },
    { index: 2, codec: "hdmv_pgs_subtitle", text: false },
  ],
  selection: {
    audio_index: 0,
    subtitle_index: 2,
    subtitle_requires_burn_in: true,
    subtitle_burn_in_blocked_by_hdr: false,
  },
};

test("a pre-play burn rides the first session open rather than a restart", () => {
  const applied = detailHarness().prePlayApplication(PGS_DECISION, {
    audio: null,
    subtitle: 2,
  });
  assert.deepEqual(applied, {
    subtitle: 2,
    blockedByHdr: false,
    burnedSub: 2,
    textSub: null,
  });
});

test("a pre-play text subtitle is a <track>, not a second session", () => {
  const applied = detailHarness().prePlayApplication(
    {
      method: "direct_play",
      delivered_dynamic_range: "sdr",
      subtitles: [{ index: 0, codec: "subrip", text: true }],
      selection: {
        audio_index: 0,
        subtitle_index: 0,
        subtitle_requires_burn_in: false,
        subtitle_burn_in_blocked_by_hdr: false,
      },
    },
    { audio: null, subtitle: 0 },
  );
  assert.equal(applied.burnedSub, null);
  assert.equal(applied.textSub, 0, "applied locally, with no stream restart");
});

test("the HDR guard refuses a pre-play burn before the stream exists", () => {
  const applied = detailHarness().prePlayApplication(
    {
      ...PGS_DECISION,
      method: "remux",
      delivered_dynamic_range: "dolby_vision",
      selection: { ...PGS_DECISION.selection, subtitle_burn_in_blocked_by_hdr: true },
    },
    { audio: null, subtitle: 2 },
  );
  assert.equal(applied.blockedByHdr, true);
  assert.equal(applied.burnedSub, null, "HDR is kept, exactly as in-player");
  assert.equal(applied.textSub, null);
});

test("an audio-only choice never burns the subtitle the server merely echoed", () => {
  // `selection.subtitle_index` is the POLICY default here — the request carried
  // no `subtitle=`. Reading that echo as a choice would burn a bitmap track
  // into a film the viewer only asked to hear in French.
  const applied = detailHarness().prePlayApplication(PGS_DECISION, {
    audio: 1,
    subtitle: null,
  });
  assert.deepEqual(applied, {
    subtitle: null,
    blockedByHdr: false,
    burnedSub: null,
    textSub: null,
  });
});

test("a burn this browser needs is honoured even when the plan omits it", () => {
  // PGS with the application overlay enabled: the server plans a remux, because
  // a native client would draw the overlay itself. This player has no overlay
  // route, so following the plan would start a stream that cannot show the
  // chosen track and replace it a moment later.
  const overlayPlan = {
    method: "remux",
    delivered_dynamic_range: "sdr",
    subtitles: [{ index: 2, codec: "hdmv_pgs_subtitle", text: false }],
    selection: {
      audio_index: 0,
      subtitle_index: 2,
      subtitle_requires_burn_in: false,
      subtitle_burn_in_blocked_by_hdr: false,
    },
  };
  const h = detailHarness();
  assert.equal(h.prePlayBurnNeeded(overlayPlan, 2), true);
  assert.equal(h.prePlayApplication(overlayPlan, { subtitle: 2 }).burnedSub, 2);
});

// ---- the carry ends with the playback (review finding 1 on PR #293) --------
//
// play() asks playbackSelection() which tracks to run with, and the answer
// turns on one thing: is this still the playback that is already open? A
// quality change is; the same file played again after the viewer closed the
// player is not. closePlayer() does not replace PLAYER, so that distinction
// exists only because closePlayer() drops `preplay` — without it the pickers on
// the detail screen become decoration after a file's first playback.
//
// The whole scenario runs the SHIPPED closePlayer(), not a description of it.
function carryHarness(player) {
  const stubEl = () => ({
    classList: { remove() {}, add() {} },
    style: {},
    dataset: {},
    innerHTML: "",
    querySelectorAll: () => [],
    pause() {},
    removeAttribute() {},
    load() {},
  });
  const build = new Function(
    "document",
    "PLAYER",
    "exitPresentationModes",
    "reportProgress",
    "releaseSession",
    "stopPlayerTimers",
    "clearInterval",
    "STATS_TIMER",
    "teardownHls",
    "setLoading",
    "location",
    "setTimeout",
    "prePlayPreview",
    [
      shippedBinding("let", "PREPLAY"),
      shippedSource("prePlaySelection"),
      shippedSource("clearPrePlay"),
      shippedSource("playbackSelection"),
      shippedSource("setPrePlay"),
      shippedSource("rememberPlaybackSelection"),
      shippedSource("closePlayer"),
      "return {prePlaySelection, clearPrePlay, playbackSelection, setPrePlay," +
        " rememberPlaybackSelection, closePlayer};",
    ].join("\n"),
  );
  return build(
    { getElementById: stubEl },
    player,
    () => {},
    () => {},
    () => {},
    () => {},
    () => {},
    null,
    () => {},
    () => {},
    // Not an item route: the deferred re-render is a different concern, and the
    // test performs loadItem()'s picker reset explicitly where it happens.
    { hash: "#/" },
    () => {},
    () => {},
  );
}

test("closing the player ends its track choice instead of arming the next play", () => {
  const player = { fileId: 42, preplay: { audio: 1, subtitle: null } };
  const h = carryHarness(player);
  // While it is open, this playback's own tracks are the answer — that is the
  // carry a quality change depends on.
  assert.deepEqual(h.playbackSelection(player, 42), { audio: 1, subtitle: null });
  h.closePlayer();
  // loadItem() empties the pickers on the way back to the detail screen, so
  // "Default" is what the viewer now sees on both of them.
  h.clearPrePlay();
  assert.equal(
    h.playbackSelection(player, 42),
    null,
    "a closed playback's tracks must not be reused by the next cold start, " +
      "which the screen is showing as Default",
  );
});

test("a picker changed after a playback is not overruled by that playback", () => {
  const player = { fileId: 42, preplay: null };
  const h = carryHarness(player);
  // An in-player switch records itself the same way a pre-play choice does.
  h.rememberPlaybackSelection("audio", 0);
  assert.deepEqual(h.playbackSelection(player, 42), { audio: 0, subtitle: null });
  h.closePlayer();
  h.clearPrePlay();
  // Back on the detail screen the viewer picks the French track instead.
  h.setPrePlay(42, "audio", "1");
  assert.deepEqual(
    h.playbackSelection(player, 42),
    { audio: 1, subtitle: null },
    "the explicit choice on screen wins, not the previous playback's",
  );
});

test("a quality change still reproduces the tracks that are playing", () => {
  // The other half of the contract: ending the carry at close must not end it
  // mid-playback, or changing quality would silently revert the viewer's tracks
  // to the cold-start policy default.
  const player = { fileId: 42, preplay: null };
  const h = carryHarness(player);
  h.rememberPlaybackSelection("audio", 1);
  h.rememberPlaybackSelection("subtitle", 2);
  assert.deepEqual(h.playbackSelection(player, 42), { audio: 1, subtitle: 2 });
  // A different file is a cold start even while this one is open.
  h.setPrePlay(43, "subtitle", "-1");
  assert.deepEqual(h.playbackSelection(player, 43), { audio: null, subtitle: -1 });
});

// ---- HEVC capability probe -------------------------------------------------
// The probe is this client's only statement about what its decoder can do, and
// the failure it guards against is silent: a 4K Main10 stream direct-played to
// an 8-bit decoder is accepted by every layer above the decoder and rendered as
// a black picture with working sound. No `error` event fires, so no fallback
// runs. The shipped ladder, fold and query builder are exercised here, not a
// copy of them, because a copy is exactly what stops telling the truth.
function hevcHarness({
  supported = [],
  pqSupported = [],
  mediaCapabilities = "auto",
  hdrDisplay = false,
} = {}) {
  const hit = (list) => (type) => list.some((c) => String(type).includes(c));
  const decodes = hit(supported);
  const MediaSource = { isTypeSupported: decodes };
  const win = { MediaSource };
  const doc = {
    createElement: () => ({ canPlayType: (t) => (decodes(t) ? "probably" : "") }),
  };
  const calls = [];
  const nav = {};
  if (mediaCapabilities === "auto") {
    nav.mediaCapabilities = {
      decodingInfo(config) {
        calls.push({
          type: config.type,
          contentType: config.video.contentType,
          width: config.video.width,
          height: config.video.height,
          transferFunction: config.video.transferFunction || null,
        });
        const answers =
          config.video.transferFunction === "pq" ? pqSupported : supported;
        const ok = hit(answers)(config.video.contentType);
        return Promise.resolve({ supported: ok, smooth: ok, powerEfficient: ok });
      },
    };
  } else if (mediaCapabilities !== null) {
    nav.mediaCapabilities = mediaCapabilities;
  }
  const build = new Function(
    "window",
    "document",
    "MediaSource",
    "navigator",
    "displayIsHdr",
    [
      shippedBinding("const", "HEVC_TIERS"),
      shippedSource("hevcTierSummary"),
      shippedSource("hevcTiersSync"),
      shippedSource("hevcTiersMediaCapabilities"),
      shippedSource("buildPlayCaps"),
      shippedSource("capsQuery"),
      "return {HEVC_TIERS, hevcTierSummary, hevcTiersSync," +
        " hevcTiersMediaCapabilities, buildPlayCaps, capsQuery};",
    ].join("\n"),
  );
  const shipped = build(win, doc, MediaSource, nav, () => hdrDisplay);
  return {
    ...shipped,
    calls,
    codecs: shipped.HEVC_TIERS.map((t) => t.codec),
    // Everything the client would put on the wire, from the synchronous ladder.
    syncCaps() {
      const summary = shipped.hevcTierSummary(
        shipped.hevcTiersSync(
          (t) => doc.createElement().canPlayType(t) !== "",
          (t) => MediaSource.isTypeSupported(t),
        ),
        null,
      );
      return shipped.buildPlayCaps(summary);
    },
    // …and from the MediaCapabilities refinement, exactly as PLAY_CAPS_READY
    // does it: the synchronous answer stands when the API says nothing.
    async refinedCaps() {
      const mc = await shipped.hevcTiersMediaCapabilities();
      if (!mc) return this.syncCaps();
      return shipped.buildPlayCaps(
        shipped.hevcTierSummary(mc.passed, mc.pqPassed),
      );
    },
  };
}
const MAIN8 = ["hvc1.1.6.L93.B0", "hvc1.1.6.L120.B0", "hvc1.1.6.L153.B0"];
const MAIN10 = ["hvc1.2.4.L120.B0", "hvc1.2.4.L153.B0"];

test("the reported height is one every claimed HEVC profile actually decodes", () => {
  const h = hevcHarness();
  const rows = [
    [[], [], { depth8: 0, depth10: 0, maxheight: 0, pq10: false }],
    [
      ["hvc1.1.6.L93.B0"],
      [],
      { depth8: 720, depth10: 0, maxheight: 720, pq10: false },
    ],
    [MAIN8, [], { depth8: 2160, depth10: 0, maxheight: 2160, pq10: false }],
    [
      MAIN8.concat(MAIN10),
      MAIN10,
      { depth8: 2160, depth10: 2160, maxheight: 2160, pq10: true },
    ],
    // The whole point. Main10 decodes only to 1080p while 8-bit reaches 4K: the
    // client may not say "2160" and "hevc10" in one breath, because the server's
    // max_height is codec-agnostic and would then direct-play a 4K Main10.
    [
      MAIN8.concat(["hvc1.2.4.L120.B0"]),
      ["hvc1.2.4.L120.B0"],
      { depth8: 2160, depth10: 1080, maxheight: 1080, pq10: true },
    ],
    // PQ proven at 1080 only while the reported height is 2160 — hdr10t is a
    // claim about the reported height, so it must not be made here.
    [
      MAIN8.concat(MAIN10),
      ["hvc1.2.4.L120.B0"],
      { depth8: 2160, depth10: 2160, maxheight: 2160, pq10: false },
    ],
  ];
  for (const [passing, pq, expected] of rows) {
    assert.deepEqual(
      h.hevcTierSummary(
        h.codecs.map((c) => passing.includes(c)),
        h.codecs.map((c) => pq.includes(c)),
      ),
      expected,
      JSON.stringify(passing),
    );
  }
});

test("an 8-bit-only decoder is never reported to the server as Main10", () => {
  const caps = hevcHarness({
    supported: ["hvc1.1.6.L93.B0", "hvc1.1.6.L120.B0"],
  }).syncCaps();
  assert.equal(caps.vcodec.split(",").includes("hevc"), true);
  assert.equal(caps.vcodec.split(",").includes("hevc10"), false);
  assert.equal(caps.maxheight, 1080);
  assert.equal(caps.hdr10t, 0);
});

test("no HEVC decoder means no maxheight, so an HEVC cap never caps H.264", () => {
  const h = hevcHarness({ supported: ["avc1.640033"] });
  const caps = h.syncCaps();
  assert.equal(caps.vcodec.includes("hevc"), false);
  assert.equal(caps.maxheight, null);
  assert.equal(h.capsQuery(caps).includes("maxheight"), false);
});

test("the synchronous fallback cannot prove PQ, so it never claims hdr10t", () => {
  // isTypeSupported/canPlayType have no transfer-function axis. A Main10 yes
  // plus an HDR panel is the guess this probe exists to refuse to make.
  const caps = hevcHarness({
    supported: MAIN8.concat(MAIN10),
    hdrDisplay: true,
  }).syncCaps();
  assert.equal(caps.vcodec.split(",").includes("hevc10"), true);
  assert.equal(caps.maxheight, 2160);
  assert.equal(caps.hdr, 1); // the loose, long-standing claim is unchanged
  assert.equal(caps.hdr10t, 0); // the strict new one is not made
});

asyncTest(
  "hdr10t needs a PQ answer AND an HDR display, and says so on the wire",
  async () => {
    const hdr = await hevcHarness({
      supported: MAIN8.concat(MAIN10),
      pqSupported: MAIN10,
      hdrDisplay: true,
    }).refinedCaps();
    assert.equal(hdr.maxheight, 2160);
    assert.equal(hdr.hdr10t, 1);

    const sdrPanel = await hevcHarness({
      supported: MAIN8.concat(MAIN10),
      pqSupported: MAIN10,
      hdrDisplay: false,
    }).refinedCaps();
    assert.equal(sdrPanel.vcodec.split(",").includes("hevc10"), true);
    assert.equal(sdrPanel.maxheight, 2160, "the Main10 claim stays honest on SDR");
    assert.equal(sdrPanel.hdr10t, 0);

    // Decodes 10-bit but cannot put PQ on the wire — a 10-bit SDR rip must
    // still direct-play, so `hevc10` stays and only `hdr10t` goes.
    const noPq = await hevcHarness({
      supported: MAIN8.concat(MAIN10),
      pqSupported: [],
      hdrDisplay: true,
    }).refinedCaps();
    assert.equal(noPq.vcodec.split(",").includes("hevc10"), true);
    assert.equal(noPq.hdr10t, 0);

    const h = hevcHarness({
      supported: MAIN8.concat(MAIN10),
      pqSupported: MAIN10,
      hdrDisplay: true,
    });
    const query = h.capsQuery(await h.refinedCaps());
    assert.match(query, /&maxheight=2160&/);
    assert.match(query, /&hdr10t=1$/);
    assert.equal(
      h.calls.some((c) => c.transferFunction === "pq"),
      true,
      "the HDR rungs are asked with transferFunction:'pq'",
    );
  },
);

asyncTest(
  "a browser with no MediaCapabilities degrades without crashing or over-claiming",
  async () => {
    const absent = hevcHarness({
      supported: MAIN8.concat(MAIN10),
      mediaCapabilities: null,
      hdrDisplay: true,
    });
    assert.equal(await absent.hevcTiersMediaCapabilities(), null);
    const caps = await absent.refinedCaps();
    assert.equal(caps.vcodec.split(",").includes("hevc10"), true);
    assert.equal(caps.maxheight, 2160);
    assert.equal(caps.hdr10t, 0);

    // Present but rejecting every configuration: the same fallback, not an
    // exception escaping into boot() and a page that never renders.
    const throws = hevcHarness({
      supported: MAIN8,
      mediaCapabilities: {
        decodingInfo() {
          throw new TypeError("unsupported configuration");
        },
      },
      hdrDisplay: true,
    });
    assert.equal(await throws.hevcTiersMediaCapabilities(), null);
    assert.equal((await throws.refinedCaps()).maxheight, 2160);
  },
);

asyncTest("the Dolby Vision probe is unchanged by the tiering", async () => {
  // Chrome answers no to dvh1.05.06 and must keep doing so; Safari answers yes
  // to both profiles. Neither answer may move because HEVC learned about depth.
  const chrome = await hevcHarness({
    supported: MAIN8.concat(MAIN10),
    pqSupported: MAIN10,
    hdrDisplay: true,
  }).refinedCaps();
  assert.equal(chrome.dv, 0);
  assert.equal(chrome.dvprofile, "");

  const safari = hevcHarness({
    supported: MAIN8.concat(MAIN10, ["dvh1.05.06", "dvhe.08.07"]),
    mediaCapabilities: null,
    hdrDisplay: true,
  });
  const caps = await safari.refinedCaps();
  assert.equal(caps.dv, 1);
  assert.equal(caps.dvprofile, "5,8");
  assert.match(safari.capsQuery(caps), /&dv=1&dvprofile=5,8&/);
});

test("a session that lands on a different range repaints the badge", () => {
  // The field bug: on a tone-mapped Dexter episode the chip read "DV P7 →
  // HDR10" while the stats panel one line below read "Dynamic range: SDR".
  // Both surfaces call dynamicRangeBadge() with the same arguments — the
  // panel just repaints every second, and the chip was painted once at
  // session open, before the route was chosen. attachSession is the one site
  // every session passes through, so it owns the repaint.
  let repaints = 0;
  const build = new Function(
    "PLAYER",
    "renderPlayerInfo",
    "attachHls",
    [shippedSource("attachSession"), "return {attachSession};"].join("\n"),
  );
  const player = { deliveredRange: "hdr10" };
  const { attachSession } = build(player, () => { repaints += 1; }, () => {});

  attachSession({}, player, {
    start_seconds: 0,
    playlist_url: "/x.m3u8",
    delivered_dynamic_range: "sdr",
  }, 0);
  assert.equal(player.deliveredRange, "sdr", "the session is the source of truth");
  assert.equal(repaints, 1, "the chip must be repainted, not left at the decision's guess");

  // A response with no range keeps the decision's answer — and still must not
  // leave a stale chip behind, because other fields it paints moved too.
  const before = repaints;
  attachSession({}, player, { start_seconds: 0, playlist_url: "/x.m3u8" }, 0);
  assert.equal(player.deliveredRange, "sdr");
  assert.ok(repaints > before, "every attach repaints");

  // A superseded playback must not repaint over the live one.
  const stale = { deliveredRange: "hdr10" };
  const settled = repaints;
  attachSession({}, stale, {
    start_seconds: 0,
    playlist_url: "/y.m3u8",
    delivered_dynamic_range: "sdr",
  }, 0);
  assert.equal(repaints, settled, "a stale generation never paints the live player");
});

test("every web transcode reopen preserves the decision's HDR10 request", () => {
  const build = new Function(
    "PLAYER",
    "transcodeHeight",
    "qualityForce",
    "sessionHeight",
    [shippedSource("transcodeOpts"), "return {transcodeOpts};"].join("\n"),
  );
  const player = {
    requestHdr10: true,
    deliveredRange: "hdr10",
    autoHeight: 2160,
    burnedSub: null,
    aoffset: 0,
  };
  const { transcodeOpts } = build(player, () => null, () => "auto", () => null);

  assert.deepEqual(transcodeOpts(12, 3), {
    height: 2160,
    start: 12,
    audio: 3,
    hdr10: true,
  });

  // attachSession replaces deliveredRange with what the session actually
  // produced. That mutable display truth must never erase the immutable
  // decision request when a seek/audio switch opens the next session.
  player.deliveredRange = "sdr";
  assert.equal(transcodeOpts(30, 3).hdr10, true);

  player.requestHdr10 = false;
  assert.equal("hdr10" in transcodeOpts(30, 3), false);
});

test("HDR10 Auto leaves the cold-start height to the grade-aware server", () => {
  assert.match(
    SHIPPED_UI,
    /const autoStartHeight=[\s\S]{0,520}decision\.delivered_dynamic_range!==['"]hdr10['"]/,
    "a persisted 720p SDR rung must not override the server's proved HDR10 ceiling",
  );
});

test("an upgrade needs encode headroom, not just a bandwidth estimate", () => {
  const ladder = [
    { height: 1080, total_kbps: 8160, peak_kbps: 12160 },
    { height: 720, total_kbps: 4160, peak_kbps: 6160 },
    { height: 480, total_kbps: 2160, peak_kbps: 3160 },
  ];
  // The production shape: on a JIT server the estimate measures
  // min(link, encode) of the CURRENT rung, so a fast 720p encode reads as
  // ~200 Mb/s and clears any bar the 1080p rung can set. The server's own
  // pace is what says whether one rung up is sustainable.
  const base = {
    ladder,
    currentHeight: 720,
    estimateKbps: 200_000,
    runwaySeconds: 40,
    previousRunwaySeconds: 40,
    nowMs: 500_000,
    lastSwitchAtMs: 0,
    lastStallAtMs: null,
    upgradeSinceMs: 1,
  };

  // 720 -> 1080 is 2.25x the pixels. A server holding 2.0x realtime at 720p
  // predicts 0.89x at 1080p — under realtime, so no upgrade.
  assert.equal(
    policy.decideRung({ ...base, recentSpeed: 2.0 }).height,
    720,
    "an encoder that cannot hold realtime one rung up must not be sent there",
  );
  // 3.0x predicts 1.33x — enough margin, so the upgrade proceeds.
  assert.equal(
    policy.decideRung({ ...base, recentSpeed: 3.0 }).height,
    1080,
    "real headroom still upgrades",
  );
  // No measurement is not evidence against: absence must not freeze the rung.
  assert.equal(
    policy.decideRung({ ...base, recentSpeed: null }).height,
    1080,
    "an unmeasured session can still rise",
  );
  // A rung this playback already failed to open is never re-entered, however
  // good the numbers look — the loop that oscillated 720<->1080 every ~2
  // minutes had healthy-looking numbers on every cycle.
  assert.equal(
    policy.decideRung({
      ...base,
      recentSpeed: 3.0,
      blockedHeights: new Set([1080]),
    }).height,
    720,
    "a failed rung is not a candidate",
  );
  // Blocking the rung above must not block the way DOWN.
  const pressured = policy.decideRung({
    ...base,
    recentSpeed: 3.0,
    blockedHeights: new Set([1080]),
    estimateKbps: 1_000,
    runwaySeconds: 0.5,
  });
  assert.equal(pressured.height, 480, "emergencies still fall");
  assert.equal(pressured.emergency, true);
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
