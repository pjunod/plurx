(function (root, factory) {
  const policy = factory();
  if (typeof module === "object" && module.exports) module.exports = policy;
  root.PlurxPlaybackPolicy = policy;
})(typeof globalThis !== "undefined" ? globalThis : this, function () {
  "use strict";

  const DEFAULTS = Object.freeze({
    lostPerMinute: 6,
    minimumSeconds: 150,
    minimumLost: 15,
  });

  const AUTO_DEFAULTS = Object.freeze({
    sampleMs: 5_000,
    safeEstimateFactor: 0.95,
    severeEstimateRatio: 0.7,
    mildHeadroom: 1.3,
    mildSamples: 2,
    cooldownMs: 20_000,
    upgradeHeadroom: 1.8,
    upgradeHoldMs: 45_000,
    stallWindowMs: 60_000,
    dwellMs: 60_000,
    nearEmptyRunwaySeconds: 1.5,
    restartCostSeconds: 2.5,
  });

  const DECODE_LIMIT_DEFAULTS = Object.freeze({
    schema: "v2",
    // Ignore immaterial metadata jitter without grouping the 40 and 90 Mb/s
    // 4K loads that exposed the old codec/height poisoning.
    bitrateBucketBps: 10_000_000,
    ttlMs: 30 * 24 * 60 * 60 * 1_000,
    retestMs: 7 * 24 * 60 * 60 * 1_000,
  });

  function qualityForce(quality) {
    if (quality === "auto") return "auto";
    return quality === "original" || quality === "nomse"
      ? "original"
      : "transcode";
  }

  function transcodeHeight(quality) {
    return /^\d+$/.test(String(quality || ""))
      ? Number.parseInt(quality, 10)
      : null;
  }

  function normalizedLadder(ladder, playerHeight = Infinity) {
    const limit = playerHeight > 0 ? playerHeight : Infinity;
    const byHeight = new Map();
    for (const rung of Array.isArray(ladder) ? ladder : []) {
      const height = Number(rung && rung.height);
      const totalKbps = Number(rung && rung.total_kbps);
      const peakKbps = Number(rung && rung.peak_kbps);
      if (!(height > 0) || !(totalKbps > 0) || height > limit) continue;
      byHeight.set(height, {
        height,
        total_kbps: totalKbps,
        peak_kbps: peakKbps > 0 ? peakKbps : totalKbps,
      });
    }
    return [...byHeight.values()].sort((a, b) => a.height - b.height);
  }

  function closestRungIndex(ladder, height) {
    if (!ladder.length) return -1;
    let best = 0;
    for (let i = 1; i < ladder.length; i += 1) {
      if (
        Math.abs(ladder[i].height - height) <
        Math.abs(ladder[best].height - height)
      ) {
        best = i;
      }
    }
    return best;
  }

  function highestSafeRung(ladder, estimateKbps, defaults = AUTO_DEFAULTS) {
    if (!ladder.length || !(estimateKbps > 0)) return null;
    const ceiling = estimateKbps * defaults.safeEstimateFactor;
    for (let i = ladder.length - 1; i >= 0; i -= 1) {
      if (ladder[i].total_kbps <= ceiling) return ladder[i];
    }
    return ladder[0];
  }

  function initialAutoRung({
    ladder,
    persistedHeight = null,
    priorKbps = null,
    playerHeight = Infinity,
    defaults = AUTO_DEFAULTS,
  }) {
    const fullLadder = normalizedLadder(ladder);
    if (!fullLadder.length) return null;
    const cappedLadder = normalizedLadder(fullLadder, playerHeight);
    // The cap is a ceiling, not permission to discard the ladder's floor. A
    // player shorter than the lowest rung still starts at that lowest rung
    // when a prior or persisted choice asks the client to pick explicitly.
    const available = cappedLadder.length ? cappedLadder : [fullLadder[0]];
    const prior = highestSafeRung(available, priorKbps, defaults);
    if (prior) return prior.height;
    if (persistedHeight > 0) {
      const atOrBelowPersisted = available.filter(
        (rung) => rung.height <= Number(persistedHeight),
      );
      return (atOrBelowPersisted.at(-1) || available[0]).height;
    }
    // No prior and no learned rung: omit height and preserve the server's
    // existing encoder-aware Auto choice byte-for-byte.
    return null;
  }

  function bandwidthSeedBps({
    outgoingEstimateBps = null,
    priorKbps = null,
  }) {
    if (Number.isFinite(outgoingEstimateBps) && outgoingEstimateBps > 0) {
      return outgoingEstimateBps;
    }
    return Number.isFinite(priorKbps) && priorKbps > 0
      ? priorKbps * 1_000
      : null;
  }

  function playerPixelHeight({
    layoutHeight = null,
    devicePixelRatio = 1,
    intrinsicHeight = null,
  } = {}) {
    const cssHeight = Number(layoutHeight);
    if (cssHeight > 0) {
      const ratio = Number(devicePixelRatio);
      // Extreme browser zoom or synthetic environments can report enormous
      // ratios. Understating them is safe for a resolution ceiling; overstating
      // the real backing pixels is not.
      const scale = Number.isFinite(ratio) && ratio > 0
        ? Math.min(ratio, 4)
        : 1;
      return Math.round(cssHeight * scale);
    }
    const decodedHeight = Number(intrinsicHeight);
    return decodedHeight > 0 ? Math.round(decodedHeight) : Infinity;
  }

  function voluntaryGainSeconds({
    current,
    target,
    estimateKbps,
    recentSpeed,
    defaults = AUTO_DEFAULTS,
  }) {
    if (!current || !target) return 0;
    if (target.height > current.height) {
      return (
        ((target.total_kbps - current.total_kbps) / current.total_kbps) *
        (defaults.dwellMs / 1_000)
      );
    }
    if (recentSpeed > 0 && recentSpeed < 1) {
      return (
        (1 / recentSpeed - 1) *
        (defaults.dwellMs / 1_000)
      );
    }
    if (!(estimateKbps > 0)) return 0;
    return Math.max(
      0,
      (current.total_kbps * defaults.mildHeadroom / estimateKbps - 1) *
        (defaults.dwellMs / 1_000),
    );
  }

  // Pure restart-aware Auto policy. The caller owns sampling and feeds the
  // returned counters into the next sample; this function never reads hls.js,
  // the DOM, a clock, or persistence.
  function decideRung({
    ladder,
    currentHeight,
    estimateKbps = null,
    runwaySeconds = null,
    previousRunwaySeconds = null,
    recentSpeed = null,
    activeSupplyStall = false,
    supplyStalls = 0,
    lastStallAtMs = null,
    nowMs = 0,
    lastSwitchAtMs = null,
    mildSamples = 0,
    upgradeSinceMs = null,
    playerHeight = Infinity,
    defaults = AUTO_DEFAULTS,
  }) {
    // Pressure decisions need the complete ladder. Filtering it by the player
    // ceiling first can map a currently-running 720p session onto the sole
    // surviving 360p rung and make the emergency branch think it is already at
    // the floor. The ceiling only vetoes an upgrade; it must never strand a
    // downgrade from a rung that is already above it.
    const available = normalizedLadder(ladder);
    const currentIndex = closestRungIndex(available, Number(currentHeight));
    if (currentIndex < 0) {
      return {
        height: null,
        reason: null,
        emergency: false,
        mildSamples: 0,
        upgradeSinceMs: null,
      };
    }
    const current = available[currentIndex];
    const estimate = estimateKbps == null ? NaN : Number(estimateKbps);
    const runway = runwaySeconds == null ? NaN : Number(runwaySeconds);
    const priorRunway = previousRunwaySeconds == null
      ? NaN
      : Number(previousRunwaySeconds);
    const draining =
      Number.isFinite(runway) &&
      Number.isFinite(priorRunway) &&
      runway < priorRunway - 0.25;
    const nearEmpty =
      Number.isFinite(runway) && runway <= defaults.nearEmptyRunwaySeconds;
    const farBelow =
      estimate > 0 &&
      estimate < current.total_kbps * defaults.severeEstimateRatio;
    const supplyBurst = supplyStalls >= 3;
    const severe = activeSupplyStall || nearEmpty || farBelow || supplyBurst;

    if (severe && currentIndex > 0) {
      const safe = highestSafeRung(available, estimate, defaults);
      const safeIndex = safe
        ? closestRungIndex(available, safe.height)
        : currentIndex - 1;
      const target = available[Math.min(currentIndex - 1, safeIndex)];
      const reason = supplyBurst || activeSupplyStall
        ? "supply stalls"
        : nearEmpty
          ? "buffer ran dry"
          : "bandwidth cliff";
      return {
        height: target.height,
        reason,
        emergency: true,
        mildSamples: 0,
        upgradeSinceMs: null,
      };
    }
    if (severe) {
      return {
        height: current.height,
        reason: null,
        emergency: false,
        mildSamples: 0,
        upgradeSinceMs: null,
      };
    }

    const estimatePressure =
      estimate > 0 && estimate < current.total_kbps * defaults.mildHeadroom;
    const serverPressure = recentSpeed > 0 && recentSpeed < 1 && draining;
    const nextMildSamples = estimatePressure || serverPressure
      ? mildSamples + 1
      : 0;
    const sinceSwitch = lastSwitchAtMs == null
      ? Infinity
      : nowMs - lastSwitchAtMs;
    const voluntaryAllowed =
      sinceSwitch >= defaults.cooldownMs && sinceSwitch >= defaults.dwellMs;

    if (
      currentIndex > 0 &&
      nextMildSamples >= defaults.mildSamples &&
      voluntaryAllowed
    ) {
      const target = available[currentIndex - 1];
      const gain = voluntaryGainSeconds({
        current,
        target,
        estimateKbps: estimate,
        recentSpeed,
        defaults,
      });
      if (gain > defaults.restartCostSeconds) {
        return {
          height: target.height,
          reason: serverPressure ? "server supply" : "bandwidth pressure",
          emergency: false,
          mildSamples: 0,
          upgradeSinceMs: null,
        };
      }
    }

    const playerCeiling = Number(playerHeight) > 0
      ? Number(playerHeight)
      : Infinity;
    const nextCandidate = available[currentIndex + 1];
    const next = nextCandidate && nextCandidate.height <= playerCeiling
      ? nextCandidate
      : null;
    const stallFree =
      lastStallAtMs == null || nowMs - lastStallAtMs >= defaults.stallWindowMs;
    const upgradeReady =
      next &&
      estimate > next.total_kbps * defaults.upgradeHeadroom &&
      stallFree &&
      !estimatePressure &&
      !serverPressure;
    const nextUpgradeSince = upgradeReady
      ? (upgradeSinceMs == null ? nowMs : upgradeSinceMs)
      : null;
    if (
      next &&
      nextUpgradeSince != null &&
      nowMs - nextUpgradeSince >= defaults.upgradeHoldMs &&
      voluntaryAllowed &&
      voluntaryGainSeconds({
        current,
        target: next,
        estimateKbps: estimate,
        recentSpeed,
        defaults,
      }) > defaults.restartCostSeconds
    ) {
      return {
        height: next.height,
        reason: "bandwidth recovered",
        emergency: false,
        mildSamples: 0,
        upgradeSinceMs: null,
      };
    }

    return {
      height: current.height,
      reason: null,
      emergency: false,
      mildSamples: nextMildSamples,
      upgradeSinceMs: nextUpgradeSince,
    };
  }

  function sessionHeight({
    quality,
    sourceHeight,
    decidedMethod,
    burnedSubtitle = false,
    refusedOriginal = false,
  }) {
    if (!(sourceHeight > 0)) return null;
    if (qualityForce(quality) === "original") return sourceHeight;
    if (
      qualityForce(quality) === "auto" &&
      decidedMethod !== "transcode" &&
      (burnedSubtitle || refusedOriginal)
    ) {
      return sourceHeight;
    }
    return null;
  }

  function nativeHlsAvailable({
    canPlayNativeHls,
    hasWebKitPlaybackTarget,
    hlsJsSupported,
  }) {
    if (!canPlayNativeHls) return false;
    if (hasWebKitPlaybackTarget) return true;
    return !hlsJsSupported;
  }

  function hlsTransport({ nativeHls, hevcCopy, hlsJsSupported = true }) {
    return nativeHls && (hevcCopy || !hlsJsSupported) ? "native" : "mse";
  }

  function copyAudioNeedsTranscode({
    codec,
    clientAudioCodecs = [],
    msePairSupported = false,
    nativeHls = false,
  }) {
    const normalized = String(codec || "").toLowerCase();
    if (!normalized) return true;
    return nativeHls
      ? !clientAudioCodecs.some(
          (candidate) => String(candidate).toLowerCase() === normalized,
        )
      : !msePairSupported;
  }

  function initialRoute({
    method,
    selectedAudioIndex = 0,
    nativeHls = false,
    segmentedRemux = false,
  }) {
    if (method === "transcode") return "transcode_hls";
    if (method === "remux" && (nativeHls || segmentedRemux)) {
      return "copy_hls";
    }
    if (method === "remux" || (method === "direct_play" && selectedAudioIndex !== 0)) {
      return "progressive_remux";
    }
    return "direct";
  }

  function fallbackAction({
    method,
    alreadyTried = false,
    playbackIsReal = false,
    mediaFailure = true,
  }) {
    return mediaFailure &&
      !alreadyTried &&
      !playbackIsReal &&
      (method === "direct_play" || method === "remux")
      ? "transcode"
      : "fail";
  }

  // A wait that persists has already outlived hls.js/browser nudges. Auto may
  // trade an original remux for a compatible transcode; an explicit quality
  // choice is respected and merely reconnected. One automatic attempt is the
  // hard bound that prevents a bad file or dead network from restart-looping.
  function stallRecoveryAction({
    method,
    quality = "auto",
    alreadyRecovered = false,
  }) {
    if (alreadyRecovered) return "prompt";
    if (method === "remux" && qualityForce(quality) === "auto") {
      return "transcode";
    }
    return ["direct_play", "remux", "transcode"].includes(method)
      ? "restart"
      : "prompt";
  }

  function fallbackResetBeforeOpen({ reason }) {
    return ["stall-recovery", "stall-manual", "auto-supply"].includes(reason);
  }

  function waitingOverlayAction({ started = false, stallPrompt = false }) {
    if (!started) return "ignore";
    return stallPrompt ? "preserve_prompt" : "buffer";
  }

  function subtitleBurnAction({ requiresBurn, deliveredRange = null }) {
    if (!requiresBurn) return "native";
    return ["dolby_vision", "hdr10", "hlg"].includes(
      String(deliveredRange || "").toLowerCase(),
    )
      ? "keep_hdr"
      : "burn";
  }

  function seekDeltaSeconds(key) {
    switch (key) {
      case "ArrowLeft":
        return -10;
      case "ArrowRight":
        return 10;
      case "ArrowDown":
        return -30;
      case "ArrowUp":
        return 30;
      default:
        return null;
    }
  }

  function lostFrameRate(hitches, playedSeconds) {
    if (!hitches || !(playedSeconds > 0)) return null;
    const lost =
      (hitches.back | 0) + (hitches.drop | 0) + (hitches.gap | 0);
    return { lost, rate: +(lost / (playedSeconds / 60)).toFixed(1) };
  }

  function decodeMarginVerdict(hitches, playedSeconds, thresholds = DEFAULTS) {
    if (!(playedSeconds >= thresholds.minimumSeconds)) return null;
    const result = lostFrameRate(hitches, playedSeconds);
    if (
      !result ||
      result.lost < thresholds.minimumLost ||
      result.rate < thresholds.lostPerMinute
    ) {
      return null;
    }
    const budgetMs = hitches.fps ? +(1000 / hitches.fps).toFixed(1) : null;
    return {
      lost: result.lost,
      rate: result.rate,
      secs: Math.round(playedSeconds),
      decodeMs:
        hitches.decodeMs == null ? null : +hitches.decodeMs.toFixed(1),
      budgetMs,
    };
  }

  function normalizedText(value, fallback = "unknown") {
    const text = value == null
      ? ""
      : String(value).trim().toLowerCase().replace(/\s+/g, " ");
    return text || fallback;
  }

  function positiveInteger(value) {
    const number = Number(value);
    return Number.isFinite(number) && number > 0
      ? Math.trunc(Math.min(number, Number.MAX_SAFE_INTEGER))
      : 0;
  }

  function sourceDynamicRange(source = {}) {
    const declared = normalizedText(source && source.hdr, "");
    if (declared) return declared;
    const rich = normalizedText(source && source.hdr_format, "");
    if (/dolby vision|\bdv\b/.test(rich)) return "dolby_vision";
    if (/hdr10/.test(rich)) return "hdr10";
    if (/\bhlg\b/.test(rich)) return "hlg";
    return "sdr_or_unknown";
  }

  function bitrateBucket(
    bitrateBps,
    bucketBps = DECODE_LIMIT_DEFAULTS.bitrateBucketBps,
  ) {
    const bitrate = Number(bitrateBps);
    const width = Number(bucketBps);
    if (!(bitrate > 0) || !Number.isFinite(bitrate) || !(width > 0)) {
      return null;
    }
    const index = Math.floor(
      Math.min(bitrate, Number.MAX_SAFE_INTEGER) / width,
    );
    return {
      index,
      lower_bps: index * width,
      upper_bps: (index + 1) * width,
    };
  }

  // A learned verdict describes one decoder load, not every file sharing a
  // codec and height. The schema prefix deliberately makes the old
  // `codec@height` keys unreadable instead of pretending they were precise
  // enough to migrate. JSON over a fixed-order array is collision-free and
  // deterministic even when a future source string contains our separators.
  function decodeLimitIdentity(source = {}) {
    if (!source || typeof source !== "object" || Array.isArray(source)) {
      return null;
    }
    const bucket = bitrateBucket(source.bitrate);
    const fields = [
      normalizedText(source.video_codec),
      normalizedText(source.video_profile),
      positiveInteger(source.width),
      positiveInteger(source.height),
      positiveInteger(source.bit_depth),
      sourceDynamicRange(source),
      normalizedText(source.hdr_format, "none"),
      bucket ? bucket.index : "unknown",
    ];
    return `decode-${DECODE_LIMIT_DEFAULTS.schema}:${JSON.stringify(fields)}`;
  }

  function dynamicRangeLabel(value) {
    const normalized = normalizedText(value, "unknown");
    const labels = {
      dolby_vision: "Dolby Vision",
      hdr10: "HDR10",
      hlg: "HLG",
      sdr: "SDR",
      sdr_or_unknown: "SDR or unknown range",
      unknown: "unknown range",
    };
    return labels[normalized] || String(value || "unknown range").trim();
  }

  function mediaLoadLabel(source = {}) {
    const codec = normalizedText(source && source.video_codec).toUpperCase();
    const profile = normalizedText(source && source.video_profile, "");
    const width = positiveInteger(source && source.width);
    const height = positiveInteger(source && source.height);
    const depth = positiveInteger(source && source.bit_depth);
    const richRange = source && source.hdr_format != null
      ? String(source.hdr_format).trim()
      : "";
    const bucket = bitrateBucket(source && source.bitrate);
    const facts = [codec];
    if (profile) facts.push(profile);
    facts.push(width && height ? `${width}\u00d7${height}` : "unknown resolution");
    facts.push(depth ? `${depth}-bit` : "unknown bit depth");
    facts.push(richRange || dynamicRangeLabel(sourceDynamicRange(source)));
    facts.push(bucket
      ? `${bucket.lower_bps / 1_000_000}\u2013<${bucket.upper_bps / 1_000_000} Mb/s`
      : "unknown bitrate");
    return facts.join(" \u00b7 ");
  }

  function isDecodeLimitIdentity(key) {
    const prefix = `decode-${DECODE_LIMIT_DEFAULTS.schema}:`;
    if (typeof key !== "string" || !key.startsWith(prefix)) return false;
    try {
      const fields = JSON.parse(key.slice(prefix.length));
      return Array.isArray(fields) && fields.length === 8 &&
        key === `${prefix}${JSON.stringify(fields)}`;
    } catch (_) {
      return false;
    }
  }

  function pruneDecodeLimits(
    entries,
    {
      nowMs = 0,
      ttlMs = DECODE_LIMIT_DEFAULTS.ttlMs,
    } = {},
  ) {
    const source = entries && typeof entries === "object" && !Array.isArray(entries)
      ? entries
      : {};
    const limits = {};
    let changed = source !== entries;
    for (const [key, limit] of Object.entries(source)) {
      const at = Number(limit && limit.at);
      const rate = Number(limit && limit.rate);
      const age = Number(nowMs) - at;
      const valid =
        isDecodeLimitIdentity(key) &&
        at > 0 &&
        limit.rate != null &&
        Number.isFinite(rate) &&
        age >= 0 &&
        age <= ttlMs;
      if (valid) limits[key] = limit;
      else changed = true;
    }
    return { limits, changed };
  }

  // Pure lookup policy for the cached route. The caller owns localStorage and
  // the server request; this function says whether the exact media-load entry
  // may steer Auto, must be re-tested, or is bypassed by the viewer.
  function learnedDecodeLimitAction({
    quality = "auto",
    source = {},
    limits = {},
    nowMs = 0,
    ttlMs = DECODE_LIMIT_DEFAULTS.ttlMs,
    retestMs = DECODE_LIMIT_DEFAULTS.retestMs,
  } = {}) {
    const key = decodeLimitIdentity(source);
    if (!key) return { action: "none", key: null, limit: null };
    if (qualityForce(quality) !== "auto") {
      return { action: "bypass", key, limit: null };
    }
    const pruned = pruneDecodeLimits(limits, { nowMs, ttlMs }).limits;
    const limit = Object.prototype.hasOwnProperty.call(pruned, key)
      ? pruned[key]
      : null;
    if (!limit) return { action: "none", key, limit: null };
    return {
      action: Number(nowMs) - Number(limit.at) > retestMs ? "retest" : "apply",
      key,
      limit,
    };
  }

  function withoutLearnedDecodeLimit(entries, source) {
    const key = decodeLimitIdentity(source);
    const limits = entries && typeof entries === "object" && !Array.isArray(entries)
      ? { ...entries }
      : {};
    const removed = !!key && Object.prototype.hasOwnProperty.call(limits, key);
    if (removed) delete limits[key];
    return { limits, removed, key };
  }

  function learnedDecodeLimitView({
    source = {},
    limit = null,
    ordinaryRange = null,
    deliveredRange = null,
  } = {}) {
    const loadLabel = mediaLoadLabel(source);
    const measurement = limit && Number.isFinite(Number(limit.rate))
      ? `lost ${Number(limit.lost) || 0} frames in ${Number(limit.secs) || 0}s (${Number(limit.rate)}/min)`
      : "measured unstable original playback";
    const ordinary = normalizedText(ordinaryRange, "unknown");
    const delivered = normalizedText(deliveredRange, "unknown");
    const rangeConsequence =
      ["dolby_vision", "hdr10", "hlg"].includes(ordinary) && delivered === "sdr"
        ? `${dynamicRangeLabel(ordinary)} \u2192 SDR`
        : null;
    return {
      identity: decodeLimitIdentity(source),
      loadLabel,
      measurement,
      rangeConsequence,
      reason: `learned client-performance limit for ${loadLabel}: ${measurement}` +
        (rangeConsequence ? `; ${rangeConsequence}` : ""),
    };
  }

  return Object.freeze({
    DEFAULTS,
    AUTO_DEFAULTS,
    DECODE_LIMIT_DEFAULTS,
    qualityForce,
    transcodeHeight,
    normalizedLadder,
    initialAutoRung,
    bandwidthSeedBps,
    playerPixelHeight,
    decideRung,
    sessionHeight,
    nativeHlsAvailable,
    hlsTransport,
    copyAudioNeedsTranscode,
    initialRoute,
    fallbackAction,
    stallRecoveryAction,
    fallbackResetBeforeOpen,
    waitingOverlayAction,
    subtitleBurnAction,
    seekDeltaSeconds,
    lostFrameRate,
    decodeMarginVerdict,
    bitrateBucket,
    decodeLimitIdentity,
    mediaLoadLabel,
    pruneDecodeLimits,
    learnedDecodeLimitAction,
    withoutLearnedDecodeLimit,
    learnedDecodeLimitView,
  });
});
