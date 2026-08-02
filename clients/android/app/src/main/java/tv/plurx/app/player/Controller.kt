@file:androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)

package tv.plurx.app.player

import android.content.Context
import android.content.res.Configuration
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
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusProperties
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.media3.common.AudioAttributes
import androidx.media3.common.C
import androidx.media3.common.Format
import androidx.media3.common.MediaItem
import androidx.media3.common.PlaybackException
import androidx.media3.common.Player
import androidx.media3.common.TrackSelectionOverride
import androidx.media3.common.Tracks
import androidx.media3.common.util.UnstableApi
import androidx.media3.datasource.okhttp.OkHttpDataSource
import androidx.media3.exoplayer.DefaultRenderersFactory
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.exoplayer.source.DefaultMediaSourceFactory
import androidx.media3.exoplayer.trackselection.DefaultTrackSelector
import androidx.media3.session.MediaSession
import kotlinx.coroutines.CancellationException
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
 * How this player is currently getting bytes. Distinct from the *label* the
 * UI shows ([Controller.deliveryMode]) because a copy-HLS session is a remux
 * to the viewer and a session to the code: the video stream is untouched, but
 * seeks, subtitles, and teardown all go through the session machinery.
 */
private enum class Transport { Direct, ProgressiveRemux, Session }

/**
 * A subtitle selection the viewer actually made, where `null` inside means
 * "Off". Distinct from no wrapper at all, which means nobody has chosen yet
 * and the §3.1 automatic policy still applies.
 */
