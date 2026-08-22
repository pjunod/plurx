package tv.plurx.app.ui

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Job
import kotlinx.coroutines.async
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.withTimeoutOrNull
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
import tv.plurx.app.data.OfflineNetwork
import tv.plurx.app.data.OfflineQuality
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
import tv.plurx.app.data.MediaFileDto
import tv.plurx.app.data.offline.OfflineBook
import tv.plurx.app.data.offline.OfflineBookQueueRequest
import tv.plurx.app.data.offline.OfflineBooks
import tv.plurx.app.data.offline.OfflineDownloads
import tv.plurx.app.data.offline.OfflineQueueRequest
import tv.plurx.app.data.offline.OfflineRecord
import tv.plurx.app.data.offline.authoritativeOfflineNetwork
import tv.plurx.app.player.PreplayTracks
import tv.plurx.app.player.decisionForce
import tv.plurx.app.player.preplayQueryParams

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
) {
    /** Anything worth painting. A spinner over real content is a regression. */
    val hasContent: Boolean
        get() = libraries.isNotEmpty() || hubs.continue_watching.isNotEmpty() ||
            hubs.next_up.isNotEmpty() || hubs.recently_added.isNotEmpty()
}

data class PlaybackTarget(val itemId: Long, val fileId: Long, val startMs: Long = 0)

data class EpisodePlaybackTarget(val episode: Item, val playback: PlaybackTarget)

