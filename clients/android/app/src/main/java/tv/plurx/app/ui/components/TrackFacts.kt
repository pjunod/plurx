package tv.plurx.app.ui.components

import tv.plurx.app.data.AudioStream
import tv.plurx.app.data.PlaybackTrackDefault
import tv.plurx.app.data.SubtitleStream
import tv.plurx.app.player.languageName
import tv.plurx.app.player.titleMarksForced

/**
 * The detail screen's half of the shared track-facts contract, as pure
 * functions (docs/CLIENTS.md §"Shared track facts — clients render the server's
 * answer").
 *
 * Everything here answers "what does this file carry, and what does the server
 * say will play?" from `audio_streams`, `subtitle_streams` and
 * `playback_defaults` alone. It deliberately contains no policy: which track is
 * the default is [PlaybackTrackDefault.selected_index], never a re-derivation,
 * because the server can pick Japanese original audio while reporting an
 * available English dub and every platform has to tell the same story.
 */

/**
 * The five states of `playback_defaults.*.preferred_language_status`.
 *
 * [Unknown] is not [Missing] and must never be folded into it: an untagged
 * track means the absence of the preferred language *cannot be claimed*, which
 * is a different sentence for the viewer than "this file has no English".
 */
internal enum class PreferredLanguageStatus {
    Selected,
    Available,
    Missing,
    Unknown,
    NoTracks;

    companion object {
        /**
         * Null for an absent or unrecognized value — a server older than the
         * contract, or a state a future server adds. Saying nothing is the
         * honest answer there; guessing one of the five would put a claim on
         * screen that nobody made.
         */
        fun fromWire(value: String?): PreferredLanguageStatus? = when (value?.trim()) {
            "selected" -> Selected
            "available" -> Available
            "missing" -> Missing
            "unknown" -> Unknown
            "no_tracks" -> NoTracks
            else -> null
        }
    }
}

internal enum class TrackKind { Audio, Subtitle }

private val TrackKind.plural: String
    get() = when (this) {
        TrackKind.Audio -> "audio"
        TrackKind.Subtitle -> "subtitles"
    }

/**
 * One sentence answering "does this have my language?", so a viewer learns
 * "English audio. No English subtitles." without starting playback.
 *
 * @param defaultTrackLabel the display language of the track the server picked,
 *   used only by [PreferredLanguageStatus.Available] to name what plays
 *   instead. Null means the server selected nothing (subtitles Off) or the
 *   track carries no language tag.
 * @return null when the server said nothing this build understands.
 */
internal fun preferredLanguageLine(
    kind: TrackKind,
    status: PreferredLanguageStatus?,
    preferredLanguage: String,
    defaultTrackLabel: String? = null,
): String? {
    if (status == null) return null
    val language = languageName(preferredLanguage)
        ?: preferredLanguage.trim().takeIf { it.isNotEmpty() }
    val noun = kind.plural
    return when (status) {
        PreferredLanguageStatus.NoTracks -> when (kind) {
            TrackKind.Audio -> "No audio tracks in this file."
            TrackKind.Subtitle -> "No subtitles in this file."
        }
        PreferredLanguageStatus.Selected ->
            if (language != null) "$language $noun." else "Your preferred $noun."
        PreferredLanguageStatus.Available -> {
            val head = if (language != null) {
                "$language $noun available"
            } else {
                "Your preferred $noun available"
            }
            val instead = when {
                defaultTrackLabel != null -> "$defaultTrackLabel plays by default"
                kind == TrackKind.Subtitle -> "subtitles are off by default"
                else -> "another track plays by default"
            }
            "$head — $instead."
        }
        PreferredLanguageStatus.Missing ->
            if (language != null) "No $language $noun." else "None in your preferred language."
        // "Can't tell", never "missing": an untagged track is the whole reason
        // the server refuses to claim absence here.
        PreferredLanguageStatus.Unknown -> if (language != null) {
            "Can't tell whether this has $language $noun — a track has no language tag."
        } else {
            "Can't tell which languages this has — a track has no language tag."
        }
    }
}

/** [preferredLanguageLine] for one file's audio, resolving the default's name. */
internal fun audioPreferredLanguageLine(
    default: PlaybackTrackDefault?,
    streams: List<AudioStream>,
): String? {
    if (default == null) return null
    val selected = streams.firstOrNull { it.index != null && it.index == default.selected_index }
    return preferredLanguageLine(
        kind = TrackKind.Audio,
        status = PreferredLanguageStatus.fromWire(default.preferred_language_status),
        preferredLanguage = default.preferred_language,
        defaultTrackLabel = languageName(selected?.language),
    )
}

/** [preferredLanguageLine] for one file's subtitles. */
internal fun subtitlePreferredLanguageLine(
    default: PlaybackTrackDefault?,
    streams: List<SubtitleStream>,
): String? {
    if (default == null) return null
    val selected = streams.firstOrNull { it.index != null && it.index == default.selected_index }
    return preferredLanguageLine(
        kind = TrackKind.Subtitle,
        status = PreferredLanguageStatus.fromWire(default.preferred_language_status),
        preferredLanguage = default.preferred_language,
        defaultTrackLabel = languageName(selected?.language),
    )
}

/**
 * An untagged track is named as such rather than left blank: it is the fact
 * that produces [PreferredLanguageStatus.Unknown], so hiding it here would make
 * that status unreadable.
 */
private const val UNTAGGED_LANGUAGE = "Unknown language"

internal fun audioStreamLabel(stream: AudioStream): String = listOfNotNull(
    languageName(stream.language) ?: UNTAGGED_LANGUAGE,
    stream.title?.takeIf { it.isNotBlank() },
    stream.channels?.let(::audioChannelName),
    stream.codec?.takeIf { it.isNotBlank() }?.uppercase(),
).distinct().joinToString(" · ")

/** Language, format, and the forced / SDH markers criterion 1 asks for. */
internal fun subtitleStreamLabel(stream: SubtitleStream): String = listOfNotNull(
    languageName(stream.language) ?: UNTAGGED_LANGUAGE,
    stream.title?.takeIf { it.isNotBlank() },
    stream.codec?.takeIf { it.isNotBlank() }?.let(::subtitleFormatLabel),
    if (isForcedSubtitleStream(stream)) "Forced" else null,
    if (stream.hearing_impaired) "SDH" else null,
).distinct().joinToString(" · ")

/**
 * The server's own rule: both the disposition and the title count, because
 * real files carry `forced = false` on a track titled "Forced".
 */
internal fun isForcedSubtitleStream(stream: SubtitleStream): Boolean =
    stream.forced || stream.title?.let(::titleMarksForced) == true

internal fun audioChannelName(channels: Int): String = when (channels) {
    1 -> "Mono"
    2 -> "Stereo"
    6 -> "5.1"
    7 -> "6.1"
    8 -> "7.1"
    else -> "${channels}ch"
}

internal fun subtitleFormatLabel(codec: String): String = when (codec.lowercase().trim()) {
    "subrip", "srt" -> "SRT"
    "webvtt", "vtt" -> "WebVTT"
    "ass" -> "ASS"
    "ssa" -> "SSA"
    "mov_text", "tx3g" -> "MOV Text"
    "hdmv_pgs_subtitle", "pgssub", "pgs" -> "PGS"
    "dvd_subtitle", "dvdsub" -> "VobSub"
    "dvb_subtitle" -> "DVB"
    "eia_608", "cc_dec" -> "CEA-608"
    else -> codec.uppercase()
}
