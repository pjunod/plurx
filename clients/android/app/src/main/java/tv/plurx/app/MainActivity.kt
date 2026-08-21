package tv.plurx.app

import android.net.Uri
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.ui.Modifier
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleEventEffect
import androidx.lifecycle.lifecycleScope
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.navigation.NavType
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.rememberNavController
import androidx.navigation.navArgument
import kotlinx.coroutines.launch
import tv.plurx.app.data.Caps
import tv.plurx.app.player.PlayerScreen
import tv.plurx.app.player.OfflinePlayerScreen
import tv.plurx.app.player.preplayRouteQuery
import tv.plurx.app.player.preplayTracksFromRoute
import tv.plurx.app.ui.AppViewModel
import tv.plurx.app.ui.ConnectScreen
import tv.plurx.app.ui.DetailScreen
import tv.plurx.app.ui.DownloadsScreen
import tv.plurx.app.ui.HomeScreen
import tv.plurx.app.ui.LibraryScreen
import tv.plurx.app.ui.LoginScreen
import tv.plurx.app.ui.OfflineBookReaderScreen
import tv.plurx.app.ui.Phase
import tv.plurx.app.ui.PhotoScreen
import tv.plurx.app.ui.ReaderScreen
import tv.plurx.app.ui.SearchScreen
import tv.plurx.app.ui.SettingsScreen
import tv.plurx.app.ui.components.LoadingBox
import tv.plurx.app.ui.theme.PlurxTheme

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Keep a real-hardware capability snapshot in logcat even before sign
        // in. Decoder/display regressions otherwise surface only as a later
        // server transcode, after the evidence that caused it is gone.
        lifecycleScope.launch { Caps.query(this@MainActivity) }
        setContent {
            val vm: AppViewModel = viewModel()
            val preferences by vm.preferences.collectAsStateWithLifecycle()
            PlurxTheme(preferences.theme, preferences.appearance) {
                Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
                    AppRoot(vm)
                }
            }
        }
    }
}

@Composable
private fun AppRoot(vm: AppViewModel) {
    val phase by vm.phase.collectAsStateWithLifecycle()
    val busy by vm.busy.collectAsStateWithLifecycle()
    val authError by vm.authError.collectAsStateWithLifecycle()

    LifecycleEventEffect(Lifecycle.Event.ON_START) { vm.onForeground() }
    LaunchedEffect(phase) {
        if (phase == Phase.Ready) vm.onForeground()
    }

    when (phase) {
        Phase.Loading -> LoadingBox()
        Phase.NeedServer -> ConnectScreen(vm, busy, authError)
        Phase.NeedLogin -> LoginScreen(vm, busy, authError)
        Phase.Ready -> MainNav(vm)
    }
}

