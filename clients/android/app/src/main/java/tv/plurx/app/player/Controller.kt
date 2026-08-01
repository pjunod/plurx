@file:androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)

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
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusProperties
import androidx.compose.ui.focus.focusRequester
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
import androidx.media3.session.MediaSession
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.launch
import tv.plurx.app.data.CreateSessionReq
import tv.plurx.app.data.AudioTrack
import tv.plurx.app.data.SubTrack
import tv.plurx.app.data.Net
import tv.plurx.app.data.Session
import tv.plurx.app.ui.AppViewModel
import tv.plurx.app.ui.theme.Accent
import tv.plurx.app.ui.theme.Muted
import tv.plurx.app.ui.components.tvFocusRing
import tv.plurx.app.ui.components.RequestInitialFocus
import java.util.Locale
import java.util.UUID

/**
 * Bridges plurx's delivery plan to one ExoPlayer, executing the mode the
 * server chose instead of re-deriving policy from the verdict:
 *  - direct → the original file over HTTP range; ExoPlayer seeks natively.
 *  - remux → `stream.mp4?start=…`, a live fast-seek remux that can't be
 *    range-sought, so a seek re-requests the stream at the new offset.
 *  - transcode → an HLS session (this used to go through `stream.mp4` too,
 *    which never re-encodes video — a tone-map or downscale verdict shipped
 *    the copied source anyway). A seek opens a session at the new offset,
 *    like the web player, and the old one is released with a DELETE rather
 *    than left to the server's idle reaper.
 * Either way [realPosition] reports the true timeline position (base + player
 * pos), which is what gets scrobbled.
 */
@UnstableApi
class Controller(
    context: Context,
    val player: ExoPlayer,
    private val plan: PlanLike,
    private val caps: Map<String, String>,
    private val vm: AppViewModel,
    private val scope: CoroutineScope,
    private val onError: () -> Unit = {},
) {
    private var activeMode = plan.mode
    private var baseMs = 0L
    var selectedAudio: Long? = plan.audio.firstOrNull { it.default }?.index
        private set
    var selectedSubtitle: Long? = null
        private set
    val deliveryMode: String get() = activeMode
    var encoder: String? = null
        private set

    private val mediaSession = MediaSession.Builder(context, player).build()

    /** The HLS session this player owns, if the plan opened one. */
    private var sessionId: String? = null

    /** Stable for this player instance — the server's supersession key. */
    private val playbackId = UUID.randomUUID().toString()

    /** Only the newest asynchronous session request may replace the player. */
    private var sessionRequestVersion = 0L

    fun startAt(ms: Long) {
        when (activeMode) {
            "direct" -> {
                leaveSessionPlayback()
                encoder = null
                player.setMediaItem(MediaItem.fromUri(plan.playUrl), ms.coerceAtLeast(0))
                player.prepare()
                player.playWhenReady = true
            }
            "remux" -> {
                leaveSessionPlayback()
                encoder = null
                baseMs = ms.coerceAtLeast(0)
                player.setMediaItem(MediaItem.fromUri(remuxUri(baseMs)))
                player.prepare()
                player.playWhenReady = true
            }
            else -> openSession(ms.coerceAtLeast(0))
        }
    }

    fun realPosition(): Long {
        val pos = player.currentPosition.coerceAtLeast(0)
        return if (activeMode == "direct") pos else baseMs + pos
    }

    fun seekTo(targetMs: Long) {
        val t = targetMs.coerceIn(0, if (plan.durationMs > 0) plan.durationMs else Long.MAX_VALUE)
        when (activeMode) {
            "direct" -> player.seekTo(t)
            "remux" -> {
                leaveSessionPlayback()
                baseMs = t
                player.setMediaItem(MediaItem.fromUri(remuxUri(t)))
                player.prepare()
                player.playWhenReady = true
            }
            else -> openSession(t)
        }
    }

    fun playPause() {
        player.playWhenReady = !player.playWhenReady
    }

    fun release() {
        sessionRequestVersion++
        sessionId?.let { vm.endHlsSession(it) }
        sessionId = null
        mediaSession.release()
        player.release()
    }

    fun switchAudio(index: Long) {
        val position = realPosition()
        selectedAudio = index
        restartAt(position)
    }

    fun switchSubtitle(index: Long?) {
        val position = realPosition()
        selectedSubtitle = index
        activeMode = if (index == null) plan.mode else "transcode"
        restartAt(position)
    }

    private fun restartAt(positionMs: Long) {
        when (activeMode) {
            "direct" -> {
                leaveSessionPlayback()
                player.setMediaItem(MediaItem.fromUri(plan.playUrl), positionMs)
                player.prepare()
                player.playWhenReady = true
            }
            "remux" -> {
                leaveSessionPlayback()
                baseMs = positionMs
                player.setMediaItem(MediaItem.fromUri(remuxUri(positionMs)))
                player.prepare()
                player.playWhenReady = true
            }
            else -> openSession(positionMs)
        }
    }

    /**
     * Open a transcode session at `ms`, releasing the one it replaces. A seek
     * is a new session, exactly as the web player does it; `height` is never
     * sent, so the rung is the server's Auto choice.
     */
    private fun openSession(ms: Long) {
        val requestVersion = ++sessionRequestVersion
        sessionId?.let { vm.endHlsSession(it) }
        sessionId = null
        encoder = null
        scope.launch {
            val hls = try {
                vm.createHlsSession(
                    plan.fileId,
                    CreateSessionReq(
                        playback_id = playbackId,
                        request_id = UUID.randomUUID().toString(),
                        start = ms / 1000.0,
                        height = vm.preferences.value.playbackQuality.storageValue.toIntOrNull(),
                        audio = selectedAudio?.toInt(),
                        subtitle_burn = selectedSubtitle?.toInt(),
                    ),
                )
            } catch (_: Exception) {
                if (requestVersion == sessionRequestVersion) onError()
                return@launch
            }
            // A later seek or track switch won while this request was in
            // flight. Release this now-stale server session instead of letting
            // its older timeline replace the current one.
            if (requestVersion != sessionRequestVersion) {
                vm.endHlsSession(hls.session_id)
                return@launch
            }
            sessionId = hls.session_id
            encoder = hls.encoder
            baseMs = (hls.start_seconds * 1000).toLong()
            player.setMediaItem(MediaItem.fromUri(Session.url(hls.playlist_url)))
            player.prepare()
            player.playWhenReady = true
        }
    }

    private fun leaveSessionPlayback() {
        sessionRequestVersion++
        sessionId?.let { vm.endHlsSession(it) }
        sessionId = null
        encoder = null
    }

    private fun remuxUri(ms: Long): String {
        val sb = StringBuilder(plan.playUrl)
        sb.append(if (plan.playUrl.contains('?')) '&' else '?')
        sb.append("start=").append(ms / 1000.0)
        selectedAudio?.let { sb.append("&audio=").append(it) }
        caps.forEach { (k, v) -> sb.append('&').append(k).append('=').append(Uri.encode(v)) }
        return sb.toString()
    }
}

