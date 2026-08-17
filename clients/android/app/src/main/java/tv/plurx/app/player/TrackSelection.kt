package tv.plurx.app.player

import tv.plurx.app.data.AudioTrack
import tv.plurx.app.data.Decision

/**
 * The pre-play half of the track contract, as pure functions.
 *
 * A viewer can choose audio and subtitles on the detail screen, before pressing
 * play. That choice belongs to *one playback*: it travels on the navigation
 * route and then on the `/decision` request, and it never writes the server's
 * Playback defaults (docs/CLIENTS.md §"Shared track facts").
 */

/**
 * A pre-play choice, where each half is independently "not chosen".
 *
 * [subtitle] wraps a nullable index because Off is a choice and absence is not:
 * `SubtitleChoice(null)` means the viewer asked for no subtitles, while a null
 * [subtitle] means nobody has chosen and the server's own pick still applies.
 */
data class PreplayTracks(
    val audio: Long? = null,
    val subtitle: SubtitleChoice? = null,
) {
    val isEmpty: Boolean get() = audio == null && subtitle == null

    /**
     * What `/decision?subtitle=` should carry: the absolute stream index, `-1`
     * for Off, or null to leave the parameter off entirely and keep the shared
     * playback-default policy.
     */
    val subtitleQuery: Long? get() = subtitle?.let { it.index ?: OFF_SUBTITLE }

    companion object {
        const val OFF_SUBTITLE = -1L
        val NONE = PreplayTracks()
    }
}

/**
 * The optional query suffix for the player navigation route — empty when
 * nothing was chosen, so an ordinary Play navigates to exactly the route it
 * always did.
 */
internal fun preplayRouteQuery(tracks: PreplayTracks): String {
    val parts = buildList {
        tracks.audio?.let { add("audio=$it") }
        tracks.subtitleQuery?.let { add("subtitle=$it") }
    }
    return if (parts.isEmpty()) "" else "?" + parts.joinToString("&")
}

/**
 * The inverse: rebuild the choice from the route's two optional arguments. An
 * unparseable value is treated as absent rather than as Off — a broken deep
 * link must not silently switch a viewer's subtitles off.
 */
internal fun preplayTracksFromRoute(audio: String?, subtitle: String?): PreplayTracks {
    val audioIndex = audio?.toLongOrNull()?.takeIf { it >= 0 }
    val subtitleIndex = subtitle?.toLongOrNull()
    return PreplayTracks(
        audio = audioIndex,
        subtitle = when {
            subtitleIndex == null -> null
            subtitleIndex < 0 -> SubtitleChoice(null)
            else -> SubtitleChoice(subtitleIndex)
        },
    )
}

/**
 * The `/decision` query parameters a choice adds. An omitted parameter keeps
 * the shared playback-default policy — and keeps the response byte-for-byte
 * what an older client would have received — so an unchosen playback sends
 * neither key rather than sending a null.
 */
internal fun preplayQueryParams(tracks: PreplayTracks): Map<String, String> = buildMap {
    tracks.audio?.let { put("audio", it.toString()) }
    tracks.subtitleQuery?.let { put("subtitle", it.toString()) }
}

/**
 * The progressive-remux URL for one position.
 *
 * [plannedUrl] is the server's own plan URL, executed as given: when it already
 * carries the selection as `?audio=<index>` that parameter is *normalized*
 * rather than appended to, so the request never has two of them and a later
 * in-player switch cannot leave the superseded track in front of the new one.
 * Rebuilding from a bare `stream.mp4` instead would throw the server's answer
 * away and deliver its language default.
 *
 * [encode] is `Uri.encode` in the app and a plain identity in tests; the caps
 * are the only values that can contain characters needing escaping.
 */
internal fun progressiveRemuxUri(
    plannedUrl: String,
    startSeconds: Double,
    audioIndex: Long?,
    audioOffsetMs: Long,
    caps: Map<String, String>,
    encode: (String) -> String,
): String {
    val base = if (audioIndex != null) withoutAudioQueryParam(plannedUrl) else plannedUrl
    val out = StringBuilder(base)
    out.append(if (base.contains('?')) '&' else '?')
    out.append("start=").append(startSeconds)
    audioIndex?.let { out.append("&audio=").append(it) }
    if (audioOffsetMs != 0L) out.append("&audio_offset_ms=").append(audioOffsetMs)
    caps.forEach { (key, value) -> out.append('&').append(key).append('=').append(encode(value)) }
    return out.toString()
}

