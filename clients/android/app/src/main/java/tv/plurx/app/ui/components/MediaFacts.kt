package tv.plurx.app.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.AutoAwesome
import androidx.compose.material.icons.filled.GraphicEq
import androidx.compose.material.icons.filled.Movie
import androidx.compose.material.icons.filled.Tv
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.unit.dp
import androidx.compose.ui.text.font.FontWeight
import tv.plurx.app.data.AudioStream
import tv.plurx.app.data.AudioTrack
import tv.plurx.app.data.DynamicRange
import tv.plurx.app.data.MediaFileDto
import kotlin.math.min

internal enum class MediaFactKind { Resolution, Video, DynamicRange, Audio }

/**
 * How a fact relates to what is actually happening right now
 * (MEDIA-BADGES-PLAN.md §2.3). The chip's text always names what the *source*
 * carries; the state says whether the viewer is getting it.
 *
 * The mechanics are deliberately generic — resolution and audio have the same
 * shape of lie to tell (4K source on a 1080p rung, Atmos downmixed to AAC) —
 * but this pass wires only the dynamic-range fact. Everything else keeps
 * emitting [Source].
 */
internal enum class FactState {
    /** No live session to compare against: the detail screens, and pre-play. */
    Source,

    /** The session is delivering and this device is rendering exactly this. */
    Active,

    /** Something weaker is on screen; [MediaFact.activeLabel] names it. */
    Downgraded,
}

internal data class MediaFact(
    val kind: MediaFactKind,
    val label: String,
    val accessibilityLabel: String = label,
    val state: FactState = FactState.Source,
    /** What is really on screen, when [state] is [FactState.Downgraded]. */
    val activeLabel: String? = null,
)

