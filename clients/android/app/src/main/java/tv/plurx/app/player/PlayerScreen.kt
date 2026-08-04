@file:androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)

package tv.plurx.app.player

import android.view.KeyEvent
import android.app.PictureInPictureParams
import android.content.pm.PackageManager
import android.graphics.Rect
import android.os.Build
import android.util.Rational
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.focusable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.ClosedCaption
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Forward10
import androidx.compose.material.icons.filled.Info
import androidx.compose.material.icons.filled.Pause
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.PictureInPictureAlt
import androidx.compose.material.icons.filled.Replay10
import androidx.compose.material.icons.filled.Tune
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Slider
import androidx.compose.material3.SliderDefaults
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusProperties
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.key.onPreviewKeyEvent
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.media3.common.C
import androidx.media3.common.Format
import androidx.media3.common.Player
import androidx.media3.common.VideoSize
import androidx.media3.common.util.UnstableApi
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.ui.AspectRatioFrameLayout
import androidx.media3.ui.PlayerView
import androidx.core.app.PictureInPictureModeChangedInfo
import androidx.core.util.Consumer
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import java.text.SimpleDateFormat
import java.util.Locale
import kotlin.math.roundToInt
import tv.plurx.app.data.AudioTrack
import tv.plurx.app.data.Caps
import tv.plurx.app.data.Decision
import tv.plurx.app.data.Marker
import tv.plurx.app.data.MediaFileDto
import tv.plurx.app.data.Rung
import tv.plurx.app.data.Session
import tv.plurx.app.data.SubTrack
import tv.plurx.app.ui.AppViewModel
import tv.plurx.app.ui.PlaybackTarget
import tv.plurx.app.ui.catchingUnlessCancelled
import tv.plurx.app.ui.components.LoadingBox
import tv.plurx.app.ui.components.MediaFact
import tv.plurx.app.ui.components.MediaFactChip
import tv.plurx.app.ui.components.RequestInitialFocus
import tv.plurx.app.ui.components.TvButton
import tv.plurx.app.ui.components.TvIconButton
import tv.plurx.app.ui.components.dynamicRangeLabel
import tv.plurx.app.ui.components.formatTime
import tv.plurx.app.ui.components.playerMediaFacts
import tv.plurx.app.ui.components.sourceDynamicRange
import tv.plurx.app.ui.components.tvFocusRing
import tv.plurx.app.ui.theme.Accent
import tv.plurx.app.ui.theme.Muted
import tv.plurx.app.ui.theme.Surface

private data class Plan(
    val title: String,
    val subtitle: String?,
    val releaseDate: String?,
    val overview: String?,
    override val durationMs: Long,
    override val fileId: Long,
    override val playUrl: String,
    override val mode: String,
    override val sourceHeight: Int?,
    override val aac: Boolean,
    override val preserveDolbyVision: Boolean,
    override val deliveredDynamicRange: String?,
    val markers: List<Marker>,
    val reasons: List<String>,
    val videoWidth: Int?,
    val videoHeight: Int?,
    val source: MediaFileDto?,
    override val audio: List<AudioTrack>,
    override val subtitles: List<SubTrack>,
    val ladder: List<Rung>,
    val declaredOffsetMs: Long?,
) : PlanLike

private suspend fun loadPlan(vm: AppViewModel, itemId: Long, fileId: Long): Plan? = try {
    val detail = vm.itemDetail(itemId)
    val decision: Decision = vm.decision(fileId)
    val file = detail.files.firstOrNull { it.id == fileId } ?: detail.files.firstOrNull()
    val mode = decision.delivery?.mode ?: when (decision.method) {
        "direct_play" -> "direct"
        "remux" -> "remux"
        else -> "transcode"
    }
    Plan(
        title = detail.item.title,
        subtitle = playerSubtitle(detail.item),
        releaseDate = playerDateLabel(detail.item.air_date, detail.item.year),
        overview = detail.item.overview,
        durationMs = file?.duration_ms ?: detail.item.runtime_ms ?: 0L,
        fileId = fileId,
        playUrl = Session.url(decision.delivery?.url ?: decision.play_url),
        mode = mode,
        // The decision's own reading of the source: the number every height
        // promise is made of. The item's file row is the fallback for a
        // server too old to send `source`.
        sourceHeight = (decision.source?.height ?: file?.height)?.toInt(),
        aac = decision.delivery?.aac ?: decision.transcode_audio,
        // Direct delivery has no remux-specific field, but the flattened
        // decision still says whether these exact source bytes are DV. Keep
        // that fact so a decoder failure can try a DV-preserving MP4 remux
        // before the final SDR compatibility transcode.
        preserveDolbyVision = decision.delivery?.preserve_dolby_vision
            ?: decision.preserve_dolby_vision,
        deliveredDynamicRange = decision.delivered_dynamic_range,
        markers = decision.markers,
        reasons = decision.reasons,
        videoWidth = file?.width?.toInt(),
        videoHeight = file?.height?.toInt(),
        source = file,
        audio = decision.audio,
        subtitles = decision.subtitles,
        ladder = decision.ladder,
        declaredOffsetMs = decision.declared_offset_ms,
    )
} catch (cancelled: CancellationException) {
    // A superseded load (Retry, or a new file) unwinding. Returning null here
    // would report "Couldn't start playback" for the attempt that replaced it.
    throw cancelled
} catch (_: Exception) {
    null
}

internal fun playerSubtitle(item: tv.plurx.app.data.Item): String? = buildList {
    item.show_title?.takeIf { it.isNotBlank() }?.let(::add)
    if (item.season_number != null && item.episode_number != null) {
        add("S${item.season_number}E${item.episode_number}")
    }
}.takeIf { it.isNotEmpty() }?.joinToString("   ·   ")

internal fun playerDateLabel(airDate: String?, year: Int?): String? {
    val raw = airDate?.trim().orEmpty()
    if (raw.isNotEmpty()) {
        val parsed = runCatching {
            SimpleDateFormat("yyyy-MM-dd", Locale.US).apply { isLenient = false }
                .parse(raw.take(10))
        }.getOrNull()
        if (parsed != null) {
            return SimpleDateFormat("MMM d, yyyy", Locale.US).format(parsed)
        }
        return raw
    }
    return year?.toString()
}

internal fun playerRuntimeLabel(milliseconds: Long): String {
    val totalMinutes = milliseconds / 60_000
    val hours = totalMinutes / 60
    val minutes = totalMinutes % 60
    return if (hours > 0) "${hours}h ${minutes}m" else "${minutes}m"
}

private enum class PlayerPanel { Tracks, Settings, Info }

internal enum class PlayerBackAction { ClosePanel, HideControls, ExitPlayback }

internal fun playerBackAction(panelOpen: Boolean, controlsVisible: Boolean): PlayerBackAction = when {
    panelOpen -> PlayerBackAction.ClosePanel
    controlsVisible -> PlayerBackAction.HideControls
    else -> PlayerBackAction.ExitPlayback
}

/**
 * Directional playback shortcuts only own the bare video surface. Visible
 * controls keep normal D-pad focus navigation.
 */