/**
 * What a pre-play subtitle choice would cost, read from a selection-aware
 * `/decision` before playback starts.
 */
internal data class PreplaySubtitleCost(
    /** Showing this track re-encodes the picture. */
    val requiresBurnIn: Boolean,
    /**
     * The HDR guard refused that burn and kept the current delivery, so the
     * subtitle will *not* appear. Reported as such rather than as subtitles-on.
     */
    val blockedByHdr: Boolean,
)

/**
 * Price one pre-play subtitle choice.
 *
 * Two different questions, exactly as docs/CLIENTS.md splits them: a *bitmap*
 * track's cost is the server's own `selection.subtitle_requires_burn_in`, while
 * a *text* track's is the existing `native` flag — ASS/SSA and `mov_text` carry
 * text and still burn, so the server's bitmap answer is false for them and
 * asking it alone would under-report the cost.
 */
internal fun preplaySubtitleCost(decision: Decision, chosenIndex: Long): PreplaySubtitleCost {
    val selection = decision.selection
    val track = decision.subtitles.firstOrNull { it.index == chosenIndex }
    val requiresBurnIn = selection?.subtitle_requires_burn_in == true ||
        (track != null && !track.isNativeHls && !track.isPgsOverlay)
    return PreplaySubtitleCost(
        requiresBurnIn = requiresBurnIn,
        blockedByHdr = selection?.subtitle_burn_in_blocked_by_hdr == true,
    )
}

/**
 * The disclosure the detail screen shows before playback starts, or null when
 * the choice costs nothing. An HDR-blocked burn says the subtitles will not
 * appear, because that is what the server did.
 */
internal fun preplaySubtitleNotice(cost: PreplaySubtitleCost?): String? = when {
    cost == null -> null
    cost.blockedByHdr ->
        "These subtitles need an SDR burn-in. HDR playback is kept unchanged, " +
            "so they will not be shown."
    cost.requiresBurnIn ->
        "These subtitles are burned into the picture, which re-encodes the video."
    else -> null
}

/**
 * Which of the player's own tracks is the server's track [serverIndex], matched
 * by language and then by order within that language.
 *
 * Raw position is the tempting rule and it fails silently: the server names
 * every stream in the file while the player publishes only the ones its
 * renderers accepted, so dropping one track makes every row after it point at
 * its neighbour. Counting within a language degrades to nothing — the honest
 * answer for a track the player does not have — instead.
 */
internal fun embeddedTrackOrdinal(
    serverIndex: Long,
    serverTracks: List<Pair<Long, String?>>,
    embeddedLanguages: List<String?>,
): Int? {
    val position = serverTracks.indexOfFirst { it.first == serverIndex }
    if (position < 0) return null
    val language = normalizedLanguage(serverTracks[position].second)
    val rank = serverTracks.take(position).count { normalizedLanguage(it.second) == language }
    return embeddedLanguages.withIndex()
        .filter { normalizedLanguage(it.value) == language }
        .getOrNull(rank)
        ?.index
}

/**
 * [embeddedTrackOrdinal] for audio, used only where the delivered container
 * actually carries every source track — direct play. A remux or transcode
 * session delivers the one audio stream the *server* selected, so overriding
 * anything there would index into a list of one.
 */
internal fun embeddedAudioTrackIndex(
    serverIndex: Long,
    serverTracks: List<AudioTrack>,
    embeddedLanguages: List<String?>,
): Int? = embeddedTrackOrdinal(
    serverIndex,
    serverTracks.map { it.index to it.language },
    embeddedLanguages,
)

/**
 * Drop an `audio=` the server already applied to a plan URL.
 *
 * The remux transport rebuilds its URL every seek (it appends `start`, the caps
 * and any A/V correction), and a selection-aware plan arrives as
 * `stream.mp4?audio=3`. Appending the current selection on top of that would
 * send the parameter twice; re-deriving the URL from scratch would throw the
 * server's answer away. Normalizing exactly one key does neither, and leaves
 * `audio_offset_ms` — a different parameter with the same prefix — alone.
 */
internal fun withoutAudioQueryParam(url: String): String {
    val query = url.indexOf('?')
    if (query < 0) return url
    val base = url.substring(0, query)
    val kept = url.substring(query + 1)
        .split('&')
        .filter { it.isNotEmpty() && it.substringBefore('=') != "audio" }
    return if (kept.isEmpty()) base else base + "?" + kept.joinToString("&")
}