/** Minimal view of [Plan] so the controller doesn't depend on the screen file. */
interface PlanLike {
    val fileId: Long
    val playUrl: String
    val mode: String // "direct" | "remux" | "transcode"
    val durationMs: Long
    val audio: List<AudioTrack>
    val subtitles: List<SubTrack>
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
fun TrackMenu(
    player: ExoPlayer,
    serverAudio: List<AudioTrack>,
    serverSubtitles: List<SubTrack>,
    serverControlledAudio: Boolean,
    selectedServerAudio: Long?,
    selectedServerSubtitle: Long?,
    onServerAudio: (Long) -> Unit,
    onServerSubtitle: (Long?) -> Unit,
    onDismiss: () -> Unit,
) {
    val tracks = player.currentTracks
    val audio = tracks.groups.filter { it.type == C.TRACK_TYPE_AUDIO }
    val text = tracks.groups.filter { it.type == C.TRACK_TYPE_TEXT }
    val initialFocusRequester = remember { FocusRequester() }
    var initialFocusAttached = false

    fun initialFocusModifier(enabled: Boolean): Modifier {
        if (!enabled || initialFocusAttached) return Modifier
        initialFocusAttached = true
        return Modifier.focusRequester(initialFocusRequester)
    }

    Box(
        Modifier
            .fillMaxSize()
            .background(Color(0x99000000))
            .focusProperties { canFocus = false }
            .clickable(onClick = onDismiss),
    ) {
        Column(
            Modifier
                .align(Alignment.CenterEnd)
                .fillMaxHeight()
                .widthIn(max = 380.dp)
                .fillMaxWidth(0.92f)
                .background(Color(0xFF141418))
                .verticalScroll(rememberScrollState())
                .padding(20.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            if (serverControlledAudio && serverAudio.isNotEmpty()) {
                Text("Audio", style = MaterialTheme.typography.titleMedium, modifier = Modifier.padding(bottom = 4.dp))
                serverAudio.forEach { track ->
                    TrackRow(
                        label = serverAudioLabel(track),
                        selected = selectedServerAudio == track.index,
                        enabled = true,
                        modifier = initialFocusModifier(enabled = true),
                    ) { onServerAudio(track.index) }
                }
            } else if (audio.isNotEmpty()) {
                Text("Audio", style = MaterialTheme.typography.titleMedium, modifier = Modifier.padding(bottom = 4.dp))
                audio.forEach { group ->
                    for (i in 0 until group.length) {
                        val enabled = group.isTrackSupported(i)
                        TrackRow(
                            label = audioLabel(group.getTrackFormat(i)),
                            selected = group.isTrackSelected(i),
                            enabled = enabled,
                            modifier = initialFocusModifier(enabled),
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

            val selectableServerSubs = if (serverControlledAudio) serverSubtitles else serverSubtitles.filterNot { it.text }
            if (text.isNotEmpty() || selectableServerSubs.isNotEmpty()) {
                Text("Subtitles", style = MaterialTheme.typography.titleMedium, modifier = Modifier.padding(top = 14.dp, bottom = 4.dp))
                TrackRow(
                    label = "Off",
                    selected = selectedServerSubtitle == null && tracks.isTypeSelected(C.TRACK_TYPE_TEXT).not(),
                    enabled = true,
                    modifier = initialFocusModifier(enabled = true),
                ) {
                    player.trackSelectionParameters = player.trackSelectionParameters.buildUpon()
                        .clearOverridesOfType(C.TRACK_TYPE_TEXT)
                        .setTrackTypeDisabled(C.TRACK_TYPE_TEXT, true)
                        .build()
                    onServerSubtitle(null)
                    onDismiss()
                }
                text.forEach { group ->
                    for (i in 0 until group.length) {
                        val enabled = group.isTrackSupported(i)
                        TrackRow(
                            label = subLabel(group.getTrackFormat(i)),
                            selected = group.isTrackSelected(i),
                            enabled = enabled,
                            modifier = initialFocusModifier(enabled),
                        ) {
                            player.trackSelectionParameters = player.trackSelectionParameters.buildUpon()
                                .setOverrideForType(TrackSelectionOverride(group.mediaTrackGroup, i))
                                .setTrackTypeDisabled(C.TRACK_TYPE_TEXT, false)
                                .build()
                            onDismiss()
                        }
                    }
                }
                selectableServerSubs.forEach { track ->
                    TrackRow(
                        label = serverSubtitleLabel(track),
                        selected = selectedServerSubtitle == track.index,
                        enabled = true,
                        modifier = initialFocusModifier(enabled = true),
                    ) { onServerSubtitle(track.index) }
                }
            }

            if (audio.isEmpty() && text.isEmpty() && serverAudio.isEmpty() && serverSubtitles.isEmpty()) {
                Text("No selectable tracks", color = Muted, style = MaterialTheme.typography.bodyMedium)
            }
        }
    }

    if (initialFocusAttached) {
        RequestInitialFocus(initialFocusRequester)
    }
}

@Composable
private fun TrackRow(
    label: String,
    selected: Boolean,
    enabled: Boolean,
    modifier: Modifier = Modifier,
    onClick: () -> Unit,
) {
    Text(
        text = (if (selected) "● " else "   ") + label,
        color = when {
            selected -> Accent
            !enabled -> Muted
            else -> Color(0xFFECECEF)
        },
        fontWeight = if (selected) FontWeight.SemiBold else FontWeight.Normal,
        style = MaterialTheme.typography.bodyMedium,
        modifier = modifier
            .fillMaxWidth()
            .tvFocusRing(MaterialTheme.shapes.small, focusedScale = 1.02f)
            .clickable(enabled = enabled, onClick = onClick)
            .padding(vertical = 8.dp),
    )
}

internal fun audioLabel(f: Format): String {
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

internal fun subLabel(f: Format): String {
    val parts = mutableListOf<String>()
    languageName(f.language)?.let { parts.add(it) }
    f.label?.let { parts.add(it) }
    if (f.selectionFlags and C.SELECTION_FLAG_FORCED != 0) parts.add("Forced")
    return parts.distinct().joinToString(" · ").ifBlank { "Subtitle" }
}

internal fun serverAudioLabel(track: AudioTrack): String = listOfNotNull(
    languageName(track.language),
    track.title,
    track.channels?.let {
        when (it) {
            1 -> "Mono"; 2 -> "Stereo"; 6 -> "5.1"; 8 -> "7.1"; else -> "${it}ch"
        }
    },
    track.codec.uppercase(),
).distinct().joinToString(" · ").ifBlank { "Audio" }

internal fun serverSubtitleLabel(track: SubTrack): String = listOfNotNull(
    languageName(track.language),
    track.title,
    if (track.forced) "Forced" else null,
    if (!track.text) "Burn-in" else null,
).distinct().joinToString(" · ").ifBlank { "Subtitle" }

internal fun languageName(code: String?): String? {
    if (code.isNullOrBlank() || code == "und") return null
    return try {
        Locale.forLanguageTag(code).displayLanguage.ifBlank { code }
    } catch (_: Exception) {
        code
    }
}

internal fun codecShort(mime: String?): String? = when {
    mime == null -> null
    mime.contains("hevc", true) || mime.contains("h265", true) -> "HEVC"
    mime.contains("avc", true) || mime.contains("h264", true) -> "H.264"
    mime.contains("av01", true) || mime.contains("av1", true) -> "AV1"
    mime.contains("vp9", true) -> "VP9"
    mime.contains("ac3", true) && mime.contains("e", true) -> "E-AC3"
    mime.contains("ac3", true) -> "AC3"
    mime.contains("dts", true) -> "DTS"
    mime.contains("truehd", true) -> "TrueHD"
    mime.contains("aac", true) -> "AAC"
    mime.contains("flac", true) -> "FLAC"
    mime.contains("opus", true) -> "Opus"
    else -> null
}
