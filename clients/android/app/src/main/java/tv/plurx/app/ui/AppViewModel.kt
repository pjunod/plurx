package tv.plurx.app.ui

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import retrofit2.HttpException
import java.net.URI
import tv.plurx.app.data.Caps
import tv.plurx.app.data.Appearance
import tv.plurx.app.data.HomeGrouping
import tv.plurx.app.data.PlaybackQuality
import tv.plurx.app.data.PosterSize
import tv.plurx.app.data.ThemeId
import tv.plurx.app.data.User
import tv.plurx.app.data.ViewerPreferences
import tv.plurx.app.data.CreateSessionReq
import tv.plurx.app.data.Decision
import tv.plurx.app.data.DiscoveredServer
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
import tv.plurx.app.data.ServerDiscovery
import tv.plurx.app.data.Server
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

data class EpisodePlaybackTarget(val episode: Item, val playback: PlaybackTarget)

/**
 * Single view-model for the whole app (manual DI — no Hilt). Owns the session
 * lifecycle (silent reconnect, connect, login, logout) and exposes suspend
 * loaders the screens call from `LaunchedEffect`. [Session] is the source of
 * truth the OkHttp interceptor reads, so setting a field here changes auth for
 * every subsequent request, image load, and video stream at once.
 */
class AppViewModel(app: Application) : AndroidViewModel(app) {

    private val settings = SettingsStore(app)
    private val serverBrowser = ServerDiscovery(app)

    private val _phase = MutableStateFlow<Phase>(Phase.Loading)
    val phase: StateFlow<Phase> = _phase.asStateFlow()

    private val _busy = MutableStateFlow(false)
    val busy: StateFlow<Boolean> = _busy.asStateFlow()

    private val _authError = MutableStateFlow<String?>(null)
    val authError: StateFlow<String?> = _authError.asStateFlow()

