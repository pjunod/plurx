package tv.plurx.app.ui

import android.Manifest
import android.content.Intent
import android.net.Uri
import android.os.Build
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
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
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.CheckCircle
import androidx.compose.material.icons.filled.Image
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.Download
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
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.launch
import tv.plurx.app.data.Item
import tv.plurx.app.data.ItemDetail
import tv.plurx.app.data.MediaFileDto
import tv.plurx.app.data.Session
import tv.plurx.app.ui.components.LoadingBox
import tv.plurx.app.ui.components.MediaFactChip
import tv.plurx.app.ui.components.NetworkImage
import tv.plurx.app.ui.components.PosterCard
import tv.plurx.app.ui.components.RequestInitialFocus
import tv.plurx.app.ui.components.SafeBackButton
import tv.plurx.app.ui.components.TvButton
import tv.plurx.app.ui.components.TvOutlinedButton
import tv.plurx.app.ui.components.TvTextButton
import tv.plurx.app.ui.components.formatTime
import tv.plurx.app.ui.components.imageUrl
import tv.plurx.app.ui.components.safeDisplayInsets
import tv.plurx.app.ui.components.detailMediaFacts
import tv.plurx.app.ui.components.tvFocusRing
import tv.plurx.app.ui.theme.Accent
import tv.plurx.app.ui.theme.Bg
import tv.plurx.app.ui.theme.Muted
import tv.plurx.app.ui.theme.Outline
import tv.plurx.app.ui.theme.SurfaceHi
import tv.plurx.app.data.offline.OfflineDownloads
import tv.plurx.app.data.offline.needsExplicitResume

