@file:androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)

package tv.plurx.app.data.offline

import android.content.Context
import android.content.res.Configuration
import android.net.Uri
import android.os.Build
import androidx.media3.common.StreamKey
import androidx.media3.database.StandaloneDatabaseProvider
import androidx.media3.datasource.DefaultHttpDataSource
import androidx.media3.datasource.PlaceholderDataSource
import androidx.media3.datasource.cache.CacheDataSource
import androidx.media3.datasource.cache.NoOpCacheEvictor
import androidx.media3.datasource.cache.SimpleCache
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.exoplayer.hls.playlist.HlsMultivariantPlaylist
import androidx.media3.exoplayer.offline.Download
import androidx.media3.exoplayer.offline.DownloadManager
import androidx.media3.exoplayer.offline.DownloadRequest
import androidx.media3.exoplayer.offline.DownloadService
import androidx.media3.exoplayer.scheduler.Requirements
import androidx.media3.exoplayer.source.DefaultMediaSourceFactory
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.launch
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json
import tv.plurx.app.data.CreateOfflinePackageReq
import tv.plurx.app.data.OfflineLeaseReq
import tv.plurx.app.data.OfflineNetwork
import tv.plurx.app.data.Net
import tv.plurx.app.data.PlurxApi
import okhttp3.Request
import java.io.File
import java.io.IOException
import java.security.SecureRandom
import java.util.UUID
import java.util.concurrent.Executors
import java.util.concurrent.ConcurrentHashMap

data class OfflineQueueRequest(
    val api: PlurxApi,
    val origin: String,
    val serverInstanceId: String,
    val userId: Long,
    val itemId: Long,
    val fileId: Long,
    val title: String,
    val context: String?,
    val posterPath: String?,
    val durationMs: Long?,
    val audioLanguage: String,
    val subtitleLanguage: String,
    val maximumHeight: Int,
    val network: OfflineNetwork,
)

/** Process-wide owner of the one non-evicting Media3 cache and DownloadManager. */
object OfflineDownloads {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val json = Json { ignoreUnknownKeys = true; encodeDefaults = true }
    private val secureRandom = SecureRandom()
    private val preparationJobs = mutableSetOf<String>()
    private val activeApis = ConcurrentHashMap<String, PlurxApi>()
    private lateinit var appContext: Context
    private lateinit var database: StandaloneDatabaseProvider

    lateinit var catalog: OfflineCatalog
        private set
    lateinit var cache: SimpleCache
        private set
    lateinit var manager: DownloadManager
        private set

    val records: StateFlow<List<OfflineRecord>> get() = catalog.records

    fun initialize(context: Context) {
        if (::appContext.isInitialized) return
        appContext = context.applicationContext
        catalog = OfflineCatalog(appContext)
        database = StandaloneDatabaseProvider(appContext)
        cache = SimpleCache(
            File(appContext.filesDir, "offline/media"),
            NoOpCacheEvictor(),
            database,
        )
        manager = DownloadManager(
            appContext,
            database,
            cache,
            DefaultHttpDataSource.Factory().setUserAgent("Cinema Android offline"),
            Executors.newFixedThreadPool(2),
        ).apply {
            maxParallelDownloads = 2
            requirements = Requirements(
                Requirements.NETWORK_UNMETERED or Requirements.DEVICE_STORAGE_NOT_LOW,
            )
            addListener(object : DownloadManager.Listener {
                override fun onDownloadChanged(
                    downloadManager: DownloadManager,
                    download: Download,
                    finalException: Exception?,
                ) {
                    scope.launch { reflect(download, finalException) }
                }

                override fun onDownloadRemoved(
                    downloadManager: DownloadManager,
                    download: Download,
                ) {
                    scope.launch { catalog.remove(download.request.id) }
                }
            })
        }
        scope.launch { reconcile() }
    }

    fun canUse(context: Context): Boolean =
        context.resources.configuration.uiMode and Configuration.UI_MODE_TYPE_MASK !=
            Configuration.UI_MODE_TYPE_TELEVISION