    val serverDiscovery = serverBrowser.state

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
                    bindOrigin(saved.origin, saved.token)
                    var validation = validateSavedSession { api().me() }
                    if (validation == SavedSessionValidation.ServerUnavailable) {
                        val recovered = rediscoverSavedServer(saved.instanceId, saved.origin)
                        if (recovered != null) {
                            bindOrigin(recovered.origin, saved.token)
                            serverName = recovered.info.name
                            settings.saveServerIdentity(
                                recovered.origin,
                                recovered.info.instance_id,
                            )
                            validation = validateSavedSession { api().me() }
                        }
                    }
                    when (validation) {
                        is SavedSessionValidation.Authenticated -> {
                            currentUser = validation.user
                            username = validation.user.username
                            if (saved.instanceId == null) backfillServerIdentity()
                            _phase.value = Phase.Ready
                            loadHome()
                        }
                        SavedSessionValidation.InvalidToken -> {
                            Session.token = null
                            settings.clearToken()
                            _phase.value = Phase.NeedLogin
                        }
                        SavedSessionValidation.ServerUnavailable -> {
                            // Google TV often launches before networking has fully resumed.
                            // Keep the saved credentials and let Home surface a retryable
                            // connection error instead of incorrectly demanding a login.
                            _phase.value = Phase.Ready
                            loadHome()
                        }
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
                connectToOrigin(normalized)
            } catch (_: Exception) {
                _authError.value = "Couldn't reach a plurx server at $normalized"
            } finally {
                _busy.value = false
            }
        }
    }

    fun connect(server: DiscoveredServer) {
        _authError.value = null
        _busy.value = true
        viewModelScope.launch {
            try {
                connectToOrigin(serverBrowser.resolve(server))
            } catch (e: Exception) {
                _authError.value = e.message
                    ?: "The discovered server stopped responding. Try scanning again."
            } finally {
                _busy.value = false
            }
        }
    }

    fun startServerDiscovery() = serverBrowser.start()

    fun stopServerDiscovery() = serverBrowser.stop()

    fun restartServerDiscovery() {
        serverBrowser.stop()
        serverBrowser.clear()
        viewModelScope.launch {
            delay(350)
            if (_phase.value == Phase.NeedServer) serverBrowser.start()
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

    suspend fun seriesPlayback(detail: ItemDetail): EpisodePlaybackTarget? = when (detail.item.kind) {
        "season" -> playableEpisode(orderedEpisodeCandidates(detail.children))
        "show" -> {
            val children = detail.children
            val seasons = orderedSeasonCandidates(children)
            if (seasons.isEmpty()) {
                playableEpisode(directShowEpisodeCandidates(children))
            } else {
                var target: EpisodePlaybackTarget? = null
                for (season in seasons) {
                    target = playableEpisode(orderedEpisodeCandidates(itemDetail(season.id).children))
                    if (target != null) break
                }
                target
            }
        }
        else -> null
    }

    suspend fun episodePlayback(episode: Item): EpisodePlaybackTarget? = playableEpisode(listOf(episode))

    private suspend fun playableEpisode(episodes: List<Item>): EpisodePlaybackTarget? {
        for (episode in episodes) {
            val detail = itemDetail(episode.id)
            val file = detail.files.firstOrNull { it.available } ?: continue
            val positionMs = detail.item.watch?.position_ms ?: episode.watch?.position_ms ?: 0L
            val durationMs = file.duration_ms ?: detail.item.runtime_ms ?: episode.runtime_ms
            return EpisodePlaybackTarget(
                episode = detail.item,
                playback = PlaybackTarget(
                    itemId = detail.item.id,
                    fileId = file.id,
                    startMs = resumableStartMs(positionMs, durationMs),
                ),
            )
        }
        return null
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

    private suspend fun connectToOrigin(normalized: String) {
        Session.origin = normalized
        val candidate = Net.api(normalized)
        val info = candidate.server()
        origin = normalized
        api = candidate
        serverName = info.name
        settings.saveOrigin(normalized, info.instance_id)
        _phase.value = Phase.NeedLogin
    }

    private fun bindOrigin(value: String, token: String?) {
        origin = value
        Session.origin = value
        Session.token = token
        api = Net.api(value)
    }

    private suspend fun rediscoverSavedServer(
        expectedInstanceId: String?,
        savedOrigin: String,
    ): RecoveredServer? {
        for (candidate in serverBrowser.availableServers()) {
            val candidateOrigin = runCatching { serverBrowser.resolve(candidate) }.getOrNull()
                ?: continue
            val info = runCatching { Net.api(candidateOrigin).server() }.getOrNull()
                ?: continue
            if (matchesSavedServer(info.instance_id, expectedInstanceId, savedOrigin)) {
                return RecoveredServer(candidateOrigin, info)
            }
        }
        return null
    }

    private suspend fun backfillServerIdentity() {
        val info = runCatching { api().server() }.getOrNull() ?: return
        serverName = info.name
        settings.saveServerIdentity(origin, info.instance_id)
    }
}

private data class RecoveredServer(val origin: String, val info: Server)

internal sealed interface SavedSessionValidation<out T> {
    data class Authenticated<T>(val user: T) : SavedSessionValidation<T>
    data object InvalidToken : SavedSessionValidation<Nothing>
    data object ServerUnavailable : SavedSessionValidation<Nothing>
}

/**
 * Validate a persisted login without confusing a waking network with an
 * expired token. Only an explicit authentication response may discard the
 * session; transport and server failures get a few brief retries.
 */
internal suspend fun <T> validateSavedSession(
    attempts: Int = 4,
    waitBeforeRetry: suspend (Long) -> Unit = { delay(it) },
    request: suspend () -> T,
): SavedSessionValidation<T> {
    require(attempts > 0)
    var retryDelayMs = 500L
    repeat(attempts) { attempt ->
        try {
            return SavedSessionValidation.Authenticated(request())
        } catch (cancelled: CancellationException) {
            throw cancelled
        } catch (error: HttpException) {
            if (error.code() == 401 || error.code() == 403) {
                return SavedSessionValidation.InvalidToken
            }
        } catch (_: Exception) {
            // Connection, timeout, and decoding failures do not invalidate a token.
        }

        if (attempt == attempts - 1) return SavedSessionValidation.ServerUnavailable
        waitBeforeRetry(retryDelayMs)
        retryDelayMs *= 2
    }
    return SavedSessionValidation.ServerUnavailable
}

/** Normalize the manual server field to the same origin contract as Apple. */
internal fun normalizeOrigin(raw: String): String {
    val trimmed = raw.trim().trimEnd('/')
    if (trimmed.isEmpty()) return trimmed

    val suppliedScheme = trimmed.startsWith("http://") || trimmed.startsWith("https://")
    val candidate = if (suppliedScheme) trimmed else "http://$trimmed"
    val uri = runCatching { URI(candidate) }.getOrNull() ?: return candidate
    if (uri.scheme != "http" || uri.port != -1 || uri.host == null) return candidate

    return URI(
        uri.scheme,
        uri.userInfo,
        uri.host,
        32400,
        uri.path,
        uri.query,
        uri.fragment,
    ).toString()
}

internal fun connectionOriginFromQr(payload: String): String? {
    var candidate = payload.trim()
    if (candidate.isEmpty()) return null

    val code = runCatching { URI(candidate) }.getOrNull()
    if (code?.scheme.equals("plurx", ignoreCase = true)) {
        if (!code?.host.equals("connect", ignoreCase = true)) return null
        val query = code?.rawQuery.orEmpty().split('&').mapNotNull { part ->
            val pieces = part.split('=', limit = 2)
            if (pieces.size != 2) null
            else java.net.URLDecoder.decode(pieces[0], Charsets.UTF_8.name()) to
                java.net.URLDecoder.decode(pieces[1], Charsets.UTF_8.name())
        }.toMap()
        candidate = query.entries.firstOrNull {
            it.key.lowercase() in setOf("origin", "server", "address")
        }?.value ?: return null
    }

    val normalized = normalizeOrigin(candidate)
    val origin = runCatching { URI(normalized) }.getOrNull() ?: return null
    if (origin.scheme !in setOf("http", "https") ||
        origin.host.isNullOrBlank() ||
        origin.userInfo != null ||
        origin.path.orEmpty().isNotEmpty()
    ) return null
    return normalized
}

internal fun matchesSavedServer(
    candidateInstanceId: String?,
    expectedInstanceId: String?,
    savedOrigin: String,
): Boolean {
    val candidate = candidateInstanceId?.lowercase()?.takeIf { it.isNotEmpty() } ?: return false
    val expected = expectedInstanceId?.lowercase()?.takeIf { it.isNotEmpty() }
    if (expected != null) return candidate == expected

    val host = runCatching { URI(savedOrigin).host?.lowercase() }.getOrNull() ?: return false
    if (!host.startsWith("plurx-") || !host.endsWith(".local")) return false
    val savedPrefix = host.removePrefix("plurx-").removeSuffix(".local")
    val candidatePrefix = candidate.filter(Char::isLetterOrDigit).take(12)
    return savedPrefix.isNotEmpty() && savedPrefix == candidatePrefix
}
