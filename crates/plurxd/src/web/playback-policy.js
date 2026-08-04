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

  function qualityForce(quality) {
    if (quality === "auto") return "auto";
    return quality === "original" || quality === "nomse"
      ? "original"
      : "transcode";
  }

  function transcodeHeight(quality) {
    return quality === "1080" || quality === "720" || quality === "480"
      ? Number.parseInt(quality, 10)
      : null;
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

  function hlsTransport({ nativeHls, hevcCopy }) {
    return nativeHls && hevcCopy ? "native" : "mse";
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

  return Object.freeze({
    DEFAULTS,
    qualityForce,
    transcodeHeight,
    sessionHeight,
    nativeHlsAvailable,
    hlsTransport,
    initialRoute,
    fallbackAction,
    subtitleBurnAction,
    seekDeltaSeconds,
    lostFrameRate,
    decodeMarginVerdict,
  });
});
