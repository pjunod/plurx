@file:OptIn(UnstableApi::class)

package tv.plurx.app.player

import android.net.Uri
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.ClosedCaption
import androidx.compose.material.icons.filled.Forward10
import androidx.compose.material.icons.filled.Pause
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Replay10
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Slider
import androidx.compose.material3.SliderDefaults
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.media3.common.Player
import androidx.media3.common.util.UnstableApi
import androidx.media3.ui.AspectRatioFrameLayout
import androidx.media3.ui.PlayerView
import kotlinx.coroutines.delay
import tv.plurx.app.data.Decision
import tv.plurx.app.data.Marker
import tv.plurx.app.data.Net
import tv.plurx.app.data.Session
import tv.plurx.app.ui.AppViewModel
import tv.plurx.app.ui.components.LoadingBox
import tv.plurx.app.ui.components.formatTime
import tv.plurx.app.ui.theme.Accent

/** Everything the player needs, assembled from /items/{id} and /decision. */
private data class Plan(
    val title: String,
    override val durationMs: Long,
    override val playUrl: String,
    override val direct: Boolean,
    val markers: List<Marker>,
) : PlanLike

private suspend fun loadPlan(vm: AppViewModel, itemId: Long, fileId: Long): Plan? = try {
    val detail = vm.itemDetail(itemId)
    val decision: Decision = vm.decision(fileId)
    val file = detail.files.firstOrNull { it.id == fileId } ?: detail.files.firstOrNull()
    Plan(
        title = detail.item.title,
        durationMs = file?.duration_ms ?: detail.item.runtime_ms ?: 0L,
        playUrl = Session.url(decision.play_url),
        direct = decision.method == "direct_play",
        markers = decision.markers,
    )
} catch (_: Exception) {
    null
}

@Composable
fun PlayerScreen(
    vm: AppViewModel,
    itemId: Long,
    fileId: Long,
    startMs: Long,
    onExit: () -> Unit,
) {
    var plan by remember { mutableStateOf<Plan?>(null) }
    var failed by remember { mutableStateOf(false) }
    LaunchedEffect(itemId, fileId) {
        val p = loadPlan(vm, itemId, fileId)
        if (p == null) failed = true else plan = p
    }

    Box(Modifier.fillMaxSize().background(Color.Black)) {
        when {
            failed -> Column(Modifier.fillMaxSize(), verticalArrangement = Arrangement.Center, horizontalAlignment = Alignment.CenterHorizontally) {
                Text("Couldn't start playback.", color = Color.White)
                Spacer(Modifier.size(12.dp))
                Button(onClick = onExit) { Text("Back") }
            }
            plan == null -> {
                LoadingBox()
                BackChip(onExit)
            }
            else -> PlayerContent(vm, itemId, plan!!, startMs, onExit)
        }
    }
}

