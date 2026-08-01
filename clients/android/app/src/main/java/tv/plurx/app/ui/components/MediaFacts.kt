package tv.plurx.app.ui.components

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
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.unit.dp
import tv.plurx.app.data.AudioStream
import tv.plurx.app.data.AudioTrack
import tv.plurx.app.data.MediaFileDto
import kotlin.math.min

internal enum class MediaFactKind { Resolution, Video, DynamicRange, Audio }

internal data class MediaFact(
    val kind: MediaFactKind,
    val label: String,
    val accessibilityLabel: String = label,
)

/** Compact media facts using Google's Material icons rather than emoji. */
@Composable
internal fun MediaFactChip(fact: MediaFact, modifier: Modifier = Modifier) {
    val color = LocalContentColor.current.copy(alpha = 0.78f)
    Row(
        modifier = modifier
            .border(0.5.dp, color.copy(alpha = 0.34f), CircleShape)
            .padding(horizontal = 6.dp, vertical = 2.dp)
            .clearAndSetSemantics { contentDescription = fact.accessibilityLabel },
        horizontalArrangement = Arrangement.spacedBy(4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(
            imageVector = fact.icon,
            contentDescription = null,
            tint = color,
            modifier = Modifier.size(14.dp),
        )
        Text(
            text = fact.label,
            color = color,
            style = MaterialTheme.typography.labelSmall,
        )
    }
}

internal fun playerMediaFacts(file: MediaFileDto?, audio: AudioTrack?): List<MediaFact> = buildList {
    resolutionFact(file)?.let(::add)
    dynamicRangeFact(file)?.let(::add)
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

private fun resolutionFact(file: MediaFileDto?): MediaFact? {
    val label = resolutionLabel(file?.width, file?.height) ?: return null
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

private fun dynamicRangeFact(file: MediaFileDto?): MediaFact? {
    val value = file?.hdr_format ?: file?.hdr ?: return null
    val isDolbyVision = value.contains("dolby", ignoreCase = true) ||
        file?.hdr.equals("dolby_vision", ignoreCase = true)
    return if (isDolbyVision) {
        MediaFact(MediaFactKind.DynamicRange, "DV", "Dolby Vision")
    } else {
        MediaFact(MediaFactKind.DynamicRange, "HDR", value)
    }
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