internal fun playerSeekDeltaMs(keyCode: Int, controlsVisible: Boolean): Long? {
    if (controlsVisible) return null
    return when (keyCode) {
        KeyEvent.KEYCODE_DPAD_LEFT -> -10_000L
        KeyEvent.KEYCODE_DPAD_RIGHT -> 10_000L
        KeyEvent.KEYCODE_DPAD_DOWN -> -30_000L
        KeyEvent.KEYCODE_DPAD_UP -> 30_000L
        else -> null
    }
}

private const val MAX_PIP_ASPECT_RATIO = 2.39

/** Returns an Android-supported PiP aspect ratio, using 16:9 when media metadata is unusable. */
internal fun calculatePipAspectRatio(
    width: Int,
    height: Int,
    pixelWidthHeightRatio: Float = 1f,
): Rational {
    if (width <= 0 || height <= 0 || !pixelWidthHeightRatio.isFinite() || pixelWidthHeightRatio <= 0f) {
        return Rational(16, 9)
    }

    val adjustedWidth = (width.toDouble() * pixelWidthHeightRatio).roundToInt()
    if (adjustedWidth <= 0) return Rational(16, 9)

    val aspectRatio = adjustedWidth.toDouble() / height
    val minimumAspectRatio = 1.0 / MAX_PIP_ASPECT_RATIO
    return if (aspectRatio in minimumAspectRatio..MAX_PIP_ASPECT_RATIO) {
        Rational(adjustedWidth, height)
    } else {
        Rational(16, 9)
    }
}

private fun pictureInPictureParams(
    aspectRatio: Rational,
    sourceRect: Rect?,
    autoEnter: Boolean,
): PictureInPictureParams? {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return null

    val builder = PictureInPictureParams.Builder().setAspectRatio(aspectRatio)
    if (sourceRect != null && !sourceRect.isEmpty) builder.setSourceRectHint(sourceRect)
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
        builder
            .setAutoEnterEnabled(autoEnter)
            .setSeamlessResizeEnabled(true)
    }
    return builder.build()
}

private fun setPictureInPictureParams(
    activity: android.app.Activity,
    params: PictureInPictureParams?,
) {
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O && params != null) {
        activity.setPictureInPictureParams(params)
    }
}

private fun enterPictureInPicture(
    activity: android.app.Activity,
    params: PictureInPictureParams?,
) {
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O && params != null) {
        activity.enterPictureInPictureMode(params)
    }
}

private fun isInPictureInPicture(activity: android.app.Activity): Boolean =
    Build.VERSION.SDK_INT >= Build.VERSION_CODES.N && activity.isInPictureInPictureMode

@Composable
fun PlayerScreen(
    vm: AppViewModel,
    itemId: Long,
    fileId: Long,
    startMs: Long,
    onPlayNext: (PlaybackTarget) -> Unit,
    onExit: () -> Unit,
) {
    var plan by remember(itemId, fileId) { mutableStateOf<Plan?>(null) }
    var failed by remember(itemId, fileId) { mutableStateOf(false) }
    var generation by remember(itemId, fileId) { mutableIntStateOf(0) }
    var resumeAt by remember(itemId, fileId) { mutableLongStateOf(startMs) }
    // Survives the plan, like the A/V correction beside it: a quality change
    // reloads the plan and rebuilds the controller, and the viewer's audio and
    // subtitle picks must come back with them.
    var playbackAudioOffset by remember(itemId, fileId) { mutableLongStateOf(0) }
    var playbackAudio by remember(itemId, fileId) { mutableStateOf<Long?>(null) }
    var playbackSubtitle by remember(itemId, fileId) { mutableStateOf<SubtitleChoice?>(null) }

    ImmersivePlaybackEffect()

    LaunchedEffect(itemId, fileId, generation) {
        failed = false
        val loaded = loadPlan(vm, itemId, fileId)
        if (loaded == null) failed = true else plan = loaded
    }

    Box(Modifier.fillMaxSize().background(Color.Black)) {
        when {
            failed -> PlaybackFailed(
                message = "Couldn't start playback.",
                onRetry = { generation++ },
                onExit = onExit,
            )
            plan == null -> {
                LoadingBox()
                BackChip(onExit)
            }
            else -> PlayerContent(
                vm = vm,
                itemId = itemId,
                plan = plan!!,
                startMs = resumeAt,
                audioOffsetMs = playbackAudioOffset,
                onAudioOffsetChanged = { playbackAudioOffset = it },
                retainedAudio = playbackAudio,
                onAudioChanged = { playbackAudio = it },
                retainedSubtitle = playbackSubtitle,
                onSubtitleChanged = { playbackSubtitle = SubtitleChoice(it) },
                onReload = { position ->
                    resumeAt = position
                    plan = null
                    generation++
                },
                onPlayNext = onPlayNext,
                onExit = onExit,
            )
        }
    }
}

@Composable
private fun ImmersivePlaybackEffect() {
    val activity = androidx.activity.compose.LocalActivity.current ?: return
    val lifecycleOwner = LocalLifecycleOwner.current

    DisposableEffect(activity, lifecycleOwner) {
        val window = activity.window
        val decorView = window.decorView
        val insetsController = WindowCompat.getInsetsController(window, decorView)
        var active = true

        val enterImmersiveMode = Runnable {
            if (!active) return@Runnable
            WindowCompat.setDecorFitsSystemWindows(window, false)
            insetsController.systemBarsBehavior =
                WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            insetsController.hide(WindowInsetsCompat.Type.systemBars())
        }
        val focusListener = android.view.ViewTreeObserver.OnWindowFocusChangeListener { hasFocus ->
            if (hasFocus) decorView.post(enterImmersiveMode)
        }
        val lifecycleObserver = LifecycleEventObserver { _, event ->
            if (event == Lifecycle.Event.ON_RESUME) decorView.post(enterImmersiveMode)
        }

        decorView.viewTreeObserver.addOnWindowFocusChangeListener(focusListener)
        lifecycleOwner.lifecycle.addObserver(lifecycleObserver)
        // Waiting for the decor view's next frame prevents the initial request from
        // being lost while Compose and PlayerView are still taking window focus.
        decorView.post(enterImmersiveMode)

        onDispose {
            active = false
            decorView.removeCallbacks(enterImmersiveMode)
            if (decorView.viewTreeObserver.isAlive) {
                decorView.viewTreeObserver.removeOnWindowFocusChangeListener(focusListener)
            }
            lifecycleOwner.lifecycle.removeObserver(lifecycleObserver)
            insetsController.show(WindowInsetsCompat.Type.systemBars())
            WindowCompat.setDecorFitsSystemWindows(window, true)
        }
    }
}

/**
 * A real failure state. The rescue in [Controller] has already been spent by
 * the time this shows, so the viewer gets the reason and a way out — never a
 * frozen black surface with a stream that will not come back.
 */
