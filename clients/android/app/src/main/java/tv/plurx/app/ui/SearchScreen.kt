package tv.plurx.app.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Search
import androidx.compose.material3.Icon
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.delay
import tv.plurx.app.data.ConnectionFailure
import tv.plurx.app.data.ConnectionSurface
import tv.plurx.app.data.connectionSurfaceFor
import tv.plurx.app.data.Item
import tv.plurx.app.ui.components.ConnectionErrorBanner
import tv.plurx.app.ui.components.ConnectionErrorState
import tv.plurx.app.ui.components.LoadingBox
import tv.plurx.app.ui.components.PosterCard
import tv.plurx.app.ui.components.SafeTopRow
import tv.plurx.app.ui.components.TvIconButton
import tv.plurx.app.ui.theme.Muted

@Composable
fun SearchScreen(
    vm: AppViewModel,
    onOpenItem: (Long) -> Unit,
    onBack: () -> Unit,
) {
    val preferences by vm.preferences.collectAsStateWithLifecycle()
    val formFactor = currentFormFactor()
    val side = formFactor.horizontalPadding()
    val posterWidth = (preferences.posterSize.widthDp * formFactor.posterScale()).dp
    // Retained across rotation and process death: retyping a query on a TV
    // remote because the app was backgrounded is the whole reason this exists.
    var query by rememberSaveable { mutableStateOf("") }
    var results by remember { mutableStateOf<List<Item>>(emptyList()) }
    var loading by remember { mutableStateOf(false) }
    var failure by remember { mutableStateOf<ConnectionFailure?>(null) }
    // Retry re-runs the effect without the viewer retyping the query.
    var retry by remember { mutableIntStateOf(0) }

    LaunchedEffect(query, retry) {
        if (query.isBlank()) {
            results = emptyList()
            loading = false
            failure = null
            return@LaunchedEffect
        }
        delay(300)
        loading = true
        failure = null
        try {
            results = vm.search(query)
        } catch (cancelled: CancellationException) {
            // The next keystroke superseding this query, not a failed search.
            // Reporting it flashed "StandaloneCoroutine was cancelled" under
            // the field for the length of the debounce on every fast typist.
            throw cancelled
        } catch (e: Exception) {
            // `results` is deliberately left alone — a failed re-search must
            // not sweep away what the viewer is still reading.
            failure = vm.connectionFailure(e)
        } finally {
            loading = false
        }
    }

    val surface = connectionSurfaceFor(failure, results.isNotEmpty())

    Column(Modifier.fillMaxSize().navigationBarsPadding().imePadding()) {
        SafeTopRow(
            Modifier.fillMaxWidth().padding(start = side - 12.dp, end = side, top = 8.dp),
        ) {
            TvIconButton(onClick = onBack) {
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
            }
            OutlinedTextField(
                value = query,
                onValueChange = { query = it },
                modifier = Modifier.weight(1f),
                placeholder = { Text("Search movies, shows, episodes, tags…") },
                leadingIcon = { Icon(Icons.Filled.Search, contentDescription = null) },
                trailingIcon = if (query.isNotEmpty()) {
                    { TvIconButton(onClick = { query = "" }) { Icon(Icons.Filled.Close, contentDescription = "Clear") } }
                } else null,
                singleLine = true,
                keyboardOptions = KeyboardOptions(imeAction = ImeAction.Search),
            )
        }

        // A search that failed over results already on screen keeps them and
        // says so in one line (`cached_content_wins`) — the viewer was probably
        // still reading them.
        if (surface == ConnectionSurface.Banner) {
            ConnectionErrorBanner(
                failure = failure!!,
                server = vm.serverLabel,
                onRetry = { retry++ },
            )
        }

        when {
            loading -> LoadingBox()
            surface == ConnectionSurface.Full -> ConnectionErrorState(
                failure = failure!!,
                server = vm.serverLabel,
                onRetry = { retry++ },
            )
            query.isBlank() -> Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                Text("Search your library", color = Muted)
            }
            results.isEmpty() -> Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                Text("No results for “$query”", color = Muted)
            }
            else -> LazyVerticalGrid(
                columns = GridCells.Adaptive(posterWidth),
                contentPadding = PaddingValues(start = side, end = side, top = 20.dp, bottom = 32.dp),
                horizontalArrangement = Arrangement.spacedBy(16.dp),
                verticalArrangement = Arrangement.spacedBy(22.dp),
            ) {
                items(results, key = { it.id }) { item ->
                    PosterCard(item, width = posterWidth) { onOpenItem(item.id) }
                }
            }
        }
    }
}
