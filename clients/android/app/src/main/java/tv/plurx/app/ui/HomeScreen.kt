package tv.plurx.app.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBars
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Search
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusProperties
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import tv.plurx.app.data.HomeGrouping
import tv.plurx.app.data.Item
import tv.plurx.app.data.Library
import tv.plurx.app.data.ThemeId
import tv.plurx.app.ui.components.ChoicePicker
import tv.plurx.app.ui.components.LoadingBox
import tv.plurx.app.ui.components.MediaFactChip
import tv.plurx.app.ui.components.MediaRow
import tv.plurx.app.ui.components.NetworkImage
import tv.plurx.app.ui.components.PosterResolutionPlacement
import tv.plurx.app.ui.components.safeDisplayInsets
import tv.plurx.app.ui.components.TvIconButton
import tv.plurx.app.ui.components.imageUrl
import tv.plurx.app.ui.components.itemResolutionFact
import tv.plurx.app.ui.theme.Accent
import tv.plurx.app.ui.theme.Muted

/** The "Group by" picker's place in the vertical D-pad chain. */
private const val GROUPING_KEY = "grouping"

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

    Column(Modifier.fillMaxSize().navigationBarsPadding()) {
        HomeTopBar(
            theme = preferences.theme,
            username = vm.username,
            formFactor = formFactor,
            side = side,
            onRefresh = vm::loadHome,
            onSearch = onSearch,
            onOpenSettings = onOpenSettings,
        )

        when {
            // Only a cold start gets a spinner. Once anything has arrived the
            // shelves stay up and fill in — a refresh must never blank a
            // dashboard the viewer is already reading, and neither must a
            // refresh that fails.
            !state.hasContent && state.loading -> LoadingBox()
            !state.hasContent && state.error != null -> Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                Text(state.error!!, color = Muted)
            }
            else -> {
                val collections = homeCollections(state.libraries, state.libraryItems, preferences.homeGrouping)
                val continueShelfItems = continueWatchingShelfItems(
                    state.hubs.continue_watching,
                    formFactor,
                )
                // Every vertical stop on this page, in the order a D-pad walks
                // them. The "Group by" picker is one of them: it used to sit
                // between the hub shelves and the library shelves while their
                // up/down pointed straight past it at each other, so the only
                // way to change the grouping was to have a touchscreen.
                val visibleShelfKeys = buildList {
                    if (continueShelfItems.isNotEmpty()) add("continue")
                    if (state.hubs.next_up.isNotEmpty()) add("next")
                    if (state.hubs.recently_added.isNotEmpty()) add("recent")
                    if (state.libraries.isNotEmpty()) add(GROUPING_KEY)
                    collections.filter { it.items.isNotEmpty() }.forEach {
                        add("collection-${it.title}")
                    }
                }
                val shelfFocus = remember(visibleShelfKeys) {
                    visibleShelfKeys.associateWith { FocusRequester() }
                }
                fun previousShelf(key: String): FocusRequester? {
                    val index = visibleShelfKeys.indexOf(key)
                    return visibleShelfKeys.getOrNull(index - 1)?.let { shelfFocus[it] }
                }
                fun nextShelf(key: String): FocusRequester? {
                    val index = visibleShelfKeys.indexOf(key)
                    return visibleShelfKeys.getOrNull(index + 1)?.let { shelfFocus[it] }
                }
                // Scrolled, not lazy — deliberately.
                //
                // Every stop above points `up`/`down` at the *next* stop's
                // `FocusRequester`, and a `FocusRequester` only works while the
                // node it is attached to is composed. In a `LazyColumn` each
                // shelf is an item, so scrolling one out of the window detached
                // its requester and the next D-pad press towards it threw
                // `IllegalStateException: FocusRequester is not initialized` —
                // strictly worse than the horizontal case, which merely
                // no-opped. Compose's own spatial focus search handles that by
                // composing beyond-bounds items mid-search, but a custom
                // `up`/`down` destination bypasses spatial search entirely, so
                // that rescue never runs.
                //
                // The alternative — drop the explicit chain and let spatial
                // search do the vertical walk — loses the "Group by" picker:
                // it is right-aligned, so from a card on the left of a shelf
                // the beam heuristic prefers the full-width shelf below it and
                // steps straight past. §7.1 asks for the picker to be
                // reachable, so the chain stays and the container stops being
                // lazy.
                //
                // The cost is bounded: this list holds three hub shelves, the
                // picker, and one shelf per library (or per library *kind*,
                // which is a handful) — and each shelf is still a `LazyRow`, so
                // per-shelf composition stays bounded by screen width rather
                // than by the 24 items it was handed. If a library count ever
                // reaches the dozens this needs revisiting.
                Column(
                    Modifier
                        .verticalScroll(rememberScrollState())
                        .padding(bottom = 32.dp)
                ) {
                    if (formFactor == FormFactor.Compact) {
                        state.hubs.continue_watching.firstOrNull()?.let { featured ->
                            CompactContinueHero(
                                item = featured,
                                side = side,
                                onOpen = { onOpenItem(featured.id) },
                            )
                        }
                    }
                    MediaRow(
                        "Continue watching",
                        continueShelfItems,
                        posterWidth,
                        resolutionPlacement = PosterResolutionPlacement.BelowArtwork,
                        rowFocusRequester = shelfFocus["continue"],
                        previousRowFocusRequester = previousShelf("continue"),
                        nextRowFocusRequester = nextShelf("continue"),
                        onOpen = { onOpenItem(it.id) },
                    )
                    MediaRow(
                        "Next up",
                        state.hubs.next_up,
                        posterWidth,
                        rowFocusRequester = shelfFocus["next"],
                        previousRowFocusRequester = previousShelf("next"),
                        nextRowFocusRequester = nextShelf("next"),
                        onOpen = { onOpenItem(it.id) },
                    )
                    MediaRow(
                        "Recently added",
                        state.hubs.recently_added,
                        posterWidth,
                        resolutionPlacement = PosterResolutionPlacement.BelowArtwork,
                        rowFocusRequester = shelfFocus["recent"],
                        previousRowFocusRequester = previousShelf("recent"),
                        nextRowFocusRequester = nextShelf("recent"),
                        onOpen = { onOpenItem(it.id) },
                    )

                    if (state.libraries.isNotEmpty()) {
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
                                modifier = Modifier
                                    .fillMaxWidth(if (formFactor == FormFactor.Compact) 1f else .32f)
                                    .then(
                                        shelfFocus[GROUPING_KEY]
                                            ?.let { Modifier.focusRequester(it) } ?: Modifier,
                                    )
                                    .focusProperties {
                                        previousShelf(GROUPING_KEY)?.let { up = it }
                                        nextShelf(GROUPING_KEY)?.let { down = it }
                                    },
                            )
                        }
                    }

                    collections.forEach { collection ->
                        val key = "collection-${collection.title}"
                        MediaRow(
                            title = collection.title,
                            items = collection.items,
                            posterWidth = posterWidth,
                            onViewAll = { onOpenCollection(collection.title, collection.libraries) },
                            rowFocusRequester = shelfFocus[key],
                            previousRowFocusRequester = previousShelf(key),
                            nextRowFocusRequester = nextShelf(key),
                            onOpen = { onOpenItem(it.id) },
                        )
                    }

                    val empty = state.hubs.continue_watching.isEmpty() &&
                        state.hubs.next_up.isEmpty() && state.hubs.recently_added.isEmpty() &&
                        state.libraries.isEmpty()
                    if (empty) {
                        Box(Modifier.fillMaxWidth().padding(40.dp), contentAlignment = Alignment.Center) {
                            Text("Nothing here yet — add a library on your server.", color = Muted)
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun CompactContinueHero(
    item: Item,
    side: Dp,
    onOpen: () -> Unit,
) {
    val progress = compactProgress(item)
    Box(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = side, vertical = 8.dp)
            .height(238.dp)
            .clip(MaterialTheme.shapes.large)
            .clickable(onClick = onOpen),
    ) {
        NetworkImage(imageUrl(item.backdrop ?: item.poster), Modifier.fillMaxSize())
        Box(
            Modifier.fillMaxSize().background(
                Brush.verticalGradient(
                    listOf(Color.Transparent, Color(0x33000000), Color(0xF2111114)),
                ),
            ),
        )
        Column(
            Modifier.align(Alignment.BottomStart).fillMaxWidth().padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(7.dp),
        ) {
            Text(
                "CONTINUE",
                color = Accent,
                fontSize = 10.sp,
                fontWeight = FontWeight.Bold,
                letterSpacing = 1.7.sp,
            )
            Text(
                item.show_title ?: item.title,
                color = Color.White,
                fontSize = 25.sp,
                fontWeight = FontWeight.ExtraBold,
                maxLines = 1,
            )
            if (item.show_title != null) {
                Text(
                    compactEpisodeSubtitle(item),
                    color = Color.White.copy(alpha = 0.78f),
                    style = MaterialTheme.typography.labelMedium,
                    maxLines = 1,
                )
            }
            Row(
                horizontalArrangement = Arrangement.spacedBy(12.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                compactHomeFacts(item).forEach { fact ->
                    Text(fact, color = Color.White.copy(alpha = 0.7f), style = MaterialTheme.typography.labelSmall)
                }
                itemResolutionFact(item.resolution)?.let { MediaFactChip(it) }
            }
            if (progress > 0f) {
                Box(Modifier.fillMaxWidth().height(3.dp).background(Color.White.copy(alpha = 0.2f))) {
                    Box(Modifier.fillMaxWidth(progress).height(3.dp).background(Accent))
                }
            }
            Row(
                horizontalArrangement = Arrangement.spacedBy(10.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Row(
                    Modifier.background(Accent, MaterialTheme.shapes.extraLarge)
                        .padding(horizontal = 14.dp, vertical = 9.dp),
                    horizontalArrangement = Arrangement.spacedBy(5.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Icon(Icons.Filled.PlayArrow, contentDescription = null, tint = Color.White, modifier = Modifier.size(18.dp))
                    Text("Resume", color = Color.White, style = MaterialTheme.typography.labelLarge, fontWeight = FontWeight.Bold)
                }
                compactTimeRemaining(item)?.let { remaining ->
                    Text(remaining, color = Color.White.copy(alpha = 0.58f), style = MaterialTheme.typography.labelSmall)
                }
            }
        }
    }
}

internal fun continueWatchingShelfItems(items: List<Item>, formFactor: FormFactor): List<Item> =
    if (formFactor == FormFactor.Compact) items.drop(1) else items

internal fun compactRuntimeLabel(milliseconds: Long): String {
    val totalMinutes = milliseconds / 60_000
    val hours = totalMinutes / 60
    val minutes = totalMinutes % 60
    return if (hours > 0) "${hours}h ${minutes}m" else "${minutes}m"
}

private fun compactHomeFacts(item: Item): List<String> = buildList {
    item.year?.let { add(it.toString()) }
    item.runtime_ms?.takeIf { it > 0 }?.let { add(compactRuntimeLabel(it)) }
}

private fun compactEpisodeSubtitle(item: Item): String = buildList {
    if (item.season_number != null && item.episode_number != null) {
        add("S${item.season_number} E${item.episode_number}")
    }
    add(item.title)
}.joinToString("  ")

private fun compactProgress(item: Item): Float {
    val watch = item.watch ?: return 0f
    val position = watch.position_ms
    val duration = watch.duration_ms ?: item.runtime_ms ?: return 0f
    if (duration <= 0) return 0f
    return (position.toFloat() / duration).coerceIn(0f, 1f)
}

private fun compactTimeRemaining(item: Item): String? {
    val watch = item.watch ?: return null
    val position = watch.position_ms
    val duration = watch.duration_ms ?: item.runtime_ms ?: return null
    if (duration <= position) return null
    return "${maxOf(1, (duration - position) / 60_000)}m left"
}

@Composable
internal fun HomeTopBar(
    theme: ThemeId,
    username: String?,
    formFactor: FormFactor,
    side: Dp,
    onRefresh: () -> Unit,
    onSearch: () -> Unit,
    onOpenSettings: () -> Unit,
    safeInsets: WindowInsets = safeDisplayInsets(),
) {
    Row(
        Modifier
            .fillMaxWidth()
            .windowInsetsPadding(safeInsets)
            .padding(start = side, end = side - 8.dp, top = 14.dp, bottom = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            when (theme) {
                ThemeId.Classic -> "cinema"
                ThemeId.Terminal -> ":~\$ cinema ▊"
                ThemeId.Noirr -> "noirr ▬"
            },
            color = Accent,
            fontSize = if (formFactor == FormFactor.Television) 32.sp else 26.sp,
            fontWeight = FontWeight.Bold,
        )
        Box(Modifier.weight(1f))
        username?.let {
            Text(it, color = Muted, style = MaterialTheme.typography.labelMedium, modifier = Modifier.padding(end = 4.dp))
        }
        TvIconButton(onClick = onRefresh) {
            Icon(Icons.Filled.Refresh, contentDescription = "Refresh", tint = Muted)
        }
        TvIconButton(onClick = onSearch) {
            Icon(Icons.Filled.Search, contentDescription = "Search", tint = Muted)
        }
        TvIconButton(onClick = onOpenSettings) {
            Icon(Icons.Filled.Settings, contentDescription = "Settings", tint = Muted)
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
