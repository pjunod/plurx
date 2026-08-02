package tv.plurx.app.player

import java.util.Locale
import tv.plurx.app.data.DynamicRange
import tv.plurx.app.data.HdrType
import tv.plurx.app.data.PlaybackQuality
import tv.plurx.app.data.Rung
import tv.plurx.app.data.SubTrack

/**
 * Every playback decision this client makes on its own, as pure functions.
 *
 * They live together and stay free of ExoPlayer and Android so the tables are
 * one screen of code each, unit-testable on the JVM, and editable in one
 * place: the subtitle carve-out below is one branch, and the height promise is
 * one `when`.
 */

// ---- §3.1 The subtitle rule -------------------------------------------------

/**
 * Which of the server's subtitle tracks to enable at cold start, or `null`.
 *
 * > Automatic subtitle selection must never start a burn — **except a forced
 * > track**, which may, always at source height.
 *
 * | Track shape | Automatic behavior |
 * |---|---|
 * | Forced (flag or "forced" in the title), any codec | apply — the one permitted auto-burn |
 * | Default-flagged, native text | apply, via the free path |
 * | Default-flagged, bitmap or non-native text (`mov_text`, styled ASS/SSA) | **not** applied; explicit selection only |
 * | Merely the same language | never |
 *
 * "Native text" is [isNativeTextSubtitle], not `SubTrack.text` — a
 * default-flagged `mov_text` track is text the server will not put in a
 * playlist, so auto-applying it would start a burn.
 *
 * To withdraw the carve-out, delete the forced branch: a forced bitmap track
 * then needs an explicit pick like any other burn.
 */
internal fun automaticSubtitleIndex(tracks: List<SubTrack>, subtitleLanguage: String): Long? {
    if (subtitleLanguage.isBlank() || subtitleLanguage.equals("off", ignoreCase = true)) return null
    // Never fall back to a flagged default in another language: a mux whose
    // primary language is Italian flags its Italian track, and an English
    // viewer did not ask for Italian subtitles.
    val inLanguage = tracks.filter { languageMatches(it.language, subtitleLanguage) }

    inLanguage.firstOrNull { it.forced || titleSaysForced(it.title) }?.let { return it.index }
    inLanguage.firstOrNull { it.default && isNativeTextSubtitle(it) }?.let { return it.index }
    return null
}

/**
 * Can this track be delivered as a WebVTT rendition, rather than drawn into
 * the picture? The single owner of that question: [subtitleRoute],
 * [nativeSubtitleOrdinal], and the §3.1 policy above all ask it here, so the
 * menu, the routing, and the rendition ordinals can never disagree.
 *
 * **Not** the same question as `SubTrack.text`. The decision's `text` flag is
 * `!is_bitmap_subtitle` (`plurxd/src/http/stream.rs:419`) — "has extractable
 * text" — so `mov_text` and styled ASS/SSA both arrive with `text = true`.
 * But the session endpoint rejects them with a 400 (`plurxd/src/http/hls.rs:209`,
 * `is_native_text_subtitle`) and the master playlist never advertises them,
 * because authored positioning, typefaces, and karaoke effects do not survive
 * the conversion. The gap is not theoretical: a 2160p WEB-DL MP4 with 23
 * `mov_text` tracks offers every one of them on `text` alone, and every
 * explicit pick is a 400.
 *
 * `SubTrack.native` **is** that server-side predicate, sent alongside `text`,
 * so prefer it whenever the server said anything. The codec table below is the
 * fallback for a server that predates the field — the same list as
 * `plurx-core/src/tracks.rs:144`, and the only reason this client still
 * restates a server rule at all.
 */
internal fun isNativeTextSubtitle(track: SubTrack): Boolean =
    track.native ?: (track.text && track.codec.lowercase(Locale.US) in NATIVE_TEXT_SUBTITLE_CODECS)

private val NATIVE_TEXT_SUBTITLE_CODECS = setOf("subrip", "srt", "webvtt", "vtt")

private fun titleSaysForced(title: String?): Boolean =
    title?.contains("forced", ignoreCase = true) == true

/** ISO-639 comparison that treats `en` and `eng` as the same language. */
internal fun languageMatches(code: String?, target: String): Boolean {
    val a = iso3(code) ?: return false
    val b = iso3(target) ?: return false
    return a == b
}

private fun iso3(code: String?): String? {
    val raw = code?.trim()?.takeIf { it.isNotEmpty() && !it.equals("und", ignoreCase = true) }
        ?: return null
    return try {
        Locale.forLanguageTag(raw.replace('_', '-')).isO3Language.takeIf { it.isNotEmpty() }
    } catch (_: Exception) {
        null
    } ?: raw.lowercase(Locale.US)
}

// ---- §5.4 Where a subtitle selection has to go ------------------------------

