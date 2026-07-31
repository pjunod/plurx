@file:androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)

package tv.plurx.app.player

import android.view.KeyEvent
import android.app.PictureInPictureParams
import android.content.pm.PackageManager
import android.os.Build
import android.util.Rational
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.focusable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.ClosedCaption
import androidx.compose.material.icons.filled.Forward10
import androidx.compose.material.icons.filled.Info
import androidx.compose.material.icons.filled.Pause
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.PictureInPictureAlt
import androidx.compose.material.icons.filled.Replay10
import androidx.compose.material.icons.filled.Tune
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
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
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.key.onPreviewKeyEvent
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.media3.common.Player
import androidx.media3.common.util.UnstableApi
import androidx.media3.ui.AspectRatioFrameLayout
import androidx.media3.ui.PlayerView
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import tv.plurx.app.data.AudioTrack
import tv.plurx.app.data.Decision
import tv.plurx.app.data.Marker
import tv.plurx.app.data.PlaybackQuality
import tv.plurx.app.data.Session
import tv.plurx.app.data.SubTrack
import tv.plurx.app.ui.AppViewModel
import tv.plurx.app.ui.PlaybackTarget
import tv.plurx.app.ui.components.LoadingBox
import tv.plurx.app.ui.components.formatTime
import tv.plurx.app.ui.theme.Accent
import tv.plurx.app.ui.theme.Muted
import tv.plurx.app.ui.theme.Surface

private data class Plan(
    val title: String,
    override val durationMs: Long,
    override val fileId: Long,
    override val playUrl: String,
    override val mode: String,
    val markers: List<Marker>,
    val reasons: List<String>,
    override val audio: List<AudioTrack>,
    override val subtitles: List<SubTrack>,
    val audioOffsetMs: Long,
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
        durationMs = file?.duration_ms ?: detail.item.runtime_ms ?: 0L,
        fileId = fileId,
        playUrl = Session.url(decision.delivery?.url ?: decision.play_url),
        mode = mode,
        markers = decision.markers,
        reasons = decision.reasons,
        audio = decision.audio,
        subtitles = decision.subtitles,
        audioOffsetMs = decision.audio_offset_ms,
        declaredOffsetMs = decision.declared_offset_ms,
    )
} catch (_: Exception) {
    null
}

private enum class PlayerPanel { Tracks, Settings, Info }

