@file:OptIn(UnstableApi::class)

package tv.plurx.app.player

import android.content.Context
import android.net.Uri
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.media3.common.C
import androidx.media3.common.Format
import androidx.media3.common.MediaItem
import androidx.media3.common.TrackSelectionOverride
import androidx.media3.common.Tracks
import androidx.media3.common.util.UnstableApi
import androidx.media3.datasource.okhttp.OkHttpDataSource
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.exoplayer.source.DefaultMediaSourceFactory
import androidx.media3.exoplayer.trackselection.DefaultTrackSelector
import tv.plurx.app.data.Net
import tv.plurx.app.ui.AppViewModel
import tv.plurx.app.ui.theme.Accent
import tv.plurx.app.ui.theme.Muted
import java.util.Locale

/**
 * Bridges plurx's two delivery shapes to one ExoPlayer:
 *  - direct play → the original file over HTTP range; ExoPlayer seeks natively.
 *  - remux/transcode → `stream.mp4?start=…`, a live fast-seek remux that can't be
 *    range-sought, so a seek re-requests the stream at the new offset. Either
 *    way [realPosition] reports the true timeline position (base + player pos),
 *    which is what gets scrobbled.
 */
@UnstableApi
class Controller(
    val player: ExoPlayer,
    private val plan: PlanLike,
    private val caps: Map<String, String>,
) {
    private val direct = plan.direct
    private var baseMs = 0L

    fun startAt(ms: Long) {
        if (direct) {
            player.setMediaItem(MediaItem.fromUri(plan.playUrl), ms.coerceAtLeast(0))
        } else {
            baseMs = ms.coerceAtLeast(0)
            player.setMediaItem(MediaItem.fromUri(transcodeUri(baseMs)))
        }
        player.prepare()
        player.playWhenReady = true
    }

    fun realPosition(): Long {
        val pos = player.currentPosition.coerceAtLeast(0)
        return if (direct) pos else baseMs + pos
    }

    fun seekTo(targetMs: Long) {
        val t = targetMs.coerceIn(0, if (plan.durationMs > 0) plan.durationMs else Long.MAX_VALUE)
        if (direct) {
            player.seekTo(t)
        } else {
            baseMs = t
            player.setMediaItem(MediaItem.fromUri(transcodeUri(t)))
            player.prepare()
            player.playWhenReady = true
        }
    }

    fun playPause() {
        player.playWhenReady = !player.playWhenReady
    }

    fun release() = player.release()

    private fun transcodeUri(ms: Long): String {
        val sb = StringBuilder(plan.playUrl)
        sb.append(if (plan.playUrl.contains('?')) '&' else '?')
        sb.append("start=").append(ms / 1000.0)
        caps.forEach { (k, v) -> sb.append('&').append(k).append('=').append(Uri.encode(v)) }
        return sb.toString()
    }
}

/** Minimal view of [Plan] so the controller doesn't depend on the screen file. */
interface PlanLike {
    val playUrl: String
    val direct: Boolean
    val durationMs: Long
}

@UnstableApi
fun buildPlayer(context: Context, vm: AppViewModel): ExoPlayer {
    val selector = DefaultTrackSelector(context).apply {
        parameters = buildUponParameters()
            .setPreferredAudioLanguage(vm.audioLang)
            .setPreferredTextLanguage(if (vm.subLang == "off") null else vm.subLang)
            .setSelectUndeterminedTextLanguage(false)
            .build()
    }
    val dataSource: OkHttpDataSource.Factory = Net.dataSourceFactory()
    return ExoPlayer.Builder(context)
        .setTrackSelector(selector)
        .setMediaSourceFactory(DefaultMediaSourceFactory(dataSource))
        .build()
}

/**
 * A slide-in panel listing the embedded audio and subtitle tracks ExoPlayer
 * found (populated for direct play; a transcode usually carries the single
 * server-selected track). Selecting one pins it via track-selection overrides.
 */
