package tv.plurx.app.data.offline

import android.content.Context
import android.content.res.Configuration
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import kotlinx.serialization.encodeToString
import okhttp3.Request
import okhttp3.ResponseBody
import okhttp3.HttpUrl
import okhttp3.HttpUrl.Companion.toHttpUrlOrNull
import tv.plurx.app.data.MediaFileDto
import tv.plurx.app.data.Net
import tv.plurx.app.data.OfflineNetwork
import tv.plurx.app.data.OpenPublicationResponse
import tv.plurx.app.data.PlurxApi
import tv.plurx.app.data.PublicationLink
import tv.plurx.app.data.PutReadingStateRequest
import tv.plurx.app.data.ReadingLocator
import java.io.File
import java.io.FileOutputStream
import java.util.UUID

data class OfflineBookQueueRequest(
    val origin: String,
    val token: String,
    val serverInstanceId: String,
    val userId: Long,
    val itemId: Long,
    val file: MediaFileDto,
    val title: String,
    val posterPath: String?,
    val network: OfflineNetwork,
)

/** Process-wide app-private EPUB owner. It intentionally does not share the
 * Media3 video queue, cache, lease, or player lifecycle. */
object OfflineBooks {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val jobs = mutableMapOf<String, Job>()
    private lateinit var appContext: Context

    lateinit var catalog: OfflineBookCatalog
        private set

    val records get() = catalog.records

    fun initialize(context: Context) {
        if (::appContext.isInitialized) return
        appContext = context.applicationContext
        catalog = OfflineBookCatalog(File(appContext.filesDir, "offline/books"))
        scope.launch { catalog.reconcile() }
    }

    fun canUse(context: Context): Boolean =
        context.resources.configuration.uiMode and Configuration.UI_MODE_TYPE_MASK !=
            Configuration.UI_MODE_TYPE_TELEVISION

    fun profile(serverInstanceId: String?, userId: Long?): List<OfflineBook> =
        catalog.profile(serverInstanceId, userId)

    fun root(book: OfflineBook): File = catalog.root(book.id)

    fun cover(book: OfflineBook): File? = book.coverFilename
        ?.let(::safePublicationPath)
        ?.let { catalog.safeChild(root(book), it) }
        ?.takeIf(File::isFile)

    fun mimeType(book: OfflineBook, path: String): String =
        (book.publication?.readingOrder.orEmpty() + book.publication?.resources.orEmpty())
            .firstOrNull { safePublicationPath(it.href) == path }
            ?.type ?: "application/octet-stream"

    fun enqueue(request: OfflineBookQueueRequest) {
        check(canUse(appContext)) { "Offline books are unavailable on television" }
        val id = UUID.randomUUID().toString()
        val job = scope.launch {
            val existing = catalog.profile(request.serverInstanceId, request.userId)
                .firstOrNull { it.fileId == request.file.id }
            if (existing?.isPlayable == true) return@launch
            if (existing != null) removeNow(existing)
            val intent = OfflineBook(
                id = id,
                serverInstanceId = request.serverInstanceId,
                userId = request.userId,
                itemId = request.itemId,
                fileId = request.file.id,
                title = request.title,
                originalFilename = request.file.filename.ifBlank { "book.epub" },
                bytesTotal = request.file.size,
            )
            catalog.upsert(intent)
            transfer(intent, request)
        }
        synchronized(jobs) { jobs[id] = job }
        job.invokeOnCompletion { failure ->
            synchronized(jobs) {
                if (jobs[id] === job) jobs.remove(id)
            }
            if (failure is CancellationException && catalog.record(id) != null) {
                scope.launch {
                    catalog.update(id) { current ->
                        if (current.state !in setOf("intent", "downloading")) current else current.copy(
                            state = "failed",
                            phase = "interrupted",
                            errorMessage = "Download interrupted — download again",
                        )
                    }
                }
            }
        }
    }

    fun remove(book: OfflineBook) {
        synchronized(jobs) { jobs.remove(book.id) }?.cancel()
        scope.launch { removeNow(book) }
    }

    fun removeProfile(serverInstanceId: String, userId: Long) {
        scope.launch { removeProfileNow(serverInstanceId, userId) }
    }

    fun interruptProfile(serverInstanceId: String?, userId: Long?) {
        if (serverInstanceId == null || userId == null) return
        val ids = catalog.profile(serverInstanceId, userId).map(OfflineBook::id).toSet()
        val cancelled = synchronized(jobs) {
            ids.mapNotNull { id -> jobs.remove(id) }
        }
        cancelled.forEach(Job::cancel)
    }

    suspend fun removeProfileNow(serverInstanceId: String, userId: Long) {
        catalog.profile(serverInstanceId, userId).forEach { book ->
            synchronized(jobs) { jobs.remove(book.id) }?.cancel()
            removeNow(book)
        }
    }