@Composable
private fun PlaybackFailed(
    message: String,
    onRetry: (() -> Unit)?,
    onExit: () -> Unit,
) {
    val retryFocusRequester = remember { FocusRequester() }
    RequestInitialFocus(retryFocusRequester, enabled = onRetry != null)
    Column(
        Modifier.fillMaxSize(),
        verticalArrangement = Arrangement.Center,
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text(message, color = Color.White)
        Spacer(Modifier.size(12.dp))
        Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            if (onRetry != null) {
                TvButton(
                    onClick = onRetry,
                    modifier = Modifier.focusRequester(retryFocusRequester),
                ) { Text("Retry") }
            }
            TvButton(onClick = onExit) { Text("Back") }
        }
    }
}

@Composable
private fun PlayerContent(
    vm: AppViewModel,
    itemId: Long,
    plan: Plan,
    startMs: Long,
    audioOffsetMs: Long,
    onAudioOffsetChanged: (Long) -> Unit,
    retainedAudio: Long?,
    onAudioChanged: (Long) -> Unit,
    retainedSubtitle: SubtitleChoice?,
    onSubtitleChanged: (Long?) -> Unit,
    onReload: (Long) -> Unit,
    onPlayNext: (PlaybackTarget) -> Unit,
    onExit: () -> Unit,
) {
    val context = androidx.compose.ui.platform.LocalContext.current
    val activity = androidx.activity.compose.LocalActivity.current
    val componentActivity = activity as? ComponentActivity
    val canUsePip = Build.VERSION.SDK_INT >= Build.VERSION_CODES.O &&
        context.packageManager.hasSystemFeature(PackageManager.FEATURE_PICTURE_IN_PICTURE)
    val scope = rememberCoroutineScope()
    val preferences by vm.preferences.collectAsStateWithLifecycle()
    var playFailure by remember { mutableStateOf<String?>(null) }
    val controller = remember(plan) {
        Controller(
            context,
            buildPlayer(context, vm),
            plan,
            vm.playbackCaps,
            vm,
            scope,
            initialAudioOffsetMs = audioOffsetMs,
            retainedAudio = retainedAudio,
            retainedSubtitle = retainedSubtitle,
            onError = { playFailure = it },
        )
    }
    val surfaceFocusRequester = remember { FocusRequester() }
    // Which grades this panel can show. Probed once per playback: it is a
    // property of the cable, and this screen owns one HDMI route for its life.
    val displayHdrTypes = remember(context) { Caps.displayHdrTypes(context) }

    var positionMs by remember { mutableLongStateOf(startMs) }
    var scrubbing by remember { mutableStateOf(false) }
    var scrubPreview by remember { mutableLongStateOf(startMs) }
    var isPlaying by remember { mutableStateOf(true) }
    var buffering by remember { mutableStateOf(true) }
    var controlsVisible by remember { mutableStateOf(true) }
    var panel by remember { mutableStateOf<PlayerPanel?>(null) }
    var playerView by remember { mutableStateOf<PlayerView?>(null) }
    var isInPip by remember(activity) {
        mutableStateOf(
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.O &&
                activity?.isInPictureInPictureMode == true,
        )
    }
    var pipAspectRatio by remember(plan) {
        mutableStateOf(calculatePipAspectRatio(plan.videoWidth ?: 0, plan.videoHeight ?: 0))
    }
    var lastInteraction by remember { mutableLongStateOf(0L) }
    var lastAutoSkipped by remember(plan) { mutableLongStateOf(-1L) }
    var findingNext by remember { mutableStateOf(false) }

    fun poke() {
        controlsVisible = true
        lastInteraction += 1
    }

    BackHandler(enabled = !isInPip) {
        when (playerBackAction(panel != null, controlsVisible)) {
            PlayerBackAction.ClosePanel -> {
                panel = null
                poke()
            }
            PlayerBackAction.HideControls -> controlsVisible = false
            PlayerBackAction.ExitPlayback -> onExit()
        }
    }

    // Keyed on the controller alone. Keying it on the preference too meant
    // toggling "Autoplay next episode" mid-play disposed the effect,
    // released the live player, and re-registered on the corpse — restarting
    // the episode at its original position. The listener reads the current
    // value instead of being rebuilt for it.
    val autoplayNext by rememberUpdatedState(preferences.autoplayNext)
    val playNext by rememberUpdatedState(onPlayNext)
    DisposableEffect(controller) {
        val listener = object : Player.Listener {
            override fun onIsPlayingChanged(playing: Boolean) {
                isPlaying = playing
                if (!playing) vm.postProgress(itemId, controller.realPosition(), plan.durationMs)
            }

            override fun onPlaybackStateChanged(state: Int) {
                buffering = state == Player.STATE_BUFFERING
                if (state == Player.STATE_ENDED) {
                    vm.postProgress(itemId, plan.durationMs, plan.durationMs)
                    controlsVisible = true
                    if (autoplayNext && !findingNext) {
                        findingNext = true
                        scope.launch {
                            val next = catchingUnlessCancelled { vm.nextEpisode(itemId) }.getOrNull()
                            findingNext = false
                            if (next != null) playNext(next)
                        }
                    }
                }
            }

            override fun onVideoSizeChanged(videoSize: VideoSize) {
                pipAspectRatio = calculatePipAspectRatio(
                    videoSize.width,
                    videoSize.height,
                    videoSize.pixelWidthHeightRatio,
                )
            }
        }
        controller.player.addListener(listener)
        controller.startAt(startMs)
        onDispose {
            vm.postProgress(itemId, controller.realPosition(), plan.durationMs)
            controller.player.removeListener(listener)
            controller.release()
        }
    }

    fun currentPipParams(autoEnter: Boolean = isPlaying): PictureInPictureParams? {
        val sourceRect = playerView?.let { view ->
            Rect().takeIf { view.getGlobalVisibleRect(it) && !it.isEmpty }
        }
        return pictureInPictureParams(pipAspectRatio, sourceRect, autoEnter)
    }

    DisposableEffect(componentActivity, canUsePip) {
        if (!canUsePip || componentActivity == null) {
            onDispose { }
        } else {
            val pipModeListener = Consumer<PictureInPictureModeChangedInfo> { info ->
                isInPip = info.isInPictureInPictureMode
                panel = null
                if (info.isInPictureInPictureMode) {
                    controlsVisible = false
                } else {
                    controlsVisible = true
                    lastInteraction += 1
                }
            }
            componentActivity.addOnPictureInPictureModeChangedListener(pipModeListener)
            onDispose {
                componentActivity.removeOnPictureInPictureModeChangedListener(pipModeListener)
            }
        }
    }

    // Android 12+ reads auto-enter from the current PiP parameters. Android 8–11
    // instead needs an explicit request when the user leaves the activity.
    DisposableEffect(componentActivity, canUsePip, isPlaying, pipAspectRatio, playerView) {
        if (
            !canUsePip || componentActivity == null ||
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.S
        ) {
            onDispose { }
        } else {
            val leaveHintListener = Runnable {
                if (isPlaying && !isInPictureInPicture(componentActivity)) {
                    controlsVisible = false
                    panel = null
                    enterPictureInPicture(componentActivity, currentPipParams(autoEnter = false))
                }
            }
            componentActivity.addOnUserLeaveHintListener(leaveHintListener)
            onDispose { componentActivity.removeOnUserLeaveHintListener(leaveHintListener) }
        }
    }

    DisposableEffect(activity, canUsePip, isPlaying, pipAspectRatio, playerView) {
        if (!canUsePip || activity == null) {
            onDispose { }
        } else {
            val updateParams = Runnable {
                setPictureInPictureParams(activity, currentPipParams())
            }
            val layoutListener = android.view.View.OnLayoutChangeListener { _, _, _, _, _, _, _, _, _ ->
                updateParams.run()
            }
            playerView?.addOnLayoutChangeListener(layoutListener)
            playerView?.post(updateParams) ?: updateParams.run()
            onDispose {
                playerView?.removeOnLayoutChangeListener(layoutListener)
                playerView?.removeCallbacks(updateParams)
            }
        }
    }

    // Do not leave automatic PiP armed after navigating away from the player.
    DisposableEffect(activity, canUsePip) {
        onDispose {
            if (canUsePip && activity != null && Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                setPictureInPictureParams(
                    activity,
                    pictureInPictureParams(pipAspectRatio, null, autoEnter = false),
                )
            }
        }
    }

    LaunchedEffect(controller) {
        while (true) {
            if (!scrubbing) positionMs = controller.realPosition()
            delay(500)
        }
    }
    LaunchedEffect(controller.playbackNotice) {
        val notice = controller.playbackNotice ?: return@LaunchedEffect
        delay(5_000)
        if (controller.playbackNotice == notice) controller.clearPlaybackNotice()
    }
    LaunchedEffect(controller) {
        while (true) {
            delay(10_000)
            if (isPlaying) vm.reportProgress(itemId, controller.realPosition(), plan.durationMs)
        }
    }
    LaunchedEffect(lastInteraction, isPlaying, panel) {
        if (isPlaying && panel == null) {
            delay(3_800)
            controlsVisible = false
        }
    }

    val activeMarker = plan.markers.firstOrNull { positionMs in it.start_ms until it.end_ms }
    LaunchedEffect(activeMarker?.start_ms, preferences.autoSkip) {
        if (preferences.autoSkip && activeMarker != null && activeMarker.start_ms != lastAutoSkipped) {
            lastAutoSkipped = activeMarker.start_ms
            controller.seekTo(activeMarker.end_ms)
        }
    }

    LaunchedEffect(controlsVisible, panel, isInPip) {
        if (!isInPip && !controlsVisible && panel == null) {
            surfaceFocusRequester.requestFocus()
        }
    }

    playFailure?.let { message ->
        PlaybackFailed(
            message = message,
            onRetry = { onReload(controller.realPosition()) },
            onExit = onExit,
        )
        return
    }

    Box(
        Modifier.fillMaxSize()
            .focusRequester(surfaceFocusRequester)
            .focusable()
            .onPreviewKeyEvent { event ->
                if (event.nativeKeyEvent.action != KeyEvent.ACTION_DOWN) return@onPreviewKeyEvent false
                val seekDelta = playerSeekDeltaMs(
                    keyCode = event.nativeKeyEvent.keyCode,
                    controlsVisible = controlsVisible,
                )
                if (seekDelta != null) {
                    controller.seekTo(controller.realPosition() + seekDelta)
                    return@onPreviewKeyEvent true
                }
                when (event.nativeKeyEvent.keyCode) {
                    KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE -> {
                        controller.playPause()
                        true
                    }
                    KeyEvent.KEYCODE_DPAD_CENTER, KeyEvent.KEYCODE_ENTER -> {
                        if (!controlsVisible) {
                            poke()
                            true
                        } else {
                            poke()
                            false
                        }
                    }
                    KeyEvent.KEYCODE_MEDIA_REWIND -> {
                        controller.seekTo(controller.realPosition() - 10_000); poke(); true
                    }
                    KeyEvent.KEYCODE_MEDIA_FAST_FORWARD -> {
                        controller.seekTo(controller.realPosition() + 10_000); poke(); true
                    }
                    KeyEvent.KEYCODE_DPAD_LEFT,
                    KeyEvent.KEYCODE_DPAD_RIGHT,
                    KeyEvent.KEYCODE_DPAD_UP,
                    KeyEvent.KEYCODE_DPAD_DOWN,
                    -> {
                        poke()
                        false
                    }
                    else -> false
                }
            },
    ) {
        AndroidView(
            factory = {
                PlayerView(it).apply {
                    player = controller.player
                    useController = false
                    resizeMode = AspectRatioFrameLayout.RESIZE_MODE_FIT
                    setShutterBackgroundColor(android.graphics.Color.BLACK)
                    keepScreenOn = true
                    playerView = this
                }
            },
            update = { playerView = it },
            modifier = Modifier.fillMaxSize(),
        )

        if (!isInPip) {
            Box(
                Modifier.fillMaxSize().focusProperties { canFocus = false }.clickable(
                    interactionSource = remember { MutableInteractionSource() },
                    indication = null,
                ) { if (controlsVisible) controlsVisible = false else poke() },
            )
        }

        if (!isInPip && (buffering || findingNext)) {
            Column(Modifier.align(Alignment.Center), horizontalAlignment = Alignment.CenterHorizontally) {
                CircularProgressIndicator(color = Accent)
                if (findingNext) Text("Up next…", color = Color.White, modifier = Modifier.padding(top = 12.dp))
            }
        }

        if (!isInPip && activeMarker != null && !scrubbing && !preferences.autoSkip) {
            TvButton(
                onClick = { controller.seekTo(activeMarker.end_ms); poke() },
                modifier = Modifier.align(Alignment.BottomEnd).padding(end = 28.dp, bottom = 112.dp),
            ) { Text(activeMarker.label, fontWeight = FontWeight.SemiBold) }
        }

        if (!isInPip && controlsVisible) {
            Controls(
                title = plan.title,
                subtitle = plan.subtitle,
                releaseDate = plan.releaseDate,
                runtimeLabel = plan.durationMs.takeIf { it > 0 }?.let(::playerRuntimeLabel),
                overview = plan.overview,
                positionMs = if (scrubbing) scrubPreview else positionMs,
                durationMs = plan.durationMs,
                isPlaying = isPlaying,
                requestInitialFocus = panel == null,
                mediaFacts = playerMediaFacts(
                    file = plan.source,
                    audio = plan.audio.firstOrNull { it.index == controller.selectedAudio }
                        ?: plan.audio.firstOrNull { it.default },
                    delivered = controller.deliveredRange,
                    rendered = renderedRange(
                        delivered = controller.deliveredRange,
                        decoderMime = controller.player.videoFormat?.sampleMimeType,
                        decoderColorTransfer = controller.player.videoFormat?.colorInfo?.colorTransfer,
                        hdrTypes = displayHdrTypes,
                    ),
                ),
                onBack = onExit,
                onPlayPause = { controller.playPause(); poke() },
                onSeekBack = { controller.seekTo(controller.realPosition() - 10_000); poke() },
                onSeekForward = { controller.seekTo(controller.realPosition() + 10_000); poke() },
                onScrubStart = { scrubbing = true; scrubPreview = positionMs },
                onScrub = { scrubPreview = it },
                onScrubEnd = { controller.seekTo(scrubPreview); scrubbing = false; poke() },
                onTracks = { panel = PlayerPanel.Tracks },
                onSettings = { panel = PlayerPanel.Settings },
                onInfo = {
                    controlsVisible = false
                    panel = PlayerPanel.Info
                },
                onPip = if (canUsePip && activity != null) {
                    {
                        controlsVisible = false
                        panel = null
                        val params = currentPipParams(autoEnter = false)
                        setPictureInPictureParams(activity, params)
                        enterPictureInPicture(activity, params)
                    }
                } else null,
            )
        }

        when (if (isInPip) null else panel) {
            PlayerPanel.Tracks -> TrackMenu(
                player = controller.player,
                serverAudio = plan.audio,
                serverSubtitles = plan.subtitles,
                serverControlledAudio = controller.deliveryMode != "direct",
                selectedServerAudio = controller.selectedAudio,
                selectedServerSubtitle = controller.selectedSubtitle,
                onServerAudio = { onAudioChanged(it); controller.switchAudio(it); panel = null; poke() },
                onServerSubtitle = {
                    if (controller.switchSubtitle(it)) onSubtitleChanged(it)
                    panel = null
                    poke()
                },
                onDismiss = { panel = null; poke() },
            )
            PlayerPanel.Settings -> PlayerSettings(
                vm = vm,
                qualityOptions = qualityOptions(plan.ladder),
                audioOffsetMs = controller.audioOffsetMs,
                declaredOffsetMs = plan.declaredOffsetMs,
                currentPosition = controller::realPosition,
                onReload = onReload,
                onAudioOffset = {
                    controller.setAudioOffset(it)
                    onAudioOffsetChanged(it)
                },
                onDismiss = { panel = null; poke() },
            )
            PlayerPanel.Info -> PlayerInfo(
                plan = plan,
                controller = controller,
                positionMs = positionMs,
                displayHdrTypes = displayHdrTypes,
                onDismiss = { panel = null; poke() },
            )
            null -> Unit
        }

        controller.playbackNotice?.let { notice ->
            Text(
                text = notice,
                color = Color.White,
                style = MaterialTheme.typography.bodySmall,
                modifier = Modifier
                    .align(Alignment.TopCenter)
                    .padding(top = 24.dp, start = 40.dp, end = 40.dp)
                    .background(Color.Black.copy(alpha = 0.82f), MaterialTheme.shapes.medium)
                    .padding(horizontal = 14.dp, vertical = 10.dp),
            )
        }
    }
}

