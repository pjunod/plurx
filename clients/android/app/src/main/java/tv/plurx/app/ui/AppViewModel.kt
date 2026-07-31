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
import tv.plurx.app.data.Appearance
import tv.plurx.app.data.AudioOffsetReq
import tv.plurx.app.data.HomeGrouping
import tv.plurx.app.data.PlaybackQuality
import tv.plurx.app.data.PosterSize
import tv.plurx.app.data.ThemeId
import tv.plurx.app.data.User
import tv.plurx.app.data.ViewerPreferences
import tv.plurx.app.data.CreateSessionReq
import tv.plurx.app.data.Decision
import tv.plurx.app.data.HlsStart
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
    val libraryItems: Map<Long, List<Item>> = emptyMap(),
    val loading: Boolean = true,
    val error: String? = null,
)

data class PlaybackTarget(val itemId: Long, val fileId: Long, val startMs: Long = 0)

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

    private val _preferences = MutableStateFlow(ViewerPreferences())
    val preferences: StateFlow<ViewerPreferences> = _preferences.asStateFlow()

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
    var currentUser: User? = null
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
            _preferences.value = saved.preferences

            when {
                saved.origin.isNotBlank() && saved.token != null -> {
                    Session.origin = saved.origin
                    Session.token = saved.token
                    api = Net.api(saved.origin)
                    try {
                        currentUser = api().me()
                        username = currentUser?.username
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
                currentUser = resp.user
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
                val previews = libs.associate { lib ->
                    lib.id to api().libraryItems(lib.id, limit = 24, sort = "added").items
                }
                _home.value = HomeState(
                    hubs = hubs,
                    libraries = libs,
                    libraryItems = previews,
                    loading = false,
                )
            } catch (e: Exception) {
                _home.value = _home.value.copy(loading = false, error = e.message ?: "Failed to load")
            }
        }
    }

    fun logout() {
        viewModelScope.launch { settings.clearToken() }
        Session.token = null
        currentUser = null
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

    fun setTheme(theme: ThemeId) = updatePreferences { copy(theme = theme) }

    fun setAppearance(appearance: Appearance) = updatePreferences { copy(appearance = appearance) }

    fun setPosterSize(size: PosterSize) = updatePreferences { copy(posterSize = size) }

    fun setHomeGrouping(grouping: HomeGrouping) = updatePreferences { copy(homeGrouping = grouping) }

    fun setPlaybackQuality(quality: PlaybackQuality) = updatePreferences { copy(playbackQuality = quality) }

    fun setAutoSkip(enabled: Boolean) = updatePreferences { copy(autoSkip = enabled) }

    fun setAutoplayNext(enabled: Boolean) = updatePreferences { copy(autoplayNext = enabled) }

    private fun updatePreferences(change: ViewerPreferences.() -> ViewerPreferences) {
        val updated = _preferences.value.change()
        _preferences.value = updated
        viewModelScope.launch { settings.saveViewerPreferences(updated) }
    }

    // ---- Suspend loaders used by individual screens --------------------------

    suspend fun libraryItems(id: Long, sort: String = "title"): List<Item> {
        val result = mutableListOf<Item>()
        var offset = 0
        do {
            val page = api().libraryItems(id, limit = 200, offset = offset, sort = sort)
            result += page.items
            offset += page.items.size
        } while (page.items.isNotEmpty() && offset < page.total)
        return result
    }

    suspend fun search(query: String): List<Item> = api().search(query.trim()).results

    suspend fun itemDetail(id: Long): ItemDetail = api().item(id)

    suspend fun decision(fileId: Long): Decision = api().decision(
        fileId,
        caps() + ("force" to _preferences.value.playbackQuality.storageValue),
    )

    suspend fun setWatched(itemId: Long, watched: Boolean): Int = if (watched) {
        api().markWatched(itemId).updated
    } else {
        api().markUnwatched(itemId).updated
    }

    suspend fun setAudioOffset(fileId: Long, offsetMs: Long): Long =
        api().setAudioOffset(fileId, AudioOffsetReq(offsetMs)).audio_offset_ms

    suspend fun nextEpisode(itemId: Long): PlaybackTarget? {
        val current = itemDetail(itemId)
        if (current.item.kind != "episode") return null
        val season = current.ancestors.lastOrNull() ?: return null
        val show = current.ancestors.dropLast(1).lastOrNull()

        val seasonDetail = itemDetail(season.id)
        val episodes = seasonDetail.children.filter { it.kind == "episode" }
        val index = episodes.indexOfFirst { it.id == itemId }
        var next = episodes.getOrNull(index + 1)

        if (next == null && show != null) {
            val showDetail = itemDetail(show.id)
            val seasons = showDetail.children.filter { it.kind == "season" }
            val seasonIndex = seasons.indexOfFirst { it.id == season.id }
            val nextSeason = seasons.getOrNull(seasonIndex + 1)
            if (nextSeason != null) {
                next = itemDetail(nextSeason.id).children.firstOrNull { it.kind == "episode" }
            }
        }

        val item = next ?: return null
        val detail = itemDetail(item.id)
        val file = detail.files.firstOrNull { it.available } ?: return null
        return PlaybackTarget(item.id, file.id)
    }

    suspend fun createHlsSession(fileId: Long, body: CreateSessionReq): HlsStart =
        api().createHlsSession(fileId, body)

    /**
     * Fire-and-forget session release, on [viewModelScope] for the same reason
     * as [postProgress]: teardown can't await, and the scope outlives the
     * screen. Best-effort — the route is idempotent, and the stream is over
     * either way.
     */
    fun endHlsSession(sessionId: String) {
        viewModelScope.launch {
            try {
                api().endHlsSession(sessionId)
            } catch (_: Exception) {
                // The reaper remains the backstop.
            }
        }
    }

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
