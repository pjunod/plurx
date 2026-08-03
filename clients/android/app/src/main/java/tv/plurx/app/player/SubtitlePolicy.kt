package tv.plurx.app.player

import tv.plurx.app.data.CreateSessionReq
import tv.plurx.app.data.PlaybackQuality
import tv.plurx.app.data.SubTrack
import java.util.Locale

/**
 * The subtitle half of the playback contract, as pure functions.
 *
 * Everything here is deliberately free of ExoPlayer and Android so the arm a
 * selection lands in, the body that arm sends, and the one automatic-selection
 * veto are all decidable in a JVM unit test. [Controller] is then only the
 * plumbing that carries these answers to the player.
 *
 * Three facts anchor the whole file, and each is the server's, not ours:
 *
 *  1. A track is servable as a native WebVTT rendition iff the server says so
 *     (`SubTrack.isNativeHls`). Not iff it carries text — ASS/SSA carry text
 *     and are refused with "the selected subtitle requires burn-in".
 *  2. *Which* track to show at cold start is decided by `/decision`, which runs
 *     the same `select_tracks` the transcoder burns by and stamps `default` on
 *     its pick. This client applies that pick; it does not re-derive it. The
 *     Apple client re-derived and thereby behaved as subtitle mode `Always`
 *     against a server configured for `Auto`.
 *  3. The master playlist advertises native tracks only, in source order
 *     (`master_playlist_with`, crates/plurxd/src/http/hls.rs), so a rendition's
 *     ordinal is its position among the *native* tracks — bitmap and styled
 *     tracks interleaved in the menu do not shift it.
 */

/** What is carrying the picture while a given subtitle selection is up. */
internal enum class SubtitleDelivery {
    /** The plan's own delivery: direct file, progressive remux, or a plain session. */
    Plan,

    /** An HLS session opened with `native_subtitles` — the video recipe is untouched. */
    NativeSession,

    /** A transcode session with the track drawn into the frames. */
    Burn,
}

/**
 * [delivery] is where the selection belongs; [reopen] is whether getting there
 * costs a new server session (or, on direct play, a new media item).
 */
internal data class SubtitleRoute(val delivery: SubtitleDelivery, val reopen: Boolean)

/**
 * Route one selection. [planMode] is the *effective* delivery the plan would
 * use with no subtitle at all — "direct", "remux" or "transcode" — and
 * [current] is what is on screen now.
 */
internal fun subtitleRoute(
    track: SubTrack?,
    planMode: String,
    current: SubtitleDelivery,
): SubtitleRoute {
    val delivery = when {
        // Off. Renditions and embedded text tracks both switch off in place;
        // only escaping a burn needs the stream rebuilt, which [reopen] says.
        track == null -> SubtitleDelivery.Plan
        // The only thing that justifies re-encoding the picture: a track with
        // no text the client could be handed instead.
        !track.isNativeHls -> SubtitleDelivery.Burn
        // Direct play already has the tracks — they are in the file the player
        // is reading. Opening a session for them would trade an untouched
        // stream for a segmented one to show cues the demuxer already found.
        planMode == "direct" -> SubtitleDelivery.Plan
        // Remux and transcode: the same session the plan would have opened
        // anyway, plus the rendition flags.
        else -> SubtitleDelivery.NativeSession
    }
    // A burn is baked into the frames, so changing which track is burnt is a
    // different encode and unavoidably a new session. Every other transition
    // between like states — two renditions of one live session, two embedded
    // tracks of one file — is a selection change and nothing more, which is
    // the acceptance criterion "toggling between two text tracks does not
    // create a new server session".
    val reopen = delivery != current || delivery == SubtitleDelivery.Burn
    return SubtitleRoute(delivery, reopen)
}

/**
 * Position of [index] in the HLS master's rendition order, or null when the
 * track is not advertised there at all. The server publishes native tracks
 * only and preserves source order, so filtering the menu's list the same way
 * reproduces its numbering exactly.
 */
internal fun nativeSubtitleOrdinal(index: Long, tracks: List<SubTrack>): Int? =
    tracks.filter { it.isNativeHls }
        .indexOfFirst { it.index == index }
        .takeIf { it >= 0 }

/**
 * The track to show without being asked, or null for none.
 *
 * The server already chose — `/decision` runs `select_tracks` (anime
 * dual-audio, the server's language preferences, the configured subtitle mode)
 * and stamps `default` on exactly one track or none. Re-deriving that here
 * would put a second, differently-configured policy in front of the viewer's.
 *
 * One veto survives, and it is about cost rather than choice: an automatic
 * selection must never start a burn, because a burn re-encodes a stream the
 * viewer never asked to have re-encoded. A *forced* track is the carve-out —
 * a handful of signage cues the film is unwatchable without — and it may burn,
 * at source height (docs/CLIENTS-REMEDIATION-PLAN.md §3.1, owner policy).
 */
internal fun autoSubtitleSelection(tracks: List<SubTrack>): Long? {
    val pick = tracks.firstOrNull { it.default } ?: return null
    if (pick.isNativeHls) return pick.index
    return pick.index.takeIf { isForcedSubtitle(pick) }
}

/**
 * Both signals count. The server's own `track_is_forced` reads the title too,
 * because real files carry `disposition.forced = false` on a track titled
 * "Forced" — and here the forced arm is the only path by which automatic
 * selection may spend an encoder, so the two must agree.
 */
internal fun isForcedSubtitle(track: SubTrack): Boolean =
    track.forced || track.title?.let(::titleMarksForced) == true