/** Compact media facts using Google's Material icons rather than emoji. */
@Composable
internal fun MediaFactChip(fact: MediaFact, modifier: Modifier = Modifier) {
    // A downgraded chip keeps its border and dims its content — the same "off"
    // treatment as the web player's `.vbadge.off`, so the row reads at a glance
    // as "these are lit, that one isn't".
    val alpha = if (fact.state == FactState.Downgraded) 0.45f else 1f
    val color = fact.brandColor.copy(alpha = alpha)
    Row(
        modifier = modifier
            .background(color.copy(alpha = 0.12f), CircleShape)
            .border(0.5.dp, color.copy(alpha = 0.5f), CircleShape)
            .padding(horizontal = 7.dp, vertical = 3.dp)
            .clearAndSetSemantics { contentDescription = fact.accessibilityLabel },
        horizontalArrangement = Arrangement.spacedBy(4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(
            imageVector = fact.icon,
            contentDescription = null,
            tint = color,
            modifier = Modifier.size(13.dp),
        )
        Text(
            text = fact.chipText,
            color = color,
            style = MaterialTheme.typography.labelSmall,
            fontWeight = FontWeight.Bold,
        )
    }
}

/** The arrow rides inside the one chip — never a second chip beside it. */
internal val MediaFact.chipText: String
    get() = if (state == FactState.Downgraded && activeLabel != null) "$label → $activeLabel" else label

/**
 * @param delivered `delivered_dynamic_range` from the decision, overridden by
 *   the open session's answer. Null on a server that doesn't send it — the
 *   dynamic-range chip then stays source-only, exactly as before this feature.
 * @param rendered [tv.plurx.app.player.renderedRange]'s verdict for this device.
 */
internal fun playerMediaFacts(
    file: MediaFileDto?,
    audio: AudioTrack?,
    delivered: String? = null,
    rendered: String? = null,
): List<MediaFact> = buildList {
    resolutionFact(file)?.let(::add)
    dynamicRangeFact(file, delivered, rendered)?.let(::add)
    audio?.let { audioFact(it.codec, it.channels, it.title) }?.let(::add)
}

internal fun detailMediaFacts(file: MediaFileDto): List<MediaFact> = buildList {
    resolutionFact(file)?.let(::add)
    file.video_codec?.takeIf { it.isNotBlank() }?.let {
        val label = videoCodecLabel(it)
        add(MediaFact(MediaFactKind.Video, label, "$label video"))
    }
    dynamicRangeFact(file)?.let(::add)
    preferredAudioFact(file.audio_streams)?.let(::add)
}

private val MediaFact.icon: ImageVector
    get() = when (kind) {
        MediaFactKind.Resolution -> Icons.Filled.Tv
        MediaFactKind.Video -> Icons.Filled.Movie
        MediaFactKind.DynamicRange -> Icons.Filled.AutoAwesome
        MediaFactKind.Audio -> Icons.Filled.GraphicEq
    }

private val MediaFact.brandColor: Color
    get() = when (kind) {
        MediaFactKind.Resolution -> Color(0xFF66A8FF)
        MediaFactKind.Video -> Color(0xFFABB6CA)
        MediaFactKind.DynamicRange -> if (label.startsWith("DV")) {
            Color(0xFFE7B94D)
        } else {
            Color(0xFF62CBBE)
        }
        MediaFactKind.Audio -> Color(0xFFA9B5FF)
    }

private fun resolutionFact(file: MediaFileDto?): MediaFact? {
    val label = resolutionLabel(file?.width, file?.height) ?: return null
    return MediaFact(MediaFactKind.Resolution, label, label.lowercase())
}

/** A lightweight resolution badge for an item before its full media file is loaded. */
internal fun itemResolutionFact(height: Long?): MediaFact? {
    val label = resolutionLabel(null, height) ?: return null
    return MediaFact(MediaFactKind.Resolution, label, label.lowercase())
}

/** Uses both edges so 1080×1920 stays 1080p and cropped 3840-wide video stays 4K. */
internal fun resolutionLabel(width: Long?, height: Long?): String? {
    val w = width?.takeIf { it > 0 }
    val h = height?.takeIf { it > 0 }
    val shortEdge = when {
        w != null && h != null -> min(w, h)
        h != null -> h
        else -> w
    } ?: return null
    val longEdge = if (w != null && h != null) maxOf(w, h) else shortEdge
    return when {
        longEdge >= 3_200 || shortEdge >= 1_700 -> "2160P"
        longEdge >= 2_300 || shortEdge >= 1_300 -> "1440P"
        longEdge >= 1_600 || shortEdge >= 900 -> "1080P"
        longEdge >= 1_100 || shortEdge >= 650 -> "720P"
        longEdge >= 700 || shortEdge >= 400 -> "480P"
        else -> "${shortEdge}P"
    }
}

/**
 * The one chip that answers two questions at once: what the file carries, and
 * what this playback is actually putting on screen. `delivered == null` (no
 * session, or a server that predates the field) keeps the old source-only chip.
 */
private fun dynamicRangeFact(
    file: MediaFileDto?,
    delivered: String? = null,
    rendered: String? = null,
): MediaFact? {
    val value = file?.hdr_format ?: file?.hdr ?: return null
    val source = sourceDynamicRange(file) ?: return null
    val (label, spoken) = if (source == DynamicRange.DOLBY_VISION) {
        "DV" to "Dolby Vision"
    } else {
        "HDR" to value
    }
    if (delivered == null) return MediaFact(MediaFactKind.DynamicRange, label, spoken)

    val onScreen = rendered ?: delivered
    if (onScreen == source) {
        return MediaFact(MediaFactKind.DynamicRange, label, spoken, FactState.Active)
    }
    val arrow = dynamicRangeLabel(onScreen)
    return MediaFact(
        kind = MediaFactKind.DynamicRange,
        label = label,
        accessibilityLabel = "$spoken, playing as $arrow",
        state = FactState.Downgraded,
        activeLabel = arrow,
    )
}

/**
 * The source's grade in the server's own vocabulary, so it compares against
 * `delivered_dynamic_range` with string equality. `hdr_format` is the rich
 * label ("Dolby Vision · Profile 7 (HDR10-compatible)", "HDR10+"), `hdr` the
 * coarse one; a DV format string mentions HLG/HDR10 compatibility of its *base*
 * layer, so Dolby Vision has to be decided first.
 */
internal fun sourceDynamicRange(file: MediaFileDto?): String? {
    val hdr = file?.hdr?.takeIf { it.isNotBlank() }
    val format = file?.hdr_format?.takeIf { it.isNotBlank() }
    if (hdr == null && format == null) return null
    return when {
        hdr.equals(DynamicRange.DOLBY_VISION, ignoreCase = true) -> DynamicRange.DOLBY_VISION
        format?.contains("dolby", ignoreCase = true) == true -> DynamicRange.DOLBY_VISION
        hdr.equals(DynamicRange.HLG, ignoreCase = true) -> DynamicRange.HLG
        hdr == null && format?.contains("hlg", ignoreCase = true) == true -> DynamicRange.HLG
        hdr.equals(DynamicRange.SDR, ignoreCase = true) -> null
        else -> DynamicRange.HDR10
    }
}

/** How the arrow suffix and the info panel spell a grade. */
internal fun dynamicRangeLabel(range: String?): String = when (range) {
    DynamicRange.DOLBY_VISION -> "Dolby Vision"
    DynamicRange.HDR10 -> "HDR10"
    DynamicRange.HLG -> "HLG"
    DynamicRange.SDR -> "SDR"
    else -> range?.replace('_', ' ')?.uppercase().orEmpty().ifBlank { "Unknown" }
}

private fun preferredAudioFact(streams: List<AudioStream>): MediaFact? {
    val stream = streams.firstOrNull { it.title?.contains("atmos", ignoreCase = true) == true }
        ?: streams.firstOrNull { it.default }
        ?: streams.firstOrNull()
        ?: return null
    return audioFact(stream.codec, stream.channels, stream.title)
}

private fun audioFact(codec: String?, channels: Int?, title: String?): MediaFact? {
    val normalized = codec?.lowercase().orEmpty()
    val (mark, spoken) = when {
        title?.contains("atmos", ignoreCase = true) == true -> "ATMOS" to "Dolby Atmos"
        normalized in setOf("eac3", "e-ac-3") -> "DD+" to "Dolby Digital Plus"
        normalized in setOf("ac3", "ac-3") -> "DD" to "Dolby Digital"
        normalized == "truehd" -> "TRUEHD" to "Dolby TrueHD"
        normalized in setOf("dts", "dca") -> "DTS" to "DTS"
        normalized == "aac" -> "AAC" to "AAC"
        normalized == "flac" -> "FLAC" to "FLAC"
        normalized == "opus" -> "OPUS" to "Opus"
        normalized.isNotBlank() -> normalized.uppercase() to normalized.uppercase()
        else -> return null
    }
    val channelMark = channelLabel(channels)
    val label = listOfNotNull(mark, channelMark).joinToString(" ")
    val accessibility = listOfNotNull(spoken, channelMark).joinToString(" ")
    return MediaFact(MediaFactKind.Audio, label, accessibility)
}

private fun channelLabel(channels: Int?): String? = when (channels) {
    8 -> "7.1"
    7 -> "6.1"
    6 -> "5.1"
    2 -> "2.0"
    1 -> "Mono"
    null -> null
    else -> "${channels}ch"
}

private fun videoCodecLabel(codec: String): String = when (codec.lowercase()) {
    "hevc", "h265" -> "HEVC"
    "h264", "avc1" -> "H.264"
    "av1" -> "AV1"
    "vp9" -> "VP9"
    "vc1" -> "VC-1"
    "mpeg2video" -> "MPEG-2"
    else -> codec.uppercase()
}
