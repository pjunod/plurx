package tv.plurx.app.ui

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import tv.plurx.app.data.Caps
import tv.plurx.app.data.Decision
import tv.plurx.app.data.Hubs
import tv.plurx.app.data.Item
import tv.plurx.app.data.ItemDetail
import tv.plurx.app.data.Library
import tv.plurx.app.data.LoginReq
import tv.plurx.app.data.Net
import tv.plurx.app.data.PlurxApi
import tv.plurx.app.data.ProgressReq
import tv.plurx.app.data.Session
import tv.plurx.app.data.SettingsStore

/** Top-level app state: which screen the shell should show. */
sealed interface Phase {
    data object Loading : Phase      // checking a saved session on launch
    data object NeedServer : Phase   // no server yet, or the saved one is gone
    data object NeedLogin : Phase    // server reachable, needs credentials
    data object Ready : Phase        // authenticated
}

data class HomeState(
    val hubs: Hubs = Hubs(),
    val libraries: List<Library> = emptyList(),
    val loading: Boolean = true,
    val error: String? = null,
)

/**
 * Single view-model for the whole app (manual DI — no Hilt). Owns the session
 * lifecycle (silent reconnect, connect, login, logout) and exposes suspend
 * loaders the screens call from `LaunchedEffect`. [Session] is the source of
 * truth the OkHttp interceptor reads, so setting a field here changes auth for
 * every subsequent request, image load, and video stream at once.
 */
class AppViewModel(app: Application) : AndroidViewModel(app) {

    private val settings = SettingsStore(app)

    private val _phase = MutableStateFlow<Phase>(Phase.Loading)
    val phase: StateFlow<Phase> = _phase.asStateFlow()

    private val _busy = MutableStateFlow(false)
    val busy: StateFlow<Boolean> = _busy.asStateFlow()

    private val _authError = MutableStateFlow<String?>(null)
    val authError: StateFlow<String?> = _authError.asStateFlow()

    private val _home = MutableStateFlow(HomeState())
    val home: StateFlow<HomeState> = _home.asStateFlow()

    var origin: String = ""
        private set
    var username: String? = null
        private set
    var serverName: String? = null
        private set
    var audioLang: String = "eng"
        private set
    var subLang: String = "eng"
        private set

    private var api: PlurxApi? = null
    fun api(): PlurxApi = api ?: error("not connected")

    /** Runtime playback caps for this device — sent to /decision and /stream.mp4. */
    fun caps(): Map<String, String> = Caps.query(getApplication())

    init {
        viewModelScope.launch {
            val saved = settings.flow.first()
            origin = saved.origin
            username = saved.username
            audioLang = saved.audioLang
            subLang = saved.subLang

            when {
                saved.origin.isNotBlank() && saved.token != null -> {
                    Session.origin = saved.origin
                    Session.token = saved.token
                    api = Net.api(saved.origin)
                    try {
                        username = api().me().username
                        _phase.value = Phase.Ready
                        loadHome()
                    } catch (_: Exception) {
                        // Token no longer valid (rotated, server reset) — re-auth.
                        Session.token = null
                        _phase.value = Phase.NeedLogin
                    }
                }
                saved.origin.isNotBlank() -> {
                    Session.origin = saved.origin
                    api = Net.api(saved.origin)
                    _phase.value = Phase.NeedLogin
                }
                else -> _phase.value = Phase.NeedServer
            }
        }
    }

    fun connect(raw: String) {
        val normalized = normalizeOrigin(raw)
        if (normalized.isBlank()) return
        _authError.value = null
        _busy.value = true
        viewModelScope.launch {
            try {
                Session.origin = normalized
                val a = Net.api(normalized)
                val info = a.server()
                origin = normalized
                api = a
                serverName = info.name
                settings.saveOrigin(normalized)
                _phase.value = Phase.NeedLogin
            } catch (_: Exception) {
                _authError.value = "Couldn't reach a plurx server at $normalized"
            } finally {
                _busy.value = false
            }
        }
    }

    fun login(user: String, pass: String) {
        _authError.value = null
        _busy.value = true
        viewModelScope.launch {
            try {
                val resp = api().login(LoginReq(username = user.trim(), password = pass))
                Session.token = resp.token
                username = resp.user.username
                settings.saveSession(origin, resp.token, resp.user.username)
                _phase.value = Phase.Ready
                loadHome()
            } catch (_: Exception) {
                _authError.value = "Wrong username or password"
            } finally {
                _busy.value = false
            }
        }
    }

    fun loadHome() {
        _home.value = _home.value.copy(loading = true, error = null)
        viewModelScope.launch {
            try {
                val hubs = api().hubs()
                val libs = api().libraries()
                _home.value = HomeState(hubs = hubs, libraries = libs, loading = false)
            } catch (e: Exception) {
                _home.value = _home.value.copy(loading = false, error = e.message ?: "Failed to load")
            }
        }
    }

    fun logout() {
        viewModelScope.launch { settings.clearToken() }
        Session.token = null
        _home.value = HomeState()
        _phase.value = Phase.NeedLogin
    }

    fun changeServer() {
        Session.token = null
        _phase.value = Phase.NeedServer
    }

    fun setLanguages(audio: String, sub: String) {
        audioLang = audio
        subLang = sub
        viewModelScope.launch { settings.saveLangs(audio, sub) }
    }

    // ---- Suspend loaders used by individual screens --------------------------

    suspend fun libraryItems(id: Long): List<Item> = api().libraryItems(id).items

    suspend fun itemDetail(id: Long): ItemDetail = api().item(id)

    suspend fun decision(fileId: Long): Decision = api().decision(fileId, caps())

    suspend fun reportProgress(itemId: Long, positionMs: Long, durationMs: Long?) {
        try {
            api().progress(itemId, ProgressReq(positionMs, durationMs))
        } catch (_: Exception) {
            // Progress is best-effort; a dropped beat shouldn't surface an error.
        }
    }

    /**
     * Fire-and-forget progress post for teardown (the player leaving composition
     * can't await a suspend call). Runs on [viewModelScope], which outlives the
     * screen, so the final position — and the server-side Trakt scrobble it
     * drives — still lands.
     */
    fun postProgress(itemId: Long, positionMs: Long, durationMs: Long?) {
        viewModelScope.launch { reportProgress(itemId, positionMs, durationMs) }
    }

    private fun normalizeOrigin(raw: String): String {
        var s = raw.trim()
        if (s.isEmpty()) return s
        if (!s.startsWith("http://") && !s.startsWith("https://")) s = "http://$s"
        return s.trimEnd('/')
    }
}