@Composable
private fun PlayerContent(
    vm: AppViewModel,
    itemId: Long,
    plan: Plan,
    startMs: Long,
    onExit: () -> Unit,
) {
    val context = androidx.compose.ui.platform.LocalContext.current
    val controller = remember(plan) { Controller(buildPlayer(context, vm), plan, vm.caps()) }

    var positionMs by remember { mutableLongStateOf(startMs) }
    var scrubbing by remember { mutableStateOf(false) }
    var scrubPreview by remember { mutableLongStateOf(startMs) }
    var isPlaying by remember { mutableStateOf(true) }
    var buffering by remember { mutableStateOf(true) }
    var controlsVisible by remember { mutableStateOf(true) }
    var showTracks by remember { mutableStateOf(false) }
    var lastInteraction by remember { mutableLongStateOf(0L) }

    fun poke() { controlsVisible = true; lastInteraction += 1 }

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

    // Poll the real position for the scrubber.
    LaunchedEffect(controller) {
        while (true) {
            if (!scrubbing) positionMs = controller.realPosition()
            delay(500)
        }
    }
    // Periodic progress → server watch state + Trakt scrobble.
    LaunchedEffect(controller) {
        while (true) {
            delay(10_000)
            if (isPlaying) vm.reportProgress(itemId, controller.realPosition(), plan.durationMs)
        }
    }
    // Auto-hide controls a few seconds after the last interaction, while playing.
    LaunchedEffect(lastInteraction, isPlaying) {
        if (isPlaying && !showTracks) {
            delay(3800)
            controlsVisible = false
        }
    }

    Box(Modifier.fillMaxSize()) {
        AndroidView(
            factory = {
                PlayerView(it).apply {
                    player = controller.player
                    useController = false
                    resizeMode = AspectRatioFrameLayout.RESIZE_MODE_FIT
                    setShutterBackgroundColor(android.graphics.Color.BLACK)
                }
            },
            modifier = Modifier.fillMaxSize(),
        )

        // Tap anywhere to toggle the controls.
        Box(
            Modifier
                .fillMaxSize()
                .clickable(interactionSource = remember { MutableInteractionSource() }, indication = null) {
                    if (controlsVisible) controlsVisible = false else poke()
                }
        )

        if (buffering) CircularProgressIndicator(Modifier.align(Alignment.Center), color = Accent)

        // Skip Intro / Skip Credits.
        val activeMarker = plan.markers.firstOrNull { positionMs in it.start_ms until it.end_ms }
        if (activeMarker != null && !scrubbing) {
            Button(
                onClick = { controller.seekTo(activeMarker.end_ms); poke() },
                modifier = Modifier.align(Alignment.BottomEnd).padding(end = 24.dp, bottom = 96.dp),
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
                onTracks = { showTracks = true },
            )
        }

        if (showTracks) {
            TrackMenu(controller.player, onDismiss = { showTracks = false; poke() })
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
) {
    Box(Modifier.fillMaxSize().background(Color(0x66000000))) {
        Row(
            Modifier.align(Alignment.TopStart).fillMaxWidth().padding(8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            IconButton(onClick = onBack) {
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back", tint = Color.White)
            }
            Text(title, color = Color.White, style = MaterialTheme.typography.titleMedium, maxLines = 1)
            Box(Modifier.weight(1f))
            IconButton(onClick = onTracks) {
                Icon(Icons.Filled.ClosedCaption, contentDescription = "Audio & subtitles", tint = Color.White)
            }
        }

        Row(
            Modifier.align(Alignment.Center),
            horizontalArrangement = Arrangement.spacedBy(28.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            IconButton(onClick = onSeekBack, modifier = Modifier.size(52.dp)) {
                Icon(Icons.Filled.Replay10, contentDescription = "Back 10s", tint = Color.White, modifier = Modifier.size(40.dp))
            }
            IconButton(onClick = onPlayPause, modifier = Modifier.size(72.dp)) {
                Icon(
                    if (isPlaying) Icons.Filled.Pause else Icons.Filled.PlayArrow,
                    contentDescription = if (isPlaying) "Pause" else "Play",
                    tint = Color.White,
                    modifier = Modifier.size(60.dp),
                )
            }
            IconButton(onClick = onSeekForward, modifier = Modifier.size(52.dp)) {
                Icon(Icons.Filled.Forward10, contentDescription = "Forward 10s", tint = Color.White, modifier = Modifier.size(40.dp))
            }
        }

        Column(Modifier.align(Alignment.BottomCenter).fillMaxWidth().padding(horizontal = 20.dp, vertical = 16.dp)) {
            val range = if (durationMs > 0) durationMs.toFloat() else 1f
            Slider(
                value = positionMs.coerceIn(0, durationMs).toFloat(),
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
private fun BackChip(onExit: () -> Unit) {
    IconButton(onClick = onExit, modifier = Modifier.padding(4.dp)) {
        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back", tint = Color.White)
    }
}
