package tv.plurx.app.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBars
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.CheckCircle
import androidx.compose.material.icons.filled.Image
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import tv.plurx.app.data.Item
import tv.plurx.app.data.ItemDetail
import tv.plurx.app.data.MediaFileDto
import tv.plurx.app.ui.components.LoadingBox
import tv.plurx.app.ui.components.NetworkImage
import tv.plurx.app.ui.components.PosterCard
import tv.plurx.app.ui.components.SafeBackButton
import tv.plurx.app.ui.components.TvButton
import tv.plurx.app.ui.components.TvOutlinedButton
import tv.plurx.app.ui.components.TvTextButton
import tv.plurx.app.ui.components.formatTime
import tv.plurx.app.ui.components.imageUrl
import tv.plurx.app.ui.components.tvFocusRing
import tv.plurx.app.ui.theme.Accent
import tv.plurx.app.ui.theme.Bg
import tv.plurx.app.ui.theme.Muted
import tv.plurx.app.ui.theme.Outline
import tv.plurx.app.ui.theme.SurfaceHi

private data class DetailLoad(
    val detail: ItemDetail? = null,
    val seriesPlayback: EpisodePlaybackTarget? = null,
    val error: String? = null,
)

@Composable
fun DetailScreen(
    vm: AppViewModel,
    itemId: Long,
    onPlay: (itemId: Long, fileId: Long, startMs: Long) -> Unit,
    onOpenItem: (Long) -> Unit,
    onViewPhoto: (Long) -> Unit,
    onBack: () -> Unit,
) {
    var refresh by remember(itemId) { mutableIntStateOf(0) }
    val load by produceState<DetailLoad?>(initialValue = null, itemId, refresh) {
        value = try {
            val detail = vm.itemDetail(itemId)
            DetailLoad(
                detail = detail,
                seriesPlayback = runCatching { vm.seriesPlayback(detail) }.getOrNull(),
            )
        } catch (e: Exception) {
            DetailLoad(error = e.message ?: "Couldn't load this item")
        }
    }

    when {
        load == null -> Box(Modifier.fillMaxSize()) {
            LoadingBox()
            DetailBackButton(onBack)
        }
        load?.error != null -> Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
            DetailBackButton(onBack)
            Text(load?.error.orEmpty(), color = MaterialTheme.colorScheme.error)
        }
        else -> DetailContent(
            vm = vm,
            detail = load!!.detail!!,
            seriesPlayback = load!!.seriesPlayback,
            onPlay = onPlay,
            onOpenItem = onOpenItem,
            onViewPhoto = onViewPhoto,
            onWatchedChanged = { refresh++ },
            onBack = onBack,
        )
    }
}

