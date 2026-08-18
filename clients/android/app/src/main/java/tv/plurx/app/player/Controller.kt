@file:androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)

package tv.plurx.app.player

import android.content.Context
import android.content.res.Configuration
import android.net.Uri
import android.util.Log
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
import androidx.media3.common.PlaybackParameters
import androidx.media3.common.TrackGroup
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
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
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
 * A subtitle selection the viewer actually made, where `null` inside means
 * "Off". Distinct from no wrapper at all, which means nobody has chosen yet
 * and the server's own pick still applies.
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
 *    than left to the server's idle reaper — unless the session is `vod`, in
 *    which case the whole stream is already on disk and the player just seeks.
 *
 * There is exactly one piece of mutable transport state here, and it is
 * [subtitleDelivery]: where the current subtitle selection is being carried
 * (`SubtitlePolicy.kt`). Everything else — direct or session, copy or
 * re-encode, what the info panel calls it — is *derived* from that plus
 * [planMode], so two answers about the same stream cannot drift apart. A
 * subtitle also opens a session on remux/transcode plans, but a *copy* one
 * advertising HLS text renditions: the video recipe never changes, so an SRT
 * on a 4K HDR remux costs neither an encoder slot nor its HDR.
 *
 * Either way [realPosition] reports the true timeline position (base + player
 * pos), which is what gets scrobbled.
 */
