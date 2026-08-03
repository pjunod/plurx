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
import androidx.media3.common.Player
import androidx.media3.common.TrackGroup
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
    initialAudioOffsetMs: Long = 0,
    private val onError: () -> Unit = {},
) {
    private var baseMs = 0L
    var selectedAudio: Long? = plan.audio.firstOrNull { it.default }?.index
        private set

    /** Which subtitle the viewer (or the decision) asked for; null is Off. */
    var selectedSubtitle: Long? = null
        private set

    /** How that selection is being carried — see [SubtitlePolicy]. */
    private var subtitleDelivery: SubtitleDelivery = SubtitleDelivery.Plan

    var audioOffsetMs: Long = initialAudioOffsetMs.coerceIn(-15_000, 15_000)
        private set

    /**
     * The delivery this plan would use with no subtitle in play. A manual A/V
     * correction is only expressible by the remuxer, so it moves direct play
     * one step down even before subtitles are considered.
     */
    private val planMode: String
        get() = if (audioOffsetMs != 0L && plan.mode == "direct") "remux" else plan.mode

    /**
     * What the stats overlay and the menu call this. A native-rendition
     * session on a direct or remux verdict is still a remux — the video is
     * copied; only the playlist gained a subtitle group.
     */
    val deliveryMode: String
        get() = when (subtitleDelivery) {
            SubtitleDelivery.Burn -> "transcode"
            SubtitleDelivery.NativeSession -> if (plan.mode == "transcode") "transcode" else "remux"
            SubtitleDelivery.Plan -> planMode
        }

    /** True while the original file is being read directly, base timeline = 0. */
    private val directTransport: Boolean
        get() = subtitleDelivery == SubtitleDelivery.Plan && planMode == "direct"

    var encoder: String? = null
        private set

    private val mediaSession = MediaSession.Builder(context, player).build()

    /**
     * A text selection that still has to be applied to the player.
     *
     * Renditions and embedded tracks only exist once the source has been
     * prepared and its tracks published, and a forced rendition is always
     * `DEFAULT=NO` (crates/plurxd/src/http/hls.rs — Apple's authoring rules
     * make DEFAULT=YES mean something stronger than FORCED=YES), so the
     * playlist's own metadata can never be relied on to select one. Arm the
     * intent here and let [trackListener] land it whenever the tracks turn up.
     */
    private var textSelectionArmed = false

    private val trackListener = object : Player.Listener {
        override fun onTracksChanged(tracks: Tracks) = applyTextSelection()
    }

    init {
        player.addListener(trackListener)
        // The server already made this choice: `/decision` runs `select_tracks`
        // and stamps `default` on its pick. Applying it here — rather than
        // re-running a policy of our own — is what keeps the client agreeing
        // with the server's configured subtitle mode instead of overriding it.
        autoSubtitleSelection(plan.subtitles)?.let { index ->
            selectedSubtitle = index
            subtitleDelivery =
                subtitleRoute(trackFor(index), planMode, SubtitleDelivery.Plan).delivery
        }
    }

    /** The HLS session this player owns, if the plan opened one. */
    private var sessionId: String? = null

    /** Stable for this player instance — the server's supersession key. */
    private val playbackId = UUID.randomUUID().toString()

    /** Only the newest asynchronous session request may replace the player. */
    private var sessionRequestVersion = 0L

    fun startAt(ms: Long) = restartAt(ms.coerceAtLeast(0))

    fun realPosition(): Long {
        val pos = player.currentPosition.coerceAtLeast(0)
        return if (directTransport) pos else baseMs + pos
    }

    fun seekTo(targetMs: Long) {
        val t = targetMs.coerceIn(0, if (plan.durationMs > 0) plan.durationMs else Long.MAX_VALUE)
        when {
            directTransport -> player.seekTo(t)
            subtitleDelivery == SubtitleDelivery.Plan && planMode == "remux" -> {
                leaveSessionPlayback()
                baseMs = t
                player.setMediaItem(MediaItem.fromUri(remuxUri(t)))
                player.prepare()
                player.playWhenReady = true
                armTextSelection()
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
        player.removeListener(trackListener)
        mediaSession.release()
        player.release()
    }

    fun switchAudio(index: Long) {
        val position = realPosition()
        selectedAudio = index
        restartAt(position)
    }

    /**
     * Show subtitle stream [index], or nothing when it is null.
     *
     * The routing lives in [subtitleRoute]; all this does is carry the answer.
     * The point of the split is that every arm below used to be one arm — a
     * burn — so an SRT on a 4K HDR remux paid for a full re-encode to show a
     * track the server can hand over as a WebVTT rendition for free.
     */
    fun switchSubtitle(index: Long?) {
        val track = index?.let(::trackFor)
        // An index the decision never listed cannot be routed, and guessing
        // here means guessing "burn". Ignore it instead.
        if (index != null && track == null) return
        // Read the position before the state moves: which timeline the player
        // is on depends on the delivery about to change.
        val position = realPosition()
        val route = subtitleRoute(track, planMode, subtitleDelivery)
        selectedSubtitle = index
        subtitleDelivery = route.delivery
        if (route.reopen) {
            restartAt(position)
        } else {
            // No reopen means the same media item, so its tracks are already
            // published and this lands now — which is what makes switching
            // between two text tracks cost nothing.
            armTextSelection()
            applyTextSelection()
        }
    }

    /** Apply an A/V correction to this controller only and reopen in place. */
    fun setAudioOffset(offsetMs: Long) {
        val position = realPosition()
        audioOffsetMs = offsetMs.coerceIn(-15_000, 15_000)
        // The correction can move a direct play onto the remuxer, and the
        // remuxer's progressive stream carries no subtitle tracks — so the
        // current selection has to be re-routed, not just replayed.
        subtitleDelivery =
            subtitleRoute(trackFor(selectedSubtitle), planMode, subtitleDelivery).delivery
        restartAt(position)
    }

    private fun restartAt(positionMs: Long) {
        when {
            subtitleDelivery != SubtitleDelivery.Plan -> openSession(positionMs)
            planMode == "direct" -> {
                leaveSessionPlayback()
                player.setMediaItem(MediaItem.fromUri(plan.playUrl), positionMs)
                player.prepare()
                player.playWhenReady = true
                armTextSelection()
            }
            planMode == "remux" -> {
                leaveSessionPlayback()
                baseMs = positionMs
                player.setMediaItem(MediaItem.fromUri(remuxUri(positionMs)))
                player.prepare()
                player.playWhenReady = true
                armTextSelection()
            }
            else -> openSession(positionMs)
        }
    }

    /**
     * Open an HLS session at `ms`, releasing the one it replaces. A seek is a
     * new session, exactly as the web player does it. What kind of session —
     * copy or transcode, renditions or a burn — is [subtitleSessionBody]'s
     * answer, so the shape of every request this client sends is unit-tested
     * rather than assembled inline.
     */
    private fun openSession(ms: Long) {
        val requestVersion = ++sessionRequestVersion
        sessionId?.let { vm.endHlsSession(it) }
        sessionId = null
        encoder = null
        scope.launch {
            val hls = try {
                vm.createHlsSession(plan.fileId, sessionBody(ms))
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
            armTextSelection()
        }
    }

    internal fun sessionBody(ms: Long): CreateSessionReq = subtitleSessionBody(
        playbackId = playbackId,
        requestId = UUID.randomUUID().toString(),
        startSeconds = ms / 1000.0,
        delivery = subtitleDelivery,
        subtitleIndex = selectedSubtitle,
        // A transcode verdict is the only one that forbids copying the video;
        // direct and remux verdicts both mean the source stream is playable
        // as-is, which is what makes the native-rendition session free.
        copyableVideo = plan.mode != "transcode",
        aac = plan.aac,
        preserveDolbyVision = plan.preserveDolbyVision,
        audioIndex = selectedAudio,
        audioOffsetMs = audioOffsetMs,
        quality = vm.preferences.value.playbackQuality,
        sourceHeight = plan.sourceHeight,
    )

    private fun trackFor(index: Long?): SubTrack? =
        index?.let { i -> plan.subtitles.firstOrNull { it.index == i } }

    /**
     * Record that the current selection still has to reach the player.
     *
     * Deliberately does not try to apply it: right after `prepare()` the only
     * tracks on hand may still be the departing item's, and an override
     * naming a track group that is about to disappear would be dropped
     * silently — with the intent already marked as delivered. [trackListener]
     * lands it against the tracks that actually arrive.
     */
    private fun armTextSelection() {
        textSelectionArmed = true
    }

    private fun applyTextSelection() {
        if (!textSelectionArmed) return
        val index = selectedSubtitle
        // Off, and a burn, are the same instruction to the renderer: show no
        // text track. A burn's cues are already in the picture, and letting
        // ExoPlayer's own language preference pick something here would put a
        // second subtitle policy in front of the one the server decided.
        if (index == null || subtitleDelivery == SubtitleDelivery.Burn) {
            textSelectionArmed = false
            player.trackSelectionParameters = player.trackSelectionParameters.buildUpon()
                .clearOverridesOfType(C.TRACK_TYPE_TEXT)
                .setTrackTypeDisabled(C.TRACK_TYPE_TEXT, true)
                .build()
            return
        }
        val ordinal = if (subtitleDelivery == SubtitleDelivery.NativeSession) {
            nativeSubtitleOrdinal(index, plan.subtitles)
        } else {
            embeddedTextTrackIndex(index, plan.subtitles, embeddedTextLanguages())
        }
        // Nothing to select yet — the media is still being prepared, or this
        // source genuinely lacks the track. Stay armed; onTracksChanged retries.
        val target = ordinal?.let(::textTrackAt) ?: return
        textSelectionArmed = false
        player.trackSelectionParameters = player.trackSelectionParameters.buildUpon()
            .setOverrideForType(TrackSelectionOverride(target.first, target.second))
            .setTrackTypeDisabled(C.TRACK_TYPE_TEXT, false)
            .build()
    }

    /** The player's text tracks flattened in publication order. */
    private fun embeddedTextLanguages(): List<String?> =
        player.currentTracks.groups
            .filter { it.type == C.TRACK_TYPE_TEXT }
            .flatMap { group -> (0 until group.length).map { group.getTrackFormat(it).language } }

    private fun textTrackAt(ordinal: Int): Pair<TrackGroup, Int>? {
        var seen = 0
        for (group in player.currentTracks.groups) {
            if (group.type != C.TRACK_TYPE_TEXT) continue
            for (i in 0 until group.length) {
                if (seen == ordinal) return group.mediaTrackGroup to i
                seen++
            }
        }
        return null
    }

    private fun leaveSessionPlayback() {
        sessionRequestVersion++
        sessionId?.let { vm.endHlsSession(it) }
        sessionId = null
        encoder = null
    }

    private fun remuxUri(ms: Long): String {
        val base = if (plan.mode == "direct") {
            Session.url("/api/v1/files/${plan.fileId}/stream.mp4")
        } else {
            plan.playUrl
        }
        val sb = StringBuilder(base)
        sb.append(if (base.contains('?')) '&' else '?')
        sb.append("start=").append(ms / 1000.0)
        selectedAudio?.let { sb.append("&audio=").append(it) }
        if (audioOffsetMs != 0L) sb.append("&audio_offset_ms=").append(audioOffsetMs)
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

    /**
     * `DecisionResponse.source.height`. The height a session must send back
     * whenever its own height is a promise rather than a rung — a burn, or
     * Quality = Original — so a 4K burn stays 2160-tall instead of restarting
     * at the server's Auto rung (§3.2).
     */
    val sourceHeight: Int?

    /** `delivery.aac`: a copy session must re-encode the audio. */
    val aac: Boolean

    /** `delivery.preserve_dolby_vision`: the copy keeps the DV layer. */
    val preserveDolbyVision: Boolean
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

            // One row per subtitle, whichever way it will be delivered.
            //
            // These used to be two lists — the decision's tracks and whatever
            // ExoPlayer had demuxed — so on direct play every track appeared
            // twice, and the two rows behaved differently: one switched the
            // embedded track, the other started a burn. The decision's list is
            // the authoritative one (it names every track, carries the absolute
            // stream index the server routes on, and knows which are servable
            // as renditions), so it is the only list shown when it exists. The
            // player's own groups remain the fallback for a source the server
            // told us nothing about.
            if (text.isNotEmpty() || serverSubtitles.isNotEmpty()) {
                Text("Subtitles", style = MaterialTheme.typography.titleMedium, modifier = Modifier.padding(top = 14.dp, bottom = 4.dp))
                val serverOwnsSubtitles = serverSubtitles.isNotEmpty()
                TrackRow(
                    label = "Off",
                    selected = selectedServerSubtitle == null &&
                        (serverOwnsSubtitles || !tracks.isTypeSelected(C.TRACK_TYPE_TEXT)),
                    enabled = true,
                    modifier = initialFocusModifier(enabled = true),
                ) {
                    if (!serverOwnsSubtitles) {
                        player.trackSelectionParameters = player.trackSelectionParameters.buildUpon()
                            .clearOverridesOfType(C.TRACK_TYPE_TEXT)
                            .setTrackTypeDisabled(C.TRACK_TYPE_TEXT, true)
                            .build()
                    }
                    onServerSubtitle(null)
                    onDismiss()
                }
                if (serverOwnsSubtitles) {
                    serverSubtitles.forEach { track ->
                        TrackRow(
                            label = serverSubtitleLabel(track),
                            selected = selectedServerSubtitle == track.index,
                            enabled = true,
                            modifier = initialFocusModifier(enabled = true),
                        ) { onServerSubtitle(track.index); onDismiss() }
                    }
                } else {
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
    if (isForcedSubtitle(track)) "Forced" else null,
    // "Burn-in" is a warning about cost, so it follows the question that
    // actually decides cost: can the server serve this as a rendition? ASS/SSA
    // carry text and still burn, which `text` alone would have hidden.
    if (!track.isNativeHls) "Burn-in" else null,
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
