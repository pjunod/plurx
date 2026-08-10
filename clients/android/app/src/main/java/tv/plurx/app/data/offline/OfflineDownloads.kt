@file:androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)
@file:android.annotation.SuppressLint("UnsafeOptInUsageError")

package tv.plurx.app.data.offline

import android.content.Context
import android.content.res.Configuration
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.Uri
import android.os.Build
import android.os.Looper
import androidx.media3.common.StreamKey
import androidx.media3.database.StandaloneDatabaseProvider
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
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import kotlinx.coroutines.flow.first
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json
import tv.plurx.app.data.CreateOfflinePackageReq
import tv.plurx.app.data.OfflineLeaseReq
import tv.plurx.app.data.OfflineNetwork
import tv.plurx.app.data.Net
import tv.plurx.app.data.PlurxApi
import tv.plurx.app.data.SettingsStore
import okhttp3.Request
import java.io.File
import java.io.IOException
import java.security.SecureRandom
import java.util.UUID
import java.util.concurrent.Executors
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicLong

@androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)
private typealias DownloadManagerAction<T> = DownloadManager.() -> T

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
    private data class StopReasonAcknowledgement(
        val expected: Int,
        val completion: CompletableDeferred<Unit>,
    )

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val json = Json { ignoreUnknownKeys = true; encodeDefaults = true }
    private val secureRandom = SecureRandom()
    private val preparationJobs = mutableMapOf<String, Job>()
    private val activeApis = ConcurrentHashMap<String, PlurxApi>()
    private val addAcknowledgements = ConcurrentHashMap<String, CompletableDeferred<Unit>>()
    private val stopReasonAcknowledgements =
        ConcurrentHashMap<String, StopReasonAcknowledgement>()
    private val transferSequence = AtomicLong(1)
    private val networkPolicyGeneration = AtomicLong(0)
    private lateinit var appContext: Context
    private lateinit var database: StandaloneDatabaseProvider
    private lateinit var recovery: OfflineRecoveryStore
    private lateinit var transferDataSource: UidtNetworkDataSourceFactory

    lateinit var catalog: OfflineCatalog
        private set
    lateinit var cache: SimpleCache
        private set
    lateinit var manager: DownloadManager
        private set

    val records: StateFlow<List<OfflineRecord>> get() = catalog.records

    fun initialize(context: Context) {
        if (::appContext.isInitialized) return
        check(Looper.myLooper() == Looper.getMainLooper()) {
            "OfflineDownloads must be initialized on the application looper"
        }
        appContext = context.applicationContext
        catalog = OfflineCatalog(appContext)
        recovery = OfflineRecoveryStore(appContext)
        val legacyNetwork = runBlocking(Dispatchers.IO) {
            SettingsStore(appContext).flow.first().preferences.offlineNetwork
        }
        val initialNetwork = recovery.migrateNetworkPolicy(legacyNetwork)
        runBlocking(Dispatchers.IO) {
            recovery.intents().forEach { (id, encoded) ->
                if (catalog.record(id) == null) {
                    runCatching { json.decodeFromString<OfflineRecord>(encoded) }
                        .getOrNull()
                        ?.takeUnless { intent ->
                            catalog.records.value.any { current ->
                                current.serverInstanceId == intent.serverInstanceId &&
                                    current.userId == intent.userId &&
                                    current.fileId == intent.fileId &&
                                    current.state !in setOf("failed", "missing")
                            }
                        }
                        ?.let { catalog.upsert(it) }
                }
                recovery.clearIntent(id)
            }
        }
        transferSequence.set(
            (catalog.records.value.maxOfOrNull(OfflineRecord::transferSequence) ?: 0L) + 1L,
        )
        database = StandaloneDatabaseProvider(appContext)
        cache = SimpleCache(
            File(appContext.filesDir, "offline/media"),
            NoOpCacheEvictor(),
            database,
        )
        transferDataSource = UidtNetworkDataSourceFactory()
        manager = DownloadManager(
            appContext,
            database,
            cache,
            transferDataSource,
            Executors.newFixedThreadPool(2),
        ).apply {
            maxParallelDownloads = 2
            requirements = offlineRequirements(initialNetwork)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                // A cold manager must not open a process-default socket before
                // a persisted UIDT job supplies its granted Network.
                pauseDownloads()
            }
            addListener(object : DownloadManager.Listener {
                override fun onInitialized(downloadManager: DownloadManager) {
                    check(Looper.myLooper() == Looper.getMainLooper())
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                        downloadManager.currentDownloads.forEach { download ->
                            val desired = coldStartStopReason(
                                download.stopReason,
                                recovery.transferState(download.request.id),
                            )
                            if (desired != download.stopReason) {
                                downloadManager.setStopReason(
                                    download.request.id,
                                    desired,
                                )
                            }
                        }
                    }
                }

                override fun onDownloadChanged(
                    downloadManager: DownloadManager,
                    download: Download,
                    finalException: Exception?,
                ) {
                    check(Looper.myLooper() == Looper.getMainLooper())
                    addAcknowledgements.remove(download.request.id)?.complete(Unit)
                    stopReasonAcknowledgements[download.request.id]
                        ?.takeIf { it.expected == download.stopReason }
                        ?.let { acknowledgement ->
                            if (stopReasonAcknowledgements.remove(
                                    download.request.id,
                                    acknowledgement,
                                )
                            ) {
                                acknowledgement.completion.complete(Unit)
                            }
                        }
                    val snapshot = DownloadSnapshot.from(
                        download,
                        finalException?.message,
                        transferSequence.getAndIncrement(),
                    )
                    scope.launch { reflect(snapshot) }
                }

                override fun onDownloadRemoved(
                    downloadManager: DownloadManager,
                    download: Download,
                ) {
                    check(Looper.myLooper() == Looper.getMainLooper())
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
        val intent = OfflineRecord(
            id = id,
            requestId = UUID.randomUUID().toString(),
            serverInstanceId = request.serverInstanceId,
            userId = request.userId,
            itemId = request.itemId,
            fileId = request.fileId,
            title = request.title,
            context = request.context,
            durationMs = request.durationMs,
            requestedHeight = request.maximumHeight,
        )
        // commit() is deliberate: the user's tap is durable before artwork,
        // catalog IO, or foreground preparation gets a chance to fail.
        recovery.persistIntent(id, json.encodeToString(intent))
        setNetworkPolicy(request.network)
        launchPreparation(id) {
            val existing = catalog.records.value.firstOrNull {
                it.serverInstanceId == request.serverInstanceId &&
                    it.userId == request.userId && it.fileId == request.fileId
            }
            if (existing != null && existing.state !in setOf("failed", "missing")) {
                recovery.clearIntent(id)
                return@launchPreparation
            }
            if (existing != null) removeNow(existing, request.api)
            catalog.upsert(intent)
            recovery.clearIntent(id)
            cachePoster(id, request.origin, request.posterPath)?.let { poster ->
                catalog.update(id) { it.copy(posterFile = poster) }
            }
            prepare(id, request)
        }
    }

    fun resumePending(template: OfflineQueueRequest, explicitResumeId: String? = null) {
        // DownloadManager is process-local and starts with the conservative
        // Wi-Fi-only requirement below. Reapply the persisted viewer policy
        // before restoring rows, otherwise an "Any network" transfer that was
        // active before process death comes back queued and never resumes.
        val profile = catalog.profile(template.serverInstanceId, template.userId)
        profile.forEach { record ->
            val explicitUserResume = isExplicitResumeTarget(record.id, explicitResumeId)
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
                record.state == "paused" && !explicitUserResume -> Unit
                record.manifestUrl != null &&
                    record.state in setOf("ready", "queued", "paused") -> scope.launch {
                    materializeTransfer(record, explicitUserResume)
                }
                record.state in setOf("intent", "queued", "preparing") ->
                launchPreparation(record.id) {
                    prepare(
                            record.id,
                            template.copy(fileId = record.fileId, itemId = record.itemId),
                            explicitUserResume,
                        )
                }
            }
        }
    }

    fun setNetworkPolicy(policy: OfflineNetwork) {
        val changed = recovery.networkPolicy() != policy
        recovery.setNetworkPolicy(policy)
        runOnManager { requirements = offlineRequirements(policy) }
        if (changed && Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            reconfigureUidtTransfers(policy, networkPolicyGeneration.incrementAndGet())
        }
    }

    @androidx.annotation.RequiresApi(34)
    private fun reconfigureUidtTransfers(policy: OfflineNetwork, policyGeneration: Long) {
        val transfers = catalog.records.value.filter { record ->
            recovery.existingJobId(record.id) != null &&
                recovery.transferState(record.id) in setOf(
                    TransferRecoveryState.Active,
                    TransferRecoveryState.WaitingForJob,
                )
        }
        runOnManager {
            transfers.forEach { record ->
                transferDataSource.revokeCurrent(record.id)
                recovery.setTransferState(record.id, TransferRecoveryState.WaitingForJob)
                // Start undispatched while still on the app looper so the
                // fail-closed binding revocation is immediately followed by
                // Media3's queued stop transition. Job replacement waits for
                // the listener acknowledgement that the stop is durable.
                scope.launch(start = CoroutineStart.UNDISPATCHED) {
                    if (!awaitStopReason(record.id, UIDT_WAITING_REASON)) {
                        catalog.update(record.id) {
                            it.copy(
                                state = "ready",
                                phase = "waiting_for_foreground",
                                errorMessage = "Tap Resume to start this download",
                            )
                        }
                        return@launch
                    }
                    if (networkPolicyGeneration.get() != policyGeneration) return@launch
                    val scheduled = runCatching {
                        OfflineTransferJobService.schedule(
                            appContext,
                            checkNotNull(recovery.existingJobId(record.id)),
                            record.id,
                            policy,
                            record.bytesTotal,
                        )
                    }.getOrDefault(false)
                    if (!scheduled) {
                        catalog.update(record.id) {
                            it.copy(
                                state = "ready",
                                phase = "waiting_for_foreground",
                                errorMessage = "Tap Resume to start this download",
                            )
                        }
                    }
                }
            }
        }
    }

    fun currentNetworkPolicy(): OfflineNetwork = recovery.networkPolicy()

    fun remove(record: OfflineRecord, api: PlurxApi? = null) {
        synchronized(preparationJobs) { preparationJobs.remove(record.id) }?.cancel()
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

    suspend fun completedDownloadRequest(id: String): DownloadRequest? = withManager {
        downloadIndex.getDownload(id)
            ?.takeIf { it.state == Download.STATE_COMPLETED }
            ?.request
    }

    private suspend fun prepare(
        id: String,
        request: OfflineQueueRequest,
        explicitUserResume: Boolean = true,
    ) {
        try {
            activeApis[id] = request.api
            var record = catalog.record(id) ?: return
            if (record.manifestUrl != null) {
                materializeTransfer(record, explicitUserResume)
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
                    withContext(NonCancellable) {
                        catalog.update(id) { it.copy(packageId = created.id) }
                    }
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
            activeApis[id] = request.api
            materializeTransfer(record, explicitUserResume)
        } catch (cancelled: CancellationException) {
            val owned = catalog.record(id)
            owned?.packageId?.let { packageId ->
                withContext(NonCancellable) {
                    runCatching { request.api.deleteOfflinePackage(packageId) }
                }
            }
            throw cancelled
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
        }
    }

    /**
     * Put the complete request in Media3 before handing ownership to Android.
     * The listener acknowledgement closes the old sendAddDownload/job-start
     * race: a UIDT job is never registered for an id the download index has not
     * durably observed.
     */
    private suspend fun materializeTransfer(record: OfflineRecord, explicitUserResume: Boolean) {
        val manifest = record.manifestUrl ?: return
        val request = DownloadRequest.Builder(record.id, Uri.parse(manifest))
            .setMimeType("application/x-mpegURL")
            .setStreamKeys(offlineStreamKeys(record.subtitleIndex != null))
            .setData(json.encodeToString(record).encodeToByteArray())
            .build()
        val existing = withManager { downloadIndex.getDownload(record.id) }
        if (existing == null) {
            recovery.setTransferState(record.id, TransferRecoveryState.WaitingForJob)
            val acknowledged = CompletableDeferred<Unit>()
            addAcknowledgements[record.id] = acknowledged
            withManager {
                addDownload(
                    request,
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                        UIDT_WAITING_REASON
                    } else {
                        Download.STOP_REASON_NONE
                    },
                )
            }
            if (withTimeoutOrNull(ADD_ACK_TIMEOUT_MS) { acknowledged.await() } == null) {
                addAcknowledgements.remove(record.id, acknowledged)
                catalog.update(record.id) {
                    it.copy(
                        state = "ready",
                        phase = "waiting_for_foreground",
                        errorMessage = "Tap Resume to start this download",
                    )
                }
                return
            }
        }

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            if (!explicitUserResume) {
                // A persisted UIDT job, if one exists, is the only owner that
                // may clear UIDT_WAITING_REASON after a cold start. Build 24
                // transfers have no such job and honestly remain tap-to-resume.
                return
            }
            recovery.setTransferState(record.id, TransferRecoveryState.WaitingForJob)
            val currentStopReason = withManager {
                downloadIndex.getDownload(record.id)?.stopReason ?: UIDT_WAITING_REASON
            }
            val waitingReason = explicitResumeStopReason(
                currentStopReason,
                isTappedRow = true,
            )
            if (!awaitStopReason(record.id, waitingReason)) {
                catalog.update(record.id) {
                    it.copy(
                        state = "ready",
                        phase = "waiting_for_foreground",
                        errorMessage = "Tap Resume to start this download",
                    )
                }
                return
            }
            catalog.update(record.id) {
                it.copy(
                    state = "ready",
                    phase = "waiting_for_transfer_job",
                    errorMessage = null,
                    transferSequence = transferSequence.getAndIncrement(),
                )
            }
            val scheduled = runCatching {
                OfflineTransferJobService.schedule(
                    appContext,
                    recovery.jobId(record.id),
                    record.id,
                    recovery.networkPolicy(),
                    record.bytesTotal,
                )
            }.getOrDefault(false)
            if (!scheduled) {
                catalog.update(record.id) {
                    it.copy(
                        state = "ready",
                        phase = "waiting_for_foreground",
                        errorMessage = "Tap Resume to start this download",
                    )
                }
            }
            return
        }

        withManager {
            setStopReason(record.id, Download.STOP_REASON_NONE)
            resumeDownloads()
        }
        DownloadService.sendResumeDownloads(appContext, PlurxDownloadService::class.java, true)
    }

    private fun launchPreparation(
        id: String,
        block: suspend () -> Unit,
    ) {
        synchronized(preparationJobs) {
            if (preparationJobs.containsKey(id)) return
            preparationJobs[id] = scope.launch {
                try {
                    block()
                } finally {
                    synchronized(preparationJobs) { preparationJobs.remove(id) }
                }
            }
        }
    }

    private suspend fun removeNow(record: OfflineRecord, api: PlurxApi?) {
        synchronized(preparationJobs) { preparationJobs.remove(record.id) }?.cancel()
        addAcknowledgements.remove(record.id)?.cancel()
        stopReasonAcknowledgements.remove(record.id)?.completion?.cancel()
        activeApis.remove(record.id)
        val latest = catalog.record(record.id) ?: record
        recovery.existingJobId(record.id)?.let { jobId ->
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                OfflineTransferJobService.cancel(appContext, jobId)
            }
        }
        withManager { removeDownload(record.id) }
        latest.packageId?.let { packageId -> runCatching { api?.deleteOfflinePackage(packageId) } }
        latest.posterFile?.let { runCatching { File(it).delete() } }
        catalog.remove(record.id)
        recovery.removeTransfer(record.id)
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

    private suspend fun reflect(snapshot: DownloadSnapshot) {
        val state = when (snapshot.state) {
            Download.STATE_QUEUED -> "queued"
            Download.STATE_STOPPED -> "paused"
            Download.STATE_DOWNLOADING -> "downloading"
            Download.STATE_COMPLETED -> "completed"
            Download.STATE_FAILED -> "failed"
            Download.STATE_REMOVING -> "removing"
            Download.STATE_RESTARTING -> "queued"
            else -> "queued"
        }
        catalog.update(snapshot.id) { current ->
            if (!shouldApplyTransferSnapshot(current, snapshot)) return@update current
            val waitingForUidt = snapshot.stopReason == UIDT_WAITING_REASON
            val stoppedBySystem = snapshot.stopReason in setOf(
                SYSTEM_TIMEOUT_REASON,
                SYSTEM_INTERRUPTED_REASON,
            )
            current.copy(
                state = if (waitingForUidt) "ready" else state,
                phase = if (stoppedBySystem) {
                    "paused_by_system"
                } else if (waitingForUidt) {
                    "waiting_for_transfer_job"
                } else {
                    state
                },
                bytesDownloaded = maxOf(current.bytesDownloaded, snapshot.bytesDownloaded),
                bytesTotal = snapshot.contentLength.takeIf { length -> length > 0 }
                    ?: current.bytesTotal,
                percentDownloaded = maxOf(current.percentDownloaded, snapshot.percentDownloaded),
                errorMessage = if (stoppedBySystem) {
                    "Paused by system — tap Resume"
                } else {
                    snapshot.errorMessage
                },
                transferSequence = snapshot.sequence,
            )
        }
        if (state == "completed") {
            recovery.setTransferState(snapshot.id, TransferRecoveryState.Completed)
            completeServerPackage(snapshot.id)
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
            val download = runCatching {
                withManager { downloadIndex.getDownload(record.id) }
            }.getOrNull()
            when {
                download != null -> reflect(
                    DownloadSnapshot.from(download, null, transferSequence.getAndIncrement()),
                )
                record.state in setOf("downloading", "completed") -> catalog.update(record.id) {
                    it.copy(state = "missing", phase = "missing", errorMessage = "Download missing")
                }
            }
        }
    }

    internal fun bindUidtNetwork(id: String, network: Network, owner: Long): Boolean {
        check(Looper.myLooper() == Looper.getMainLooper())
        if (!uidtNetworkSatisfiesCurrentPolicy(network)) return false
        val manifest = catalog.record(id)?.manifestUrl ?: return false
        transferDataSource.bind(id, owner, manifest, network)
        return true
    }

    internal suspend fun runUserInitiatedTransfer(
        id: String,
        network: Network,
        owner: Long,
    ): Boolean {
        try {
            while (!withManager { isInitialized }) delay(25)
            val started = withManager {
                if (!bindUidtNetwork(id, network, owner)) return@withManager false
                val current = downloadIndex.getDownload(id) ?: return@withManager false
                if (
                    current.stopReason != UIDT_WAITING_REASON &&
                    current.stopReason != Download.STOP_REASON_NONE
                ) {
                    return@withManager false
                }
                recovery.setTransferState(id, TransferRecoveryState.Active)
                setStopReason(id, Download.STOP_REASON_NONE)
                resumeDownloads()
                true
            }
            if (!started) return false
            while (true) {
                delay(TRANSFER_POLL_MS)
                val download = withManager { downloadIndex.getDownload(id) } ?: return true
                when (download.state) {
                    Download.STATE_COMPLETED,
                    Download.STATE_FAILED,
                    Download.STATE_REMOVING,
                    -> return true
                    Download.STATE_STOPPED -> if (download.stopReason != Download.STOP_REASON_NONE) {
                        return false
                    }
                }
            }
        } finally {
            withContext(NonCancellable + Dispatchers.Main.immediate) {
                transferDataSource.revoke(id, owner)
            }
        }
    }

    internal fun replaceUidtNetwork(id: String, network: Network, owner: Long) {
        check(Looper.myLooper() == Looper.getMainLooper())
        manager.setStopReason(id, UIDT_WAITING_REASON)
        transferDataSource.revoke(id, owner)
        if (!bindUidtNetwork(id, network, owner)) {
            pauseForJobRetry(id)
            return
        }
        manager.setStopReason(id, Download.STOP_REASON_NONE)
        manager.resumeDownloads()
    }

    internal fun revokeUidtNetwork(id: String, owner: Long) {
        check(Looper.myLooper() == Looper.getMainLooper())
        transferDataSource.revoke(id, owner)
    }

    internal fun uidtNetworkSatisfiesCurrentPolicy(network: Network): Boolean {
        val capabilities = appContext.getSystemService(ConnectivityManager::class.java)
            .getNetworkCapabilities(network) ?: return false
        return networkSatisfiesPolicy(
            recovery.networkPolicy(),
            capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED),
        )
    }

    internal fun pauseForSystem(id: String, reason: Int) {
        check(Looper.myLooper() == Looper.getMainLooper())
        recovery.setTransferState(id, TransferRecoveryState.PausedBySystem)
        manager.setStopReason(id, reason)
        val sequence = transferSequence.getAndIncrement()
        scope.launch {
            catalog.update(id) { current ->
                if (sequence < current.transferSequence) current else current.copy(
                    state = "paused",
                    phase = "paused_by_system",
                    errorMessage = "Paused by system — tap Resume",
                    transferSequence = sequence,
                )
            }
        }
    }

    internal fun pauseForJobRetry(id: String) {
        check(Looper.myLooper() == Looper.getMainLooper())
        recovery.setTransferState(id, TransferRecoveryState.WaitingForJob)
        manager.setStopReason(id, UIDT_WAITING_REASON)
        val sequence = transferSequence.getAndIncrement()
        scope.launch {
            catalog.update(id) { current ->
                if (sequence < current.transferSequence) current else current.copy(
                    state = "ready",
                    phase = "waiting_for_transfer_job",
                    errorMessage = null,
                    transferSequence = sequence,
                )
            }
        }
    }

    private suspend fun <T> withManager(block: DownloadManagerAction<T>): T =
        withContext(Dispatchers.Main.immediate) {
            check(Looper.myLooper() == Looper.getMainLooper())
            manager.block()
        }

    /** Wait until Media3's internal thread has persisted the ownership stop reason. */
    private suspend fun awaitStopReason(id: String, expected: Int): Boolean {
        if (withManager { downloadIndex.getDownload(id)?.stopReason } == expected) return true
        val completion = CompletableDeferred<Unit>()
        val acknowledgement = StopReasonAcknowledgement(expected, completion)
        stopReasonAcknowledgements.put(id, acknowledgement)
            ?.completion
            ?.cancel()
        withManager {
            setStopReason(id, expected)
            if (
                downloadIndex.getDownload(id)?.stopReason == expected &&
                stopReasonAcknowledgements.remove(id, acknowledgement)
            ) {
                completion.complete(Unit)
            }
        }
        return try {
            withTimeoutOrNull(STOP_REASON_ACK_TIMEOUT_MS) {
                completion.await()
                true
            } ?: false
        } finally {
            stopReasonAcknowledgements.remove(id, acknowledgement)
        }
    }

    private fun runOnManager(block: DownloadManagerAction<Unit>) {
        if (Looper.myLooper() == Looper.getMainLooper()) {
            manager.block()
        } else {
            CoroutineScope(Dispatchers.Main.immediate).launch { manager.block() }
        }
    }

    private fun randomToken(): String = ByteArray(32).also(secureRandom::nextBytes)
        .joinToString("") { "%02x".format(it) }

    const val SYSTEM_TIMEOUT_REASON = 10_001
    const val SYSTEM_INTERRUPTED_REASON = 10_002
    const val UIDT_WAITING_REASON = 10_003

    private const val ADD_ACK_TIMEOUT_MS = 5_000L
    private const val STOP_REASON_ACK_TIMEOUT_MS = 5_000L
    private const val TRANSFER_POLL_MS = 1_000L
}