/**
 * Replica of the server's `title_marks_forced` (crates/plurxd/src/http/hls.rs).
 * A plain substring test is too eager: "Non-Forced" and "Unforced" are titles
 * that exist, and reading either as forced would auto-burn an ordinary track
 * on every play. Match on word boundaries and reject a negated one; "Unforced"
 * and "Reinforced" fall out for free because the letter in front is not a
 * boundary.
 */
internal fun titleMarksForced(title: String): Boolean {
    val lower = title.lowercase(Locale.ROOT)
    var from = 0
    while (from <= lower.length - FORCED.length) {
        val at = lower.indexOf(FORCED, from)
        if (at < 0) return false
        val end = at + FORCED.length
        val bounded = lower.getOrNull(at - 1)?.isLetterOrDigit() != true &&
            lower.getOrNull(end)?.isLetterOrDigit() != true
        if (bounded && !negatedBefore(lower.substring(0, at))) return true
        from = end
    }
    return false
}

private const val FORCED = "forced"

private val NEGATIONS = setOf("non", "not", "no", "never")

private fun negatedBefore(prefix: String): Boolean {
    val word = prefix.trimEnd { !it.isLetterOrDigit() }.takeLastWhile { it.isLetterOrDigit() }
    return word in NEGATIONS
}

/**
 * The `height` a session create should carry.
 *
 * §3.2: a height that is a *promise* rather than a rung — a burn, or Quality =
 * Original — sends the source's own height, which `hls.rs` recognises and
 * passes through unsnapped. A genuine Auto sends nothing, because the rung
 * depends on which encoder wins and only the create response knows that. An
 * explicit rung sends the rung and lets the server snap it onto the ladder.
 */
internal fun sessionHeight(
    quality: PlaybackQuality,
    delivery: SubtitleDelivery,
    sourceHeight: Int?,
): Int? = when {
    delivery == SubtitleDelivery.Burn -> sourceHeight
    quality == PlaybackQuality.Original -> sourceHeight
    else -> quality.storageValue.toIntOrNull()
}

/**
 * The exact body each routing arm posts to `/files/{id}/hls/sessions`.
 *
 * [copyableVideo] is whether the server's verdict permits copying the video
 * (direct or remux, i.e. anything but a transcode verdict): a native-rendition
 * session on such a plan is a copy session, so the SRT costs the stream no
 * encoder, no downscale and no tone-map. A burn is never a copy — drawing into
 * the frames is exactly what a copy refuses to do.
 */
internal fun subtitleSessionBody(
    playbackId: String,
    requestId: String,
    startSeconds: Double,
    delivery: SubtitleDelivery,
    subtitleIndex: Long?,
    copyableVideo: Boolean,
    aac: Boolean,
    preserveDolbyVision: Boolean,
    audioIndex: Long?,
    audioOffsetMs: Long,
    quality: PlaybackQuality,
    sourceHeight: Int?,
): CreateSessionReq {
    val copy = copyableVideo && delivery != SubtitleDelivery.Burn
    val native = delivery == SubtitleDelivery.NativeSession
    return CreateSessionReq(
        playback_id = playbackId,
        request_id = requestId,
        // `height` is ignored under `copy`; sending it anyway would suggest a
        // rung this session is not going to honour.
        height = if (copy) null else sessionHeight(quality, delivery, sourceHeight),
        start = startSeconds,
        audio = audioIndex?.toInt(),
        subtitle_burn = subtitleIndex?.toInt().takeIf { delivery == SubtitleDelivery.Burn },
        native_subtitles = true.takeIf { native },
        subtitle = subtitleIndex?.toInt().takeIf { native },
        audio_offset_ms = audioOffsetMs.takeIf { it != 0L },
        copy = true.takeIf { copy },
        aac = aac.takeIf { copy },
        preserve_dolby_vision = true.takeIf { copy && preserveDolbyVision },
    )
}

/**
 * Which of the player's own text tracks is the server's track [serverIndex] —
 * the map that lets the menu show one row per subtitle instead of two.
 *
 * [embeddedLanguages] is the player's text tracks flattened in order.
 *
 * Matching is by language *and then* order within that language, rather than
 * by raw position: the two lists are not guaranteed to be the same length,
 * because the decision names every subtitle stream in the file while the
 * player only publishes the ones its renderers accepted. Raw position is the
 * tempting rule and it fails silently — drop one track and every row after it
 * points at its neighbour, so the menu says "Italian" and the screen shows
 * English. Counting within a language degrades to the right answer instead:
 * a track whose language is absent from the player resolves to nothing, and
 * nothing is the honest answer for a track the player does not have.
 * Untagged tracks are matched against untagged tracks by the same rule.
 */
internal fun embeddedTextTrackIndex(
    serverIndex: Long,
    serverTracks: List<SubTrack>,
    embeddedLanguages: List<String?>,
): Int? {
    val position = serverTracks.indexOfFirst { it.index == serverIndex }
    if (position < 0) return null
    val language = normalizedLanguage(serverTracks[position].language)
    val rank = serverTracks.take(position).count { normalizedLanguage(it.language) == language }
    return embeddedLanguages.withIndex()
        .filter { normalizedLanguage(it.value) == language }
        .getOrNull(rank)
        ?.index
}

/**
 * ISO 639-2/T for comparison, because the two sides spell languages
 * differently: ffprobe hands the server "eng", and Media3 normalizes a
 * `Format.language` to "en".
 */
internal fun normalizedLanguage(code: String?): String? {
    val trimmed = code?.trim()?.lowercase(Locale.ROOT)
        ?.takeIf { it.isNotEmpty() && it != "und" }
        ?: return null
    val primary = trimmed.substringBefore('-').substringBefore('_')
    return runCatching {
        Locale.forLanguageTag(primary).isO3Language.ifBlank { primary }
    }.getOrDefault(primary)
}
