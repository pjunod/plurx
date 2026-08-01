package tv.plurx.app.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import tv.plurx.app.data.Item
import tv.plurx.app.ui.components.ChoicePicker
import tv.plurx.app.ui.components.LoadingBox
import tv.plurx.app.ui.components.PosterCard
import tv.plurx.app.ui.components.SafeTopRow
import tv.plurx.app.ui.theme.Muted

private enum class WatchFilter(val label: String) {
    Everything("Everything"), Unwatched("Unwatched"), InProgress("In progress"), Watched("Watched")
}

private data class LibraryLoad(val items: List<Item> = emptyList(), val error: String? = null)

@Composable
fun LibraryScreen(
    vm: AppViewModel,
    libraryIds: List<Long>,
    title: String,
    onOpenItem: (Long) -> Unit,
    onBack: () -> Unit,
) {
    val preferences by vm.preferences.collectAsStateWithLifecycle()
    val home by vm.home.collectAsStateWithLifecycle()
    val formFactor = currentFormFactor()
    val side = formFactor.horizontalPadding()
    val kind = home.libraries.firstOrNull { it.id in libraryIds }?.kind
    var sort by remember { mutableStateOf(if (kind == "home") "recorded" else "title") }
    var filter by remember { mutableStateOf(WatchFilter.Everything) }
    val load by produceState<LibraryLoad?>(initialValue = null, libraryIds, sort) {
        value = try {
            val all = libraryIds.flatMap { vm.libraryItems(it, sort) }
            LibraryLoad(sortMerged(all, sort))
        } catch (e: Exception) {
            LibraryLoad(error = e.message ?: "Couldn't load this library")
        }
    }
    val shown = load?.items.orEmpty().filter { matchesFilter(it, filter) }
    val posterWidth = (preferences.posterSize.widthDp * formFactor.posterScale()).dp

    Column(Modifier.fillMaxSize().navigationBarsPadding()) {
        SafeTopRow(
            Modifier.fillMaxWidth().padding(start = side - 12.dp, end = side, top = 8.dp),
        ) {
            IconButton(onClick = onBack) {
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
            }
            Column(Modifier.weight(1f)) {
                Text(title, style = MaterialTheme.typography.titleLarge)
                if (load != null && load?.error == null) {
                    Text("${shown.size} of ${load?.items?.size ?: 0}", color = Muted, style = MaterialTheme.typography.labelMedium)
                }
            }
        }

        Row(
            Modifier.fillMaxWidth().padding(horizontal = side, vertical = 12.dp),
            horizontalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            ChoicePicker(
                label = "Sort",
                value = sort,
                options = listOf("title", "added", "recorded", "year", "resolution"),
                optionLabel = {
                    when (it) {
                        "title" -> "Title (A–Z)"
                        "added" -> "Recently added"
                        "recorded" -> "Date recorded"
                        "year" -> "Year"
                        else -> "Resolution"
                    }
                },
                onSelect = { sort = it },
                modifier = Modifier.weight(1f),
            )
            ChoicePicker(
                label = "Show",
                value = filter,
                options = WatchFilter.entries,
                optionLabel = { it.label },
                onSelect = { filter = it },
                modifier = Modifier.weight(1f),
            )
        }

        when {
            load == null -> LoadingBox()
            load?.error != null -> Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                Text(load?.error.orEmpty(), color = MaterialTheme.colorScheme.error)
            }
            shown.isEmpty() -> Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                Text(if (filter == WatchFilter.Everything) "This library is empty." else "No titles match this filter.", color = Muted)
            }
            else -> LazyVerticalGrid(
                columns = GridCells.Adaptive(minSize = posterWidth),
                contentPadding = PaddingValues(start = side, end = side, top = 8.dp, bottom = 32.dp),
                horizontalArrangement = Arrangement.spacedBy(16.dp),
                verticalArrangement = Arrangement.spacedBy(22.dp),
            ) {
                items(shown, key = { it.id }) { item ->
                    PosterCard(item, width = posterWidth) { onOpenItem(item.id) }
                }
            }
        }
    }
}

private fun matchesFilter(item: Item, filter: WatchFilter): Boolean {
    val watched = item.watch?.watched == true
    val inProgress = item.watch?.let { it.position_ms > 3_000 && !it.watched } == true
    return when (filter) {
        WatchFilter.Everything -> true
        WatchFilter.Unwatched -> !watched && !inProgress
        WatchFilter.InProgress -> inProgress
        WatchFilter.Watched -> watched
    }
}

private fun sortMerged(items: List<Item>, sort: String): List<Item> = when (sort) {
    "title" -> items.sortedBy { sortableTitle(it.title) }
    "year" -> items.sortedWith(compareByDescending<Item> { it.year ?: Int.MIN_VALUE }.thenBy { sortableTitle(it.title) })
    "recorded" -> items.sortedWith(compareByDescending<Item> { it.recorded_at.orEmpty() }.thenBy { sortableTitle(it.title) })
    "resolution" -> items.sortedWith(compareByDescending<Item> { it.resolution ?: Long.MIN_VALUE }.thenBy { sortableTitle(it.title) })
    else -> items
}

private fun sortableTitle(title: String): String {
    val lower = title.lowercase()
    return listOf("the ", "a ", "an ").firstOrNull { lower.startsWith(it) }
        ?.let { lower.removePrefix(it) } ?: lower
}
