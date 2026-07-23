package tv.plurx.app.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material3.Button
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.produceState
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import tv.plurx.app.data.ItemDetail
import tv.plurx.app.ui.components.LoadingBox
import tv.plurx.app.ui.components.NetworkImage
import tv.plurx.app.ui.components.PosterCard
import tv.plurx.app.ui.components.formatTime
import tv.plurx.app.ui.components.imageUrl
import tv.plurx.app.ui.theme.Bg
import tv.plurx.app.ui.theme.Muted

@Composable
fun DetailScreen(
    vm: AppViewModel,
    itemId: Long,
    onPlay: (itemId: Long, fileId: Long, startMs: Long) -> Unit,
    onOpenItem: (Long) -> Unit,
    onBack: () -> Unit,
) {
    val detail by produceState<ItemDetail?>(initialValue = null, itemId) {
        value = try {
            vm.itemDetail(itemId)
        } catch (_: Exception) {
            null
        }
    }

    when (val d = detail) {
        null -> Box(Modifier.fillMaxSize()) {
            LoadingBox()
            IconButton(onClick = onBack, modifier = Modifier.padding(4.dp)) {
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
            }
        }
        else -> DetailContent(d, onPlay, onOpenItem, onBack)
    }
}

@Composable
private fun DetailContent(
    d: ItemDetail,
    onPlay: (Long, Long, Long) -> Unit,
    onOpenItem: (Long) -> Unit,
    onBack: () -> Unit,
) {
    val item = d.item
    val file = d.files.firstOrNull()
    val durationMs = file?.duration_ms ?: item.runtime_ms
    val resumeMs = item.watch?.position_ms ?: 0L
    val nearlyDone = durationMs != null && durationMs > 0 && resumeMs > durationMs * 0.95
    val canResume = resumeMs > 3_000 && !nearlyDone

    LazyColumn(Modifier.fillMaxSize()) {
        item {
            Box(Modifier.fillMaxWidth().height(230.dp)) {
                NetworkImage(imageUrl(item.backdrop ?: item.poster), Modifier.fillMaxSize())
                Box(
                    Modifier.fillMaxSize().background(
                        Brush.verticalGradient(listOf(Color.Transparent, Bg))
                    )
                )
                IconButton(onClick = onBack, modifier = Modifier.padding(4.dp)) {
                    Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back", tint = Color.White)
                }
            }
        }

        item {
            Column(Modifier.padding(horizontal = 20.dp)) {
                Text(item.title, style = MaterialTheme.typography.headlineMedium)
                Text(metaLine(item, durationMs), color = Muted, style = MaterialTheme.typography.labelMedium, modifier = Modifier.padding(top = 4.dp))

                if (file != null && item.isMovieOrEpisode) {
                    Row(
                        Modifier.padding(top = 16.dp),
                        horizontalArrangement = Arrangement.spacedBy(12.dp),
                    ) {
                        Button(onClick = { onPlay(item.id, file.id, if (canResume) resumeMs else 0L) }) {
                            Icon(Icons.Filled.PlayArrow, contentDescription = null, modifier = Modifier.size(20.dp))
                            Text(
                                if (canResume) "  Resume · ${formatTime(resumeMs)}" else "  Play",
                                fontWeight = FontWeight.SemiBold,
                            )
                        }
                        if (canResume) {
                            OutlinedButton(onClick = { onPlay(item.id, file.id, 0L) }) {
                                Icon(Icons.Filled.Refresh, contentDescription = null, modifier = Modifier.size(18.dp))
                                Text("  Start over")
                            }
                        }
                    }
                }

                item.overview?.takeIf { it.isNotBlank() }?.let {
                    Text(
                        it,
                        color = Muted,
                        style = MaterialTheme.typography.bodyMedium,
                        modifier = Modifier.padding(top = 16.dp),
                    )
                }
            }
        }

        if (d.children.isNotEmpty()) {
            item {
                Text(
                    childrenHeading(item.kind),
                    style = MaterialTheme.typography.titleMedium,
                    modifier = Modifier.padding(start = 20.dp, top = 24.dp, bottom = 10.dp),
                )
                LazyRow(
                    contentPadding = androidx.compose.foundation.layout.PaddingValues(horizontal = 20.dp),
                    horizontalArrangement = Arrangement.spacedBy(14.dp),
                ) {
                    items(d.children, key = { it.id }) { child ->
                        PosterCard(child) { onOpenItem(child.id) }
                    }
                }
            }
        }
        item { Box(Modifier.height(24.dp)) }
    }
}

private fun metaLine(item: tv.plurx.app.data.Item, durationMs: Long?): String {
    val parts = mutableListOf<String>()
    if (item.kind == "episode") {
        item.show_title?.let { parts.add(it) }
        if (item.season_number != null && item.episode_number != null) {
            parts.add("S${item.season_number} · E${item.episode_number}")
        }
    }
    item.year?.let { parts.add(it.toString()) }
    durationMs?.takeIf { it > 0 }?.let { parts.add(formatTime(it)) }
    return parts.joinToString("  ·  ")
}

private fun childrenHeading(kind: String): String = when (kind) {
    "show" -> "Seasons"
    "season" -> "Episodes"
    else -> "Contents"
}