internal enum class OfflineResumeTrigger(val explicitUserAction: Boolean) {
    Lifecycle(false),
    UserControl(true),
}

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
    val offlineRecords = OfflineDownloads.records
    val offlineBookRecords = OfflineBooks.records

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
    var currentUserId: Long? = null
        private set
    var serverInstanceId: String? = null
        private set

    private var api: PlurxApi? = null
    fun api(): PlurxApi = api ?: error("not connected")

    /** One dashboard load at a time — a refresh replaces the one in flight. */
    private var homeJob: Job? = null

    /**
     * The caps most recently reported to `/decision`, so a progressive remux
     * URL asks for the stream the decision was actually made about. Never a
     * substitute for probing: audio support is route-dependent, so every
     * decision re-probes (see [Caps]).
     */
    @Volatile
    var playbackCaps: Map<String, String> = emptyMap()
        private set

    /** Runtime playback caps for this device — sent to /decision and /stream.mp4. */
    private suspend fun caps(): Map<String, String> =
        Caps.query(getApplication<Application>()).also { playbackCaps = it }

    init {
        viewModelScope.launch {
            val saved = settings.flow.first()
            origin = saved.origin
            username = saved.username
            currentUserId = saved.userId
            serverInstanceId = saved.instanceId
            audioLang = saved.audioLang
            subLang = saved.subLang
            _preferences.value = saved.preferences.copy(
                offlineNetwork = authoritativeOfflineNetwork(
                    OfflineDownloads.currentNetworkPolicy(),
                    saved.preferences.offlineNetwork,
                ),
            )
            if (_preferences.value.offlineNetwork != saved.preferences.offlineNetwork) {
                settings.saveViewerPreferences(_preferences.value)
            }

            if (
                OfflineDownloads.catalog.profile(saved.instanceId, saved.userId).isNotEmpty() ||
                OfflineBooks.profile(saved.instanceId, saved.userId).isNotEmpty()
            ) {
                // Downloads is a local library. Do not hold it behind the
                // saved server's reachability check on an airplane launch.
                _phase.value = Phase.Ready
            }

            when {
                saved.origin.isNotBlank() && saved.token != null -> {
                    bindOrigin(saved.origin, saved.token)
                    // A persisted bearer is enough to restore the app shell.
                    // Reachability is not: after Android reclaimed the process,
                    // waiting here left a resumed app on the otherwise-empty
                    // Loading surface for the whole validation window whenever
                    // the server was asleep. Paint Home immediately and let its
                    // existing error + refresh surface own transport failures.
                    _phase.value = Phase.Ready
                    loadHome()
                    // Four attempts against a 20 s connect timeout, twice over
                    // if rediscovery finds a candidate, can still hold recovery
                    // for well over a minute on a TV whose network is coming up.
                    // Bound the background check even though it no longer gates
                    // first paint.
                    val validation = withTimeoutOrNull(LAUNCH_VALIDATION_TIMEOUT_MS) {
                        var result = validateSavedSession { api().me() }
                        if (result == SavedSessionValidation.ServerUnavailable) {
                            val recovered = rediscoverSavedServer(saved.instanceId, saved.origin)
                            if (recovered != null) {
                                bindOrigin(recovered.origin, saved.token)
                                serverName = recovered.info.name
                                settings.saveServerIdentity(
                                    recovered.origin,
                                    recovered.info.instance_id,
                                )
                                result = validateSavedSession { api().me() }
                            }
                        }
                        result
                    } ?: SavedSessionValidation.ServerUnavailable
                    when (validation) {
                        is SavedSessionValidation.Authenticated -> {
                            currentUser = validation.user
                            currentUserId = validation.user.id
                            settings.saveSession(
                                origin,
                                saved.token,
                                validation.user.username,
                                validation.user.id,
                            )
                            username = validation.user.username
                            if (saved.instanceId == null) backfillServerIdentity()
                            _phase.value = Phase.Ready
                            loadHome()
                            resumeOfflineProfile()
                            syncOfflineProgress()
                        }
                        SavedSessionValidation.InvalidToken -> {
                            Session.token = null
                            settings.clearToken()
                            _phase.value = Phase.NeedLogin
                        }
                        SavedSessionValidation.ServerUnavailable -> {
                            // Google TV often launches before networking has fully resumed.
                            // Keep the saved credentials and the Home request already in
                            // flight instead of incorrectly demanding a login or replacing
                            // its error with another long request.
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
                _authError.value = "Couldn't reach a Cinema server at $normalized"
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
                currentUserId = resp.user.id
                username = resp.user.username
                settings.saveSession(origin, resp.token, resp.user.username, resp.user.id)
                _phase.value = Phase.Ready
                loadHome()
                resumeOfflineProfile()
                syncOfflineProgress()
            } catch (_: Exception) {
                _authError.value = "Wrong username or password"
            } finally {
                _busy.value = false
            }
        }
    }

    /**
     * Load the dashboard, painting each answer as it lands.
     *
     * The old shape was hubs → libraries → one preview request per library,
     * strictly in series: 2 + N round trips before a single poster appeared,
     * and the slowest library held the whole screen. All of it is issued at
     * once now and each result is published on arrival, so first paint is the
     * *first* response rather than the last. The state keeps whatever it
     * already had — a refresh never blanks a populated dashboard, and a failed
     * refresh leaves the previous content up with an error beside it.
     */
    fun loadHome() {
        homeJob?.cancel()
        _home.value = _home.value.copy(loading = true, error = null)
        homeJob = viewModelScope.launch {
            try {
                coroutineScope {
                    val hubs = async { api().hubs() }
                    val libraries = async { api().libraries() }
                    val libs = libraries.await()
                    _home.value = _home.value.copy(libraries = libs, loading = false)
                    // Every preview at once. One library that times out costs
                    // its own shelf, not the dashboard.
                    val previews = libs.map { lib ->
                        lib.id to async {
                            catchingUnlessCancelled {
                                api().libraryItems(lib.id, limit = 24, sort = "added").items
                            }.getOrDefault(emptyList())
                        }
                    }
                    _home.value = _home.value.copy(hubs = hubs.await(), loading = false)
                    previews.forEach { (id, items) ->
                        _home.value = _home.value.copy(
                            libraryItems = _home.value.libraryItems + (id to items.await()),
                            loading = false,
                        )
                    }
                }
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (e: Exception) {
                _home.value = _home.value.copy(loading = false, error = e.message ?: "Failed to load")
            }
        }
    }

    fun logout() = logout(removeDownloads = false)

    fun logoutAndRemoveDownloads() = logout(removeDownloads = true)

    private fun logout(removeDownloads: Boolean) {
        viewModelScope.launch {
            val instance = serverInstanceId
            val user = currentUserId
            OfflineBooks.interruptProfile(instance, user)
            if (removeDownloads && instance != null && user != null) {
                OfflineDownloads.removeProfileNow(instance, user, api)
                OfflineBooks.removeProfileNow(instance, user)
            }
            settings.clearToken()
            Session.token = null
            currentUser = null
            currentUserId = null
            _home.value = HomeState()
            _phase.value = Phase.NeedLogin
        }
    }

    /**
     * Forget this server. The persisted token goes with the origin in the
     * same write — a bearer belongs to the server that issued it, and a
     * relaunch before the next login must not offer it to a different one.
     */
    fun changeServer() {
        OfflineBooks.interruptProfile(serverInstanceId, currentUserId)
        Session.token = null
        currentUser = null
        currentUserId = null
        serverInstanceId = null
        _home.value = HomeState()
        viewModelScope.launch { settings.clearServer() }
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

    fun setOfflineQuality(quality: OfflineQuality) =
        updatePreferences { copy(offlineQuality = quality) }

    fun setOfflineNetwork(network: OfflineNetwork) = applyOfflineNetworkChange(
        persistRecovery = { OfflineDownloads.setNetworkPolicy(network) },
        publishPreferences = { updatePreferences { copy(offlineNetwork = network) } },
    )

    fun queueOffline(item: Item, file: MediaFileDto): String? {
        val instance = serverInstanceId
            ?: return "Reconnect to this Cinema server before downloading"
        val user = currentUserId
            ?: return "Sign in again before downloading"
        val preferences = _preferences.value
        OfflineDownloads.enqueue(
            OfflineQueueRequest(
                api = api(),
                origin = origin,
                serverInstanceId = instance,
                userId = user,
                itemId = item.id,
                fileId = file.id,
                title = item.title,
                context = buildList {
                    item.show_title?.takeIf(String::isNotBlank)?.let(::add)
                    if (item.season_number != null && item.episode_number != null) {
                        add("S${item.season_number}E${item.episode_number}")
                    }
                }.takeIf(List<String>::isNotEmpty)?.joinToString(" · "),
                posterPath = item.poster,
                durationMs = file.duration_ms ?: item.runtime_ms,
                audioLanguage = audioLang,
                subtitleLanguage = subLang,
                maximumHeight = preferences.offlineQuality.maximumHeight,
                network = preferences.offlineNetwork,
            ),
        )
        return null
    }

    fun queueOfflineBook(item: Item, file: MediaFileDto): String? = queueOfflineBook(
        itemId = item.id,
        file = file,
        title = item.title,
        posterPath = item.poster,
    )

    fun retryOfflineBook(book: OfflineBook): String? = queueOfflineBook(
        itemId = book.itemId,
        file = MediaFileDto(
            id = book.fileId,
            filename = book.originalFilename,
            size = book.revision?.size ?: book.bytesTotal,
            container = "epub",
        ),
        title = book.title,
        posterPath = null,
    )

    private fun queueOfflineBook(
        itemId: Long,
        file: MediaFileDto,
        title: String,
        posterPath: String?,
    ): String? {
        if (!file.supportsOfflineBookReader) {
            return "Cinema cannot download this ebook format for in-app reading"
        }
        val instance = serverInstanceId
            ?: return "Reconnect to this Cinema server before downloading"
        val user = currentUserId
            ?: return "Sign in again before downloading"
        val token = Session.token
            ?: return "Sign in again before downloading"
        OfflineBooks.enqueue(
            OfflineBookQueueRequest(
                origin = origin,
                token = token,
                serverInstanceId = instance,
                userId = user,
                itemId = itemId,
                file = file,
                title = title,
                posterPath = posterPath,
                network = _preferences.value.offlineNetwork,
            ),
        )
        return null
    }

    fun resumeOffline(record: OfflineRecord) =
        resumeOffline(record, OfflineResumeTrigger.UserControl)

    private fun resumeOffline(record: OfflineRecord, trigger: OfflineResumeTrigger) {
        val instance = serverInstanceId ?: return
        val user = currentUserId ?: return
        val preferences = _preferences.value
        OfflineDownloads.resumePending(
            OfflineQueueRequest(
                api = api(),
                origin = origin,
                serverInstanceId = instance,
                userId = user,
                itemId = record.itemId,
                fileId = record.fileId,
                title = record.title,
                context = record.context,
                posterPath = null,
                durationMs = record.durationMs,
                audioLanguage = audioLang,
                subtitleLanguage = subLang,
                maximumHeight = preferences.offlineQuality.maximumHeight,
                network = preferences.offlineNetwork,
            ),
            explicitResumeId = record.id.takeIf { trigger.explicitUserAction },
        )
    }

    fun resumeOfflineProfile() {
        val instance = serverInstanceId ?: return
        val user = currentUserId ?: return
        OfflineDownloads.catalog.profile(instance, user).firstOrNull()?.let { record ->
            resumeOffline(record, OfflineResumeTrigger.Lifecycle)
        }
    }

    fun onForeground() {
        if (_phase.value != Phase.Ready || api == null) return
        resumeOfflineProfile()
        syncOfflineProgress()
        syncOfflineBookProgress()
    }

    fun removeOffline(record: OfflineRecord) = OfflineDownloads.remove(record, api)

    fun removeOfflineBook(book: OfflineBook) = OfflineBooks.remove(book)

    private fun syncOfflineProgress() {
        val instance = serverInstanceId ?: return
        val user = currentUserId ?: return
        viewModelScope.launch {
            val pending = OfflineDownloads.catalog.profile(instance, user)
                .filter { it.pendingProgress }
                .groupBy { it.itemId }
                .mapNotNull { (_, records) -> records.maxByOrNull { it.progressRecordedAt ?: 0 } }
            for (record in pending) {
                try {
                    api().progress(
                        record.itemId,
                        ProgressReq(
                            record.positionMs,
                            record.durationMs,
                            record.progressRecordedAt,
                        ),
                    )
                    OfflineDownloads.catalog.update(record.id) { it.copy(pendingProgress = false) }
                } catch (_: Exception) {
                    break
                }
            }
        }
    }

    private fun syncOfflineBookProgress() {
        val instance = serverInstanceId ?: return
        val user = currentUserId ?: return
        val boundApi = api ?: return
        viewModelScope.launch { OfflineBooks.syncPending(boundApi, instance, user) }
    }

    private fun updatePreferences(change: ViewerPreferences.() -> ViewerPreferences) {
        val updated = _preferences.value.change()
        _preferences.value = updated
        viewModelScope.launch { settings.saveViewerPreferences(updated) }
    }

    // ---- Suspend loaders used by individual screens --------------------------

    /**
     * Walk a library's pages, handing each one over as it arrives.
     *
     * The grid used to wait for every page of a thousand-item library behind a
     * spinner; it now paints the first page after one round trip and fills in
     * behind. Sorting is the client's job (`sortMerged`), so the server sort is
     * fixed and a sort change never re-fetches.
     */
    suspend fun libraryPages(id: Long, sort: String = "title", onPage: (List<Item>) -> Unit) {
        var offset = 0
        do {
            val page = api().libraryItems(id, limit = 200, offset = offset, sort = sort)
            if (page.items.isNotEmpty()) onPage(page.items)
            offset += page.items.size
        } while (page.items.isNotEmpty() && offset < page.total)
    }

    suspend fun search(query: String): List<Item> = api().search(query.trim()).results

    suspend fun itemDetail(id: Long): ItemDetail = api().item(id)

    /**
     * `force` is a verdict override, not a height: the server parses anything
     * outside `auto|original|transcode` as Auto, so sending a bare rung meant
     * an explicit "720p" silently did nothing. The height rides on the session
     * create instead (`sessionHeight`).
     */
    suspend fun decision(
        fileId: Long,
        tracks: PreplayTracks = PreplayTracks.NONE,
    ): Decision = api().decision(
        fileId,
        caps() + ("force" to decisionForce(_preferences.value.playbackQuality)) +
            // Request-local only. Omitting a parameter keeps the shared
            // playback-default policy and the response older clients get; the
            // server never writes a Playback setting from these.
            preplayQueryParams(tracks),
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

    suspend fun hlsSessionStatus(sessionId: String) = api().hlsSessionStatus(sessionId)

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
        // In memory and on disk, the token travels with the origin: this
        // server has not authenticated us yet, so nothing may be sent as if
        // it had.
        Session.token = null
        currentUser = null
        Session.origin = normalized
        val candidate = Net.api(normalized)
        val info = candidate.server()
        origin = normalized
        api = candidate
        serverName = info.name
        serverInstanceId = info.instance_id
        settings.saveOrigin(normalized, info.instance_id)
        _phase.value = Phase.NeedLogin
    }

    private fun bindOrigin(value: String, token: String?) {
        origin = value
        Session.origin = value
        Session.token = token
        api = Net.api(value)
    }

    /**
     * Find the saved server again after its address moved.
     *
     * `availableServers()` starts NSD to do it. Whoever starts multicast
     * discovery owns stopping it, and this path is not the connect screen — no
     * one else was ever going to — so a successful recovery used to leave the
     * browser resolving for the rest of the process's life. The `finally` is
     * the whole point of the function's shape.
     */
    private suspend fun rediscoverSavedServer(
        expectedInstanceId: String?,
        savedOrigin: String,
    ): RecoveredServer? = try {
        var found: RecoveredServer? = null
        for (candidate in serverBrowser.availableServers()) {
            val candidateOrigin = catchingUnlessCancelled { serverBrowser.resolve(candidate) }.getOrNull()
                ?: continue
            val info = catchingUnlessCancelled { Net.api(candidateOrigin).server() }.getOrNull()
                ?: continue
            if (matchesSavedServer(info.instance_id, expectedInstanceId, savedOrigin)) {
                found = RecoveredServer(candidateOrigin, info)
                break
            }
        }
        found
    } finally {
        serverBrowser.stop()
    }

    private suspend fun backfillServerIdentity() {
        val info = catchingUnlessCancelled { api().server() }.getOrNull() ?: return
        serverName = info.name
        serverInstanceId = info.instance_id
        settings.saveServerIdentity(origin, info.instance_id)
    }
}

/**
 * The longest a saved session may hold the splash screen before Home takes
 * over. Home can retry and say why; the splash can only spin.
 */
private const val LAUNCH_VALIDATION_TIMEOUT_MS = 12_000L

private data class RecoveredServer(val origin: String, val info: Server)

internal sealed interface SavedSessionValidation<out T> {
    data class Authenticated<T>(val user: T) : SavedSessionValidation<T>
    data object InvalidToken : SavedSessionValidation<Nothing>
    data object ServerUnavailable : SavedSessionValidation<Nothing>
}

/**
 * `runCatching`, minus the bug.
 *
 * `runCatching` catches `Throwable`, and inside a coroutine that includes the
 * `CancellationException` the machinery throws to unwind a cancelled job — so
 * a screen that has already left composition keeps running, "recovers" from
 * its own cancellation, and writes state nobody is watching. Cancellation is
 * not a failure to handle; it is the caller saying stop.
 */
internal inline fun <T> catchingUnlessCancelled(block: () -> T): Result<T> = try {
    Result.success(block())
} catch (cancelled: CancellationException) {
    throw cancelled
} catch (error: Throwable) {
    Result.failure(error)
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
internal fun applyOfflineNetworkChange(
    persistRecovery: () -> Unit,
    publishPreferences: () -> Unit,
) {
    persistRecovery()
    publishPreferences()
}

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