@Composable
internal fun Controls(
    title: String,
    subtitle: String? = null,
    releaseDate: String? = null,
    runtimeLabel: String? = null,
    overview: String? = null,
    positionMs: Long,
    durationMs: Long,
    isPlaying: Boolean,
    requestInitialFocus: Boolean,
    mediaFacts: List<MediaFact> = emptyList(),
    onBack: () -> Unit,
    onPlayPause: () -> Unit,
    onSeekBack: () -> Unit,
    onSeekForward: () -> Unit,
    onScrubStart: () -> Unit,
    onScrub: (Long) -> Unit,
    onScrubEnd: () -> Unit,
    onTracks: () -> Unit,
    onSettings: () -> Unit,
    onInfo: () -> Unit,
    onPip: (() -> Unit)?,
) {
    val playFocusRequester = remember { FocusRequester() }
    RequestInitialFocus(playFocusRequester, enabled = requestInitialFocus)
    Box(Modifier.fillMaxSize()) {
        Row(
            Modifier.align(Alignment.TopStart).fillMaxWidth().padding(12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            TvIconButton(onClick = onBack) {
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back", tint = Color.White)
            }
            Spacer(Modifier.weight(1f))
            TvIconButton(onClick = onTracks) {
                Icon(Icons.Filled.ClosedCaption, contentDescription = "Audio and subtitles", tint = Color.White)
            }
            TvIconButton(onClick = onSettings) {
                Icon(Icons.Filled.Tune, contentDescription = "Playback settings", tint = Color.White)
            }
            TvIconButton(onClick = onInfo) {
                Icon(Icons.Filled.Info, contentDescription = "Playback info", tint = Color.White)
            }
            if (onPip != null) {
                TvIconButton(onClick = onPip) {
                    Icon(Icons.Filled.PictureInPictureAlt, contentDescription = "Picture in picture", tint = Color.White)
                }
            }
        }

        Column(
            Modifier
                .align(Alignment.BottomCenter)
                .fillMaxWidth()
                .background(
                    Brush.verticalGradient(
                        listOf(Color.Transparent, Color.Black.copy(alpha = 0.9f)),
                    ),
                )
                .padding(start = 28.dp, top = 72.dp, end = 28.dp, bottom = 22.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Text(
                playerHeading(title, subtitle),
                color = Color.White,
                style = MaterialTheme.typography.headlineSmall,
                fontWeight = FontWeight.Bold,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            if (mediaFacts.isNotEmpty()) {
                Row(
                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                ) {
                    mediaFacts.forEach { fact -> MediaFactChip(fact) }
                }
            }

            playerContextLine(releaseDate, runtimeLabel)?.let { context ->
                Text(
                    context,
                    color = Color.White.copy(alpha = 0.74f),
                    style = MaterialTheme.typography.bodyMedium,
                    maxLines = 1,
                )
            }

            overview?.trim()?.takeIf { it.isNotEmpty() }?.let { summary ->
                Text(
                    summary,
                    color = Color.White.copy(alpha = 0.88f),
                    style = MaterialTheme.typography.bodyMedium,
                    maxLines = 3,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.widthIn(max = 980.dp),
                )
            }

            BoxWithConstraints(Modifier.fillMaxWidth().padding(top = 4.dp)) {
                if (maxWidth >= 700.dp) {
                    Row(
                        Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.spacedBy(10.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        TransportButtons(
                            isPlaying = isPlaying,
                            playFocusRequester = playFocusRequester,
                            onPlayPause = onPlayPause,
                            onSeekBack = onSeekBack,
                            onSeekForward = onSeekForward,
                        )
                        PlaybackTime(positionMs)
                        PlaybackPositionSlider(
                            positionMs = positionMs,
                            durationMs = durationMs,
                            playFocusRequester = playFocusRequester,
                            onScrubStart = onScrubStart,
                            onScrub = onScrub,
                            onScrubEnd = onScrubEnd,
                            modifier = Modifier.weight(1f),
                        )
                        PlaybackTime(durationMs)
                    }
                } else {
                    Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
                        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.Center) {
                            TransportButtons(
                                isPlaying = isPlaying,
                                playFocusRequester = playFocusRequester,
                                onPlayPause = onPlayPause,
                                onSeekBack = onSeekBack,
                                onSeekForward = onSeekForward,
                            )
                        }
                        PlaybackPositionSlider(
                            positionMs = positionMs,
                            durationMs = durationMs,
                            playFocusRequester = playFocusRequester,
                            onScrubStart = onScrubStart,
                            onScrub = onScrub,
                            onScrubEnd = onScrubEnd,
                            modifier = Modifier.fillMaxWidth(),
                        )
                        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                            PlaybackTime(positionMs)
                            PlaybackTime(durationMs)
                        }
                    }
                }
            }
        }
    }
}

internal fun playerHeading(title: String, subtitle: String?): String =
    listOfNotNull(subtitle?.trim()?.takeIf { it.isNotEmpty() }, title.trim())
        .joinToString("   ·   ")

internal fun playerContextLine(releaseDate: String?, runtimeLabel: String?): String? =
    listOfNotNull(
        releaseDate?.trim()?.takeIf { it.isNotEmpty() },
        runtimeLabel?.trim()?.takeIf { it.isNotEmpty() },
    ).takeIf { it.isNotEmpty() }?.joinToString("   ·   ")

@Composable
private fun TransportButtons(
    isPlaying: Boolean,
    playFocusRequester: FocusRequester,
    onPlayPause: () -> Unit,
    onSeekBack: () -> Unit,
    onSeekForward: () -> Unit,
) {
    Row(
        horizontalArrangement = Arrangement.spacedBy(8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        TvIconButton(onClick = onSeekBack, modifier = Modifier.size(48.dp)) {
            Icon(
                Icons.Filled.Replay10,
                contentDescription = "Back 10 seconds",
                tint = Color.White,
                modifier = Modifier.size(34.dp),
            )
        }
        TvIconButton(
            onClick = onPlayPause,
            modifier = Modifier.size(56.dp).focusRequester(playFocusRequester),
        ) {
            Icon(
                if (isPlaying) Icons.Filled.Pause else Icons.Filled.PlayArrow,
                contentDescription = if (isPlaying) "Pause" else "Play",
                tint = Color.White,
                modifier = Modifier.size(42.dp),
            )
        }
        TvIconButton(onClick = onSeekForward, modifier = Modifier.size(48.dp)) {
            Icon(
                Icons.Filled.Forward10,
                contentDescription = "Forward 10 seconds",
                tint = Color.White,
                modifier = Modifier.size(34.dp),
            )
        }
    }
}

@Composable
private fun PlaybackPositionSlider(
    positionMs: Long,
    durationMs: Long,
    playFocusRequester: FocusRequester,
    onScrubStart: () -> Unit,
    onScrub: (Long) -> Unit,
    onScrubEnd: () -> Unit,
    modifier: Modifier,
) {
    val range = if (durationMs > 0) durationMs.toFloat() else 1f
    Slider(
        value = positionMs.coerceIn(0, durationMs.coerceAtLeast(0)).toFloat(),
        onValueChange = { onScrubStart(); onScrub(it.toLong()) },
        onValueChangeFinished = onScrubEnd,
        valueRange = 0f..range,
        modifier = modifier
            .semantics { contentDescription = "Playback position" }
            .onPreviewKeyEvent { event ->
                val vertical = event.nativeKeyEvent.keyCode == KeyEvent.KEYCODE_DPAD_UP ||
                    event.nativeKeyEvent.keyCode == KeyEvent.KEYCODE_DPAD_DOWN
                if (vertical) {
                    if (event.nativeKeyEvent.action == KeyEvent.ACTION_DOWN) {
                        playFocusRequester.requestFocus()
                    }
                    true
                } else {
                    false
                }
            },
        colors = SliderDefaults.colors(
            thumbColor = Accent,
            activeTrackColor = Accent,
            inactiveTrackColor = Color(0x55FFFFFF),
        ),
    )
}

@Composable
private fun PlaybackTime(milliseconds: Long) {
    Text(
        formatTime(milliseconds),
        color = Color.White,
        style = MaterialTheme.typography.labelMedium,
    )
}

@Composable
private fun PlayerSettings(
    vm: AppViewModel,
    qualityOptions: List<QualityOption>,
    audioOffsetMs: Long,
    declaredOffsetMs: Long?,
    currentPosition: () -> Long,
    onReload: (Long) -> Unit,
    onAudioOffset: (Long) -> Unit,
    onDismiss: () -> Unit,
) {
    val preferences by vm.preferences.collectAsStateWithLifecycle()
    val initialFocusRequester = remember { FocusRequester() }
    var offset by remember(audioOffsetMs) { mutableLongStateOf(audioOffsetMs) }
    RequestInitialFocus(initialFocusRequester)
    PlayerPanelSurface("Playback settings", onDismiss) {
        Text("Quality", color = Muted, style = MaterialTheme.typography.labelMedium)
        // The rungs are the server's, filtered to what this source can feed —
        // a 1080p file never offers to upscale itself to 4K.
        qualityOptions.forEach { option ->
            val quality = option.quality
            PanelRow(
                label = option.label,
                selected = preferences.playbackQuality == quality,
                modifier = if (preferences.playbackQuality == quality) {
                    Modifier.focusRequester(initialFocusRequester)
                } else {
                    Modifier
                },
            ) {
                val position = currentPosition()
                vm.setPlaybackQuality(quality)
                onReload(position)
            }
        }
        Text("Audio sync", color = Muted, style = MaterialTheme.typography.labelMedium, modifier = Modifier.padding(top = 12.dp))
        Text(offsetLabel(offset), color = Color.White, style = MaterialTheme.typography.bodyMedium)
        declaredOffsetMs?.let {
            Text("Container declares ${if (it > 0) "+" else ""}$it ms (already honored)", color = Muted, style = MaterialTheme.typography.labelMedium)
        }
        listOf(-250L, -50L, 50L, 250L).forEach { delta ->
            PanelRow("${if (delta > 0) "+" else ""}$delta ms · audio ${if (delta > 0) "later" else "earlier"}", false) {
                offset = (offset + delta).coerceIn(-15_000, 15_000)
                onAudioOffset(offset)
            }
        }
        if (offset != 0L) {
            PanelRow("Reset sync to 0 ms", false) {
                offset = 0
                onAudioOffset(0)
            }
        }
        PanelSwitch("Auto-skip intro and credits", preferences.autoSkip, vm::setAutoSkip)
        PanelSwitch("Autoplay next episode", preferences.autoplayNext, vm::setAutoplayNext)
    }
}

@Composable
private fun PlayerInfo(
    plan: Plan,
    controller: Controller,
    positionMs: Long,
    displayHdrTypes: Set<Int>,
    onDismiss: () -> Unit,
) {
    val player = controller.player
    val selectedAudio = player.audioFormat?.let(::audioLabel)
        ?: plan.audio.firstOrNull { it.index == controller.selectedAudio }?.let(::serverAudioLabel)
        ?: plan.audio.firstOrNull { it.default }?.let(::serverAudioLabel)
    val selectedSubtitle =
        selectedSubtitleLabel(player, plan.subtitles, controller.selectedSubtitle)
    PlaybackInfoOverlay(
        details = PlaybackInfoDetails(
            title = plan.title,
            fileId = plan.fileId,
            delivery = deliveryLabel(controller.deliveryMode),
            position = "${formatTime(positionMs)} / ${formatTime(plan.durationMs)}",
            buffer = "${formatTime((player.bufferedPosition - player.currentPosition).coerceAtLeast(0))} ahead · " +
                "${player.bufferedPercentage.coerceIn(0, 100)}%",
            videoHealth = videoHealthSummary(player),
            sourceFile = plan.source?.filename,
            sourceVideo = sourceVideoSummary(plan.source),
            sourceAudio = sourceAudioSummary(plan.source),
            playingVideo = videoFormatSummary(player.videoFormat),
            playingAudio = selectedAudio,
            dynamicRange = dynamicRangeSummary(
                source = sourceDynamicRange(plan.source),
                delivered = controller.deliveredRange,
                rendered = renderedRange(
                    delivered = controller.deliveredRange,
                    decoderMime = player.videoFormat?.sampleMimeType,
                    decoderColorTransfer = player.videoFormat?.colorInfo?.colorTransfer,
                    hdrTypes = displayHdrTypes,
                ),
                reasons = plan.reasons,
            ),
            subtitles = selectedSubtitle,
            encoder = controller.encoder,
            audioSync = controller.audioOffsetMs.takeIf { it != 0L }?.let(::offsetLabel),
        ),
        reasons = plan.reasons,
        onDismiss = onDismiss,
    )
}

internal data class PlaybackInfoDetails(
    val title: String,
    val fileId: Long,
    val delivery: String,
    val position: String,
    val buffer: String,
    val videoHealth: String? = null,
    val sourceFile: String? = null,
    val sourceVideo: String? = null,
    val sourceAudio: String? = null,
    val playingVideo: String? = null,
    val playingAudio: String? = null,
    val dynamicRange: String? = null,
    val subtitles: String = "Off",
    val encoder: String? = null,
    val audioSync: String? = null,
)

/** Floating playback details that preserve the video as their background. */
@Composable
internal fun PlaybackInfoOverlay(
    details: PlaybackInfoDetails,
    reasons: List<String>,
    onDismiss: () -> Unit,
) {
    val shape = MaterialTheme.shapes.large
    val closeFocusRequester = remember { FocusRequester() }
    RequestInitialFocus(closeFocusRequester)
    Box(
        Modifier.fillMaxSize().focusProperties { canFocus = false }.clickable(
            interactionSource = remember { MutableInteractionSource() },
            indication = null,
            onClick = onDismiss,
        ),
    ) {
        Column(
            Modifier
                .align(Alignment.Center)
                .padding(horizontal = 20.dp, vertical = 28.dp)
                .widthIn(max = 680.dp)
                .fillMaxWidth()
                .heightIn(max = 640.dp)
                .clip(shape)
                .background(Color(0xD917181E))
                .border(1.dp, Color.White.copy(alpha = 0.14f), shape)
                .focusProperties { canFocus = false }
                .clickable(
                    interactionSource = remember { MutableInteractionSource() },
                    indication = null,
                    onClick = {},
                )
                .verticalScroll(rememberScrollState())
                .padding(20.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Row(verticalAlignment = Alignment.Top) {
                Column(Modifier.weight(1f)) {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Text(
                            "Playback info",
                            color = Color.White,
                            style = MaterialTheme.typography.titleLarge,
                        )
                        Text(
                            details.position,
                            color = Accent,
                            style = MaterialTheme.typography.labelLarge,
                            fontWeight = FontWeight.SemiBold,
                            modifier = Modifier.padding(start = 16.dp),
                            maxLines = 1,
                        )
                    }
                    Text(
                        details.title,
                        color = Color.White.copy(alpha = 0.72f),
                        style = MaterialTheme.typography.bodyMedium,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
                TvIconButton(
                    onClick = onDismiss,
                    modifier = Modifier.focusRequester(closeFocusRequester),
                ) {
                    Icon(Icons.Filled.Close, contentDescription = "Close playback info", tint = Color.White)
                }
            }

            HorizontalDivider(color = Color.White.copy(alpha = 0.12f))

            PlaybackInfoRow(
                label = "Delivery",
                value = details.delivery,
            )
            PlaybackInfoRow("Buffer", details.buffer)
            details.videoHealth?.let { PlaybackInfoRow("Frames", it) }

            PlaybackInfoSection("SOURCE MEDIA")
            details.sourceFile?.let { PlaybackInfoRow("File", it) }
            details.sourceVideo?.let { PlaybackInfoRow("Video", it) }
            details.sourceAudio?.let { PlaybackInfoRow("Audio", it) }

            PlaybackInfoSection("NOW PLAYING")
            details.playingVideo?.let { PlaybackInfoRow("Video", it) }
            details.dynamicRange?.let { PlaybackInfoRow("Dynamic range", it) }
            details.playingAudio?.let { PlaybackInfoRow("Audio", it) }
            PlaybackInfoRow("Subtitles", details.subtitles)
            details.encoder?.let { PlaybackInfoRow("Encoder", it) }
            details.audioSync?.let { PlaybackInfoRow("Audio sync", it) }
            PlaybackInfoRow("File ID", "#${details.fileId}")

            if (reasons.isNotEmpty()) {
                PlaybackInfoSection("PLAYBACK DECISION")
                reasons.forEach { reason ->
                    Row(
                        Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.spacedBy(10.dp),
                        verticalAlignment = Alignment.Top,
                    ) {
                        Text("•", color = Accent, fontWeight = FontWeight.Bold)
                        Text(
                            reason,
                            color = Color.White.copy(alpha = 0.86f),
                            style = MaterialTheme.typography.bodyMedium,
                            modifier = Modifier.weight(1f),
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun PlaybackInfoSection(title: String) {
    Text(
        title,
        color = Color.White.copy(alpha = 0.58f),
        style = MaterialTheme.typography.labelMedium,
        fontWeight = FontWeight.Bold,
        modifier = Modifier.padding(top = 4.dp),
    )
}

@Composable
private fun PlaybackInfoRow(label: String, value: String) {
    Row(
        Modifier
            .fillMaxWidth()
            .background(Color.White.copy(alpha = 0.07f), MaterialTheme.shapes.medium)
            .padding(horizontal = 14.dp, vertical = 12.dp),
        horizontalArrangement = Arrangement.spacedBy(16.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            label,
            color = Color.White.copy(alpha = 0.62f),
            style = MaterialTheme.typography.labelLarge,
            modifier = Modifier.width(112.dp),
        )
        Text(
            value,
            color = Color.White,
            style = MaterialTheme.typography.bodyMedium,
            fontWeight = FontWeight.SemiBold,
            modifier = Modifier.weight(1f),
        )
    }
}

internal fun deliveryLabel(mode: String): String = when (mode) {
    "direct" -> "Direct play"
    "remux" -> "Direct stream · remux"
    "transcode" -> "Transcode · HLS"
    else -> mode.ifBlank { "Unknown" }.replace('_', ' ').replaceFirstChar { it.uppercase() }
}

internal fun sourceVideoSummary(file: MediaFileDto?): String? {
    if (file == null) return null
    return listOfNotNull(
        file.video_codec?.uppercase(),
        file.video_profile?.takeIf { it.isNotBlank() },
        if (file.width != null && file.height != null) "${file.width}×${file.height}" else file.height?.let { "${it}p" },
        (file.hdr_format ?: file.hdr)?.takeIf { it.isNotBlank() },
        file.bit_depth?.takeIf { it > 0 }?.let { "${it}-bit" },
        file.bitrate?.takeIf { it > 0 }?.let(::formatBitrate),
    ).joinToString(" · ").ifBlank { null }
}

internal fun sourceAudioSummary(file: MediaFileDto?): String? {
    val streams = file?.audio_streams.orEmpty()
    val stream = streams.firstOrNull { it.default } ?: streams.firstOrNull() ?: return null
    val base = listOfNotNull(
        stream.codec?.uppercase(),
        stream.channels?.let(::channelLabel),
        languageName(stream.language),
        stream.title?.takeIf { it.isNotBlank() },
    ).distinct().joinToString(" · ")
    val others = streams.size - 1
    return if (others > 0) "$base · +$others track${if (others == 1) "" else "s"}" else base
}

/**
 * The info panel's "Dynamic range" row — the chip's arrow, in words, with the
 * server's own reason for it where one exists.
 *
 * ```
 * Dolby Vision (rendering)
 * HDR10 — Dolby Vision metadata removed for this device; compatible HDR base kept
 * SDR — tone-mapped from HDR10
 * ```
 */
internal fun dynamicRangeSummary(
    source: String?,
    delivered: String?,
    rendered: String?,
    reasons: List<String>,
): String? {
    if (source == null) return null
    if (delivered == null) return dynamicRangeLabel(source)
    val onScreen = rendered ?: delivered
    if (onScreen == source) return "${dynamicRangeLabel(source)} (rendering)"
    val why = reasons.firstOrNull { reason ->
        reason.contains("dolby vision", ignoreCase = true) || reason.contains("hdr", ignoreCase = true) ||
            reason.contains("tone", ignoreCase = true)
    } ?: "${dynamicRangeLabel(source)} source is not what this session is putting on screen"
    return "${dynamicRangeLabel(onScreen)} — $why"
}

private fun videoFormatSummary(format: Format?): String? {
    if (format == null) return null
    val hdr = when (format.colorInfo?.colorTransfer) {
        C.COLOR_TRANSFER_ST2084 -> "HDR10 / PQ"
        C.COLOR_TRANSFER_HLG -> "HLG"
        else -> null
    }
    return listOfNotNull(
        codecShort(format.sampleMimeType) ?: format.codecs?.takeIf { it.isNotBlank() },
        if (format.width != Format.NO_VALUE && format.height != Format.NO_VALUE) "${format.width}×${format.height}" else null,
        hdr,
        format.bitrate.takeIf { it != Format.NO_VALUE && it > 0 }?.toLong()?.let(::formatBitrate),
    ).joinToString(" · ").ifBlank { null }
}

private fun selectedSubtitleLabel(
    player: Player,
    serverTracks: List<SubTrack>,
    selectedServerTrack: Long?,
): String {
    // The controller's selection is the truth now that one menu row covers
    // both deliveries: a burnt track has no selected text track to read back,
    // and a rendition's own label is the less informative of the two.
    serverTracks.firstOrNull { it.index == selectedServerTrack }?.let { return serverSubtitleLabel(it) }
    player.currentTracks.groups.filter { it.type == C.TRACK_TYPE_TEXT }.forEach { group ->
        repeat(group.length) { index ->
            if (group.isTrackSelected(index)) return subLabel(group.getTrackFormat(index))
        }
    }
    return "Off"
}

private fun channelLabel(channels: Int): String = when (channels) {
    1 -> "Mono"
    2 -> "Stereo"
    6 -> "5.1"
    8 -> "7.1"
    else -> "${channels}ch"
}

private fun formatBitrate(bitsPerSecond: Long): String = if (bitsPerSecond >= 1_000_000) {
    "%.1f Mbps".format(bitsPerSecond / 1_000_000.0)
} else {
    "${bitsPerSecond / 1_000} kbps"
}

private fun videoHealthSummary(player: ExoPlayer): String? {
    val counters = player.videoDecoderCounters ?: return null
    counters.ensureUpdated()
    return videoHealthSummary(
        renderedFrames = counters.renderedOutputBufferCount,
        droppedFrames = counters.droppedBufferCount,
        maxConsecutiveDroppedFrames = counters.maxConsecutiveDroppedBufferCount,
    )
}

internal fun videoHealthSummary(
    renderedFrames: Int,
    droppedFrames: Int,
    maxConsecutiveDroppedFrames: Int,
): String {
    val rendered = renderedFrames.coerceAtLeast(0)
    val dropped = droppedFrames.coerceAtLeast(0)
    val total = rendered.toLong() + dropped
    val dropRate = if (total == 0L) 0.0 else dropped * 100.0 / total
    return String.format(
        Locale.US,
        "%,d rendered · %,d dropped (%.1f%%) · max streak %,d",
        rendered,
        dropped,
        dropRate,
        maxConsecutiveDroppedFrames.coerceAtLeast(0),
    )
}

@Composable
private fun PlayerPanelSurface(title: String, onDismiss: () -> Unit, content: @Composable ColumnScope.() -> Unit) {
    Box(
        Modifier
            .fillMaxSize()
            .background(Color(0x99000000))
            .focusProperties { canFocus = false }
            .clickable(onClick = onDismiss),
    ) {
        Column(
            Modifier.align(Alignment.CenterEnd).fillMaxHeight().widthIn(max = 420.dp).fillMaxWidth(0.92f)
                .background(Surface).verticalScroll(rememberScrollState()).padding(24.dp)
                .focusProperties { canFocus = false }
                .clickable(onClick = {}),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(title, style = MaterialTheme.typography.titleLarge, modifier = Modifier.weight(1f))
                TvIconButton(onClick = onDismiss) {
                    Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Close")
                }
            }
            content()
        }
    }
}

@Composable
private fun PanelRow(
    label: String,
    selected: Boolean,
    modifier: Modifier = Modifier,
    onClick: () -> Unit,
) {
    Text(
        (if (selected) "●  " else "    ") + label,
        color = if (selected) Accent else Color.White,
        fontWeight = if (selected) FontWeight.SemiBold else FontWeight.Normal,
        modifier = modifier
            .fillMaxWidth()
            .tvFocusRing(MaterialTheme.shapes.small, focusedScale = 1.02f)
            .clickable(onClick = onClick)
            .padding(horizontal = 8.dp, vertical = 9.dp),
    )
}

@Composable
private fun PanelSwitch(label: String, checked: Boolean, onCheckedChange: (Boolean) -> Unit) {
    Row(
        Modifier
            .fillMaxWidth()
            .padding(top = 8.dp)
            .tvFocusRing(MaterialTheme.shapes.small, focusedScale = 1.02f)
            .clickable { onCheckedChange(!checked) }
            .padding(horizontal = 8.dp, vertical = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(label, color = Color.White, modifier = Modifier.weight(1f))
        Switch(checked = checked, onCheckedChange = null)
    }
}

private fun offsetLabel(ms: Long): String = if (ms == 0L) {
    "0 ms (no correction)"
} else {
    "${if (ms > 0) "+" else ""}$ms ms — audio plays ${if (ms > 0) "later" else "earlier"}"
}

@Composable
private fun BackChip(onExit: () -> Unit) {
    TvIconButton(onClick = onExit, modifier = Modifier.padding(4.dp)) {
        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back", tint = Color.White)
    }
}