@Composable
private fun DetailContent(
    vm: AppViewModel,
    detail: ItemDetail,
    seriesPlayback: EpisodePlaybackTarget?,
    onPlay: (Long, Long, Long) -> Unit,
    onOpenItem: (Long) -> Unit,
    onViewPhoto: (Long) -> Unit,
    onWatchedChanged: () -> Unit,
    onBack: () -> Unit,
) {
    val formFactor = currentFormFactor()
    val side = formFactor.horizontalPadding()
    val item = detail.item
    val scope = rememberCoroutineScope()
    var startingEpisodeId by remember(item.id) { mutableStateOf<Long?>(null) }
    val best = detail.files.firstOrNull()
    val durationMs = best?.duration_ms ?: item.runtime_ms
    val resumeMs = item.watch?.position_ms ?: 0L
    val nearlyDone = durationMs != null && durationMs > 0 && resumeMs > durationMs * 0.95
    val canResume = resumeMs > 3_000 && !nearlyDone
    val heroHeight = when (formFactor) {
        FormFactor.Compact -> 230.dp
        FormFactor.Expanded -> 300.dp
        FormFactor.Television -> 360.dp
    }

    LazyColumn(Modifier.fillMaxSize().navigationBarsPadding()) {
        item {
            Box(Modifier.fillMaxWidth().height(heroHeight)) {
                NetworkImage(imageUrl(item.backdrop ?: item.poster), Modifier.fillMaxSize())
                Box(Modifier.fillMaxSize().background(Brush.verticalGradient(listOf(Color(0x22000000), Bg))))
                DetailBackButton(onBack)
            }
        }

        item {
            Row(
                Modifier.fillMaxWidth().padding(horizontal = side),
                horizontalArrangement = Arrangement.spacedBy(if (formFactor == FormFactor.Compact) 0.dp else 28.dp),
                verticalAlignment = Alignment.Top,
            ) {
                if (formFactor != FormFactor.Compact && item.poster != null) {
                    NetworkImage(
                        imageUrl(item.poster),
                        Modifier.width(if (formFactor == FormFactor.Television) 220.dp else 170.dp)
                            .aspectRatio(2f / 3f).clip(MaterialTheme.shapes.large),
                    )
                }
                Column(Modifier.weight(1f)) {
                    if (detail.ancestors.isNotEmpty()) {
                        Text(
                            detail.ancestors.joinToString("  /  ") { it.title },
                            color = Accent,
                            style = MaterialTheme.typography.labelMedium,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                    Text(item.title, style = MaterialTheme.typography.headlineMedium)
                    Text(
                        metaLine(item, durationMs),
                        color = Muted,
                        style = MaterialTheme.typography.labelMedium,
                        modifier = Modifier.padding(top = 4.dp),
                    )
                    if (item.tags.isNotEmpty()) {
                        LazyRow(
                            Modifier.padding(top = 10.dp),
                            horizontalArrangement = Arrangement.spacedBy(8.dp),
                        ) {
                            items(item.tags) { tag -> SpecChip(tag) }
                        }
                    }

                    Actions(
                        vm = vm,
                        item = item,
                        files = detail.files,
                        seriesPlayback = seriesPlayback,
                        resumeMs = resumeMs,
                        canResume = canResume,
                        onPlay = onPlay,
                        onViewPhoto = onViewPhoto,
                        onWatchedChanged = onWatchedChanged,
                    )

                    item.overview?.takeIf { it.isNotBlank() }?.let {
                        Text(it, color = Muted, style = MaterialTheme.typography.bodyMedium, modifier = Modifier.padding(top = 16.dp))
                    }
                }
            }
        }

        if (detail.files.isNotEmpty()) {
            item {
                Column(Modifier.padding(horizontal = side, vertical = 22.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
                    Text(if (detail.files.size > 1) "Versions" else "Media", style = MaterialTheme.typography.titleMedium)
                    detail.files.forEachIndexed { index, file ->
                        VersionCard(
                            file = file,
                            showPlay = detail.files.size > 1 && file.available,
                            onPlay = { onPlay(item.id, file.id, if (canResume) resumeMs else 0L) },
                            label = if (detail.files.size > 1) "Version ${index + 1}" else null,
                        )
                    }
                }
            }
        }

        if (detail.children.isNotEmpty()) {
            item {
                Text(
                    childrenHeading(item.kind),
                    style = MaterialTheme.typography.titleMedium,
                    modifier = Modifier.padding(start = side, end = side, top = 16.dp, bottom = 10.dp),
                )
            }
            if (detail.children.firstOrNull()?.kind == "episode") {
                items(detail.children, key = { it.id }) { child ->
                    EpisodeRow(child, side, starting = startingEpisodeId == child.id) {
                        if (startingEpisodeId == null) {
                            startingEpisodeId = child.id
                            scope.launch {
                                val result = runCatching { vm.episodePlayback(child) }.getOrNull()
                                startingEpisodeId = null
                                if (result != null) {
                                    val target = result.playback
                                    onPlay(target.itemId, target.fileId, target.startMs)
                                } else {
                                    onOpenItem(child.id)
                                }
                            }
                        }
                    }
                }
            } else {
                item {
                    LazyRow(
                        contentPadding = androidx.compose.foundation.layout.PaddingValues(horizontal = side),
                        horizontalArrangement = Arrangement.spacedBy(16.dp),
                    ) {
                        items(detail.children, key = { it.id }) { child ->
                            PosterCard(child, width = if (formFactor == FormFactor.Television) 166.dp else 132.dp) {
                                onOpenItem(child.id)
                            }
                        }
                    }
                }
            }
        }
        item { Spacer(Modifier.height(32.dp)) }
    }
}

@Composable
private fun Actions(
    vm: AppViewModel,
    item: Item,
    files: List<MediaFileDto>,
    seriesPlayback: EpisodePlaybackTarget?,
    resumeMs: Long,
    canResume: Boolean,
    onPlay: (Long, Long, Long) -> Unit,
    onViewPhoto: (Long) -> Unit,
    onWatchedChanged: () -> Unit,
) {
    val scope = rememberCoroutineScope()
    var changingWatch by remember { mutableStateOf(false) }
    val playable = files.firstOrNull { it.available }
    LazyRow(
        Modifier.padding(top = 16.dp),
        horizontalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        if (item.kind == "photo") {
            item {
                TvButton(onClick = { onViewPhoto(item.id) }) {
                    Icon(Icons.Filled.Image, contentDescription = null)
                    Text("  View full size")
                }
            }
        } else if (seriesPlayback != null) {
            item {
                TvButton(onClick = {
                    val target = seriesPlayback.playback
                    onPlay(target.itemId, target.fileId, target.startMs)
                }) {
                    Icon(Icons.Filled.PlayArrow, contentDescription = null, modifier = Modifier.size(20.dp))
                    Text("  ${seriesPlayLabel(seriesPlayback)}", fontWeight = FontWeight.SemiBold)
                }
            }
        } else if (playable != null && item.isPlayableVideo) {
            item {
                TvButton(onClick = { onPlay(item.id, playable.id, if (canResume) resumeMs else 0L) }) {
                    Icon(Icons.Filled.PlayArrow, contentDescription = null, modifier = Modifier.size(20.dp))
                    Text(if (canResume) "  Resume · ${formatTime(resumeMs)}" else "  Play", fontWeight = FontWeight.SemiBold)
                }
            }
            if (canResume) {
                item {
                    TvOutlinedButton(onClick = { onPlay(item.id, playable.id, 0L) }) {
                        Icon(Icons.Filled.Refresh, contentDescription = null, modifier = Modifier.size(18.dp))
                        Text("  Start over")
                    }
                }
            }
        }

        val rollup = item.rollup
        val watched = item.watch?.watched == true
        if (rollup != null) {
            if (rollup.leaves > rollup.watched) {
                item {
                    TvTextButton(enabled = !changingWatch, onClick = {
                        changingWatch = true
                        scope.launch { runCatching { vm.setWatched(item.id, true) }; changingWatch = false; onWatchedChanged() }
                    }) {
                        Icon(Icons.Filled.CheckCircle, contentDescription = null, modifier = Modifier.size(18.dp))
                        Text("  ${markWatchedLabel(item.kind)}")
                    }
                }
            }
            if (rollup.watched > 0) {
                item {
                    TvTextButton(enabled = !changingWatch, onClick = {
                        changingWatch = true
                        scope.launch { runCatching { vm.setWatched(item.id, false) }; changingWatch = false; onWatchedChanged() }
                    }) {
                        Icon(Icons.Filled.Refresh, contentDescription = null, modifier = Modifier.size(18.dp))
                        Text("  ${markUnwatchedLabel(item.kind)}")
                    }
                }
            }
        } else if (item.kind != "photo") {
            item {
                TvTextButton(enabled = !changingWatch, onClick = {
                    changingWatch = true
                    scope.launch { runCatching { vm.setWatched(item.id, !watched) }; changingWatch = false; onWatchedChanged() }
                }) {
                    Icon(
                        if (watched) Icons.Filled.Refresh else Icons.Filled.CheckCircle,
                        contentDescription = null,
                        modifier = Modifier.size(18.dp),
                    )
                    Text(if (watched) "  Mark unwatched" else "  Mark watched")
                }
            }
        }
    }
}

@Composable
private fun VersionCard(file: MediaFileDto, showPlay: Boolean, onPlay: () -> Unit, label: String?) {
    Column(
        Modifier.fillMaxWidth().background(SurfaceHi, MaterialTheme.shapes.medium)
            .border(1.dp, Outline, MaterialTheme.shapes.medium).padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(label ?: file.filename, style = MaterialTheme.typography.titleMedium, modifier = Modifier.weight(1f))
            if (!file.available) SpecChip("Missing", MaterialTheme.colorScheme.error)
            if (showPlay) TvOutlinedButton(onClick = onPlay) { Text("Play") }
        }
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            videoBadges(file).forEach { SpecChip(it) }
        }
        Text(file.filename, color = Muted, style = MaterialTheme.typography.labelMedium)
        Text(fileSpecLine(file), color = Muted, style = MaterialTheme.typography.bodyMedium)
        if (!file.available) {
            Text(
                file.missing_path ?: "This file is missing on the server and cannot be played.",
                color = MaterialTheme.colorScheme.error,
                style = MaterialTheme.typography.bodyMedium,
            )
        } else if (!file.probed) {
            Text("Media details have not been read yet; re-analyze it from the web admin.", color = MaterialTheme.colorScheme.error)
        }
    }
}

@Composable
private fun EpisodeRow(item: Item, side: androidx.compose.ui.unit.Dp, starting: Boolean, onClick: () -> Unit) {
    Row(
        Modifier
            .fillMaxWidth()
            .tvFocusRing(MaterialTheme.shapes.medium, focusedScale = 1.02f)
            .clickable(enabled = !starting, onClick = onClick)
            .padding(horizontal = side, vertical = 9.dp),
        horizontalArrangement = Arrangement.spacedBy(14.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(Modifier.width(142.dp).aspectRatio(16f / 9f).clip(MaterialTheme.shapes.medium).background(SurfaceHi)) {
            NetworkImage(imageUrl(item.backdrop ?: item.poster), Modifier.fillMaxSize())
            if (item.watch?.watched == true) {
                Text("✓", color = Color.White, modifier = Modifier.align(Alignment.TopEnd).padding(6.dp).background(Accent).padding(4.dp))
            }
        }
        Column(Modifier.weight(1f)) {
            Text(
                if (item.episode_number != null) "${item.episode_number}. ${item.title}" else item.title,
                style = MaterialTheme.typography.titleMedium,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Text(
                listOfNotNull(item.air_date, item.runtime_ms?.let(::formatTime)).joinToString("  ·  "),
                color = Muted,
                style = MaterialTheme.typography.labelMedium,
            )
            item.overview?.let {
                Text(it, color = Muted, style = MaterialTheme.typography.bodyMedium, maxLines = 2, overflow = TextOverflow.Ellipsis)
            }
        }
        if (starting) {
            CircularProgressIndicator(Modifier.size(24.dp), color = Accent, strokeWidth = 2.dp)
        } else {
            Icon(Icons.Filled.PlayArrow, contentDescription = "Play episode", tint = Accent)
        }
    }
}

@Composable
private fun SpecChip(text: String, color: Color = Accent) {
    Text(
        text,
        color = color,
        style = MaterialTheme.typography.labelMedium,
        modifier = Modifier.border(1.dp, color, MaterialTheme.shapes.small).padding(horizontal = 8.dp, vertical = 4.dp),
    )
}

private fun videoBadges(file: MediaFileDto): List<String> = buildList {
    file.video_codec?.uppercase()?.let(::add)
    file.height?.let { add(if (it >= 2160) "4K" else "${it}p") }
    (file.hdr_format ?: file.hdr)?.let(::add)
}

private fun fileSpecLine(file: MediaFileDto): String {
    val audio = file.audio_streams.joinToString(" / ") { stream ->
        listOfNotNull(stream.codec?.uppercase(), stream.channels?.let { "${it}ch" }, stream.language).joinToString(" · ")
    }
    val bits = listOfNotNull(
        file.container?.uppercase(),
        file.bitrate?.let { "%.1f Mbps".format(it / 1_000_000.0) },
        file.size.takeIf { it > 0 }?.let { "%.1f GB".format(it / 1_073_741_824.0) },
        audio.takeIf { it.isNotBlank() },
    )
    return bits.joinToString("  ·  ")
}

private fun metaLine(item: Item, durationMs: Long?): String = buildList {
    if (item.kind == "episode") {
        item.show_title?.let(::add)
        if (item.season_number != null && item.episode_number != null) add("S${item.season_number} · E${item.episode_number}")
    }
    item.recorded_at?.let(::add)
    item.year?.let { add(it.toString()) }
    durationMs?.takeIf { it > 0 }?.let { add(formatTime(it)) }
    add(item.kind.replaceFirstChar { it.uppercase() })
    item.rollup?.takeIf { it.leaves > 0 }?.let { add("${it.watched} of ${it.leaves} watched") }
}.joinToString("  ·  ")

private fun childrenHeading(kind: String): String = when (kind) {
    "show" -> "Seasons"
    "season" -> "Episodes"
    else -> "Contents"
}

private fun seriesPlayLabel(target: EpisodePlaybackTarget): String {
    val episode = target.episode
    val number = if (episode.season_number != null && episode.episode_number != null) {
        "S${episode.season_number} · E${episode.episode_number}"
    } else {
        episode.title
    }
    return if (target.playback.startMs > 0L) "Resume $number" else "Play $number"
}

private fun markWatchedLabel(kind: String): String = when (kind) {
    "show" -> "Mark all watched"
    "season" -> "Mark season watched"
    else -> "Mark watched"
}

private fun markUnwatchedLabel(kind: String): String = when (kind) {
    "show" -> "Mark all unwatched"
    "season" -> "Mark season unwatched"
    else -> "Mark unwatched"
}

@Composable
internal fun DetailBackButton(
    onBack: () -> Unit,
    safeInsets: WindowInsets = WindowInsets.statusBars,
) {
    SafeBackButton(onBack = onBack, safeInsets = safeInsets)
}