private data class DetailLoad(
    val detail: ItemDetail? = null,
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
            DetailLoad(detail = vm.itemDetail(itemId))
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            DetailLoad(error = e.message ?: "Couldn't load this item")
        }
    }
    // Resolving "which episode does Play mean" walks seasons and episodes
    // serially. That is the answer to a button's *label*, not to whether the
    // page can be drawn, so the page draws first and the button resolves
    // behind it.
    val detail = load?.detail
    val seriesPlayback by produceState<EpisodePlaybackTarget?>(null, detail) {
        val loaded = detail ?: return@produceState
        value = try {
            vm.seriesPlayback(loaded)
        } catch (e: CancellationException) {
            throw e
        } catch (_: Exception) {
            null
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
            seriesPlayback = seriesPlayback,
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
    val resumeMs = item.watch?.position_ms ?: 0L
    val best = playbackFile(item, detail.files, resumeMs)
    val durationMs = if (item.isAudiobook) item.runtime_ms ?: best?.duration_ms else best?.duration_ms ?: item.runtime_ms
    val nearlyDone = durationMs != null && durationMs > 0 && resumeMs > durationMs * 0.95
    val canResume = resumeMs > 3_000 && !nearlyDone
    val heroHeight = when (formFactor) {
        FormFactor.Compact -> 230.dp
        FormFactor.Expanded -> 300.dp
        FormFactor.Television -> 360.dp
    }

    LazyColumn(Modifier.fillMaxSize().navigationBarsPadding()) {
        item {
            if (formFactor == FormFactor.Compact) {
                CompactDetailHero(
                    item = item,
                    file = best,
                    durationMs = durationMs,
                    onBack = onBack,
                )
            } else {
                Box(Modifier.fillMaxWidth().height(heroHeight)) {
                    NetworkImage(imageUrl(item.backdrop ?: item.poster), Modifier.fillMaxSize())
                    Box(Modifier.fillMaxSize().background(Brush.verticalGradient(listOf(Color(0x22000000), Bg))))
                    DetailBackButton(onBack)
                }
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
                    if (formFactor != FormFactor.Compact) {
                        if (detail.ancestors.isNotEmpty()) {
                            AncestorBreadcrumb(
                                ancestors = detail.ancestors,
                                onOpenItem = onOpenItem,
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
                    }

                    Actions(
                        vm = vm,
                        item = item,
                        files = detail.files,
                        seriesPlayback = seriesPlayback,
                        resumeMs = resumeMs,
                        canResume = canResume,
                        requestInitialFocus = formFactor == FormFactor.Television,
                        onPlay = onPlay,
                        onViewPhoto = onViewPhoto,
                        onWatchedChanged = onWatchedChanged,
                    )

                    item.overview?.takeIf { it.isNotBlank() }?.let {
                        if (formFactor == FormFactor.Compact) {
                            Text(
                                "About",
                                style = MaterialTheme.typography.titleMedium,
                                modifier = Modifier.padding(top = 18.dp, bottom = 6.dp),
                            )
                        }
                        Text(
                            it,
                            color = Muted,
                            style = MaterialTheme.typography.bodyMedium,
                            modifier = Modifier.padding(top = if (formFactor == FormFactor.Compact) 0.dp else 16.dp),
                        )
                    }
                }
            }
        }

        if (detail.files.isNotEmpty()) {
            item {
                Column(Modifier.padding(horizontal = side, vertical = 22.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
                    Text(
                        when {
                            item.isAudiobook -> "Parts & chapters"
                            detail.files.size > 1 -> "Versions"
                            else -> "Media"
                        },
                        style = MaterialTheme.typography.titleMedium,
                    )
                    detail.files.forEachIndexed { index, file ->
                        VersionCard(
                            file = file,
                            showPlay = detail.files.size > 1 && file.available,
                            onPlay = { startMs ->
                                onPlay(
                                    item.id,
                                    file.id,
                                    if (item.isAudiobook) startMs else if (canResume) resumeMs else 0L,
                                )
                            },
                            label = if (detail.files.size > 1) (if (item.isAudiobook) "Part ${index + 1}" else "Version ${index + 1}") else null,
                            showChapters = item.isAudiobook,
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
                                val result = catchingUnlessCancelled { vm.episodePlayback(child) }.getOrNull()
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
private fun CompactDetailHero(
    item: Item,
    file: MediaFileDto?,
    durationMs: Long?,
    onBack: () -> Unit,
) {
    val progress = detailProgress(item, durationMs)
    Box(Modifier.fillMaxWidth().height(300.dp)) {
        NetworkImage(imageUrl(item.backdrop ?: item.poster), Modifier.fillMaxSize())
        Box(
            Modifier.fillMaxSize().background(
                Brush.verticalGradient(
                    listOf(Color(0x22000000), Color(0x44000000), Bg),
                ),
            ),
        )
        DetailBackButton(onBack)
        Column(
            Modifier.align(Alignment.BottomStart).fillMaxWidth().padding(horizontal = 20.dp, vertical = 16.dp),
            verticalArrangement = Arrangement.spacedBy(7.dp),
        ) {
            item.show_title?.let { showTitle ->
                Text(
                    showTitle.uppercase(),
                    color = Accent,
                    style = MaterialTheme.typography.labelSmall,
                    fontWeight = FontWeight.Bold,
                )
            }
            Text(
                item.title,
                color = Color.White,
                style = MaterialTheme.typography.headlineMedium,
                fontWeight = FontWeight.ExtraBold,
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
            )
            file?.let { mediaFile ->
                LazyRow(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                    items(detailMediaFacts(mediaFile), key = { it.kind }) { fact ->
                        MediaFactChip(fact)
                    }
                }
            }
            Row(
                horizontalArrangement = Arrangement.spacedBy(12.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                compactDetailFacts(item, durationMs).forEach { fact ->
                    Text(fact, color = Color.White.copy(alpha = 0.7f), style = MaterialTheme.typography.labelSmall)
                }
            }
            if (progress > 0f) {
                Box(Modifier.fillMaxWidth().height(3.dp).background(Color.White.copy(alpha = 0.18f))) {
                    Box(Modifier.fillMaxWidth(progress).height(3.dp).background(Accent))
                }
            }
        }
    }
}

internal fun compactDetailFacts(item: Item, durationMs: Long?): List<String> = buildList {
    if (item.kind == "episode" && item.season_number != null && item.episode_number != null) {
        add("S${item.season_number} E${item.episode_number}")
    }
    item.year?.let { add(it.toString()) }
    durationMs?.takeIf { it > 0 }?.let { add(compactRuntimeLabel(it)) }
    addAll(item.tags.take(2))
}

private fun detailProgress(item: Item, durationMs: Long?): Float {
    val watch = item.watch ?: return 0f
    val position = watch.position_ms
    val duration = watch.duration_ms ?: durationMs ?: return 0f
    if (duration <= 0) return 0f
    return (position.toFloat() / duration).coerceIn(0f, 1f)
}

@Composable
internal fun AncestorBreadcrumb(
    ancestors: List<Item>,
    onOpenItem: (Long) -> Unit,
    modifier: Modifier = Modifier,
) {
    LazyRow(
        modifier = modifier.fillMaxWidth(),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        itemsIndexed(ancestors, key = { _, ancestor -> ancestor.id }) { index, ancestor ->
            if (index > 0) {
                Text(
                    "/",
                    color = Muted,
                    style = MaterialTheme.typography.labelMedium,
                    modifier = Modifier.padding(horizontal = 2.dp),
                )
            }
            Text(
                ancestor.title,
                color = Accent,
                style = MaterialTheme.typography.labelMedium,
                fontWeight = FontWeight.SemiBold,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier
                    .tvFocusRing(MaterialTheme.shapes.small, focusedScale = 1.04f)
                    .clickable { onOpenItem(ancestor.id) }
                    .padding(horizontal = 6.dp, vertical = 4.dp),
            )
        }
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
    requestInitialFocus: Boolean,
    onPlay: (Long, Long, Long) -> Unit,
    onViewPhoto: (Long) -> Unit,
    onWatchedChanged: () -> Unit,
) {
    val scope = rememberCoroutineScope()
    val context = androidx.compose.ui.platform.LocalContext.current
    val offlineRecords by vm.offlineRecords.collectAsStateWithLifecycle()
    val notificationPermission = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { /* denial is benign; Android still exposes foreground work */ }
    var changingWatch by remember { mutableStateOf(false) }
    var downloadError by remember(item.id) { mutableStateOf<String?>(null) }
    val playable = playbackFile(item, files, resumeMs)
    val startOverFile = if (item.isAudiobook) files.firstOrNull { it.available } ?: playable else playable
    val localResume = if (item.isAudiobook) (resumeMs - (playable?.part_offset_ms ?: 0)).coerceAtLeast(0) else resumeMs
    val offline = playable?.let { file -> offlineRecords.firstOrNull {
        it.serverInstanceId == vm.serverInstanceId && it.userId == vm.currentUserId &&
            it.fileId == file.id
    } }
    LazyRow(
        Modifier.padding(top = 16.dp),
        contentPadding = PaddingValues(horizontal = 8.dp, vertical = 6.dp),
        horizontalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        if (item.kind == "photo") {
            item {
                DetailPrimaryActionButton(
                    onClick = { onViewPhoto(item.id) },
                    requestInitialFocus = requestInitialFocus,
                ) {
                    Icon(Icons.Filled.Image, contentDescription = null)
                    Text("  View full size")
                }
            }
        } else if (item.isBook && playable != null) {
            item {
                DetailPrimaryActionButton(
                    onClick = {
                        val url = Session.mediaUrl("/api/v1/files/${playable.id}/content")
                        context.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(url)))
                    },
                    requestInitialFocus = requestInitialFocus,
                ) { Text("Open book", fontWeight = FontWeight.SemiBold) }
            }
        } else if (seriesPlayback != null) {
            item {
                DetailPrimaryActionButton(
                    onClick = {
                        val target = seriesPlayback.playback
                        onPlay(target.itemId, target.fileId, target.startMs)
                    },
                    requestInitialFocus = requestInitialFocus,
                ) {
                    Icon(Icons.Filled.PlayArrow, contentDescription = null, modifier = Modifier.size(20.dp))
                    Text("  ${seriesPlayLabel(seriesPlayback)}", fontWeight = FontWeight.SemiBold)
                }
            }
        } else if (playable != null && item.isPlayable) {
            item {
                DetailPrimaryActionButton(
                    onClick = { onPlay(item.id, playable.id, if (canResume) localResume else 0L) },
                    requestInitialFocus = requestInitialFocus,
                ) {
                    Icon(Icons.Filled.PlayArrow, contentDescription = null, modifier = Modifier.size(20.dp))
                    Text(if (canResume) "  Resume  ${formatTime(resumeMs)}" else "  Play", fontWeight = FontWeight.SemiBold)
                }
            }
            if (canResume) {
                item {
                    TvOutlinedButton(onClick = { onPlay(item.id, startOverFile?.id ?: playable.id, 0L) }) {
                        Icon(Icons.Filled.Refresh, contentDescription = null, modifier = Modifier.size(18.dp))
                        Text("  Start over")
                    }
                }
            }
            if (OfflineDownloads.canUse(context)) {
                item {
                    TvOutlinedButton(
                        enabled = offline?.isPlayable != true,
                        onClick = {
                            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                                notificationPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
                            }
                            when {
                                offline == null || offline.state in setOf("failed", "missing") -> {
                                    downloadError = vm.queueOffline(item, playable)
                                }
                                offline.needsExplicitResume -> {
                                    downloadError = null
                                    vm.resumeOffline(offline)
                                }
                                else -> {
                                    downloadError = null
                                    vm.removeOffline(offline)
                                }
                            }
                        },
                    ) {
                        Icon(Icons.Filled.Download, contentDescription = null, modifier = Modifier.size(18.dp))
                        Text(
                            when {
                                offline == null -> "  Download"
                                offline.isPlayable -> "  Downloaded"
                                offline.needsExplicitResume -> "  Resume download"
                                offline.state in setOf("failed", "missing") -> "  Download again"
                                else -> "  Cancel download"
                            },
                        )
                    }
                }
                downloadError?.let { message ->
                    item {
                        Text(
                            message,
                            color = MaterialTheme.colorScheme.error,
                            style = MaterialTheme.typography.bodySmall,
                        )
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
                        scope.launch { catchingUnlessCancelled { vm.setWatched(item.id, true) }; changingWatch = false; onWatchedChanged() }
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
                        scope.launch { catchingUnlessCancelled { vm.setWatched(item.id, false) }; changingWatch = false; onWatchedChanged() }
                    }) {
                        Icon(Icons.Filled.Refresh, contentDescription = null, modifier = Modifier.size(18.dp))
                        Text("  ${markUnwatchedLabel(item.kind)}")
                    }
                }
            }
        } else if (item.kind != "photo" && !item.isBook) {
            item {
                TvTextButton(enabled = !changingWatch, onClick = {
                    changingWatch = true
                    scope.launch { catchingUnlessCancelled { vm.setWatched(item.id, !watched) }; changingWatch = false; onWatchedChanged() }
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
internal fun DetailPrimaryActionButton(
    onClick: () -> Unit,
    requestInitialFocus: Boolean,
    modifier: Modifier = Modifier,
    content: @Composable RowScope.() -> Unit,
) {
    val focusRequester = remember { FocusRequester() }
    RequestInitialFocus(focusRequester, enabled = requestInitialFocus)
    TvButton(
        onClick = onClick,
        modifier = modifier.focusRequester(focusRequester),
        content = content,
    )
}

@Composable
private fun VersionCard(
    file: MediaFileDto,
    showPlay: Boolean,
    onPlay: (Long) -> Unit,
    label: String?,
    showChapters: Boolean,
) {
    Column(
        Modifier.fillMaxWidth().background(SurfaceHi, MaterialTheme.shapes.medium)
            .border(1.dp, Outline, MaterialTheme.shapes.medium).padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(label ?: file.filename, style = MaterialTheme.typography.titleMedium, modifier = Modifier.weight(1f))
            if (!file.available) SpecChip("Missing", MaterialTheme.colorScheme.error)
            if (showPlay) TvOutlinedButton(onClick = { onPlay(0L) }) { Text("Play") }
        }
        LazyRow(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            items(detailMediaFacts(file), key = { it.kind }) { fact ->
                MediaFactChip(fact)
            }
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
        if (showChapters && file.chapters.isNotEmpty()) {
            Text("Chapters", style = MaterialTheme.typography.labelLarge, modifier = Modifier.padding(top = 4.dp))
            file.chapters.forEach { chapter ->
                TvTextButton(
                    onClick = { onPlay(chapter.start_ms) },
                    enabled = file.available,
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Text(chapter.title, modifier = Modifier.weight(1f), maxLines = 1, overflow = TextOverflow.Ellipsis)
                    Text(formatTime(file.part_offset_ms + chapter.start_ms), color = Muted)
                }
            }
        }
    }
}

@Composable
private fun EpisodeRow(item: Item, side: androidx.compose.ui.unit.Dp, starting: Boolean, onClick: () -> Unit) {
    val focusEndPadding = 12.dp
    Row(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = side - focusEndPadding)
            .tvFocusRing(MaterialTheme.shapes.medium, focusedScale = 1.02f)
            .clickable(enabled = !starting, onClick = onClick)
            .padding(horizontal = focusEndPadding, vertical = 9.dp),
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
                listOfNotNull(item.air_date, item.runtime_ms?.let(::compactRuntimeLabel)).joinToString("   "),
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

private fun fileSpecLine(file: MediaFileDto): String {
    val audio = file.audio_streams.joinToString(" / ") { stream ->
        listOfNotNull(stream.codec?.uppercase(), stream.channels?.let { "${it}ch" }, stream.language).joinToString("  ")
    }
    val bits = listOfNotNull(
        file.duration_ms?.takeIf { it > 0 }?.let(::formatTime),
        file.container?.uppercase(),
        file.bitrate?.let { "%.1f Mbps".format(it / 1_000_000.0) },
        file.size.takeIf { it > 0 }?.let { "%.1f GB".format(it / 1_073_741_824.0) },
        audio.takeIf { it.isNotBlank() },
    )
    return bits.joinToString("   ")
}

private fun playbackFile(item: Item, files: List<MediaFileDto>, positionMs: Long): MediaFileDto? {
    val available = files.filter { it.available }
    if (!item.isAudiobook) return available.firstOrNull()
    return available.lastOrNull { positionMs >= it.part_offset_ms } ?: available.firstOrNull()
}

private fun metaLine(item: Item, durationMs: Long?): String = buildList {
    if (item.kind == "episode") {
        item.show_title?.let(::add)
        if (item.season_number != null && item.episode_number != null) add("S${item.season_number} E${item.episode_number}")
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
        "S${episode.season_number} E${episode.episode_number}"
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
    safeInsets: WindowInsets = safeDisplayInsets(),
) {
    SafeBackButton(onBack = onBack, safeInsets = safeInsets)
}