    fun enqueue(request: OfflineQueueRequest) {
        check(canUse(appContext)) { "Offline downloads are unavailable on television" }
        val id = UUID.randomUUID().toString()
        scope.launch {
            val existing = catalog.records.value.firstOrNull {
                it.serverInstanceId == request.serverInstanceId &&
                    it.userId == request.userId && it.fileId == request.fileId
            }
            if (existing != null && existing.state !in setOf("failed", "missing")) return@launch
            if (existing != null) removeNow(existing, request.api)
            catalog.upsert(
                OfflineRecord(
                    id = id,
                    requestId = UUID.randomUUID().toString(),
                    serverInstanceId = request.serverInstanceId,
                    userId = request.userId,
                    itemId = request.itemId,
                    fileId = request.fileId,
                    title = request.title,
                    context = request.context,
                    posterFile = cachePoster(id, request.origin, request.posterPath),
                    durationMs = request.durationMs,
                    requestedHeight = request.maximumHeight,
                ),
            )
            setNetworkPolicy(request.network)
            prepare(id, request)
        }
    }

    fun resumePending(template: OfflineQueueRequest) {
        // DownloadManager is process-local and starts with the conservative
        // Wi-Fi-only requirement below. Reapply the persisted viewer policy
        // before restoring rows, otherwise an "Any network" transfer that was
        // active before process death comes back queued and never resumes.
        setNetworkPolicy(template.network)
        val profile = catalog.profile(template.serverInstanceId, template.userId)
        profile.forEach { record ->
            // Media3 owns transfer progress across process death, but the API
            // object that releases the server package is process-local. Bind
            // every restored row to the now-authenticated profile before
            // either resuming it or reflecting a completion that happened
            // while the app was dead.
            activeApis[record.id] = template.api
            when {
                record.needsServerCompletion -> scope.launch {
                    completeServerPackage(record.id, template.api)
                }
                record.state in setOf("intent", "queued", "preparing", "ready", "paused") ->
                scope.launch {
                    prepare(
                        record.id,
                        template.copy(fileId = record.fileId, itemId = record.itemId),
                    )
                }
            }
        }
        manager.resumeDownloads()
        DownloadService.sendResumeDownloads(appContext, PlurxDownloadService::class.java, true)
    }

    fun setNetworkPolicy(policy: OfflineNetwork) {
        manager.requirements = offlineRequirements(policy)
    }

    fun remove(record: OfflineRecord, api: PlurxApi? = null) {
        scope.launch { removeNow(record, api) }
    }

    fun removeProfile(serverInstanceId: String, userId: Long) {
        scope.launch { removeProfileNow(serverInstanceId, userId, null) }
    }

    suspend fun removeProfileNow(serverInstanceId: String, userId: Long, api: PlurxApi?) {
        catalog.profile(serverInstanceId, userId).forEach { record -> removeNow(record, api) }
    }

    suspend fun recordProgress(id: String, positionMs: Long, durationMs: Long?) {
        catalog.update(id) {
            it.copy(
                positionMs = positionMs.coerceAtLeast(0),
                durationMs = durationMs ?: it.durationMs,
                progressRecordedAt = System.currentTimeMillis() / 1000,
                pendingProgress = true,
            )
        }
    }

    fun cacheOnlyPlayer(context: Context): ExoPlayer {
        val cacheOnly = CacheDataSource.Factory()
            .setCache(cache)
            .setUpstreamDataSourceFactory(PlaceholderDataSource.FACTORY)
        return ExoPlayer.Builder(context)
            .setMediaSourceFactory(DefaultMediaSourceFactory(cacheOnly))
            .build()
    }

