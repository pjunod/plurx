package tv.plurx.app.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
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
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import tv.plurx.app.data.Item
import tv.plurx.app.ui.components.LoadingBox
import tv.plurx.app.ui.components.PosterCard
import tv.plurx.app.ui.theme.Muted

@Composable
fun LibraryScreen(
    vm: AppViewModel,
    libraryId: Long,
    title: String,
    onOpenItem: (Long) -> Unit,
    onBack: () -> Unit,
) {
    val items by produceState<List<Item>?>(initialValue = null, libraryId) {
        value = try {
            vm.libraryItems(libraryId)
        } catch (_: Exception) {
            emptyList()
        }
    }

    Column(Modifier.fillMaxSize()) {
        Row(
            Modifier.fillMaxWidth().padding(start = 4.dp, end = 20.dp, top = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            IconButton(onClick = onBack) {
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
            }
            Text(title, style = MaterialTheme.typography.titleLarge)
        }

        when (val list = items) {
            null -> LoadingBox()
            else -> if (list.isEmpty()) {
                Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                    Text("This library is empty.", color = Muted)
                }
            } else {
                LazyVerticalGrid(
                    columns = GridCells.Adaptive(minSize = 118.dp),
                    contentPadding = PaddingValues(20.dp),
                    horizontalArrangement = Arrangement.spacedBy(14.dp),
                    verticalArrangement = Arrangement.spacedBy(18.dp),
                ) {
                    items(list, key = { it.id }) { item ->
                        PosterCard(item, width = 118.dp) { onOpenItem(item.id) }
                    }
                }
            }
        }
    }
}
