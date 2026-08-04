package tv.plurx.app.ui.components

import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.focusGroup
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.scale
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusProperties
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.Dp
import coil.compose.AsyncImage
import coil.request.ImageRequest
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import tv.plurx.app.data.Item
import tv.plurx.app.data.Session
import tv.plurx.app.ui.theme.Accent
import tv.plurx.app.ui.theme.Muted
import tv.plurx.app.ui.theme.SurfaceHi
import tv.plurx.app.ui.theme.Outline

enum class PosterResolutionPlacement {
    ArtworkOverlay,
    BelowArtwork,
}

/** Absolute image URL for a server-relative poster/backdrop path (or null). */
fun imageUrl(path: String?): String? = path?.let { Session.url(it) }

@Composable
fun NetworkImage(url: String?, modifier: Modifier = Modifier) {
    if (url == null) {
        Box(modifier.background(SurfaceHi))
        return
    }
    AsyncImage(
        model = ImageRequest.Builder(LocalContext.current).data(url).crossfade(true).build(),
        contentDescription = null,
        modifier = modifier,
        contentScale = androidx.compose.ui.layout.ContentScale.Crop,
    )
}

/**
 * A poster tile that grows and gains a red outline when focused — the D-pad
 * affordance on Android TV, and a pleasant hover on touch. For episodes it
 * shows "S1 E3", for everything else the year.
 */
@Composable
fun PosterCard(
    item: Item,
    modifier: Modifier = Modifier,
    width: Dp = 128.dp,
    resolutionPlacement: PosterResolutionPlacement = PosterResolutionPlacement.ArtworkOverlay,
    onClick: () -> Unit,
) {
    var focused by remember { mutableStateOf(false) }
    val scale by animateFloatAsState(if (focused) 1.08f else 1f, label = "poster-scale")
    val focusRing = tvFocusGradient()

    Column(
        modifier
            .width(width)
            .scale(scale)
            .onFocusChanged { focused = it.isFocused }
            // One focus target, not two. `clickable` already delegates a
            // focusable node, so the trailing `focusable()` was a second focus
            // target on the same layout node. Removing it is hygiene, not a
            // bug fix: the outer target is the one focus search collects, so
            // the ring still lit and centre press still worked. The behaviour
            // that *is* pinned by a test is that the focused card is the node
            // carrying the click — see ShelfFocusTest.
            .clickable(interactionSource = remember { MutableInteractionSource() }, indication = null) { onClick() }
    ) {
        Box(
            Modifier
                .fillMaxWidth()
                .testTag("poster-artwork")
                .aspectRatio(2f / 3f)
                .clip(MaterialTheme.shapes.medium)
                .background(SurfaceHi)
                .then(
                    if (focused) {
                        Modifier.border(2.dp, focusRing, MaterialTheme.shapes.medium)
                    } else {
                        Modifier
                    },
                )
        ) {
            NetworkImage(imageUrl(item.poster), Modifier.fillMaxSize())
            if (resolutionPlacement == PosterResolutionPlacement.ArtworkOverlay) item.resolution?.let { height ->
                Text(
                    resolutionLabel(height),
                    color = Color.White,
                    style = MaterialTheme.typography.labelMedium,
                    modifier = Modifier
                        .testTag("poster-resolution-overlay")
                        .align(Alignment.TopEnd)
                        .padding(5.dp)
                        .background(Color(0xB3000000), MaterialTheme.shapes.small)
                        .padding(horizontal = 6.dp, vertical = 3.dp),
                )
            }
            if (item.watch?.watched == true) {
                Text(
                    "✓",
                    color = Color.White,
                    fontWeight = FontWeight.Bold,
                    modifier = Modifier
                        .align(Alignment.TopStart)
                        .padding(5.dp)
                        .background(Accent, MaterialTheme.shapes.small)
                        .padding(horizontal = 6.dp, vertical = 2.dp),
                )
            }
            item.watch?.let { w ->
                val pct = progressFraction(w.position_ms, w.duration_ms ?: item.runtime_ms)
                if (pct > 0f) {
                    Box(
                        Modifier
                            .align(Alignment.BottomStart)
                            .fillMaxWidth(pct)
                            .height(3.dp)
                            .background(Accent)
                    )
                }
            }
        }
        Text(
            item.title,
            style = MaterialTheme.typography.labelMedium,
            fontWeight = FontWeight.SemiBold,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.padding(top = 6.dp),
        )
        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            Text(
                subtitleFor(item),
                style = MaterialTheme.typography.labelMedium,
                color = Muted,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f),
            )
            if (resolutionPlacement == PosterResolutionPlacement.BelowArtwork) item.resolution?.let { height ->
                itemResolutionFact(height)?.let { fact ->
                    MediaFactChip(
                        fact,
                        Modifier.testTag("poster-resolution-metadata").padding(start = 6.dp),
                    )
                }
            }
        }
    }
}

