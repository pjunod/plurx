package tv.plurx.app

import android.net.Uri
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Modifier
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.navigation.NavType
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.rememberNavController
import androidx.navigation.navArgument
import tv.plurx.app.player.PlayerScreen
import tv.plurx.app.ui.AppViewModel
import tv.plurx.app.ui.ConnectScreen
import tv.plurx.app.ui.DetailScreen
import tv.plurx.app.ui.HomeScreen
import tv.plurx.app.ui.LibraryScreen
import tv.plurx.app.ui.LoginScreen
import tv.plurx.app.ui.Phase
import tv.plurx.app.ui.SettingsScreen
import tv.plurx.app.ui.components.LoadingBox
import tv.plurx.app.ui.theme.PlurxTheme

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            PlurxTheme {
                Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
                    AppRoot(viewModel())
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
                onOpenLibrary = { lib -> nav.navigate("library/${lib.id}/${Uri.encode(lib.name)}") },
                onOpenSettings = { nav.navigate("settings") },
            )
        }
        composable(
            "library/{id}/{name}",
            arguments = listOf(
                navArgument("id") { type = NavType.LongType },
                navArgument("name") { type = NavType.StringType },
            ),
        ) { entry ->
            LibraryScreen(
                vm = vm,
                libraryId = entry.arguments!!.getLong("id"),
                title = entry.arguments!!.getString("name").orEmpty(),
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
                onPlay = { itemId, fileId, startMs -> nav.navigate("player/$itemId/$fileId/$startMs") },
                onOpenItem = { id -> nav.navigate("detail/$id") },
                onBack = { nav.popBackStack() },
            )
        }
        composable("settings") {
            SettingsScreen(vm = vm, onBack = { nav.popBackStack() })
        }
        composable(
            "player/{itemId}/{fileId}/{startMs}",
            arguments = listOf(
                navArgument("itemId") { type = NavType.LongType },
                navArgument("fileId") { type = NavType.LongType },
                navArgument("startMs") { type = NavType.LongType },
            ),
        ) { entry ->
            val a = entry.arguments!!
            PlayerScreen(
                vm = vm,
                itemId = a.getLong("itemId"),
                fileId = a.getLong("fileId"),
                startMs = a.getLong("startMs"),
                onExit = { nav.popBackStack() },
            )
        }
    }
}