@Composable
fun PlayerScreen(
    vm: AppViewModel,
    itemId: Long,
    fileId: Long,
    startMs: Long,
    onPlayNext: (PlaybackTarget) -> Unit,
    onExit: () -> Unit,
) {
    val activity = androidx.activity.compose.LocalActivity.current
    var plan by remember(itemId, fileId) { mutableStateOf<Plan?>(null) }
    var failed by remember(itemId, fileId) { mutableStateOf(false) }
    var generation by remember(itemId, fileId) { mutableIntStateOf(0) }
    var resumeAt by remember(itemId, fileId) { mutableLongStateOf(startMs) }

    DisposableEffect(activity) {
        val window = activity?.window
        if (window != null) {
            WindowCompat.setDecorFitsSystemWindows(window, false)
            WindowCompat.getInsetsController(window, window.decorView).apply {
                hide(WindowInsetsCompat.Type.systemBars())
                systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            }
        }
        onDispose {
            if (window != null) {
                WindowCompat.getInsetsController(window, window.decorView)
                    .show(WindowInsetsCompat.Type.systemBars())
                WindowCompat.setDecorFitsSystemWindows(window, true)
            }
        }
    }

    LaunchedEffect(itemId, fileId, generation) {
        failed = false
        val loaded = loadPlan(vm, itemId, fileId)
        if (loaded == null) failed = true else plan = loaded
    }

    Box(Modifier.fillMaxSize().background(Color.Black)) {
        when {
            failed -> PlaybackFailed(onExit)
            plan == null -> {
                LoadingBox()
                BackChip(onExit)
            }
            else -> PlayerContent(
                vm = vm,
                itemId = itemId,
                plan = plan!!,
                startMs = resumeAt,
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
private fun PlaybackFailed(onExit: () -> Unit) {
    Column(
        Modifier.fillMaxSize(),
        verticalArrangement = Arrangement.Center,
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text("Couldn't start playback.", color = Color.White)
        Spacer(Modifier.size(12.dp))
        Button(onClick = onExit) { Text("Back") }
    }
}

@Composable
private fun PlayerContent(
    vm: AppViewModel,
    itemId: Long,
    plan: Plan,
    startMs: Long,
    onReload: (Long) -> Unit,
    onPlayNext: (PlaybackTarget) -> Unit,
    onExit: () -> Unit,
) {
    val context = androidx.compose.ui.platform.LocalContext.current
    val activity = androidx.activity.compose.LocalActivity.current
    val canUsePip = Build.VERSION.SDK_INT >= Build.VERSION_CODES.O &&
        context.packageManager.hasSystemFeature(PackageManager.FEATURE_PICTURE_IN_PICTURE)
    val scope = rememberCoroutineScope()
    val preferences by vm.preferences.collectAsStateWithLifecycle()
    var playFailed by remember { mutableStateOf(false) }
    val controller = remember(plan) {
        Controller(context, buildPlayer(context, vm), plan, vm.caps(), vm, scope, onError = { playFailed = true })
    }
    val focusRequester = remember { FocusRequester() }

    var positionMs by remember { mutableLongStateOf(startMs) }
    var scrubbing by remember { mutableStateOf(false) }
    var scrubPreview by remember { mutableLongStateOf(startMs) }
    var isPlaying by remember { mutableStateOf(true) }
    var buffering by remember { mutableStateOf(true) }
    var controlsVisible by remember { mutableStateOf(true) }
    var panel by remember { mutableStateOf<PlayerPanel?>(null) }
    var lastInteraction by remember { mutableLongStateOf(0L) }
    var lastAutoSkipped by remember(plan) { mutableLongStateOf(-1L) }
    var findingNext by remember { mutableStateOf(false) }

    fun poke() {
        controlsVisible = true
        lastInteraction += 1
    }

    BackHandler {
        if (panel != null) panel = null else onExit()
    }

    DisposableEffect(controller, preferences.autoplayNext) {
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
                    if (preferences.autoplayNext && !findingNext) {
                        findingNext = true
                        scope.launch {
                            val next = runCatching { vm.nextEpisode(itemId) }.getOrNull()
                            findingNext = false
                            if (next != null) onPlayNext(next)
                        }
                    }
                }
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

    LaunchedEffect(controller) {
        while (true) {
            if (!scrubbing) positionMs = controller.realPosition()
            delay(500)
        }
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

    LaunchedEffect(Unit) { focusRequester.requestFocus() }

    if (playFailed) {
        PlaybackFailed(onExit)
        return
    }

    Box(
        Modifier.fillMaxSize()
            .focusRequester(focusRequester)
            .focusable()
            .onPreviewKeyEvent { event ->
                if (event.nativeKeyEvent.action != KeyEvent.ACTION_DOWN) return@onPreviewKeyEvent false
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
                            false
                        }
                    }
                    KeyEvent.KEYCODE_MEDIA_REWIND -> {
                        controller.seekTo(controller.realPosition() - 10_000); poke(); true
                    }
                    KeyEvent.KEYCODE_MEDIA_FAST_FORWARD -> {
                        controller.seekTo(controller.realPosition() + 10_000); poke(); true
                    }
                    KeyEvent.KEYCODE_DPAD_LEFT -> if (!controlsVisible) {
                        controller.seekTo(controller.realPosition() - 10_000); poke(); true
                    } else false
                    KeyEvent.KEYCODE_DPAD_RIGHT -> if (!controlsVisible) {
                        controller.seekTo(controller.realPosition() + 10_000); poke(); true
                    } else false
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
                }
            },
            modifier = Modifier.fillMaxSize(),
        )

        Box(
            Modifier.fillMaxSize().clickable(
                interactionSource = remember { MutableInteractionSource() },
                indication = null,
            ) { if (controlsVisible) controlsVisible = false else poke() },
        )

        if (buffering || findingNext) {
            Column(Modifier.align(Alignment.Center), horizontalAlignment = Alignment.CenterHorizontally) {
                CircularProgressIndicator(color = Accent)
                if (findingNext) Text("Up next…", color = Color.White, modifier = Modifier.padding(top = 12.dp))
            }
        }

        if (activeMarker != null && !scrubbing && !preferences.autoSkip) {
            Button(
                onClick = { controller.seekTo(activeMarker.end_ms); poke() },
                modifier = Modifier.align(Alignment.BottomEnd).padding(end = 28.dp, bottom = 112.dp),
            ) { Text(activeMarker.label, fontWeight = FontWeight.SemiBold) }
        }

        if (controlsVisible) {
            Controls(
                title = plan.title,
                positionMs = if (scrubbing) scrubPreview else positionMs,
                durationMs = plan.durationMs,
                isPlaying = isPlaying,
                onBack = onExit,
                onPlayPause = { controller.playPause(); poke() },
                onSeekBack = { controller.seekTo(controller.realPosition() - 10_000); poke() },
                onSeekForward = { controller.seekTo(controller.realPosition() + 10_000); poke() },
                onScrubStart = { scrubbing = true; scrubPreview = positionMs },
                onScrub = { scrubPreview = it },
                onScrubEnd = { controller.seekTo(scrubPreview); scrubbing = false; poke() },
                onTracks = { panel = PlayerPanel.Tracks },
                onSettings = { panel = PlayerPanel.Settings },
                onInfo = { panel = PlayerPanel.Info },
                onPip = if (canUsePip && activity != null) {
                    {
                        controlsVisible = false
                        activity.enterPictureInPictureMode(
                            PictureInPictureParams.Builder().setAspectRatio(Rational(16, 9)).build(),
                        )
                    }
                } else null,
            )
        }

        when (panel) {
            PlayerPanel.Tracks -> TrackMenu(
                player = controller.player,
                serverAudio = plan.audio,
                serverSubtitles = plan.subtitles,
                serverControlledAudio = plan.mode != "direct",
                selectedServerAudio = controller.selectedAudio,
                selectedServerSubtitle = controller.selectedSubtitle,
                onServerAudio = { controller.switchAudio(it); panel = null; poke() },
                onServerSubtitle = { controller.switchSubtitle(it); panel = null; poke() },
                onDismiss = { panel = null; poke() },
            )
            PlayerPanel.Settings -> PlayerSettings(
                vm = vm,
                fileId = plan.fileId,
                audioOffsetMs = plan.audioOffsetMs,
                declaredOffsetMs = plan.declaredOffsetMs,
                currentPosition = controller::realPosition,
                onReload = onReload,
                onDismiss = { panel = null; poke() },
            )
            PlayerPanel.Info -> PlayerInfo(
                plan = plan,
                deliveryMode = controller.deliveryMode,
                bufferedPercent = controller.player.bufferedPercentage,
                onDismiss = { panel = null; poke() },
            )
            null -> Unit
        }
    }
}

@Composable
private fun Controls(
    title: String,
    positionMs: Long,
    durationMs: Long,
    isPlaying: Boolean,
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
    Box(Modifier.fillMaxSize().background(Color(0x66000000))) {
        Row(
            Modifier.align(Alignment.TopStart).fillMaxWidth().padding(12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            IconButton(onClick = onBack) {
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back", tint = Color.White)
            }
            Text(
                title,
                color = Color.White,
                style = MaterialTheme.typography.titleMedium,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f),
            )
            IconButton(onClick = onTracks) {
                Icon(Icons.Filled.ClosedCaption, contentDescription = "Audio and subtitles", tint = Color.White)
            }
            IconButton(onClick = onSettings) {
                Icon(Icons.Filled.Tune, contentDescription = "Playback settings", tint = Color.White)
            }
            IconButton(onClick = onInfo) {
                Icon(Icons.Filled.Info, contentDescription = "Playback info", tint = Color.White)
            }
            if (onPip != null) {
                IconButton(onClick = onPip) {
                    Icon(Icons.Filled.PictureInPictureAlt, contentDescription = "Picture in picture", tint = Color.White)
                }
            }
        }

        Row(
            Modifier.align(Alignment.Center),
            horizontalArrangement = Arrangement.spacedBy(28.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            IconButton(onClick = onSeekBack, modifier = Modifier.size(56.dp)) {
                Icon(Icons.Filled.Replay10, contentDescription = "Back 10 seconds", tint = Color.White, modifier = Modifier.size(42.dp))
            }
            IconButton(onClick = onPlayPause, modifier = Modifier.size(76.dp)) {
                Icon(
                    if (isPlaying) Icons.Filled.Pause else Icons.Filled.PlayArrow,
                    contentDescription = if (isPlaying) "Pause" else "Play",
                    tint = Color.White,
                    modifier = Modifier.size(62.dp),
                )
            }
            IconButton(onClick = onSeekForward, modifier = Modifier.size(56.dp)) {
                Icon(Icons.Filled.Forward10, contentDescription = "Forward 10 seconds", tint = Color.White, modifier = Modifier.size(42.dp))
            }
        }

        Column(Modifier.align(Alignment.BottomCenter).fillMaxWidth().padding(horizontal = 28.dp, vertical = 22.dp)) {
            val range = if (durationMs > 0) durationMs.toFloat() else 1f
            Slider(
                value = positionMs.coerceIn(0, durationMs.coerceAtLeast(0)).toFloat(),
                onValueChange = { onScrubStart(); onScrub(it.toLong()) },
                onValueChangeFinished = onScrubEnd,
                valueRange = 0f..range,
                colors = SliderDefaults.colors(
                    thumbColor = Accent,
                    activeTrackColor = Accent,
                    inactiveTrackColor = Color(0x55FFFFFF),
                ),
            )
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                Text(formatTime(positionMs), color = Color.White, style = MaterialTheme.typography.labelMedium)
                Text(formatTime(durationMs), color = Color.White, style = MaterialTheme.typography.labelMedium)
            }
        }
    }
}

@Composable
private fun PlayerSettings(
    vm: AppViewModel,
    fileId: Long,
    audioOffsetMs: Long,
    declaredOffsetMs: Long?,
    currentPosition: () -> Long,
    onReload: (Long) -> Unit,
    onDismiss: () -> Unit,
) {
    val preferences by vm.preferences.collectAsStateWithLifecycle()
    val scope = rememberCoroutineScope()
    var offset by remember(audioOffsetMs) { mutableLongStateOf(audioOffsetMs) }
    PlayerPanelSurface("Playback settings", onDismiss) {
        Text("Quality", color = Muted, style = MaterialTheme.typography.labelMedium)
        PlaybackQuality.entries.forEach { quality ->
            PanelRow(quality.label, preferences.playbackQuality == quality) {
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
                val position = currentPosition()
                scope.launch {
                    offset = runCatching { vm.setAudioOffset(fileId, offset + delta) }.getOrDefault(offset)
                    onReload(position)
                }
            }
        }
        if (offset != 0L) {
            PanelRow("Reset sync to 0 ms", false) {
                val position = currentPosition()
                scope.launch {
                    offset = runCatching { vm.setAudioOffset(fileId, 0) }.getOrDefault(offset)
                    onReload(position)
                }
            }
        }
        PanelSwitch("Auto-skip intro and credits", preferences.autoSkip, vm::setAutoSkip)
        PanelSwitch("Autoplay next episode", preferences.autoplayNext, vm::setAutoplayNext)
    }
}

@Composable
private fun PlayerInfo(plan: Plan, deliveryMode: String, bufferedPercent: Int, onDismiss: () -> Unit) {
    PlayerPanelSurface("Playback info", onDismiss) {
        Text("Delivery · ${deliveryMode.replaceFirstChar { it.uppercase() }}", color = Color.White)
        Text("Buffered · $bufferedPercent%", color = Color.White)
        Text("Duration · ${formatTime(plan.durationMs)}", color = Color.White)
        if (plan.reasons.isNotEmpty()) {
            Text("Decision", color = Muted, style = MaterialTheme.typography.labelMedium, modifier = Modifier.padding(top = 12.dp))
            plan.reasons.forEach { Text("• $it", color = Color.White, style = MaterialTheme.typography.bodyMedium) }
        }
    }
}

@Composable
private fun PlayerPanelSurface(title: String, onDismiss: () -> Unit, content: @Composable ColumnScope.() -> Unit) {
    Box(Modifier.fillMaxSize().background(Color(0x99000000)).clickable(onClick = onDismiss)) {
        Column(
            Modifier.align(Alignment.CenterEnd).fillMaxHeight().widthIn(max = 420.dp).fillMaxWidth(0.92f)
                .background(Surface).verticalScroll(rememberScrollState()).padding(24.dp)
                .clickable(onClick = {}),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(title, style = MaterialTheme.typography.titleLarge, modifier = Modifier.weight(1f))
                IconButton(onClick = onDismiss) {
                    Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Close")
                }
            }
            content()
        }
    }
}

@Composable
private fun PanelRow(label: String, selected: Boolean, onClick: () -> Unit) {
    Text(
        (if (selected) "●  " else "    ") + label,
        color = if (selected) Accent else Color.White,
        fontWeight = if (selected) FontWeight.SemiBold else FontWeight.Normal,
        modifier = Modifier.fillMaxWidth().clickable(onClick = onClick).padding(vertical = 9.dp),
    )
}

@Composable
private fun PanelSwitch(label: String, checked: Boolean, onCheckedChange: (Boolean) -> Unit) {
    Row(Modifier.fillMaxWidth().padding(top = 8.dp), verticalAlignment = Alignment.CenterVertically) {
        Text(label, color = Color.White, modifier = Modifier.weight(1f))
        Switch(checked = checked, onCheckedChange = onCheckedChange)
    }
}

private fun offsetLabel(ms: Long): String = if (ms == 0L) {
    "0 ms (no correction)"
} else {
    "${if (ms > 0) "+" else ""}$ms ms — audio plays ${if (ms > 0) "later" else "earlier"}"
}

@Composable
private fun BackChip(onExit: () -> Unit) {
    IconButton(onClick = onExit, modifier = Modifier.padding(4.dp)) {
        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back", tint = Color.White)
    }
}