@Composable
fun MediaRow(
    title: String,
    items: List<Item>,
    posterWidth: Dp = 128.dp,
    resolutionPlacement: PosterResolutionPlacement = PosterResolutionPlacement.ArtworkOverlay,
    onViewAll: (() -> Unit)? = null,
    rowFocusRequester: FocusRequester? = null,
    previousRowFocusRequester: FocusRequester? = null,
    nextRowFocusRequester: FocusRequester? = null,
    onOpen: (Item) -> Unit,
) {
    if (items.isEmpty()) return
    val viewAllFocusRequester = remember { FocusRequester() }
    Column(Modifier.padding(vertical = 10.dp)) {
        androidx.compose.foundation.layout.Row(
            Modifier.fillMaxWidth().padding(start = 20.dp, end = 12.dp, bottom = 10.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(title, style = MaterialTheme.typography.titleMedium, modifier = Modifier.weight(1f))
            if (onViewAll != null) {
                Text(
                    "View all",
                    color = Accent,
                    style = MaterialTheme.typography.labelLarge,
                    modifier = Modifier
                        .focusRequester(viewAllFocusRequester)
                        .focusProperties {
                            if (previousRowFocusRequester != null) up = previousRowFocusRequester
                            if (rowFocusRequester != null) down = rowFocusRequester
                        }
                        .tvFocusRing(MaterialTheme.shapes.small)
                        .clickable(onClick = onViewAll)
                        .padding(8.dp),
                )
            }
        }
        // The two halves of a shelf's focus wiring live in different places,
        // and they have to.
        //
        // *The requester* belongs on the row container. It used to ride on the
        // card at index 0, which a `LazyRow` disposes the moment the viewer
        // scrolls past it; every neighbour aiming `up`/`down` at this shelf was
        // then calling `requestFocus()` on a requester attached to nothing —
        // `IllegalStateException: FocusRequester is not initialized`. The row
        // survives scrolling because the row is what exists, and `focusGroup()`
        // gives the requester a focus target of our own to bind to, which then
        // hands focus to whichever card is on screen.
        //
        // *The `up`/`down` overrides* cannot follow it out here. A card's
        // `FocusTargetNode` resolves its properties with
        // `visitSelfAndAncestors(FocusProperties, untilType = FocusTarget)` —
        // the walk stops at the first ancestor focus target. `LazyRow` carries
        // `Modifier.scrollable` internally, and `ScrollableNode` delegates a
        // `FocusTargetModifierNode` (`Focusability.Never`), which sits between
        // the cards and anything the caller hangs on the row. A
        // `focusProperties` block declared out here is therefore invisible to
        // every card inside it. It goes on the card — where it costs nothing,
        // because what it points at is the neighbour's *container*, and
        // containers do not dispose.
        val upTarget = if (onViewAll != null) viewAllFocusRequester else previousRowFocusRequester
        val cardFocusOrder = remember(upTarget, nextRowFocusRequester) {
            Modifier.focusProperties {
                upTarget?.let { up = it }
                nextRowFocusRequester?.let { down = it }
            }
        }
        LazyRow(
            modifier = Modifier
                .testTag(shelfTestTag(title))
                .then(rowFocusRequester?.let { Modifier.focusRequester(it) } ?: Modifier)
                .focusGroup(),
            contentPadding = PaddingValues(horizontal = 20.dp),
            horizontalArrangement = Arrangement.spacedBy(14.dp),
        ) {
            items(items, key = { item -> item.id }) { item ->
                PosterCard(
                    item,
                    modifier = cardFocusOrder,
                    width = posterWidth,
                    resolutionPlacement = resolutionPlacement,
                ) { onOpen(item) }
            }
        }
    }
}

/** Stable handle on a shelf's scrolling row, for the D-pad focus tests. */
internal fun shelfTestTag(title: String): String = "shelf-$title"

@Composable
fun <T> ChoicePicker(
    label: String,
    value: T,
    options: List<T>,
    optionLabel: (T) -> String,
    onSelect: (T) -> Unit,
    modifier: Modifier = Modifier,
) {
    var open by remember { mutableStateOf(false) }
    val selectorShape = MaterialTheme.shapes.small
    Column(modifier) {
        Text(label, color = Muted, style = MaterialTheme.typography.labelMedium, modifier = Modifier.padding(bottom = 6.dp))
        Box {
            Text(
                optionLabel(value),
                fontWeight = FontWeight.SemiBold,
                modifier = Modifier
                    .fillMaxWidth()
                    // The focus transform must wrap the painted selector so the ring, surface,
                    // outline, and label scale as one shape without side gaps.
                    .tvFocusRing(selectorShape, focusedScale = 1.03f)
                    .background(SurfaceHi, selectorShape)
                    .border(1.dp, Outline, selectorShape)
                    // `clickable` carries the focus target; see PosterCard.
                    .clickable { open = true }
                    .padding(horizontal = 16.dp, vertical = 14.dp),
            )
            DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
                options.forEach { option ->
                    DropdownMenuItem(
                        modifier = Modifier.tvFocusRing(MaterialTheme.shapes.small, focusedScale = 1.02f),
                        text = {
                            Text(
                                optionLabel(option),
                                color = if (option == value) Accent else MaterialTheme.colorScheme.onSurface,
                            )
                        },
                        onClick = { onSelect(option); open = false },
                    )
                }
            }
        }
    }
}