internal data class DownloadSnapshot(
    val id: String,
    val state: Int,
    val stopReason: Int,
    val bytesDownloaded: Long,
    val contentLength: Long,
    val percentDownloaded: Float,
    val errorMessage: String?,
    val sequence: Long,
) {
    companion object {
        fun from(download: Download, errorMessage: String?, sequence: Long) = DownloadSnapshot(
            id = download.request.id,
            state = download.state,
            stopReason = download.stopReason,
            bytesDownloaded = download.bytesDownloaded,
            contentLength = download.contentLength,
            percentDownloaded = download.percentDownloaded,
            errorMessage = errorMessage,
            sequence = sequence,
        )
    }
}

internal fun coldStartStopReason(
    current: Int,
    recoveryState: TransferRecoveryState = TransferRecoveryState.Preparing,
): Int = when {
    current != Download.STOP_REASON_NONE -> current
    recoveryState == TransferRecoveryState.PausedBySystem ->
        OfflineDownloads.SYSTEM_INTERRUPTED_REASON
    else -> OfflineDownloads.UIDT_WAITING_REASON
}

internal fun shouldApplyTransferSnapshot(
    current: OfflineRecord,
    snapshot: DownloadSnapshot,
): Boolean {
    if (snapshot.sequence < current.transferSequence) return false
    if (current.state == "completed" && snapshot.state != Download.STATE_COMPLETED) return false
    if (
        current.phase == "paused_by_system" &&
        snapshot.stopReason !in setOf(
            OfflineDownloads.SYSTEM_TIMEOUT_REASON,
            OfflineDownloads.SYSTEM_INTERRUPTED_REASON,
        )
    ) {
        return false
    }
    return true
}