@Composable
private fun MainNav(vm: AppViewModel) {
    val nav = rememberNavController()
    NavHost(navController = nav, startDestination = "home") {
        composable("home") {
            HomeScreen(
                vm = vm,
                onOpenItem = { id -> nav.navigate("detail/$id") },
                onOpenCollection = { title, libraries ->
                    nav.navigate("library/${libraries.joinToString(",") { it.id.toString() }}/${Uri.encode(title)}")
                },
                onSearch = { nav.navigate("search") },
                onOpenDownloads = { nav.navigate("downloads") },
                onOpenSettings = { nav.navigate("settings") },
            )
        }
        composable(
            "library/{ids}/{name}",
            arguments = listOf(
                navArgument("ids") { type = NavType.StringType },
                navArgument("name") { type = NavType.StringType },
            ),
        ) { entry ->
            LibraryScreen(
                vm = vm,
                libraryIds = entry.arguments!!.getString("ids").orEmpty().split(',').mapNotNull(String::toLongOrNull),
                title = entry.arguments!!.getString("name").orEmpty(),
                onOpenItem = { id -> nav.navigate("detail/$id") },
                onBack = { nav.popBackStack() },
            )
        }
        composable("search") {
            SearchScreen(
                vm = vm,
                onOpenItem = { id -> nav.navigate("detail/$id") },
                onBack = { nav.popBackStack() },
            )
        }
        composable(
            "detail/{id}",
            arguments = listOf(navArgument("id") { type = NavType.LongType }),
        ) { entry ->
            DetailScreen(
                vm = vm,
                itemId = entry.arguments!!.getLong("id"),
                onPlay = { itemId, fileId, startMs, tracks ->
                    nav.navigate("player/$itemId/$fileId/$startMs" + preplayRouteQuery(tracks))
                },
                onOpenItem = { id -> nav.navigate("detail/$id") },
                onViewPhoto = { id -> nav.navigate("photo/$id") },
                onRead = { itemId, fileId -> nav.navigate("reader/$itemId/$fileId") },
                onBack = { nav.popBackStack() },
            )
        }
        composable(
            "reader/{itemId}/{fileId}",
            arguments = listOf(
                navArgument("itemId") { type = NavType.LongType },
                navArgument("fileId") { type = NavType.LongType },
            ),
        ) { entry ->
            ReaderScreen(
                itemId = entry.arguments!!.getLong("itemId"),
                fileId = entry.arguments!!.getLong("fileId"),
                onExit = { nav.popBackStack() },
            )
        }
        composable(
            "photo/{id}",
            arguments = listOf(navArgument("id") { type = NavType.LongType }),
        ) { entry ->
            PhotoScreen(
                itemId = entry.arguments!!.getLong("id"),
                onBack = { nav.popBackStack() },
            )
        }
        composable("settings") {
            SettingsScreen(vm = vm, onBack = { nav.popBackStack() })
        }
        composable("downloads") {
            DownloadsScreen(
                vm = vm,
                onPlay = { id -> nav.navigate("offline/$id") },
                onRead = { id -> nav.navigate("offline-book/$id") },
                onBack = { nav.popBackStack() },
            )
        }
        composable(
            "offline-book/{id}",
            arguments = listOf(navArgument("id") { type = NavType.StringType }),
        ) { entry ->
            OfflineBookReaderScreen(
                bookId = entry.arguments!!.getString("id").orEmpty(),
                onExit = { nav.popBackStack() },
            )
        }
        composable(
            "offline/{id}",
            arguments = listOf(navArgument("id") { type = NavType.StringType }),
        ) { entry ->
            OfflinePlayerScreen(
                downloadId = entry.arguments!!.getString("id").orEmpty(),
                onExit = { nav.popBackStack() },
            )
        }
        composable(
            // `audio` and `subtitle` are the viewer's pre-play choice and are
            // optional: an ordinary Play navigates to exactly the route it
            // always did, and the next episode below carries neither — the
            // choice belongs to one playback, not to the queue.
            "player/{itemId}/{fileId}/{startMs}?audio={audio}&subtitle={subtitle}",
            arguments = listOf(
                navArgument("itemId") { type = NavType.LongType },
                navArgument("fileId") { type = NavType.LongType },
                navArgument("startMs") { type = NavType.LongType },
                navArgument("audio") {
                    type = NavType.StringType
                    nullable = true
                    defaultValue = null
                },
                navArgument("subtitle") {
                    type = NavType.StringType
                    nullable = true
                    defaultValue = null
                },
            ),
        ) { entry ->
            val a = entry.arguments!!
            PlayerScreen(
                vm = vm,
                itemId = a.getLong("itemId"),
                fileId = a.getLong("fileId"),
                startMs = a.getLong("startMs"),
                preplayTracks = preplayTracksFromRoute(
                    audio = a.getString("audio"),
                    subtitle = a.getString("subtitle"),
                ),
                onPlayNext = { target ->
                    nav.navigate("detail/${target.itemId}") { popUpTo("home") }
                    nav.navigate("player/${target.itemId}/${target.fileId}/${target.startMs}")
                },
                onExit = { nav.popBackStack() },
            )
        }
    }
}
