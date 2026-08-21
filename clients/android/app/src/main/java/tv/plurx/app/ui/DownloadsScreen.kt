package tv.plurx.app.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.MenuBook
import androidx.compose.material.icons.filled.Delete
import androidx.compose.material.icons.filled.Download
import androidx.compose.material.icons.filled.Pause
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Icon
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import tv.plurx.app.data.offline.OfflineBook
import tv.plurx.app.data.offline.OfflineBooks
import tv.plurx.app.data.offline.OfflineDownloads
import tv.plurx.app.data.offline.OfflineRecord
import tv.plurx.app.data.offline.needsExplicitResume
import tv.plurx.app.ui.components.SafeTopRow
import tv.plurx.app.ui.components.TvIconButton
import tv.plurx.app.ui.theme.Accent
import tv.plurx.app.ui.theme.Muted
import coil.compose.AsyncImage
import java.io.File

@Composable
fun DownloadsScreen(
    vm: AppViewModel,
    onPlay: (String) -> Unit,
    onRead: (String) -> Unit,
    onBack: () -> Unit,
) {
    val all by OfflineDownloads.records.collectAsStateWithLifecycle()
    val allBooks by OfflineBooks.records.collectAsStateWithLifecycle()
    val current = all.filter {
        it.serverInstanceId == vm.serverInstanceId && it.userId == vm.currentUserId
    }.sortedByDescending { it.updatedAt }
    val currentBooks = allBooks.filter {
        it.serverInstanceId == vm.serverInstanceId && it.userId == vm.currentUserId
    }.sortedByDescending { it.updatedAt }
    val otherVideos = all.filterNot { it in current }
        .groupBy { it.serverInstanceId to it.userId }
    val otherBooks = allBooks.filterNot { it in currentBooks }
        .groupBy { it.serverInstanceId to it.userId }
    val otherProfiles = (otherVideos.keys + otherBooks.keys).associateWith { profile ->
        otherVideos[profile].orEmpty() to otherBooks[profile].orEmpty()
    }
    val side = currentFormFactor().horizontalPadding()
    var profileToDelete by remember { mutableStateOf<Pair<String, Long>?>(null) }

    LaunchedEffect(vm.serverInstanceId, vm.currentUserId) {
        vm.resumeOfflineProfile()
    }

    profileToDelete?.let { profile ->
        AlertDialog(
            onDismissRequest = { profileToDelete = null },
            title = { Text("Delete these downloads?") },
            text = {
                Text("This removes every download saved by the other profile from this device.")
            },
            confirmButton = {
                TextButton(
                    onClick = {
                        OfflineDownloads.removeProfile(profile.first, profile.second)
                        OfflineBooks.removeProfile(profile.first, profile.second)
                        profileToDelete = null
                    },
                ) {
                    Text("Delete")
                }
            },
            dismissButton = {
                TextButton(onClick = { profileToDelete = null }) {
                    Text("Cancel")
                }
            },
        )
    }

    Column(Modifier.fillMaxSize().navigationBarsPadding()) {
        SafeTopRow(
            Modifier.fillMaxWidth().padding(start = side - 12.dp, end = side, top = 8.dp),
        ) {
            TvIconButton(onClick = onBack) {
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
            }
            Text("Downloads", style = MaterialTheme.typography.titleLarge)
            Spacer(Modifier.weight(1f))
            Text(
                formatBytes(
                    current.sumOf { it.bytesTotal ?: it.bytesDownloaded } +
                        currentBooks.sumOf(OfflineBook::bytesTotal),
                ),
                color = Muted,
            )
        }

        if (current.isEmpty() && currentBooks.isEmpty() && otherProfiles.isEmpty()) {
            Column(
                Modifier.fillMaxSize().padding(40.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
                verticalArrangement = Arrangement.Center,
            ) {
                Icon(Icons.Filled.Download, contentDescription = null, modifier = Modifier.size(42.dp), tint = Muted)
                Text("No downloads yet", style = MaterialTheme.typography.titleMedium)
                Text("Tap Download on a video or book to use it without a connection.", color = Muted)
            }
            return@Column
        }

        LazyColumn(
            Modifier.fillMaxSize(),
            contentPadding = androidx.compose.foundation.layout.PaddingValues(
                horizontal = side,
                vertical = 20.dp,
            ),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            if (current.isNotEmpty() || currentBooks.isNotEmpty()) {
                item { Text("On this device", style = MaterialTheme.typography.titleMedium) }
                items(current, key = { "video:${it.id}" }) { record ->
                    DownloadRow(
                        record = record,
                        onOpen = {
                            when {
                                record.isPlayable -> onPlay(record.id)
                                record.needsExplicitResume -> vm.resumeOffline(record)
                            }
                        },
                        onRemove = { vm.removeOffline(record) },
                    )
                }
                items(currentBooks, key = { "book:${it.id}" }) { book ->
                    BookDownloadRow(
                        book = book,
                        onOpen = {
                            when {
                                book.isPlayable -> onRead(book.id)
                                book.state in setOf("failed", "missing") -> vm.retryOfflineBook(book)
                            }
                        },
                        onRemove = { vm.removeOfflineBook(book) },
                    )
                }
            }
            if (otherProfiles.isNotEmpty()) {
                item {
                    Text(
                        "Other profiles",
                        style = MaterialTheme.typography.titleMedium,
                        modifier = Modifier.padding(top = 18.dp),
                    )
                }
                otherProfiles.forEach { (profile, downloads) ->
                    val (records, books) = downloads
                    item(key = "${profile.first}:${profile.second}") {
                        Row(
                            Modifier.fillMaxWidth().padding(vertical = 8.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Column(Modifier.weight(1f)) {
                                Text("Another Cinema profile")
                                Text(
                                    "${records.size + books.size} items · ${formatBytes(
                                        records.sumOf { it.bytesTotal ?: it.bytesDownloaded } +
                                            books.sumOf(OfflineBook::bytesTotal)
                                    )}",
                                    color = Muted,
                                    style = MaterialTheme.typography.labelMedium,
                                )
                            }
                            TvIconButton(
                                onClick = { profileToDelete = profile },
                            ) {
                                Icon(Icons.Filled.Delete, contentDescription = "Delete profile downloads")
                            }
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun BookDownloadRow(
    book: OfflineBook,
    onOpen: () -> Unit,
    onRemove: () -> Unit,
) {
    val cover = OfflineBooks.cover(book)
    Row(
        Modifier.fillMaxWidth().clickable(onClick = onOpen).padding(vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(14.dp),
    ) {
        if (cover?.isFile == true) {
            AsyncImage(
                model = cover,
                contentDescription = null,
                modifier = Modifier.width(48.dp).height(72.dp).clip(RoundedCornerShape(7.dp)),
                contentScale = ContentScale.Crop,
            )
        } else {
            Icon(
                Icons.AutoMirrored.Filled.MenuBook,
                contentDescription = null,
                tint = if (book.isPlayable) Accent else Muted,
                modifier = Modifier.width(48.dp),
            )
        }
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Text(book.title, style = MaterialTheme.typography.titleMedium)
            book.author?.let { Text(it, color = Muted, style = MaterialTheme.typography.labelMedium) }
            Text(bookDownloadStateLabel(book), color = Muted, style = MaterialTheme.typography.labelMedium)
            if (book.state == "downloading" && book.bytesTotal > 0) {
                LinearProgressIndicator(
                    progress = {
                        (book.bytesDownloaded.toFloat() / book.bytesTotal.toFloat()).coerceIn(0f, 1f)
                    },
                    modifier = Modifier.fillMaxWidth(),
                )
            }
            book.errorMessage?.takeIf { book.state in setOf("failed", "missing") }?.let {
                Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.labelSmall)
            }
        }
        TvIconButton(onClick = onRemove) {
            Icon(Icons.Filled.Delete, contentDescription = "Remove book download")
        }
    }
}

internal fun bookDownloadStateLabel(book: OfflineBook): String = when {
    book.isPlayable -> buildList {
        add("Downloaded")
        add(formatBytes(book.bytesTotal))
        if (book.progression > 0) add("${(book.progression * 100).toInt().coerceIn(0, 100)}% read")
    }.joinToString(" · ")
    book.state == "intent" -> "Preparing book"
    book.state == "downloading" -> "Downloading book"
    book.state == "failed" -> "Download failed — tap to try again"
    book.state == "missing" -> "Download missing — tap to download again"
    else -> "Book download unavailable"
}

@Composable
private fun DownloadRow(
    record: OfflineRecord,
    onOpen: () -> Unit,
    onRemove: () -> Unit,
) {
    Row(
        Modifier.fillMaxWidth().clickable(onClick = onOpen).padding(vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(14.dp),
    ) {
        if (record.posterFile != null && File(record.posterFile).isFile) {
            AsyncImage(
                model = File(record.posterFile),
                contentDescription = null,
                modifier = Modifier.width(48.dp).height(72.dp).clip(RoundedCornerShape(7.dp)),
                contentScale = ContentScale.Crop,
            )
        } else {
            Icon(
                when {
                    record.isPlayable -> Icons.Filled.PlayArrow
                    record.state == "paused" -> Icons.Filled.Pause
                    else -> Icons.Filled.Download
                },
                contentDescription = null,
                tint = if (record.isPlayable) Accent else Muted,
                modifier = Modifier.width(48.dp),
            )
        }
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Text(record.title, style = MaterialTheme.typography.titleMedium)
            record.context?.let { Text(it, color = Muted, style = MaterialTheme.typography.labelMedium) }
            Text(downloadStateLabel(record), color = Muted, style = MaterialTheme.typography.labelMedium)
            if (record.state == "downloading" && record.percentDownloaded >= 0) {
                LinearProgressIndicator(
                    progress = { record.percentDownloaded.coerceIn(0f, 100f) / 100f },
                    modifier = Modifier.fillMaxWidth(),
                )
            }
            record.errorMessage?.takeIf { record.state == "failed" }?.let {
                Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.labelSmall)
            }
        }
        TvIconButton(onClick = onRemove) {
            Icon(Icons.Filled.Delete, contentDescription = "Remove download")
        }
    }
}

internal fun downloadStateLabel(record: OfflineRecord): String = when {
    record.isPlayable -> buildList {
        add("Downloaded")
        record.actualHeight?.let { add("${it}p") }
        record.bytesTotal?.let { add(formatBytes(it)) }
    }.joinToString(" · ")
    record.phase == "paused_by_system" -> "Paused by system — tap Resume"
    record.state == "paused" -> "Paused — tap Resume"
    record.state == "preparing" -> "Preparing on server · keep or reopen Cinema to start transfer"
    record.state == "ready" -> "Ready to download"
    record.state == "downloading" -> "Downloading"
    record.state == "failed" -> "Download failed"
    record.state == "missing" -> "Download missing"
    else -> "Queued · Waiting for server"
}

internal fun formatBytes(bytes: Long): String {
    val gib = bytes.toDouble() / (1024 * 1024 * 1024)
    return if (gib >= 1) "%.1f GB".format(gib) else "%.0f MB".format(bytes / (1024.0 * 1024.0))
}