internal fun jobStopRequiresExplicitResume(stopReason: Int): Boolean =
    stopReason == android.app.job.JobParameters.STOP_REASON_USER ||
        stopReason == android.app.job.JobParameters.STOP_REASON_TIMEOUT ||
        stopReason == android.app.job.JobParameters.STOP_REASON_TIMEOUT_ABANDONED

internal fun offlineRequirements(policy: OfflineNetwork): Requirements {
    val network = if (policy == OfflineNetwork.WifiOnly) {
        Requirements.NETWORK_UNMETERED
    } else {
        Requirements.NETWORK
    }
    return Requirements(network or Requirements.DEVICE_STORAGE_NOT_LOW)
}

internal fun authoritativeOfflineNetwork(
    recovery: OfflineNetwork,
    @Suppress("UNUSED_PARAMETER") legacyDataStore: OfflineNetwork,
): OfflineNetwork = recovery

internal fun networkSatisfiesPolicy(policy: OfflineNetwork, isUnmetered: Boolean): Boolean =
    policy == OfflineNetwork.Any || isUnmetered

internal val OfflineRecord.needsServerCompletion: Boolean
    get() = state == "completed" && packageId != null

internal val OfflineRecord.needsExplicitResume: Boolean
    get() = state == "paused" ||
        (state == "ready" && phase in setOf("waiting_for_foreground", "waiting_for_transfer_job"))

internal fun isExplicitResumeTarget(recordId: String, explicitResumeId: String?): Boolean =
    explicitResumeId != null && recordId == explicitResumeId

internal fun explicitResumeStopReason(current: Int, isTappedRow: Boolean): Int =
    if (isTappedRow) {
        OfflineDownloads.UIDT_WAITING_REASON
    } else {
        current
    }

internal fun offlineStreamKeys(hasSubtitle: Boolean): List<StreamKey> = buildList {
    add(StreamKey(HlsMultivariantPlaylist.GROUP_INDEX_VARIANT, 0))
    if (hasSubtitle) {
        add(StreamKey(HlsMultivariantPlaylist.GROUP_INDEX_SUBTITLE, 0))
    }
}