@Composable
fun LoadingBox(modifier: Modifier = Modifier) {
    Box(modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
        CircularProgressIndicator(color = Accent)
    }
}

private fun subtitleFor(item: Item): String = when {
    item.kind == "folder" && item.child_count != null -> "${item.child_count} items"
    item.kind == "episode" && item.season_number != null && item.episode_number != null ->
        "S${item.season_number} E${item.episode_number}"
    item.year != null -> item.year.toString()
    item.show_title != null -> item.show_title
    else -> item.kind.replaceFirstChar { it.uppercase() }
}

private fun resolutionLabel(height: Long): String = when {
    height >= 2160 -> "4K"
    height >= 1440 -> "1440p"
    height >= 1080 -> "1080p"
    height >= 720 -> "720p"
    else -> "${height}p"
}

private fun progressFraction(positionMs: Long, durationMs: Long?): Float {
    if (durationMs == null || durationMs <= 0) return 0f
    return (positionMs.toFloat() / durationMs).coerceIn(0f, 1f)
}

/** mm:ss or h:mm:ss for the player scrubber. */
fun formatTime(ms: Long): String {
    if (ms <= 0) return "0:00"
    val totalSec = ms / 1000
    val h = totalSec / 3600
    val m = (totalSec % 3600) / 60
    val s = totalSec % 60
    return if (h > 0) "%d:%02d:%02d".format(h, m, s) else "%d:%02d".format(m, s)
}
