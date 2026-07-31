package tv.plurx.app.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.Search
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import tv.plurx.app.data.HomeGrouping
import tv.plurx.app.data.Item
import tv.plurx.app.data.Library
import tv.plurx.app.data.ThemeId
import tv.plurx.app.ui.components.ChoicePicker
import tv.plurx.app.ui.components.LoadingBox
import tv.plurx.app.ui.components.MediaRow
import tv.plurx.app.ui.theme.Accent
import tv.plurx.app.ui.theme.Muted

private data class HomeCollection(
    val title: String,
    val libraries: List<Library>,
    val items: List<Item>,
)

@Composable
fun HomeScreen(
    vm: AppViewModel,
    onOpenItem: (Long) -> Unit,
    onOpenCollection: (title: String, libraries: List<Library>) -> Unit,
    onSearch: () -> Unit,
    onOpenSettings: () -> Unit,
) {
    val state by vm.home.collectAsStateWithLifecycle()
    val preferences by vm.preferences.collectAsStateWithLifecycle()
    val formFactor = currentFormFactor()
    val side = formFactor.horizontalPadding()
    val posterWidth = (preferences.posterSize.widthDp * formFactor.posterScale()).dp

    Column(Modifier.fillMaxSize()) {
        Row(
            Modifier.fillMaxWidth().padding(start = side, end = side - 8.dp, top = 14.dp, bottom = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                when (preferences.theme) {
                    ThemeId.Classic -> "plurx"
                    ThemeId.Terminal -> ":~\$ plurx ▊"
                    ThemeId.Noirr -> "noirr ▬"
                },
                color = Accent,
                fontSize = if (formFactor == FormFactor.Television) 32.sp else 26.sp,
                fontWeight = FontWeight.Bold,
            )
            Box(Modifier.weight(1f))
            vm.username?.let {
                Text(it, color = Muted, style = MaterialTheme.typography.labelMedium, modifier = Modifier.padding(end = 4.dp))
            }
            IconButton(onClick = vm::loadHome) {
                Icon(Icons.Filled.Refresh, contentDescription = "Refresh", tint = Muted)
            }
            IconButton(onClick = onSearch) {
                Icon(Icons.Filled.Search, contentDescription = "Search", tint = Muted)
            }
            IconButton(onClick = onOpenSettings) {
                Icon(Icons.Filled.Settings, contentDescription = "Settings", tint = Muted)
            }
        }

        when {
            state.loading -> LoadingBox()
            state.error != null -> Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                Text(state.error!!, color = Muted)
            }
            else -> {
                val collections = homeCollections(state.libraries, state.libraryItems, preferences.homeGrouping)
                LazyColumn(contentPadding = androidx.compose.foundation.layout.PaddingValues(bottom = 32.dp)) {
                    item { MediaRow("Continue watching", state.hubs.continue_watching, posterWidth, onOpen = { onOpenItem(it.id) }) }
                    item { MediaRow("Next up", state.hubs.next_up, posterWidth, onOpen = { onOpenItem(it.id) }) }
                    item { MediaRow("Recently added", state.hubs.recently_added, posterWidth, onOpen = { onOpenItem(it.id) }) }

                    if (state.libraries.isNotEmpty()) {
                        item {
                            Row(
                                Modifier.fillMaxWidth().padding(horizontal = side, vertical = 12.dp),
                                horizontalArrangement = Arrangement.End,
                            ) {
                                ChoicePicker(
                                    label = "Group by",
                                    value = preferences.homeGrouping,
                                    options = HomeGrouping.entries,
                                    optionLabel = { it.label },
                                    onSelect = vm::setHomeGrouping,
                                    modifier = Modifier.fillMaxWidth(if (formFactor == FormFactor.Compact) 1f else .32f),
                                )
                            }
                        }
                    }

                    collections.forEach { collection ->
                        item(key = "collection-${collection.title}") {
                            MediaRow(
                                title = collection.title,
                                items = collection.items,
                                posterWidth = posterWidth,
                                onViewAll = { onOpenCollection(collection.title, collection.libraries) },
                                onOpen = { onOpenItem(it.id) },
                            )
                        }
                    }

                    val empty = state.hubs.continue_watching.isEmpty() &&
                        state.hubs.next_up.isEmpty() && state.hubs.recently_added.isEmpty() &&
                        state.libraries.isEmpty()
                    if (empty) {
                        item {
                            Box(Modifier.fillMaxWidth().padding(40.dp), contentAlignment = Alignment.Center) {
                                Text("Nothing here yet — add a library on your server.", color = Muted)
                            }
                        }
                    }
                }
            }
        }
    }
}

private fun homeCollections(
    libraries: List<Library>,
    items: Map<Long, List<Item>>,
    grouping: HomeGrouping,
): List<HomeCollection> {
    if (grouping == HomeGrouping.Library) {
        return libraries.map { lib -> HomeCollection(lib.name, listOf(lib), items[lib.id].orEmpty()) }
    }
    return libraries
        .groupBy { it.kind }
        .entries
        .sortedWith(compareBy({ libraryKindOrder(it.key) }, { it.key }))
        .map { (kind, libs) ->
            val title = when (kind) {
                "movie", "movies" -> "Movies"
                "show", "shows" -> "TV shows"
                "home" -> "Home videos"
                else -> kind.replaceFirstChar { it.uppercase() }
            }
            HomeCollection(
                title = title,
                libraries = libs,
                items = libs.flatMap { items[it.id].orEmpty() }.distinctBy { it.id }.take(24),
            )
        }
}

private fun libraryKindOrder(kind: String): Int = when (kind) {
    "movie", "movies" -> 0
    "show", "shows" -> 1
    "home" -> 2
    else -> Int.MAX_VALUE
}