    suspend fun record(
        id: String,
        locator: ReadingLocator,
        progression: Double,
        completed: Boolean,
        recordedAt: Long,
        preferences: OfflineBookPreferences,
    ) {
        catalog.update(id) { book ->
            if (recordedAt < (book.recordedAt ?: Long.MIN_VALUE)) book else book.copy(
                locator = locator,
                progression = progression.coerceIn(0.0, 1.0),
                completed = completed,
                recordedAt = recordedAt,
                pendingProgress = true,
                preferences = preferences,
            )
        }
    }

    suspend fun syncPending(api: PlurxApi, serverInstanceId: String, userId: Long) {
        val pending = newestPendingBooks(catalog.profile(serverInstanceId, userId))
        for (snapshot in pending) {
            val revision = snapshot.revision ?: continue
            val locator = snapshot.locator ?: continue
            val recordedAt = snapshot.recordedAt ?: continue
            try {
                api.putReadingState(
                    snapshot.itemId,
                    PutReadingStateRequest(
                        file_id = snapshot.fileId,
                        revision = revision,
                        locator = locator,
                        progression = snapshot.progression,
                        completed = snapshot.completed,
                        recorded_at = recordedAt,
                    ),
                )
                catalog.update(snapshot.id) { current ->
                    if (current.recordedAt != recordedAt || current.locator != locator) current
                    else current.copy(pendingProgress = false, errorMessage = null)
                }
            } catch (_: Exception) {
                // Disconnection is the normal case. Preserve the newest write.
                break
            }
        }
    }

    private suspend fun transfer(book: OfflineBook, request: OfflineBookQueueRequest) {
        val authenticatedClient = Net.profileClient(request.token)
        val api = Net.api(request.origin, authenticatedClient)
        val sessions = linkedSetOf<String>()
        try {
            requireNetwork(request.network)
            var opened = api.openPublication(book.fileId)
            sessions += opened.session_id
            requireSpace(opened)
            catalog.update(book.id) {
                it.copy(
                    revision = opened.revision,
                    publication = opened.publication,
                    limits = opened.limits,
                    title = opened.publication.metadata.title.ifBlank { request.title },
                    author = opened.publication.metadata.author,
                    state = "downloading",
                    phase = "downloading_original",
                    bytesTotal = Math.addExact(
                        opened.revision.size,
                        opened.limits.total_uncompressed_bytes,
                    ),
                )
            }
            val staging = catalog.staging(book.id)
            try {
                val originalResponse = api.bookContent(book.fileId)
                if (!originalResponse.isSuccessful) {
                    originalResponse.errorBody()?.close()
                    error("Cinema could not download the original EPUB (${originalResponse.code()})")
                }
                val originalBytes = originalResponse.body()?.use { body ->
                    download(body, File(staging, "book.epub"), opened.revision.size)
                } ?: error("The Cinema server returned an empty EPUB")
                check(originalBytes == opened.revision.size) {
                    "The EPUB changed while Cinema downloaded it"
                }

                if (runCatching { api.closePublication(opened.session_id) }.isSuccess) {
                    sessions.remove(opened.session_id)
                }
                val reopened = api.openPublication(book.fileId)
                sessions += reopened.session_id
                check(reopened.revision == opened.revision && reopened.publication == opened.publication) {
                    "The EPUB changed while Cinema downloaded it"
                }
                opened = reopened

                val resources = publicationResources(opened)
                val publicationRoot = File(staging, "publication").apply { mkdirs() }
                var extractedBytes = 0L
                resources.forEachIndexed { index, resource ->
                    val target = checkNotNull(catalog.safeChild(publicationRoot, resource.path))
                    target.parentFile?.mkdirs()
                    val response = Net.capabilityClient.newCall(
                        Request.Builder().url(resourceUrl(request.origin, opened.resource_base, resource.path)).build(),
                    ).execute()
                    val bytes = response.use {
                        check(it.isSuccessful) { "Cinema could not download an EPUB resource (${it.code})" }
                        download(checkNotNull(it.body), target, opened.limits.resource_bytes)
                    }
                    extractedBytes = boundedAdd(
                        extractedBytes,
                        bytes,
                        opened.limits.total_uncompressed_bytes,
                    )
                    catalog.update(book.id) {
                        it.copy(
                            phase = "downloading_resources_${index + 1}_of_${resources.size}",
                            bytesDownloaded = originalBytes + extractedBytes,
                        )
                    }
                }
                writeSync(
                    File(staging, "publication.json"),
                    Net.json.encodeToString(opened.publication).toByteArray(),
                )
                val coverBytes = cacheCover(request, staging, authenticatedClient)
                val localBytes = Math.addExact(Math.addExact(originalBytes, extractedBytes), coverBytes)
                val published = catalog.publish(staging, book.id)
                val completed = catalog.update(book.id) {
                    it.copy(
                        state = "downloaded",
                        phase = "ready",
                        bytesDownloaded = localBytes,
                        bytesTotal = localBytes,
                        coverFilename = "cover".takeIf { coverBytes > 0 },
                        errorMessage = null,
                    )
                }
                if (completed == null) {
                    published.deleteRecursively()
                    throw CancellationException()
                }
            } catch (error: Throwable) {
                catalog.discardStaging(book.id)
                throw error
            }
        } catch (cancelled: CancellationException) {
            catalog.discardStaging(book.id)
            throw cancelled
        } catch (error: Throwable) {
            if (catalog.record(book.id) != null) {
                catalog.update(book.id) {
                    it.copy(
                        state = "failed",
                        phase = "failed",
                        errorMessage = error.message ?: "Cinema could not download this EPUB",
                    )
                }
            }
        } finally {
            sessions.forEach { session -> runCatching { api.closePublication(session) } }
        }
    }