    private suspend fun prepare(id: String, request: OfflineQueueRequest) {
        synchronized(preparationJobs) {
            if (!preparationJobs.add(id)) return
        }
        try {
            activeApis[id] = request.api
            var record = catalog.record(id) ?: return
            if (record.state == "paused" && manager.downloadIndex.getDownload(id) != null) {
                manager.setStopReason(id, Download.STOP_REASON_NONE)
                manager.resumeDownloads()
                return
            }
            val options = request.api.offlineOptions(
                record.fileId,
                request.audioLanguage,
                request.subtitleLanguage,
            )
            val quality = options.qualities.filter { it.height <= request.maximumHeight }
                .maxByOrNull { it.height } ?: options.qualities.minByOrNull { it.height }
                ?: error("No offline quality is available")
            val audio = options.audio.firstOrNull { it.index == options.recommended_audio_index }
            val subtitle = options.subtitles.firstOrNull {
                it.index == options.recommended_subtitle_index && it.offline_mode == "native"
            }
            catalog.update(id) {
                it.copy(
                    state = "queued",
                    phase = "waiting_for_server",
                    requestedHeight = quality.height,
                    bytesTotal = quality.estimated_bytes,
                    audioIndex = audio?.index,
                    subtitleIndex = subtitle?.index,
                    audioLabel = audio?.title ?: audio?.language,
                    subtitleLabel = subtitle?.title ?: subtitle?.language,
                    errorMessage = null,
                )
            }
            record = catalog.record(id) ?: return
            var status = if (record.packageId == null) {
                request.api.createOfflinePackage(
                    record.fileId,
                    CreateOfflinePackageReq(
                        record.requestId,
                        record.requestedHeight,
                        record.audioIndex,
                        record.subtitleIndex,
                    ),
                ).also { created ->
                    catalog.update(id) { it.copy(packageId = created.id) }
                }
            } else {
                request.api.offlinePackage(record.packageId)
            }
            while (status.state !in setOf("ready", "failed")) {
                catalog.update(id) {
                    it.copy(
                        state = if (status.state == "queued") "queued" else "preparing",
                        phase = status.phase,
                        actualHeight = status.output.height,
                        bytesDownloaded = status.bytes_ready,
                        bytesTotal = status.actual_bytes ?: status.estimated_bytes,
                    )
                }
                delay(2_000)
                status = request.api.offlinePackage(status.id)
            }
            if (status.state == "failed") {
                error(status.error?.message ?: "The server could not prepare this download")
            }
            record = catalog.record(id) ?: return
            val token = record.leaseToken ?: randomToken().also { stable ->
                catalog.update(id) { it.copy(leaseToken = stable) }
            }
            val lease = request.api.putOfflineLease(status.id, OfflineLeaseReq(token))
            val manifest = if (lease.manifest_url.startsWith("http")) {
                lease.manifest_url
            } else {
                request.origin.trimEnd('/') + lease.manifest_url
            }
            val required = (lease.bytes * 1.10).toLong() + 256L * 1024 * 1024
            check(appContext.filesDir.usableSpace >= required) {
                "Not enough device storage for this download"
            }
            record = (catalog.record(id) ?: return).copy(
                packageId = status.id,
                manifestUrl = manifest,
                state = "ready",
                phase = "ready_to_download",
                actualHeight = status.output.height,
                bytesTotal = lease.bytes,
                durationMs = lease.duration_ms,
                errorMessage = null,
            )
            catalog.upsert(record)
            val streamKeys = offlineStreamKeys(record.subtitleIndex != null)
            val download = DownloadRequest.Builder(id, Uri.parse(manifest))
                .setMimeType("application/x-mpegURL")
                .setStreamKeys(streamKeys)
                .setData(json.encodeToString(record).encodeToByteArray())
                .build()
            activeApis[id] = request.api
            try {
                DownloadService.sendAddDownload(
                    appContext,
                    PlurxDownloadService::class.java,
                    download,
                    true,
                )
            } catch (error: Exception) {
                if (!isBackgroundForegroundServiceRefusal(error)) throw error
                // Android 12+ can forbid a background foreground-service
                // start. The lease and request remain valid; foreground
                // catch-up will enqueue this exact ready record.
                catalog.update(id) {
                    it.copy(
                        state = "ready",
                        phase = "waiting_for_foreground",
                        errorMessage = "Open Cinema to start this download",
                    )
                }
                return
            }
        } catch (error: IOException) {
            catalog.update(id) {
                it.copy(
                    state = "queued",
                    phase = "waiting_for_server",
                    errorMessage = "Waiting for server",
                )
            }
        } catch (error: Exception) {
            catalog.update(id) {
                it.copy(state = "failed", phase = "failed", errorMessage = error.message)
            }
        } finally {
            synchronized(preparationJobs) { preparationJobs.remove(id) }
        }
    }

    private suspend fun removeNow(record: OfflineRecord, api: PlurxApi?) {
        DownloadService.sendRemoveDownload(
            appContext,
            PlurxDownloadService::class.java,
            record.id,
            true,
        )
        record.packageId?.let { packageId -> runCatching { api?.deleteOfflinePackage(packageId) } }
        record.posterFile?.let { runCatching { File(it).delete() } }
        catalog.remove(record.id)
    }

