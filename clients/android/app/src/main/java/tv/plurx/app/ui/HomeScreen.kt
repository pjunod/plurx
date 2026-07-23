package tv.plurx.app.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.focusable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
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
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import tv.plurx.app.data.Library
import tv.plurx.app.ui.components.LoadingBox
import tv.plurx.app.ui.components.MediaRow
import tv.plurx.app.ui.theme.Accent
import tv.plurx.app.ui.theme.Muted
import tv.plurx.app.ui.theme.Outline
import tv.plurx.app.ui.theme.SurfaceHi

@Composable
fun HomeScreen(
    vm: AppViewModel,
    onOpenItem: (Long) -> Unit,
    onOpenLibrary: (Library) -> Unit,
    onOpenSettings: () -> Unit,
) {
    val state by vm.home.collectAsStateWithLifecycle()

    Column(Modifier.fillMaxSize()) {
        Row(
            Modifier.fillMaxWidth().padding(start = 20.dp, end = 8.dp, top = 14.dp, bottom = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text("plurx", color = Accent, fontSize = 26.sp, style = MaterialTheme.typography.headlineMedium)
            Box(Modifier.weight(1f))
            vm.username?.let {
                Text(it, color = Muted, style = MaterialTheme.typography.labelMedium, modifier = Modifier.padding(end = 4.dp))
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
            else -> LazyColumn(contentPadding = androidx.compose.foundation.layout.PaddingValues(bottom = 24.dp)) {
                item {
                    if (state.libraries.isNotEmpty()) {
                        Text(
                            "Libraries",
                            style = MaterialTheme.typography.titleMedium,
                            modifier = Modifier.padding(start = 20.dp, top = 10.dp, bottom = 10.dp),
                        )
                        LazyRow(
                            contentPadding = androidx.compose.foundation.layout.PaddingValues(horizontal = 20.dp),
                            horizontalArrangement = Arrangement.spacedBy(12.dp),
                        ) {
                            items(state.libraries, key = { it.id }) { lib ->
                                LibraryChip(lib) { onOpenLibrary(lib) }
                            }
                        }
                    }
                }
                item { MediaRow("Continue Watching", state.hubs.continue_watching) { onOpenItem(it.id) } }
                item { MediaRow("Next Up", state.hubs.next_up) { onOpenItem(it.id) } }
                item { MediaRow("Recently Added", state.hubs.recently_added) { onOpenItem(it.id) } }

                val empty = state.hubs.continue_watching.isEmpty() &&
                    state.hubs.next_up.isEmpty() &&
                    state.hubs.recently_added.isEmpty()
                if (empty && state.libraries.isEmpty()) {
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

@Composable
private fun LibraryChip(lib: Library, onClick: () -> Unit) {
    var focused by remember { mutableStateOf(false) }
    Box(
        Modifier
            .clip(RoundedCornerShape(10.dp))
            .background(SurfaceHi)
            .border(
                width = if (focused) 2.dp else 1.dp,
                color = if (focused) Accent else Outline,
                shape = RoundedCornerShape(10.dp),
            )
            .onFocusChanged { focused = it.isFocused }
            .focusable()
            .clickable { onClick() }
            .padding(horizontal = 20.dp, vertical = 16.dp),
    ) {
        Column {
            Text(lib.name, style = MaterialTheme.typography.titleMedium, fontWeight = FontWeight.Bold)
            Text(
                lib.kind.replaceFirstChar { it.uppercase() },
                color = Muted,
                style = MaterialTheme.typography.labelMedium,
            )
        }
    }
}