@UnstableApi
@Composable
fun TrackMenu(player: ExoPlayer, onDismiss: () -> Unit) {
    val tracks = player.currentTracks
    val audio = tracks.groups.filter { it.type == C.TRACK_TYPE_AUDIO }
    val text = tracks.groups.filter { it.type == C.TRACK_TYPE_TEXT }

    Box(
        Modifier
            .fillMaxSize()
            .background(Color(0x99000000))
            .clickable(onClick = onDismiss),
    ) {
        Column(
            Modifier
                .align(Alignment.CenterEnd)
                .fillMaxHeight()
                .width(320.dp)
                .background(Color(0xFF141418))
                .verticalScroll(rememberScrollState())
                .padding(20.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            if (audio.isNotEmpty()) {
                Text("Audio", style = MaterialTheme.typography.titleMedium, modifier = Modifier.padding(bottom = 4.dp))
                audio.forEach { group ->
                    for (i in 0 until group.length) {
                        TrackRow(
                            label = audioLabel(group.getTrackFormat(i)),
                            selected = group.isTrackSelected(i),
                            enabled = group.isTrackSupported(i),
                        ) {
                            player.trackSelectionParameters = player.trackSelectionParameters.buildUpon()
                                .setOverrideForType(TrackSelectionOverride(group.mediaTrackGroup, i))
                                .setTrackTypeDisabled(C.TRACK_TYPE_AUDIO, false)
                                .build()
                            onDismiss()
                        }
                    }
                }
            }

            if (text.isNotEmpty()) {
                Text("Subtitles", style = MaterialTheme.typography.titleMedium, modifier = Modifier.padding(top = 14.dp, bottom = 4.dp))
                TrackRow(label = "Off", selected = tracks.isTypeSelected(C.TRACK_TYPE_TEXT).not(), enabled = true) {
                    player.trackSelectionParameters = player.trackSelectionParameters.buildUpon()
                        .clearOverridesOfType(C.TRACK_TYPE_TEXT)
                        .setTrackTypeDisabled(C.TRACK_TYPE_TEXT, true)
                        .build()
                    onDismiss()
                }
                text.forEach { group ->
                    for (i in 0 until group.length) {
                        TrackRow(
                            label = subLabel(group.getTrackFormat(i)),
                            selected = group.isTrackSelected(i),
                            enabled = group.isTrackSupported(i),
                        ) {
                            player.trackSelectionParameters = player.trackSelectionParameters.buildUpon()
                                .setOverrideForType(TrackSelectionOverride(group.mediaTrackGroup, i))
                                .setTrackTypeDisabled(C.TRACK_TYPE_TEXT, false)
                                .build()
                            onDismiss()
                        }
                    }
                }
            }

            if (audio.isEmpty() && text.isEmpty()) {
                Text("No selectable tracks", color = Muted, style = MaterialTheme.typography.bodyMedium)
            }
        }
    }
}

@Composable
private fun TrackRow(label: String, selected: Boolean, enabled: Boolean, onClick: () -> Unit) {
    Text(
        text = (if (selected) "● " else "   ") + label,
        color = when {
            selected -> Accent
            !enabled -> Muted
            else -> Color(0xFFECECEF)
        },
        fontWeight = if (selected) FontWeight.SemiBold else FontWeight.Normal,
        style = MaterialTheme.typography.bodyMedium,
        modifier = Modifier
            .fillMaxWidth()
            .clickable(enabled = enabled, onClick = onClick)
            .padding(vertical = 8.dp),
    )
}

private fun audioLabel(f: Format): String {
    val parts = mutableListOf<String>()
    languageName(f.language)?.let { parts.add(it) }
    f.label?.let { parts.add(it) }
    if (f.channelCount != Format.NO_VALUE) {
        parts.add(
            when (f.channelCount) {
                1 -> "Mono"; 2 -> "Stereo"; 6 -> "5.1"; 8 -> "7.1"
                else -> "${f.channelCount}ch"
            }
        )
    }
    codecShort(f.sampleMimeType)?.let { parts.add(it) }
    return parts.distinct().joinToString(" · ").ifBlank { "Audio" }
}

private fun subLabel(f: Format): String {
    val parts = mutableListOf<String>()
    languageName(f.language)?.let { parts.add(it) }
    f.label?.let { parts.add(it) }
    if (f.selectionFlags and C.SELECTION_FLAG_FORCED != 0) parts.add("Forced")
    return parts.distinct().joinToString(" · ").ifBlank { "Subtitle" }
}

private fun languageName(code: String?): String? {
    if (code.isNullOrBlank() || code == "und") return null
    return try {
        Locale(code).displayLanguage.ifBlank { code }
    } catch (_: Exception) {
        code
    }
}

private fun codecShort(mime: String?): String? = when {
    mime == null -> null
    mime.contains("ac3", true) && mime.contains("e", true) -> "E-AC3"
    mime.contains("ac3", true) -> "AC3"
    mime.contains("dts", true) -> "DTS"
    mime.contains("truehd", true) -> "TrueHD"
    mime.contains("aac", true) -> "AAC"
    mime.contains("flac", true) -> "FLAC"
    mime.contains("opus", true) -> "Opus"
    else -> null
}