    private fun cachePoster(id: String, origin: String, path: String?): String? {
        val source = path?.takeIf(String::isNotBlank) ?: return null
        val url = if (source.startsWith("http://") || source.startsWith("https://")) {
            source
        } else {
            origin.trimEnd('/') + "/" + source.trimStart('/')
        }
        return runCatching {
            Net.client.newCall(Request.Builder().url(url).build()).execute().use { response ->
                if (!response.isSuccessful) return@use null
                val bytes = response.body?.bytes() ?: return@use null
                if (bytes.isEmpty()) return@use null
                val directory = File(appContext.filesDir, "offline/artwork").apply { mkdirs() }
                val target = File(directory, "$id.image")
                val temporary = File(directory, "$id.image.tmp")
                temporary.writeBytes(bytes)
                check(temporary.renameTo(target)) { "could not commit offline artwork" }
                target.absolutePath
            }
        }.getOrNull()
    }

    private suspend fun reflect(download: Download, finalException: Exception?) {
        val state = when (download.state) {
            Download.STATE_QUEUED -> "queued"
            Download.STATE_STOPPED -> "paused"
            Download.STATE_DOWNLOADING -> "downloading"
            Download.STATE_COMPLETED -> "completed"
            Download.STATE_FAILED -> "failed"
            Download.STATE_REMOVING -> "removing"
            Download.STATE_RESTARTING -> "queued"
            else -> "queued"
        }
        catalog.update(download.request.id) {
            it.copy(
                state = state,
                phase = if (download.stopReason == SYSTEM_TIMEOUT_REASON) {
                    "paused_by_system"
                } else {
                    state
                },
                bytesDownloaded = download.bytesDownloaded,
                bytesTotal = download.contentLength.takeIf { length -> length > 0 } ?: it.bytesTotal,
                percentDownloaded = download.percentDownloaded,
                errorMessage = finalException?.message,
            )
        }
        if (state == "completed") {
            completeServerPackage(download.request.id)
        }
    }

    private suspend fun completeServerPackage(id: String, restoredApi: PlurxApi? = null) {
        val record = catalog.record(id) ?: return
        val packageId = record.packageId ?: return
        val api = restoredApi ?: activeApis[id] ?: return
        // Keep the binding when the foreground session is temporarily
        // offline. A later Media3 reflection or foreground catch-up retries
        // the idempotent completion call instead of pinning the package for
        // its full seven-day lease.
        if (runCatching { api.completeOfflinePackage(packageId) }.isSuccess) {
            activeApis.remove(id, api)
        }
    }

    private suspend fun reconcile() {
        catalog.records.value.forEach { record ->
            val download = runCatching { manager.downloadIndex.getDownload(record.id) }.getOrNull()
            when {
                download != null -> reflect(download, null)
                record.state in setOf("downloading", "completed") -> catalog.update(record.id) {
                    it.copy(state = "missing", phase = "missing", errorMessage = "Download missing")
                }
            }
        }
    }

    private fun randomToken(): String = ByteArray(32).also(secureRandom::nextBytes)
        .joinToString("") { "%02x".format(it) }

    const val SYSTEM_TIMEOUT_REASON = 10_001
}

internal fun offlineRequirements(policy: OfflineNetwork): Requirements {
    val network = if (policy == OfflineNetwork.WifiOnly) {
        Requirements.NETWORK_UNMETERED
    } else {
        Requirements.NETWORK
    }
    return Requirements(network or Requirements.DEVICE_STORAGE_NOT_LOW)
}

internal val OfflineRecord.needsServerCompletion: Boolean
    get() = state == "completed" && packageId != null

internal fun offlineStreamKeys(hasSubtitle: Boolean): List<StreamKey> = buildList {
    add(StreamKey(HlsMultivariantPlaylist.GROUP_INDEX_VARIANT, 0))
    if (hasSubtitle) {
        add(StreamKey(HlsMultivariantPlaylist.GROUP_INDEX_SUBTITLE, 0))
    }
}

private fun isBackgroundForegroundServiceRefusal(error: Exception): Boolean =
    Build.VERSION.SDK_INT >= Build.VERSION_CODES.S &&
        error is android.app.ForegroundServiceStartNotAllowedException
