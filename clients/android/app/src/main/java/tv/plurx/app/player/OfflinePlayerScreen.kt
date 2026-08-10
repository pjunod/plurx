@file:androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)

package tv.plurx.app.player

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.media3.common.PlaybackException
import androidx.media3.common.Player
import androidx.media3.ui.PlayerView
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import tv.plurx.app.data.offline.OfflineDownloads
import tv.plurx.app.ui.components.TvButton

/** Plays only Media3 cache bytes. PlaceholderDataSource throws on every miss. */
@Composable
fun OfflinePlayerScreen(downloadId: String, onExit: () -> Unit) {
    val records by OfflineDownloads.records.collectAsStateWithLifecycle()
    val record = records.firstOrNull { it.id == downloadId }
    val context = androidx.compose.ui.platform.LocalContext.current
    var failure by remember(downloadId) { mutableStateOf<String?>(null) }
    val player = remember(downloadId) { OfflineDownloads.cacheOnlyPlayer(context) }

    BackHandler(onBack = onExit)

    LaunchedEffect(downloadId) {
        val request = runCatching { OfflineDownloads.completedDownloadRequest(downloadId) }.getOrNull()
        if (request == null) {
            failure = "Download incomplete"
            return@LaunchedEffect
        }
        player.setMediaItem(request.toMediaItem(), record?.positionMs ?: 0)
        player.prepare()
        player.playWhenReady = true
    }

    DisposableEffect(player, record?.id) {
        val listener = object : Player.Listener {
            override fun onPlayerError(error: PlaybackException) {
                failure = "Download incomplete"
            }
        }
        player.addListener(listener)
        onDispose {
            val position = player.currentPosition
            val duration = player.duration.takeIf { it > 0 }
            CoroutineScope(Dispatchers.IO).launch {
                OfflineDownloads.recordProgress(downloadId, position, duration)
            }
            player.removeListener(listener)
            player.release()
        }
    }

    LaunchedEffect(player, record?.id) {
        while (true) {
            delay(10_000)
            OfflineDownloads.recordProgress(
                downloadId,
                player.currentPosition,
                player.duration.takeIf { it > 0 },
            )
        }
    }

    Box(Modifier.fillMaxSize().background(Color.Black)) {
        AndroidView(
            factory = { PlayerView(it).apply { useController = true; this.player = player } },
            modifier = Modifier.fillMaxSize(),
        )
        failure?.let { message ->
            Box(Modifier.fillMaxSize().background(Color.Black), contentAlignment = Alignment.Center) {
                androidx.compose.foundation.layout.Column(horizontalAlignment = Alignment.CenterHorizontally) {
                    Text(message, color = Color.White)
                    TvButton(onClick = onExit, modifier = Modifier.padding(top = 12.dp)) { Text("Back") }
                }
            }
        }
    }
}