    private data class Resource(val path: String)

    private fun publicationResources(opened: OpenPublicationResponse): List<Resource> {
        val paths = linkedSetOf<String>()
        (opened.publication.readingOrder + opened.publication.resources).forEach { link ->
            paths += checkNotNull(safePublicationPath(link.href)) {
                "The EPUB manifest contains an unsafe path"
            }
        }
        check(paths.size <= opened.limits.entries) { "The EPUB contains too many resources" }
        return paths.sorted().map(::Resource)
    }

    private fun resourceUrl(origin: String, resourceBase: String, path: String): HttpUrl {
        val root = checkNotNull(origin.toHttpUrlOrNull()) { "The Cinema server address is invalid" }
        val base = checkNotNull(root.resolve(resourceBase)) { "The EPUB capability URL is invalid" }
        check(root.scheme == base.scheme && root.host == base.host && root.port == base.port) {
            "The EPUB capability left the Cinema server"
        }
        check(
            base.query == null && base.fragment == null &&
                base.encodedPath.startsWith("/api/v1/publication/") &&
                base.encodedPath.endsWith('/'),
        ) { "The EPUB capability URL is outside the publication service" }
        val builder = base.newBuilder()
        path.split('/').forEach(builder::addPathSegment)
        return builder.build()
    }

    private fun download(body: ResponseBody, target: File, maximumBytes: Long): Long {
        val declared = body.contentLength()
        check(declared < 0 || declared <= maximumBytes) { "The EPUB resource exceeds Cinema's safety limit" }
        var total = 0L
        FileOutputStream(target).use { output ->
            body.byteStream().use { input ->
                val buffer = ByteArray(DEFAULT_BUFFER_SIZE * 8)
                while (true) {
                    val count = input.read(buffer)
                    if (count < 0) break
                    total = boundedAdd(total, count.toLong(), maximumBytes)
                    output.write(buffer, 0, count)
                }
            }
            output.fd.sync()
        }
        return total
    }

    private fun writeSync(target: File, bytes: ByteArray) {
        FileOutputStream(target).use { output ->
            output.write(bytes)
            output.fd.sync()
        }
    }

    private fun cacheCover(
        request: OfflineBookQueueRequest,
        staging: File,
        authenticatedClient: okhttp3.OkHttpClient,
    ): Long {
        val posterPath = request.posterPath?.takeIf(String::isNotBlank) ?: return 0
        val root = request.origin.toHttpUrlOrNull() ?: return 0
        val url = root.resolve(posterPath) ?: return 0
        if (root.scheme != url.scheme || root.host != url.host || root.port != url.port ||
            !url.encodedPath.startsWith("/api/v1/images/")
        ) return 0
        return runCatching {
            authenticatedClient.newCall(Request.Builder().url(url).build()).execute().use { response ->
                val type = response.header("Content-Type").orEmpty()
                if (!response.isSuccessful || !type.startsWith("image/")) return@use 0L
                val body = response.body ?: return@use 0L
                download(body, File(staging, "cover"), 8L * 1024 * 1024)
            }
        }.getOrDefault(0)
    }

    private fun boundedAdd(left: Long, right: Long, maximum: Long): Long {
        val sum = Math.addExact(left, right)
        check(sum <= maximum) { "The EPUB expands beyond Cinema's safety limit" }
        return sum
    }

    private fun requireSpace(opened: OpenPublicationResponse) {
        val required = Math.addExact(
            Math.addExact(opened.revision.size, opened.limits.total_uncompressed_bytes),
            16L * 1024 * 1024,
        )
        check(appContext.filesDir.usableSpace >= required) { "Not enough device storage for this EPUB" }
    }

    private fun requireNetwork(policy: OfflineNetwork) {
        if (policy == OfflineNetwork.Any) return
        val manager = appContext.getSystemService(ConnectivityManager::class.java)
        val capabilities = manager.activeNetwork?.let(manager::getNetworkCapabilities)
        check(capabilities?.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED) == true) {
            "Connect to Wi-Fi to download this EPUB"
        }
    }

    private suspend fun removeNow(book: OfflineBook) {
        catalog.discardStaging(book.id)
        catalog.root(book.id).deleteRecursively()
        catalog.remove(book.id)
    }
}

internal fun newestPendingBooks(books: List<OfflineBook>): List<OfflineBook> = books
    .filter(OfflineBook::pendingProgress)
    .groupBy { book -> Triple(book.itemId, book.fileId, book.revision) }
    .mapNotNull { (_, editions) -> editions.maxByOrNull { it.recordedAt ?: Long.MIN_VALUE } }
    .sortedBy { it.recordedAt ?: Long.MIN_VALUE }
