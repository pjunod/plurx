package tv.plurx.app.player

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.os.SystemClock
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.serialization.decodeFromString
import tv.plurx.app.data.Net
import tv.plurx.app.data.PlurxApi
import java.util.Locale
import kotlin.math.ceil

internal data class PGSOverlayRenderedObject(
    val object_: PGSOverlayObject,
    val bitmap: Bitmap,
)

internal data class PGSOverlayFrame(
    val revision: Long,
    val cue: PGSOverlayCue,
    val objects: List<PGSOverlayRenderedObject>,
)

/**
 * Owns the Android overlay lifecycle. The server remains the only PGS parser;
 * this class accepts a bounded JSON/PNG contract, rejects stale work with
 * selection and item generations, and publishes one active composition.
 */
internal class AndroidPGSOverlayController(
    private val api: () -> PlurxApi,
    private val scope: CoroutineScope,
    private val fileId: Long,
    private val sourcePositionMs: () -> Long,
    private val isPlaying: () -> Boolean,
    private val playbackSpeed: () -> Float,
    private val onFrame: (PGSOverlayFrame?) -> Unit,
    private val onStatus: (PGSOverlayStatus) -> Unit,
    private val onFailure: (String) -> Unit,
) {
    private var trackIndex: Long? = null
    private var manifest: PGSOverlayManifest? = null
    private var loadedWindow: PGSOverlayTimeWindow? = null
    private var selectionGeneration = 0L
    private var itemGeneration = 0L
    private var revision = 0L
    private var windowGeneration = 0L
    private var prepareJob: Job? = null
    private var windowJob: Job? = null
    private var boundaryJob: Job? = null

    private data class CacheEntry(val bitmap: Bitmap, val bytes: Long)

    private val imageCache = object : LinkedHashMap<String, CacheEntry>(16, 0.75f, true) {}
    private var imageCacheBytes = 0L

    val isActive: Boolean
        get() = trackIndex != null

    fun select(index: Long?) {
        if (index == trackIndex && index != null) {
            reconcile(forceWindow = false)
            return
        }
        clearState()
        if (index == null) return

        trackIndex = index
        onStatus(PGSOverlayStatus.Preparing)
        val selection = selectionGeneration
        prepareJob = scope.launch {
            val deadline = SystemClock.elapsedRealtime() + PGSOverlayPolicy.maximumPrepareMs
            try {
                while (SystemClock.elapsedRealtime() < deadline) {
                    ensureSelection(selection, index)
                    when (val fetch = fetchManifest(index)) {
                        is ManifestFetch.Ready -> {
                            val ready = withContext(Dispatchers.Default) {
                                fetch.manifest.validated(fileId, index)
                            }
                            ensureSelection(selection, index)
                            manifest = ready
                            reconcile(forceWindow = true)
                            return@launch
                        }
                        is ManifestFetch.Preparing -> delay(fetch.retryAfterMs.toLong())
                    }
                }
                fail("PGS subtitles took too long to prepare.", selection)
            } catch (_: CancellationException) {
                // Selection/item lifecycle cancellation is expected.
            } catch (error: Exception) {
                fail(error.message ?: "PGS subtitles are unavailable.", selection)
            }
        }
    }

    fun itemChanged() {
        itemGeneration++
        windowGeneration++
        windowJob?.cancel()
        boundaryJob?.cancel()
        loadedWindow = null
        onFrame(null)
        if (trackIndex != null) reconcile(forceWindow = true)
    }

    fun reconcile(forceWindow: Boolean = false) {
        val ready = manifest ?: return
        val index = trackIndex ?: return
        val position = sourcePositionMs().coerceAtLeast(0)
        if (forceWindow || PGSOverlayPolicy.shouldRefresh(position, loadedWindow)) {
            loadWindow(
                ready,
                index,
                position,
                clearFrame = loadedWindow?.contains(position) != true,
            )
            return
        }
        publishActive(ready, position)
    }

    fun release() = clearState()

    private fun clearState() {
        selectionGeneration++
        windowGeneration++
        prepareJob?.cancel()
        windowJob?.cancel()
        boundaryJob?.cancel()
        prepareJob = null
        windowJob = null
        boundaryJob = null
        trackIndex = null
        manifest = null
        loadedWindow = null
        imageCache.clear()
        imageCacheBytes = 0
        onFrame(null)
        onStatus(PGSOverlayStatus.Off)
    }

    private fun loadWindow(
        ready: PGSOverlayManifest,
        index: Long,
        position: Long,
        clearFrame: Boolean,
    ) {
        val window = PGSOverlayPolicy.windowAt(position, ready.durationMs)
        val cues = PGSOverlayPolicy.cuesInWindow(ready.cues, window)
        val selection = selectionGeneration
        val item = itemGeneration
        val windowToken = ++windowGeneration
        windowJob?.cancel()
        boundaryJob?.cancel()
        if (clearFrame) onFrame(null)
        windowJob = scope.launch {
            try {
                require(cues.size <= PGSOverlayPolicy.maximumWindowCues) {
                    "The PGS subtitle window contains too many cues."
                }
                require(PGSOverlayPolicy.decodedWindowBytes(cues) != null) {
                    "The PGS subtitle window exceeded the device memory limit."
                }
                val active = PGSOverlayPolicy.activeCueIndex(cues, position)
                    ?.let(cues::get)

                suspend fun load(object_: PGSOverlayObject) {
                    ensureCurrent(selection, item, windowToken, index)
                    if (imageCache[object_.image] != null) return
                    val hash = ready.objectHash(object_.image)
                        ?: error("The server returned an invalid PGS subtitle path.")
                    val bitmap = fetchBitmap(index, ready.generation, hash, object_)
                    ensureCurrent(selection, item, windowToken, index)
                    store(object_.image, bitmap)
                }

                val activePaths = active?.objects.orEmpty().mapTo(mutableSetOf()) { it.image }
                active?.objects.orEmpty().distinctBy { it.image }.forEach { load(it) }

                // Publish the current composition before filling the look-ahead
                // cache. A dense dialogue window must not delay the subtitle
                // already due on screen.
                ensureCurrent(selection, item, windowToken, index)
                loadedWindow = window
                onStatus(PGSOverlayStatus.Ready)
                publishActive(ready, sourcePositionMs().coerceAtLeast(0))

                cues.asSequence()
                    .flatMap { it.objects.asSequence() }
                    .filterNot { it.image in activePaths }
                    .distinctBy { it.image }
                    .forEach { load(it) }

                ensureCurrent(selection, item, windowToken, index)
                loadedWindow = window
                publishActive(ready, sourcePositionMs().coerceAtLeast(0))
            } catch (_: CancellationException) {
                // A newer selection, item, seek window, or release won.
            } catch (error: Exception) {
                if (isCurrent(selection, item, windowToken, index)) {
                    fail(error.message ?: "PGS subtitles are unavailable.", selection)
                }
            }
        }
    }

    private fun publishActive(ready: PGSOverlayManifest, position: Long) {
        boundaryJob?.cancel()
        val cue = PGSOverlayPolicy.activeCueIndex(ready.cues, position)?.let(ready.cues::get)
        if (cue == null) {
            onFrame(null)
        } else {
            val objects = cue.objects.map { object_ ->
                val bitmap = imageCache[object_.image]?.bitmap ?: run {
                    onFrame(null)
                    reconcile(forceWindow = true)
                    return
                }
                PGSOverlayRenderedObject(object_, bitmap)
            }
            revision++
            onFrame(PGSOverlayFrame(revision, cue, objects))
        }
        scheduleBoundary(ready, position)
    }

    private fun scheduleBoundary(ready: PGSOverlayManifest, position: Long) {
        if (!isPlaying()) return
        val boundary = PGSOverlayPolicy.nextBoundaryMs(ready.cues, position) ?: return
        val speed = playbackSpeed().takeIf { it > 0f } ?: 1f
        val waitMs = ceil((boundary - position).coerceAtLeast(1) / speed.toDouble())
            .toLong()
            .coerceAtLeast(1)
        val selection = selectionGeneration
        val item = itemGeneration
        val window = windowGeneration
        val index = trackIndex ?: return
        boundaryJob = scope.launch {
            delay(waitMs)
            try {
                ensureCurrent(selection, item, window, index)
                reconcile()
            } catch (_: CancellationException) {
                // A playback event rescheduled this boundary.
            }
        }
    }

    private suspend fun fetchManifest(index: Long): ManifestFetch {
        val response = api().pgsOverlayManifest(fileId, index)
        val body = response.body() ?: error("The server returned an empty PGS overlay response.")
        body.use {
            require(
                response.headers()["Content-Type"]
                    ?.lowercase(Locale.ROOT)
                    ?.startsWith("application/json") == true,
            ) {
                "The server returned an invalid PGS overlay response."
            }
            require(it.contentLength() <= MAXIMUM_MANIFEST_BYTES) {
                "The PGS overlay manifest is too large."
            }
            val bytes = it.bytes()
            require(bytes.size <= MAXIMUM_MANIFEST_BYTES) {
                "The PGS overlay manifest is too large."
            }
            return when (response.code()) {
                200 -> ManifestFetch.Ready(Net.json.decodeFromString<PGSOverlayManifest>(bytes.decodeToString()))
                202 -> {
                    val preparing = Net.json.decodeFromString<PGSOverlayPreparing>(bytes.decodeToString())
                    require(preparing.state == "preparing") { "invalid PGS preparation response" }
                    ManifestFetch.Preparing(preparing.retryAfterMs.coerceIn(250, 5_000))
                }
                else -> error("The PGS overlay request failed (${response.code()}).")
            }
        }
    }

    private suspend fun fetchBitmap(
        index: Long,
        generation: String,
        hash: String,
        object_: PGSOverlayObject,
    ): Bitmap {
        val response = api().pgsOverlayObject(fileId, index, generation, hash)
        val body = response.body() ?: error("The server returned an empty PGS subtitle image.")
        body.use {
            require(response.isSuccessful) { "The PGS subtitle image request failed (${response.code()})." }
            require(
                response.headers()["Content-Type"]
                    ?.lowercase(Locale.ROOT)
                    ?.startsWith("image/png") == true,
            ) {
                "The server returned a non-PNG subtitle image."
            }
            require(it.contentLength() <= MAXIMUM_PNG_BYTES) { "The PGS subtitle image is too large." }
            val bytes = it.bytes()
            require(bytes.size <= MAXIMUM_PNG_BYTES) { "The PGS subtitle image is too large." }
            return withContext(Dispatchers.Default) {
                val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
                BitmapFactory.decodeByteArray(bytes, 0, bytes.size, bounds)
                require(bounds.outWidth == object_.width && bounds.outHeight == object_.height) {
                    "The PGS subtitle image dimensions do not match its manifest."
                }
                val expectedBytes = Math.multiplyExact(
                    Math.multiplyExact(object_.width.toLong(), object_.height.toLong()),
                    4L,
                )
                require(expectedBytes in 1..PGSOverlayPolicy.maximumObjectBytes) {
                    "The PGS subtitle image exceeds the decoded-image limit."
                }
                val bitmap = BitmapFactory.decodeByteArray(
                    bytes,
                    0,
                    bytes.size,
                    BitmapFactory.Options().apply { inPreferredConfig = Bitmap.Config.ARGB_8888 },
                ) ?: error("The server returned an invalid PGS subtitle image.")
                require(bitmap.width == object_.width && bitmap.height == object_.height) {
                    "The decoded PGS subtitle image dimensions changed."
                }
                require(bitmap.allocationByteCount.toLong() in 1..PGSOverlayPolicy.maximumObjectBytes) {
                    "The decoded PGS subtitle image exceeds the memory limit."
                }
                bitmap
            }
        }
    }

    private fun store(key: String, bitmap: Bitmap) {
        val bytes = bitmap.allocationByteCount.toLong()
        require(bytes in 1..PGSOverlayPolicy.decodedImageBudgetBytes)
        imageCache.remove(key)?.let { imageCacheBytes -= it.bytes }
        while (imageCacheBytes > PGSOverlayPolicy.decodedImageBudgetBytes - bytes) {
            val oldest = imageCache.entries.firstOrNull() ?: break
            imageCache.remove(oldest.key)
            imageCacheBytes -= oldest.value.bytes
        }
        require(imageCacheBytes <= PGSOverlayPolicy.decodedImageBudgetBytes - bytes) {
            "The PGS subtitle cache exceeded the device memory limit."
        }
        imageCache[key] = CacheEntry(bitmap, bytes)
        imageCacheBytes += bytes
    }

    private fun ensureCurrent(selection: Long, item: Long, window: Long, index: Long) {
        if (!isCurrent(selection, item, window, index)) {
            throw CancellationException("stale PGS overlay work")
        }
    }

    private fun isCurrent(selection: Long, item: Long, window: Long, index: Long): Boolean =
        selection == selectionGeneration &&
            item == itemGeneration &&
            window == windowGeneration &&
            index == trackIndex

    private fun ensureSelection(selection: Long, index: Long) {
        if (selection != selectionGeneration || index != trackIndex) {
            throw CancellationException("stale PGS overlay selection")
        }
    }

    private fun fail(message: String, selection: Long) {
        if (selection != selectionGeneration) return
        prepareJob?.cancel()
        windowJob?.cancel()
        boundaryJob?.cancel()
        loadedWindow = null
        onFrame(null)
        onStatus(PGSOverlayStatus.Failed)
        onFailure("$message Video playback was kept unchanged.")
    }

    private sealed interface ManifestFetch {
        data class Preparing(val retryAfterMs: Int) : ManifestFetch
        data class Ready(val manifest: PGSOverlayManifest) : ManifestFetch
    }

    private companion object {
        const val MAXIMUM_MANIFEST_BYTES = 64 * 1_024 * 1_024
        const val MAXIMUM_PNG_BYTES = 36 * 1_024 * 1_024
    }
}