@UnstableApi
class Controller(
    context: Context,
    builtPlayer: BuiltPlayer,
    private val plan: PlanLike,
    private val caps: Map<String, String>,
    private val vm: AppViewModel,
    private val scope: CoroutineScope,
    initialAudioOffsetMs: Long = 0,
    retainedAudio: Long? = null,
    retainedSubtitle: SubtitleChoice? = null,
    private val onError: (String) -> Unit = {},
) {
    val player: ExoPlayer = builtPlayer.player

    private val progressiveMediaOrigin = builtPlayer.progressiveMediaOrigin

    var audioOffsetMs: Long = initialAudioOffsetMs.coerceIn(-15_000, 15_000)
        private set

    private var baseMs = 0L

    // A quality change rebuilds the plan and this controller. The viewer's
    // track choices belong to the *playback*, not to the controller, so they
    // come in from the screen — picking 720p used to silently drop you back to
    // the default audio track and the server's automatic subtitle.
    var selectedAudio: Long? =
        retainedAudio?.takeIf { index -> plan.audio.any { it.index == index } }
            ?: plan.audio.firstOrNull { it.default }?.index
        private set

    /**
     * Absolute subtitle-stream index, or null for Off.
     *
     * With no retained choice this is the server's pick: `/decision` runs
     * `select_tracks` and stamps `default` on exactly one track or none, and
     * [autoSubtitleSelection] applies that rather than re-deriving a policy of
     * our own — re-deriving is how a client ends up behaving as subtitle mode
     * `Always` against a server configured for `Auto`.
     */
    var selectedSubtitle: Long? = run {
        val selected = when {
            retainedSubtitle == null -> autoSubtitleSelection(plan.subtitles)
            // "Off" is a choice; only a track that vanished falls back to policy.
            retainedSubtitle.index == null -> null
            else -> retainedSubtitle.index.takeIf { index ->
                plan.subtitles.any { it.index == index }
            }
        }
        val track = plan.subtitles.firstOrNull { it.index == selected }
        selected.takeUnless {
            subtitleBurnWouldDiscardHdr(track, plan.deliveredDynamicRange)
        }
    }
        private set

    var playbackNotice: String? by mutableStateOf(null)
        private set

    internal var pgsOverlayFrame: PGSOverlayFrame? by mutableStateOf(null)
        private set

    internal var pgsOverlayStatus: PGSOverlayStatus by mutableStateOf(PGSOverlayStatus.Off)
        private set

    /** A direct DV failure may use one lossless remux before the final rescue. */
    private var compatibilityRemuxUsed = false

    /** One final H.264 compatibility transcode; failure after that is terminal. */
    private var compatibilityTranscodeUsed = false

    /** A direct DV decoder failure first gets the same video in normalized MP4. */
    private var forceCompatibilityRemux = false

    /** Set by the rescue: this playback must re-encode, whatever the plan said. */
    private var forceCompatibilityTranscode = false

    /** Once a real frame rendered, a later fault is transport, not capability. */
    private var establishedPlayback = false

    /** One reconnect of the exact HDR recipe; a repeated failure is visible. */
    private var sameHdrRetryUsed = false

    /**
     * The delivery this plan would use with no subtitle in play. A manual A/V
     * correction is only expressible by the remuxer, so it moves direct play
     * one step down even before subtitles are considered; the compatibility
     * rescue moves everything to a transcode, because "the device refused
     * these bytes" is exactly a demand for different ones.
     */
    private val planMode: String
        get() = when {
            forceCompatibilityTranscode -> "transcode"
            forceCompatibilityRemux -> "remux"
            audioOffsetMs != 0L && plan.mode == "direct" -> "remux"
            else -> plan.mode
        }

    /** How that selection is being carried — see `SubtitlePolicy.kt`. */
    private var subtitleDelivery: SubtitleDelivery =
        subtitleRoute(trackFor(selectedSubtitle), planMode, SubtitleDelivery.Plan).delivery

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

    /**
     * What the stats overlay and the menu call this. A native-rendition
     * session on a direct or remux verdict is still a remux — the video is
     * copied; only the playlist gained a subtitle group.
     */
    val deliveryMode: String
        get() = when (subtitleDelivery) {
            SubtitleDelivery.Burn -> "transcode"
            SubtitleDelivery.NativeSession -> if (planMode == "transcode") "transcode" else "remux"
            SubtitleDelivery.BitmapOverlay,
            SubtitleDelivery.Plan -> planMode
        }

    val pgsOverlayIsActive: Boolean
        get() = pgsOverlayStatus != PGSOverlayStatus.Off

    /** True while the original file is being read directly, base timeline = 0. */
    private val directTransport: Boolean
        get() = subtitleDelivery.usesPlanTransport && planMode == "direct"

    /** True while Media3 is reading the live progressive remux response. */
    private val progressiveTransport: Boolean
        get() = subtitleDelivery.usesPlanTransport && planMode == "remux"

    var encoder: String? = null
        private set

    private val mediaSession = MediaSession.Builder(context, player).build()

    /** The HLS session this player owns, if the plan opened one. */
    private var sessionId: String? = null

    private val playbackTelemetry = ControllerPlaybackTelemetry(
        plan = plan,
        player = object : PlaybackTelemetryPlayer {
            override val buffering: Boolean
                get() = player.playbackState == Player.STATE_BUFFERING
            override val playbackRequested: Boolean
                get() = player.playWhenReady
            override val playerPositionMs: Long
                get() = player.currentPosition
            override val mediaPositionMs: Long
                get() = realPosition()
            override val bufferedPositionMs: Long
                get() = player.bufferedPosition
            override val videoHeight: Int
                get() = player.videoSize.height
        },
        context = {
            PlaybackTelemetryContext(
                method = if (deliveryMode == "direct") "direct_play" else deliveryMode,
                encoder = encoder,
                sessionId = sessionId,
            )
        },
        emit = { event -> postPlaybackClientLog(scope, event) },
    )
    private val stallWatchdogJob: Job

    /** Stable for this player instance — the server's supersession key. */
    private val playbackId = UUID.randomUUID().toString()

    /** Only the newest asynchronous session request may replace the player. */
    private var sessionRequestVersion = 0L

    /**
     * The open session is the whole stream on disk (a pre-transcode cache
     * hit): its timeline starts at zero, so it seeks in place rather than
     * churning a server session per scrub.
     */
    private var sessionIsVod = false

    /**
     * A text selection that still has to be applied to the player.
     *
     * Renditions and embedded tracks only exist once the source has been
     * prepared and its tracks published, and a forced rendition is always
     * `DEFAULT=NO` (crates/plurxd/src/http/hls.rs — Apple's authoring rules
     * make DEFAULT=YES mean something stronger than FORCED=YES), so the
     * playlist's own metadata can never be relied on to select one. Arm the
     * intent here and let [listener] land it whenever the tracks turn up.
     */
    private var textSelectionArmed = false

    /**
     * The audio equivalent of [textSelectionArmed], and it exists for the same
     * reason the text one does: only direct play hands the player a container
     * with every source track in it, and those tracks only exist once the
     * source has been prepared.
     *
     * Every other transport delivers the single audio stream the *server*
     * selected — the plan's `?audio=<index>` or the session body's `audio` —
     * so there is nothing to override there. Direct play has no such selector,
     * which is why the server refuses a `direct` verdict for any choice other
     * than the container's own default; ExoPlayer's language preference is
     * then the only thing left that could put a different track on the
     * speakers than the one the detail screen marked.
     */
    private var audioSelectionArmed = false

    private val pgsOverlay = AndroidPGSOverlayController(
        api = { vm.api() },
        scope = scope,
        fileId = plan.fileId,
        sourcePositionMs = ::realPosition,
        isPlaying = { player.isPlaying },
        playbackSpeed = { player.playbackParameters.speed },
        onFrame = { pgsOverlayFrame = it },
        onStatus = { pgsOverlayStatus = it },
        onFailure = { playbackNotice = it },
    )

    private val listener = object : Player.Listener {
        override fun onPlayerError(error: PlaybackException) {
            val mediaCompatibilityFailure = isCompatibilityPlaybackError(error.errorCode)
            val action = playbackErrorAction(
                deliveryMode = deliveryMode,
                preservesDolbyVision = plan.preserveDolbyVision,
                remuxRescueAlreadyUsed = compatibilityRemuxUsed,
                transcodeRescueAlreadyUsed = compatibilityTranscodeUsed,
                mediaCompatibilityFailure = mediaCompatibilityFailure,
                deliveredRange = deliveredRange,
                establishedPlayback = establishedPlayback,
                sameHdrRetryAlreadyUsed = sameHdrRetryUsed,
            )
            playbackTelemetry.report(
                event = "playback_error",
                level = "error",
                message = error.errorCodeName,
                code = error.errorCode,
                detail = buildString {
                    append("action=").append(action)
                    append(" media_compatibility=").append(mediaCompatibilityFailure)
                    append(" preserves_dv=").append(plan.preserveDolbyVision)
                    append(" established=").append(establishedPlayback)
                    if (caps.isNotEmpty()) {
                        append(" caps=")
                        append(
                            caps.entries.sortedBy { it.key }
                                .joinToString(",") { "${it.key}=${it.value}" },
                        )
                    }
                },
            )
            Log.w(
                "plurx-playback",
                "file=${plan.fileId} delivery=$deliveryMode preservesDv=" +
                    "${plan.preserveDolbyVision} action=$action caps=$caps " +
                    "mediaFailure=${isCompatibilityPlaybackError(error.errorCode)} " +
                    "error=${error.errorCodeName}",
                error,
            )
            when (action) {
                PlaybackErrorAction.RetrySameHDRDelivery -> {
                    val position = realPosition()
                    sameHdrRetryUsed = true
                    restartAt(position, "fallback")
                }
                PlaybackErrorAction.RetryAsDolbyVisionRemux -> {
                    val position = realPosition()
                    compatibilityRemuxUsed = true
                    forceCompatibilityRemux = true
                    subtitleDelivery =
                        subtitleRoute(trackFor(selectedSubtitle), planMode, subtitleDelivery).delivery
                    restartAt(position, "fallback")
                }
                PlaybackErrorAction.RetryAsCompatibilityTranscode -> {
                    // Read the position before the mode moves: which timeline
                    // the player is on depends on the delivery about to change.
                    val position = realPosition()
                    compatibilityTranscodeUsed = true
                    forceCompatibilityTranscode = true
                    // `planMode` is a transcode now, and that can move the
                    // selection's route with it: an embedded track on a
                    // directly-played file has to become a rendition.
                    subtitleDelivery =
                        subtitleRoute(trackFor(selectedSubtitle), planMode, subtitleDelivery).delivery
                    restartAt(position, "fallback")
                }
                PlaybackErrorAction.Fail -> onError(
                    error.errorCodeName.let { "Playback stopped ($it)." },
                )
            }
        }

        override fun onRenderedFirstFrame() {
            establishedPlayback = true
            playbackTelemetry.firstFrame(monotonicNowMs())
        }

        override fun onTracksChanged(tracks: Tracks) {
            applyTextSelection()
            applyAudioSelection()
        }

        override fun onPositionDiscontinuity(
            oldPosition: Player.PositionInfo,
            newPosition: Player.PositionInfo,
            reason: Int,
        ) = pgsOverlay.reconcile()

        override fun onPlaybackParametersChanged(playbackParameters: PlaybackParameters) =
            pgsOverlay.reconcile()

        override fun onIsPlayingChanged(isPlaying: Boolean) = pgsOverlay.reconcile()

        override fun onMediaItemTransition(mediaItem: MediaItem?, reason: Int) =
            pgsOverlay.itemChanged()
    }

    init {
        player.addListener(listener)
        pgsOverlay.select(selectedSubtitle.takeIf { subtitleDelivery == SubtitleDelivery.BitmapOverlay })
        stallWatchdogJob = scope.launch {
            while (isActive) {
                playbackTelemetry.sampleStall(establishedPlayback, monotonicNowMs())
                delay(1_000)
            }
        }
    }

    fun startAt(
        ms: Long,
        reason: String = if (ms > 0) "resume" else "cold-start",
        observedAtMs: Long = monotonicNowMs(),
    ) {
        val position = ms.coerceAtLeast(0)
        restartAt(position, reason, observedAtMs)
    }

    fun realPosition(): Long {
        return realMediaPositionMs(
            playerPositionMs = player.currentPosition,
            directTransport = directTransport,
            sessionIsVod = sessionIsVod,
            progressiveTransport = progressiveTransport,
            progressiveOriginMs = progressiveMediaOrigin.currentOriginMs(),
            sessionBaseMs = baseMs,
        )
    }

    private fun beginPlaybackAttempt(
        reason: String,
        observedAtMs: Long = monotonicNowMs(),
    ): PlaybackAttempt {
        establishedPlayback = false
        return playbackTelemetry.begin(reason, observedAtMs)
    }

    fun seekTo(targetMs: Long) {
        val t = targetMs.coerceIn(0, if (plan.durationMs > 0) plan.durationMs else Long.MAX_VALUE)
        when {
            directTransport -> {
                beginPlaybackAttempt("seek")
                player.seekTo(t)
            }
            subtitleDelivery.usesPlanTransport && planMode == "remux" -> {
                val attempt = beginPlaybackAttempt("seek")
                leaveSessionPlayback()
                baseMs = t
                val uri = remuxUri(t)
                progressiveMediaOrigin.begin(uri, t)
                player.setMediaItem(MediaItem.fromUri(uri))
                player.prepare()
                playbackTelemetry.prepared(attempt)
                player.playWhenReady = true
                armTrackSelections()
            }
            // A cached session holds the whole stream: native seeking, no
            // session churn. A live one can't be range-sought, so it reopens.
            sessionIsVod -> {
                beginPlaybackAttempt("seek")
                player.seekTo(t)
            }
            else -> openSession(t, beginPlaybackAttempt("seek"))
        }
    }

    fun playPause() {
        player.playWhenReady = !player.playWhenReady
    }

    fun release() {
        stallWatchdogJob.cancel()
        pgsOverlay.release()
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
        restartAt(position, "audio")
    }

    /**
     * Show subtitle stream [index], or nothing when it is null.
     *
     * The routing lives in [subtitleRoute]; all this does is carry the answer.
     * The point of the split is that every arm below used to be one arm — a
     * burn — so an SRT on a 4K HDR remux paid for a full re-encode to show a
     * track the server can hand over as a WebVTT rendition for free.
     */
    fun switchSubtitle(index: Long?): Boolean {
        val track = index?.let(::trackFor)
        // An index the decision never listed cannot be routed, and guessing
        // here means guessing "burn". Ignore it instead.
        if (index != null && track == null) return false
        if (subtitleBurnWouldDiscardHdr(track, deliveredRange)) {
            playbackNotice = HDR_SUBTITLE_NOTICE
            return false
        }
        playbackNotice = null
        // Read the position before the state moves: which timeline the player
        // is on depends on the delivery about to change.
        val position = realPosition()
        val route = subtitleRoute(track, planMode, subtitleDelivery)
        selectedSubtitle = index
        subtitleDelivery = route.delivery
        pgsOverlay.select(index.takeIf { route.delivery == SubtitleDelivery.BitmapOverlay })
        if (route.reopen) {
            restartAt(position, "quality")
        } else {
            // No reopen means the same media item, so its tracks are already
            // published and this lands now — which is what makes switching
            // between two text tracks cost nothing.
            armTrackSelections()
            applyTextSelection()
        }
        return true
    }

    fun clearPlaybackNotice() {
        playbackNotice = null
    }

    fun allowsPictureInPictureCommand(): Boolean {
        if (!pgsOverlayIsActive) return true
        playbackNotice = PGS_OVERLAY_PIP_NOTICE
        return false
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
        restartAt(position, "audio")
    }

    private fun restartAt(
        positionMs: Long,
        reason: String,
        observedAtMs: Long = monotonicNowMs(),
    ) {
        val attempt = beginPlaybackAttempt(reason, observedAtMs)
        when {
            !subtitleDelivery.usesPlanTransport -> openSession(positionMs, attempt)
            planMode == "direct" -> {
                leaveSessionPlayback()
                player.setMediaItem(MediaItem.fromUri(plan.playUrl), positionMs)
                player.prepare()
                playbackTelemetry.prepared(attempt)
                player.playWhenReady = true
                armTrackSelections()
            }
            planMode == "remux" -> {
                leaveSessionPlayback()
                baseMs = positionMs
                val uri = remuxUri(positionMs)
                progressiveMediaOrigin.begin(uri, positionMs)
                player.setMediaItem(MediaItem.fromUri(uri))
                player.prepare()
                playbackTelemetry.prepared(attempt)
                player.playWhenReady = true
                armTrackSelections()
            }
            else -> openSession(positionMs, attempt)
        }
    }

    /**
     * Open an HLS session at `ms`, releasing the one it replaces. A seek is a
     * new session, exactly as the web player does it — unless the server
     * answers `vod`, which is the whole stream already on disk and seeks in
     * place. What kind of session — copy or transcode, renditions or a burn —
     * is [subtitleSessionBody]'s answer, so the shape of every request this
     * client sends is unit-tested rather than assembled inline.
     */
    private fun openSession(ms: Long, attempt: PlaybackAttempt) {
        val requestVersion = ++sessionRequestVersion
        sessionId?.let { vm.endHlsSession(it) }
        sessionId = null
        encoder = null
        sessionIsVod = false
        scope.launch {
            val hls = try {
                vm.createHlsSession(plan.fileId, sessionBody(ms))
            } catch (cancelled: CancellationException) {
                // The screen left composition (or a newer request superseded
                // this one) — the caller saying stop, not the server failing.
                // Swallowing it here would show a failure state for a stream
                // nobody is waiting for any more.
                throw cancelled
            } catch (_: Exception) {
                if (requestVersion == sessionRequestVersion) {
                    playbackTelemetry.cancel(attempt)
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
            // A cached session is the whole stream on disk: its timeline
            // starts at zero and the player seeks, exactly like direct play.
            val timeline = sessionPlaybackTimeline(hls, requestedStartMs = ms)
            baseMs = timeline.baseMs
            player.setMediaItem(
                MediaItem.fromUri(Session.url(hls.playlist_url)),
                timeline.attachPositionMs,
            )
            player.prepare()
            playbackTelemetry.prepared(attempt)
            player.playWhenReady = true
            armTrackSelections()
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
        // as-is, which is what makes the native-rendition session free. The
        // compatibility rescue turns `planMode` into a transcode precisely so
        // it lands here — the copy is the thing the device just refused.
        copyableVideo = planMode != "transcode",
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
     * Record that the current selections still have to reach the player.
     *
     * Deliberately does not try to apply them: right after `prepare()` the only
     * tracks on hand may still be the departing item's, and an override
     * naming a track group that is about to disappear would be dropped
     * silently — with the intent already marked as delivered. [listener]
     * lands them against the tracks that actually arrive.
     */
    private fun armTrackSelections() {
        textSelectionArmed = true
        audioSelectionArmed = true
    }

    private fun applyTextSelection() {
        if (!textSelectionArmed) return
        val index = selectedSubtitle
        // Off, and a burn, are the same instruction to the renderer: show no
        // text track. A burn's cues are already in the picture, and letting
        // ExoPlayer's own language preference pick something here would put a
        // second subtitle policy in front of the one the server decided.
        if (
            index == null ||
            subtitleDelivery == SubtitleDelivery.Burn ||
            subtitleDelivery == SubtitleDelivery.BitmapOverlay
        ) {
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

    /**
     * Pin the audio track the server said would play.
     *
     * Only direct play needs this, and only direct play may have it: the raw
     * file carries every stream, so ExoPlayer picks one — and it picks with
     * `setPreferredAudioLanguage`, a client-side language preference that can
     * disagree with the server's `select_tracks` (the dual-audio anime rule
     * selects Japanese original audio against an English preference). Leaving
     * that unpinned would make the detail screen's default marker, and any
     * pre-play choice the server answered with a `direct` verdict, describe a
     * track other than the one on the speakers.
     *
     * Every other transport already delivers exactly one audio stream — the
     * one the plan's `?audio=` or the session body's `audio` selected — so an
     * override there would index into a list of one.
     */
    private fun applyAudioSelection() {
        if (!audioSelectionArmed) return
        val index = selectedAudio
        if (index == null || !directTransport) {
            audioSelectionArmed = false
            return
        }
        val ordinal = embeddedAudioTrackIndex(index, plan.audio, embeddedLanguages(C.TRACK_TYPE_AUDIO))
        // Nothing to select yet — the media is still being prepared, or the
        // player never published this track. Stay armed and let the next
        // onTracksChanged retry; ExoPlayer's own pick is the honest fallback
        // for a track that is genuinely not there.
        val target = ordinal?.let { trackAt(C.TRACK_TYPE_AUDIO, it) } ?: return
        audioSelectionArmed = false
        player.trackSelectionParameters = player.trackSelectionParameters.buildUpon()
            .setOverrideForType(TrackSelectionOverride(target.first, target.second))
            .setTrackTypeDisabled(C.TRACK_TYPE_AUDIO, false)
            .build()
    }

    /** The player's tracks of one type, flattened in publication order. */
    private fun embeddedLanguages(type: Int): List<String?> =
        player.currentTracks.groups
            .filter { it.type == type }
            .flatMap { group -> (0 until group.length).map { group.getTrackFormat(it).language } }

    private fun embeddedTextLanguages(): List<String?> = embeddedLanguages(C.TRACK_TYPE_TEXT)

    private fun trackAt(type: Int, ordinal: Int): Pair<TrackGroup, Int>? {
        var seen = 0
        for (group in player.currentTracks.groups) {
            if (group.type != type) continue
            for (i in 0 until group.length) {
                if (seen == ordinal) return group.mediaTrackGroup to i
                seen++
            }
        }
        return null
    }

    private fun textTrackAt(ordinal: Int): Pair<TrackGroup, Int>? =
        trackAt(C.TRACK_TYPE_TEXT, ordinal)

    private fun leaveSessionPlayback() {
        sessionRequestVersion++
        sessionId?.let { vm.endHlsSession(it) }
        sessionId = null
        encoder = null
        sessionIsVod = false
        // Back on the plan's own delivery, so back to the plan's own grade —
        // otherwise a chip would keep reporting the session that just ended.
        deliveredRange = plan.deliveredDynamicRange
    }

    private fun remuxUri(ms: Long): String = progressiveRemuxUri(
        plannedUrl = if (plan.mode == "direct") {
            Session.url("/api/v1/files/${plan.fileId}/stream.mp4")
        } else {
            plan.playUrl
        },
        startSeconds = ms / 1000.0,
        audioIndex = selectedAudio,
        audioOffsetMs = audioOffsetMs,
        caps = caps,
        encode = Uri::encode,
    )
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
    val title: String
    val fileId: Long
    val playUrl: String
    val mode: String // "direct" | "remux" | "transcode"
    val durationMs: Long
    val videoCodec: String?
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

    /** `decision.delivered_dynamic_range`: the badge's starting truth. */
    val deliveredDynamicRange: String?
}

@UnstableApi
class BuiltPlayer internal constructor(
    val player: ExoPlayer,
    internal val progressiveMediaOrigin: ProgressiveMediaOrigin,
)

@UnstableApi
fun buildPlayer(context: Context, vm: AppViewModel): BuiltPlayer {
    val selector = DefaultTrackSelector(context).apply {
        parameters = buildUponParameters()
            .setPreferredAudioLanguage(vm.audioLang)
            // Tunneled playback hands decode and A/V sync to the TV SoC's own
            // pipeline, which is what 4K HDR on a Shield or a Chromecast is
            // built around. Requested only on television devices: on a phone it
            // buys nothing and some handset decoders refuse the mode outright.
            // Media3 falls back to normal playback when the device says no.
            .setTunnelingEnabled(isTelevision(context))
            // Text selection is the server's policy, carried by [Controller] —
            // not the selector's: a preferred language here re-enables the
            // "merely the same language" tail that policy deletes, and the
            // server's own renditions are deliberately DEFAULT=NO/AUTOSELECT=NO
            // so the client must say which.
            .setPreferredTextLanguage(null)
            .setSelectUndeterminedTextLanguage(false)
            .setTrackTypeDisabled(C.TRACK_TYPE_TEXT, true)
            .build()
    }
    val progressiveMediaOrigin = ProgressiveMediaOrigin()
    val dataSource: OkHttpDataSource.Factory = Net.dataSourceFactory()
        .setTransferListener(progressiveMediaOrigin)
    val renderers = DefaultRenderersFactory(context)
        // A flaky hardware decoder degrades to software instead of erroring
        // into the compatibility rescue and costing the viewer a restart.
        .setEnableDecoderFallback(true)
    val player = ExoPlayer.Builder(context)
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
    return BuiltPlayer(player, progressiveMediaOrigin)
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