internal enum class SubtitleRoute {
    /** No subtitles: renditions and embedded text off. */
    Off,

    /** Direct play — the container's own track, switched inside the player. */
    EmbeddedText,

    /** A server session that advertises the track as an HLS text rendition. */
    NativeRendition,

    /** No text to send: draw it into the frames, at source height. */
    Burn,
}

/**
 * Direct play renders any text the container carries, styling included, so
 * `mov_text` and ASS/SSA are free there — the player reads the container's own
 * track, and `/files/{id}/subs/{index}.vtt` can extract either if it has to
 * (that endpoint turns away only bitmaps, `plurxd/src/http/stream.rs:770`).
 * A server session cannot advertise them as renditions (see
 * [isNativeTextSubtitle]), so there they burn with the bitmaps.
 *
 * The invariant this table exists to hold: a track that is `text` but not
 * native is **never** [SubtitleRoute.NativeRendition].
 */
internal fun subtitleRoute(track: SubTrack?, planMode: String): SubtitleRoute = when {
    track == null -> SubtitleRoute.Off
    !track.text -> SubtitleRoute.Burn
    planMode == "direct" -> SubtitleRoute.EmbeddedText
    isNativeTextSubtitle(track) -> SubtitleRoute.NativeRendition
    else -> SubtitleRoute.Burn
}

/**
 * Position of an absolute subtitle-stream index in the HLS master's rendition
 * order. The server advertises only native-text tracks and preserves source
 * order, so this stays stable even when bitmap or `mov_text` tracks are
 * interleaved between them in the menu. Counting by the wrong predicate here
 * does not fail loudly — it silently shifts every rendition after the first
 * `mov_text` or ASS track, and the viewer gets the neighbouring language.
 */
internal fun nativeSubtitleOrdinal(index: Long, tracks: List<SubTrack>): Int? =
    tracks.filter(::isNativeTextSubtitle).indexOfFirst { it.index == index }.takeIf { it >= 0 }

// ---- §5.3 / §3.2 What the menu says, and what a height promises -------------

/**
 * The `force` query parameter for `/decision`.
 *
 * Anything the server does not recognise parses as `Auto` (`Force::parse`), so
 * sending a bare rung — as this client used to — silently did nothing for
 * direct and remux verdicts. A rung is a request to *transcode*; how tall is
 * the session create's business, not the verdict's.
 */
internal fun decisionForce(quality: PlaybackQuality): String = when (quality) {
    PlaybackQuality.Auto -> "auto"
    PlaybackQuality.Original -> "original"
    else -> "transcode"
}

/**
 * The `height` for a session create.
 *
 * ```
 * rung selected             → the rung
 * burn OR quality=Original  → source.height   (a promise, not a rung)
 * otherwise (true Auto)     → null            (the server's Auto is smarter)
 * ```
 *
 * The middle row is why an "Original" burn used to restart a 4K remux as
 * 1080p: `"original".toIntOrNull()` is null, and null means Auto.
 */
internal fun sessionHeight(
    quality: PlaybackQuality,
    burningSubtitle: Boolean,
    sourceHeight: Int?,
): Int? = when {
    quality.rungHeight != null -> quality.rungHeight
    burningSubtitle || quality == PlaybackQuality.Original -> sourceHeight
    else -> null
}

// ---- §4.2 The one-shot compatibility rescue ---------------------------------

internal enum class PlaybackErrorAction {
    /** Reopen as a forced transcode at the current position — H.264/AAC, guaranteed. */
    RetryAsCompatibilityTranscode,

    /** Out of rescues: show a real failure state instead of a frozen surface. */
    Fail,
}

/**
 * A stream the device refuses gets exactly one automatic rescue, and only from
 * a cheaper mode: a transcode that fails has nothing left to fall back to, and
 * retrying it would loop.
 */
internal fun playbackErrorAction(
    deliveryMode: String,
    rescueAlreadyUsed: Boolean,
): PlaybackErrorAction = if (
    !rescueAlreadyUsed && (deliveryMode == "direct" || deliveryMode == "remux")
) {
    PlaybackErrorAction.RetryAsCompatibilityTranscode
} else {
    PlaybackErrorAction.Fail
}

// ---- §5.6 The quality menu is the server's ladder ---------------------------

internal data class QualityOption(val quality: PlaybackQuality, val label: String)

/**
 * Auto, Original, then the rungs the server actually advertised for this
 * source — never an upscale, and never a rung the ladder dropped. An empty
 * ladder means an older server, so the stored enum is the fallback menu.
 */
