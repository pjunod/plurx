"use strict";

const assert = require("node:assert/strict");
const policy = require("../../crates/plurxd/src/web/playback-policy.js");

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