@JvmInline
value class SubtitleChoice(val index: Long?)

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
 *    than left to the server's idle reaper — unless the session is `vod`,
 *    in which case the whole stream is on disk and the player just seeks.
 *
 * A text subtitle also opens a session on remux/transcode plans, but a *copy*
 * one that advertises HLS text renditions: the video recipe never changes, so
 * an SRT on a 4K HDR remux costs neither an encoder slot nor its HDR.
 *
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
    subtitleLanguage: String = "off",
    retainedAudio: Long? = null,
    retainedSubtitle: SubtitleChoice? = null,
    private val onError: (String) -> Unit = {},
) {
    var audioOffsetMs: Long = initialAudioOffsetMs.coerceIn(-15_000, 15_000)
        private set

    private var transport = planTransport()

    /** A session that copies the source video rather than re-encoding it. */
    private var sessionCopiesVideo = false
    private var baseMs = 0L

    // A quality change rebuilds the plan and this controller. The viewer's
    // track choices belong to the *playback*, not to the controller, so they
    // come in from the screen — picking 720p used to silently drop you back to
    // the default audio track and the automatic subtitle.
    var selectedAudio: Long? =
        retainedAudio?.takeIf { index -> plan.audio.any { it.index == index } }
            ?: plan.audio.firstOrNull { it.default }?.index
        private set

    /** Absolute subtitle-stream index, or null for Off. */
    var selectedSubtitle: Long? = when {
        retainedSubtitle == null -> automaticSubtitleIndex(plan.subtitles, subtitleLanguage)
        // "Off" is a choice; only a track that vanished falls back to policy.
        retainedSubtitle.index == null -> null
        else -> retainedSubtitle.index.takeIf { index -> plan.subtitles.any { it.index == index } }
    }
        private set

    /**
     * The dynamic range the *current* delivery puts on the wire, as the server
     * reported it (MEDIA-BADGES-PLAN.md §3.2). Seeded from the decision, then
     * overwritten by every session this controller opens — a burn or a picked
     * rung produces a transcode the decision never promised, and the session's
     * answer is the one that describes the bytes now arriving. A session that
     * omits it leaves the standing value alone.
     *
     * Compose state so the badge row and the info panel recompose on a change;
     * it steers nothing.
     */
    var deliveredRange: String? by mutableStateOf(plan.deliveredDynamicRange)
        private set

    /** The label the info panel and the track menu read. */
    val deliveryMode: String get() = when {
        transport == Transport.Direct -> "direct"
        transport == Transport.ProgressiveRemux -> "remux"
        sessionCopiesVideo -> "remux"
        else -> "transcode"
    }
    var encoder: String? = null
        private set

    private val mediaSession = MediaSession.Builder(context, player).build()

    /** The HLS session this player owns, if the plan opened one. */
    private var sessionId: String? = null

    /** Stable for this player instance — the server's supersession key. */
    private val playbackId = UUID.randomUUID().toString()

    /** Only the newest asynchronous session request may replace the player. */
    private var sessionRequestVersion = 0L

    /** The whole stream is on disk: seek in place instead of reopening. */
    private var sessionIsVod = false

    /** The open session advertises this source's text tracks as renditions. */
    private var sessionHasNativeSubtitles = false

    /**
     * The track the *open* session is drawing into the picture, if any. Read
     * rather than recomputed from [selectedSubtitle], which by then already
     * describes the selection being moved to.
     */
    private var sessionBurnedSubtitle: Long? = null

    /**
     * One compatibility rescue per item (PLAYBACK.md's error fallback). A
     * second failure — or a failure in an already-transcoded mode — is a real
     * failure and says so.
     */
    private var compatibilityRescueUsed = false

    /** Set by the rescue: this session must re-encode, whatever the plan said. */
    private var forceCompatibilityTranscode = false

    /**
     * The rendition/embedded ordinal to enable once ExoPlayer publishes the
     * tracks. Forced renditions are always `DEFAULT=NO` and duplicate
     * same-language tracks are `AUTOSELECT=NO` server-side, so the chosen
     * track is never selected for us — the client always says which.
     */
    private var pendingTextOrdinal: Int? = null

    private val listener = object : Player.Listener {
        override fun onPlayerError(error: PlaybackException) {
            when (playbackErrorAction(deliveryMode, compatibilityRescueUsed)) {
                PlaybackErrorAction.RetryAsCompatibilityTranscode -> {
                    compatibilityRescueUsed = true
                    forceCompatibilityTranscode = true
                    val position = realPosition()
                    transport = Transport.Session
                    openSession(position)
                }
                PlaybackErrorAction.Fail -> onError(
                    error.errorCodeName.let { "Playback stopped ($it)." },
                )
            }
        }

        override fun onTracksChanged(tracks: Tracks) {
            applyPendingTextSelection()
        }
    }

    init {
        player.addListener(listener)
    }

    /**
     * The transport this plan wants with no subtitle asking for anything. A
     * direct plan under a manual A/V correction wants the progressive remux
     * instead: only `stream.mp4` and a session take `audio_offset_ms`.
     */
    private fun planTransport(): Transport = when {
        plan.mode == "direct" && audioOffsetMs != 0L -> Transport.ProgressiveRemux
        plan.mode == "direct" -> Transport.Direct
        plan.mode == "remux" -> Transport.ProgressiveRemux
        else -> Transport.Session
    }

    fun startAt(ms: Long) {
        // Cold start: a forced or default-flagged text track may already be
        // selected by the §3.1 policy, and it decides the session's shape.
        if (selectedSubtitle != null && subtitleRoute(track(selectedSubtitle), plan.mode) != SubtitleRoute.EmbeddedText) {
            transport = Transport.Session
        }
        restartAt(ms.coerceAtLeast(0))
    }

    fun realPosition(): Long {
        val pos = player.currentPosition.coerceAtLeast(0)
        return if (transport == Transport.Direct || sessionIsVod) pos else baseMs + pos
    }

    fun seekTo(targetMs: Long) {
        val t = targetMs.coerceIn(0, if (plan.durationMs > 0) plan.durationMs else Long.MAX_VALUE)
        when (transport) {
            Transport.Direct -> player.seekTo(t)
            Transport.ProgressiveRemux -> {
                leaveSessionPlayback()
                baseMs = t
                player.setMediaItem(MediaItem.fromUri(remuxUri(t)))
                player.prepare()
                player.playWhenReady = true
            }
            // A cached session holds the whole stream: native seeking, no
            // session churn. A live one can't be range-sought, so it reopens.
            Transport.Session -> if (sessionIsVod) player.seekTo(t) else openSession(t)
        }
    }

    fun playPause() {
        player.playWhenReady = !player.playWhenReady
    }

    fun release() {
        sessionRequestVersion++
        sessionId?.let { vm.endHlsSession(it) }
        sessionId = null
        player.removeListener(listener)
        mediaSession.release()
        player.release()
    }

    fun switchAudio(index: Long) {
        val position = realPosition()
        selectedAudio = index
        restartAt(position)
    }

    /**
     * Route a subtitle selection to the cheapest place that can show it
     * (§5.4): the player's own tracks for direct play, an HLS text rendition
     * for a server session, and the encoder only for tracks with no text to
     * send. Switching between two renditions of a live session never creates
     * a new one.
     */
    fun switchSubtitle(index: Long?) {
        val position = realPosition()
        val route = subtitleRoute(track(index), plan.mode)
        selectedSubtitle = index
        when (route) {
            SubtitleRoute.Off -> {
                disableTextTracks()
                // A burn has to be left behind — the subtitles are in the
                // pixels. Renditions just switch off, so a native session
                // (or any transport the plan already wanted) stays put.
                if (transport != planTransport() || sessionBurnedSubtitle != null) {
                    transport = planTransport()
                    restartAt(position)
                }
            }
            SubtitleRoute.EmbeddedText -> {
                if (transport != planTransport()) {
                    transport = planTransport()
                    restartAt(position)
                }
                selectTextOrdinal(embeddedSubtitleOrdinal(index))
            }
            SubtitleRoute.NativeRendition -> {
                val ordinal = nativeSubtitleOrdinal(index!!, plan.subtitles)
                if (transport == Transport.Session && sessionHasNativeSubtitles) {
                    // Same session, different rendition — metadata only, and
                    // no new encoder. A session advertising renditions is by
                    // construction not a burn.
                    selectTextOrdinal(ordinal)
                } else {
                    transport = Transport.Session
                    restartAt(position)
                }
            }
            SubtitleRoute.Burn -> {
                transport = Transport.Session
                restartAt(position)
            }
        }
    }

    /** Apply an A/V correction to this controller only and reopen in place. */
    fun setAudioOffset(offsetMs: Long) {
        val position = realPosition()
        audioOffsetMs = offsetMs.coerceIn(-15_000, 15_000)
        // Only `stream.mp4` and a session carry a correction, so a direct play
        // moves to the progressive remux — and back, when it returns to zero.
        if (transport != Transport.Session) transport = planTransport()
        restartAt(position)
    }

    private fun track(index: Long?): SubTrack? =
        index?.let { i -> plan.subtitles.firstOrNull { it.index == i } }

    /** True when the current selection has no text to send and must be drawn in. */
    private fun burningSubtitle(): Boolean =
        subtitleRoute(track(selectedSubtitle), plan.mode) == SubtitleRoute.Burn

    private fun restartAt(positionMs: Long) {
        when (transport) {
            Transport.Direct -> {
                leaveSessionPlayback()
                player.setMediaItem(MediaItem.fromUri(plan.playUrl), positionMs)
                player.prepare()
                player.playWhenReady = true
                selectTextOrdinal(embeddedSubtitleOrdinal(selectedSubtitle))
            }
            Transport.ProgressiveRemux -> {
                leaveSessionPlayback()
                baseMs = positionMs
                player.setMediaItem(MediaItem.fromUri(remuxUri(positionMs)))
                player.prepare()
                player.playWhenReady = true
            }
            Transport.Session -> openSession(positionMs)
        }
    }

    /**
     * Open a session at `ms`, releasing the one it replaces. `height` carries
     * §3.2's promise: a burn or Quality = Original sends the source's own
     * height (which the server passes through unsnapped), an explicit rung
     * sends the rung, and true Auto sends nothing at all.
     */
    private fun openSession(ms: Long) {
        val requestVersion = ++sessionRequestVersion
        sessionId?.let { vm.endHlsSession(it) }
        sessionId = null
        encoder = null
        sessionIsVod = false
        sessionHasNativeSubtitles = false
        sessionBurnedSubtitle = null

        val burn = burningSubtitle()
        val nativeTrack = selectedSubtitle
            ?.takeIf { !burn && track(it)?.let(::isNativeTextSubtitle) == true }
        // A remux verdict keeps its copied video unless the rescue forced a
        // re-encode; a transcode verdict always re-encodes.
        val copy = plan.mode == "remux" && !forceCompatibilityTranscode && !burn
        sessionCopiesVideo = copy

        scope.launch {
            val hls = try {
                vm.createHlsSession(
                    plan.fileId,
                    CreateSessionReq(
                        playback_id = playbackId,
                        request_id = UUID.randomUUID().toString(),
                        start = ms / 1000.0,
                        height = sessionHeight(
                            quality = vm.preferences.value.playbackQuality,
                            burningSubtitle = burn,
                            sourceHeight = plan.sourceHeight,
                        ),
                        audio = selectedAudio?.toInt(),
                        subtitle_burn = selectedSubtitle?.toInt()?.takeIf { burn },
                        native_subtitles = true.takeIf { nativeTrack != null },
                        subtitle = nativeTrack?.toInt(),
                        audio_offset_ms = audioOffsetMs.takeIf { it != 0L },
                        copy = true.takeIf { copy },
                        aac = plan.remuxNeedsAac.takeIf { copy },
                        preserve_dolby_vision = plan.preserveDolbyVision.takeIf { copy },
                    ),
                )
            } catch (cancelled: CancellationException) {
                // The screen left composition (or a newer request superseded
                // this one) — the caller saying stop, not the server failing.
                // Swallowing it here would show a failure state for a stream
                // nobody is waiting for any more.
                throw cancelled
            } catch (_: Exception) {
                if (requestVersion == sessionRequestVersion) {
                    onError("The server couldn't start this stream.")
                }
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
            sessionIsVod = hls.vod
            hls.delivered_dynamic_range?.let { deliveredRange = it }
            sessionHasNativeSubtitles = nativeTrack != null
            sessionBurnedSubtitle = selectedSubtitle?.takeIf { burn }
            // A cached session is the whole stream on disk: its timeline
            // starts at zero and the player seeks, exactly like direct play.
            baseMs = if (hls.vod) 0L else (hls.start_seconds * 1000).toLong()
            val startPositionMs = if (hls.vod) ms else 0L
            player.setMediaItem(MediaItem.fromUri(Session.url(hls.playlist_url)), startPositionMs)
            player.prepare()
            player.playWhenReady = true
            selectTextOrdinal(nativeTrack?.let { nativeSubtitleOrdinal(it, plan.subtitles) })
        }
    }

    private fun leaveSessionPlayback() {
        sessionRequestVersion++
        sessionId?.let { vm.endHlsSession(it) }
        sessionId = null
        encoder = null
        sessionIsVod = false
        sessionHasNativeSubtitles = false
        sessionBurnedSubtitle = null
        sessionCopiesVideo = false
        // Back on the plan's own delivery, so back to the plan's own grade —
        // otherwise a chip would keep reporting the session that just ended.
        deliveredRange = plan.deliveredDynamicRange
    }

    /**
     * Position of this absolute subtitle-stream index among the container's
     * own text tracks. Direct play surfaces every subtitle stream in source
     * order, so the absolute index is the ordinal.
     */
    private fun embeddedSubtitleOrdinal(index: Long?): Int? = index?.toInt()

    private fun selectTextOrdinal(ordinal: Int?) {
        pendingTextOrdinal = ordinal
        if (ordinal == null) disableTextTracks() else applyPendingTextSelection()
    }

    private fun disableTextTracks() {
        pendingTextOrdinal = null
        player.trackSelectionParameters = player.trackSelectionParameters.buildUpon()
            .clearOverridesOfType(C.TRACK_TYPE_TEXT)
            .setTrackTypeDisabled(C.TRACK_TYPE_TEXT, true)
            .build()
    }

    /**
     * Tracks arrive after `prepare()`, so the selection waits for them. The
     * ordinal indexes the text groups in the order the server published them.
     */
    private fun applyPendingTextSelection() {
        val ordinal = pendingTextOrdinal ?: return
        val groups = player.currentTracks.groups.filter { it.type == C.TRACK_TYPE_TEXT }
        val group = groups.getOrNull(ordinal) ?: return
        player.trackSelectionParameters = player.trackSelectionParameters.buildUpon()
            .setOverrideForType(TrackSelectionOverride(group.mediaTrackGroup, 0))
            .setTrackTypeDisabled(C.TRACK_TYPE_TEXT, false)
            .build()
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

/**
 * A ten-foot device, by the same signal the launcher uses. `UI_MODE_TYPE_
 * TELEVISION` is what `currentFormFactor()` reads in Compose; this is the
 * non-composable path for the player's construction.
 */
private fun isTelevision(context: Context): Boolean =
    context.resources.configuration.uiMode and Configuration.UI_MODE_TYPE_MASK ==
        Configuration.UI_MODE_TYPE_TELEVISION

/** Minimal view of [Plan] so the controller doesn't depend on the screen file. */
interface PlanLike {
    val fileId: Long
    val playUrl: String
    val mode: String // "direct" | "remux" | "transcode"
    val durationMs: Long
    val audio: List<AudioTrack>
    val subtitles: List<SubTrack>

    /** `DecisionResponse.source.height` — every height promise is made of it. */
    val sourceHeight: Int?

    /** `delivery.aac`: a copy session must re-encode audio this client can't take. */
    val remuxNeedsAac: Boolean

    /** `delivery.preserve_dolby_vision`: keep DV signalling through the copy. */
    val preserveDolbyVision: Boolean

    /** `decision.delivered_dynamic_range`: the badge's starting truth. */
    val deliveredDynamicRange: String?
}

@UnstableApi
fun buildPlayer(context: Context, vm: AppViewModel): ExoPlayer {
    val selector = DefaultTrackSelector(context).apply {
        parameters = buildUponParameters()
            .setPreferredAudioLanguage(vm.audioLang)
            // Tunneled playback hands decode and A/V sync to the TV SoC's own
            // pipeline, which is what 4K HDR on a Shield or a Chromecast is
            // built around. Requested only on television devices: on a phone it
            // buys nothing and some handset decoders refuse the mode outright.
            // Media3 falls back to normal playback when the device says no.
            .setTunnelingEnabled(isTelevision(context))
            // Text selection is the §3.1 policy's job, not the selector's:
            // a preferred language here re-enables the "merely same language"
            // tail the rule deletes, and the server's own renditions are
            // deliberately DEFAULT=NO/AUTOSELECT=NO so the client must choose.
            .setPreferredTextLanguage(null)
            .setSelectUndeterminedTextLanguage(false)
            .setTrackTypeDisabled(C.TRACK_TYPE_TEXT, true)
            .build()
    }
    val dataSource: OkHttpDataSource.Factory = Net.dataSourceFactory()
    val renderers = DefaultRenderersFactory(context)
        // A flaky hardware decoder degrades to software instead of erroring
        // into the compatibility rescue and costing the viewer a restart.
        .setEnableDecoderFallback(true)
    return ExoPlayer.Builder(context)
        .setTrackSelector(selector)
        .setRenderersFactory(renderers)
        .setMediaSourceFactory(DefaultMediaSourceFactory(dataSource))
        // Duck and pause for other apps rather than talking over them, and
        // stop when the headphones come out.
        .setAudioAttributes(
            AudioAttributes.Builder()
                .setUsage(C.USAGE_MEDIA)
                .setContentType(C.AUDIO_CONTENT_TYPE_MOVIE)
                .build(),
            /* handleAudioFocus = */ true,
        )
        .setHandleAudioBecomingNoisy(true)
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
    planMode: String,
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

            // One row per subtitle track, never two. The server's list and the
            // player's text tracks describe the same streams — on a session
            // they are literally the renditions of those tracks — so the
            // server's list wins whenever it has one, and the controller
            // routes each pick to the cheapest place that can show it.
            val embeddedText = if (serverSubtitles.isEmpty()) text else emptyList()
            if (embeddedText.isNotEmpty() || serverSubtitles.isNotEmpty()) {
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
                embeddedText.forEach { group ->
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
                serverSubtitles.forEach { track ->
                    TrackRow(
                        label = serverSubtitleLabel(track, planMode),
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

/**
 * "Burn-in" is a promise about cost, so it follows what the *session* can do,
 * via the one predicate that knows: `mov_text` and styled ASS both have text
 * but no rendition, and on a session they burn like a bitmap. The menu never
 * filters a track out — every track stays pickable, and the label is what
 * tells the truth about what picking it costs.
 */
internal fun serverSubtitleLabel(track: SubTrack, planMode: String = "transcode"): String = listOfNotNull(
    languageName(track.language),
    track.title,
    if (track.forced) "Forced" else null,
    if (subtitleRoute(track, planMode) == SubtitleRoute.Burn) "Burn-in" else null,
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