internal fun qualityOptions(ladder: List<Rung>): List<QualityOption> {
    val head = listOf(
        QualityOption(PlaybackQuality.Auto, PlaybackQuality.Auto.label),
        QualityOption(PlaybackQuality.Original, PlaybackQuality.Original.label),
    )
    if (ladder.isEmpty()) {
        return PlaybackQuality.entries.map { QualityOption(it, it.label) }
    }
    val rungs = ladder
        .sortedByDescending { it.height }
        .mapNotNull { rung ->
            val quality = PlaybackQuality.entries.firstOrNull { it.rungHeight == rung.height }
                ?: return@mapNotNull null
            QualityOption(quality, rungLabel(quality.label, rung.total_kbps))
        }
        .distinctBy { it.quality }
    return head + rungs
}

// ---- Badges M3: what grade is actually on screen ----------------------------

/**
 * The grade this device is *rendering*, from the grade the server says it is
 * *delivering* plus the two local signals Android actually has
 * (MEDIA-BADGES-PLAN.md §2.2). A reporter: nothing here changes a decision, a
 * capability claim, or a byte of playback.
 *
 * ```
 * delivered == null                 → null   (no session answer yet; source-only badge)
 * the panel shows no HDR at all     → sdr    (an HDR stream on an SDR screen is SDR)
 * the decoder says something        → that   (it is looking at the actual samples)
 * otherwise                         → delivered, gated by what the panel can show
 * ```
 *
 * The decoder wins over the server because it is downstream of every fallback:
 * a Dolby Vision copy that the device quietly played as its HDR10 base reports
 * `COLOR_TRANSFER_ST2084` with a non-DV sample MIME, and the badge should say
 * "DV → HDR10" rather than repeat the server's plan back at the viewer. A
 * *missing* signal is not a contradiction — an HLS variant can publish no
 * `ColorInfo` at all — so silence leaves the server's answer standing.
 *
 * @param decoderMime `Format.sampleMimeType`, or null before the first frame.
 * @param decoderColorTransfer `Format.colorInfo.colorTransfer`, or null when
 *   the stream published none.
 * @param hdrTypes `Caps.displayHdrTypes` — `Display.HdrCapabilities` constants.
 */
internal fun renderedRange(
    delivered: String?,
    decoderMime: String?,
    decoderColorTransfer: Int?,
    hdrTypes: Set<Int>,
): String? {
    if (delivered == null) return null
    if (hdrTypes.isEmpty()) return DynamicRange.SDR

    val transfer = decoderColorTransfer?.takeIf { it != COLOR_TRANSFER_UNSET }
    val decoded = when {
        decoderMime != null && decoderMime.equals(VIDEO_DOLBY_VISION, ignoreCase = true) ->
            DynamicRange.DOLBY_VISION
        transfer == COLOR_TRANSFER_ST2084 -> DynamicRange.HDR10
        transfer == COLOR_TRANSFER_HLG -> DynamicRange.HLG
        transfer != null -> DynamicRange.SDR
        else -> null
    }

    return when (decoded ?: delivered) {
        DynamicRange.DOLBY_VISION ->
            if (HdrType.DOLBY_VISION in hdrTypes) DynamicRange.DOLBY_VISION else DynamicRange.SDR
        DynamicRange.HDR10 -> if (hdrTypes.any { it in PQ_DISPLAY_TYPES }) {
            DynamicRange.HDR10
        } else {
            DynamicRange.SDR
        }
        DynamicRange.HLG -> if (hdrTypes.any { it in HLG_DISPLAY_TYPES }) {
            DynamicRange.HLG
        } else {
            DynamicRange.SDR
        }
        else -> DynamicRange.SDR
    }
}

/**
 * Media3's `MimeTypes.VIDEO_DOLBY_VISION` and `C.COLOR_TRANSFER_*`, restated
 * for the same reason `CapsPolicy` restates the framework's: this file stays
 * free of ExoPlayer so the tables above are readable and JVM-testable, and
 * these are compile-time constants either way. (`C`'s are also `@UnstableApi`,
 * which an import would drag into every caller.)
 */
private const val VIDEO_DOLBY_VISION = "video/dolby-vision"
private const val COLOR_TRANSFER_HLG = 7
private const val COLOR_TRANSFER_ST2084 = 6

/** `Format.NO_VALUE` — a `ColorInfo` that carries no transfer at all. */
private const val COLOR_TRANSFER_UNSET = -1

/** A Dolby Vision panel is a PQ panel; HDR10+ is HDR10 with dynamic metadata. */
private val PQ_DISPLAY_TYPES = setOf(HdrType.HDR10, HdrType.HDR10_PLUS, HdrType.DOLBY_VISION)
private val HLG_DISPLAY_TYPES = setOf(HdrType.HLG, HdrType.DOLBY_VISION)

private fun rungLabel(label: String, totalKbps: Int): String = when {
    totalKbps <= 0 -> label
    totalKbps >= 1_000 -> "%s · %.1f Mbps".format(Locale.US, label, totalKbps / 1_000.0)
    else -> "$label · $totalKbps kbps"
}
