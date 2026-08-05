package tv.plurx.app.player

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

@Serializable
internal data class PGSOverlayPreparing(
    val state: String,
    @SerialName("retry_after_ms") val retryAfterMs: Int,
)

@Serializable
internal data class PGSOverlayManifest(
    val schema: Int,
    val generation: String,
    @SerialName("file_id") val fileId: Long,
    @SerialName("track_index") val trackIndex: Long,
    val kind: String,
    val timebase: String,
    @SerialName("duration_ms") val durationMs: Long,
    val cues: List<PGSOverlayCue>,
) {
    fun validated(expectedFileId: Long, expectedTrackIndex: Long): PGSOverlayManifest {
        require(
            schema == 1 &&
                generation.isLowercaseSha256() &&
                fileId == expectedFileId &&
                trackIndex == expectedTrackIndex &&
                kind == "pgs" &&
                timebase == "source_ms" &&
                durationMs > 0 &&
                cues.size <= PGSOverlayPolicy.maximumManifestCues,
        ) { "invalid PGS overlay manifest header" }

        var previousEnd = 0L
        val dimensions = mutableMapOf<String, Pair<Int, Int>>()
        cues.forEach { cue ->
            require(
                cue.id.isNotBlank() &&
                    cue.startMs >= previousEnd &&
                    cue.endMs > cue.startMs &&
                    cue.endMs <= durationMs &&
                    cue.canvasWidth in 1..PGSOverlayPolicy.maximumCanvasWidth &&
                    cue.canvasHeight in 1..PGSOverlayPolicy.maximumCanvasHeight &&
                    cue.objects.size <= PGSOverlayPolicy.maximumObjectsPerCue,
            ) { "invalid PGS overlay cue" }
            previousEnd = cue.endMs

            cue.objects.forEach { object_ ->
                require(
                    object_.x >= 0 &&
                        object_.y >= 0 &&
                        object_.width > 0 &&
                        object_.height > 0 &&
                        object_.x <= cue.canvasWidth &&
                        object_.y <= cue.canvasHeight &&
                        object_.width <= cue.canvasWidth - object_.x &&
                        object_.height <= cue.canvasHeight - object_.y &&
                        objectHash(object_.image) != null,
                ) { "invalid PGS overlay object" }
                val existing = dimensions[object_.image]
                require(existing == null || existing == (object_.width to object_.height)) {
                    "PGS overlay object dimensions changed"
                }
                if (existing == null) {
                    dimensions[object_.image] = object_.width to object_.height
                }
            }
        }
        return this
    }

    fun objectHash(path: String): String? {
        val prefix = "overlay/$generation/objects/"
        if (!path.startsWith(prefix) || !path.endsWith(".png")) return null
        return path.removePrefix(prefix).removeSuffix(".png")
            .takeIf(String::isLowercaseSha256)
    }
}

@Serializable
internal data class PGSOverlayCue(
    val id: String,
    @SerialName("start_ms") val startMs: Long,
    @SerialName("end_ms") val endMs: Long,
    @SerialName("canvas_width") val canvasWidth: Int,
    @SerialName("canvas_height") val canvasHeight: Int,
    val objects: List<PGSOverlayObject>,
)

@Serializable
internal data class PGSOverlayObject(
    val image: String,
    val x: Int,
    val y: Int,
    val width: Int,
    val height: Int,
)

internal data class PGSOverlayTimeWindow(
    val lowerMs: Long,
    val upperExclusiveMs: Long,
) {
    init {
        require(lowerMs >= 0 && upperExclusiveMs > lowerMs)
    }

    fun contains(timeMs: Long): Boolean = timeMs in lowerMs until upperExclusiveMs
}

internal data class PGSOverlayRect(
    val x: Float,
    val y: Float,
    val width: Float,
    val height: Float,
)

internal enum class PGSOverlayStatus(val label: String?) {
    Off(null),
    Preparing("PGS overlay · preparing"),
    Ready("PGS overlay"),
    Failed("PGS overlay · unavailable"),
}

internal object PGSOverlayPolicy {
    const val maximumCanvasWidth = 4_096
    const val maximumCanvasHeight = 2_160
    const val maximumObjectsPerCue = 64
    const val maximumManifestCues = 250_000
    const val maximumWindowCues = 2_048
    const val maximumObjectBytes = 36L * 1_024 * 1_024
    const val decodedImageBudgetBytes = 96L * 1_024 * 1_024
    const val lookBehindMs = 5_000L
    const val lookAheadMs = 90_000L
    const val refreshMarginMs = 20_000L
    const val maximumPrepareMs = 10L * 60 * 1_000

    fun windowAt(sourceTimeMs: Long, durationMs: Long): PGSOverlayTimeWindow {
        require(durationMs > 0)
        val pivot = sourceTimeMs.coerceIn(0, durationMs - 1)
        val lower = (pivot - lookBehindMs).coerceAtLeast(0)
        val lookAheadEnd = if (pivot > Long.MAX_VALUE - lookAheadMs) {
            Long.MAX_VALUE
        } else {
            pivot + lookAheadMs
        }
        val upper = lookAheadEnd
            .coerceAtMost(durationMs)
            .coerceAtLeast(lower + 1)
        return PGSOverlayTimeWindow(lower, upper)
    }

    fun shouldRefresh(sourceTimeMs: Long, loaded: PGSOverlayTimeWindow?): Boolean =
        loaded == null ||
            sourceTimeMs < loaded.lowerMs ||
            sourceTimeMs >= loaded.upperExclusiveMs - refreshMarginMs

    fun activeCueIndex(cues: List<PGSOverlayCue>, sourceTimeMs: Long): Int? {
        var low = 0
        var high = cues.lastIndex
        var candidate = -1
        while (low <= high) {
            val middle = low + (high - low) / 2
            if (cues[middle].startMs <= sourceTimeMs) {
                candidate = middle
                low = middle + 1
            } else {
                high = middle - 1
            }
        }
        return candidate.takeIf { it >= 0 && sourceTimeMs < cues[it].endMs }
    }

    fun nextBoundaryMs(cues: List<PGSOverlayCue>, sourceTimeMs: Long): Long? {
        activeCueIndex(cues, sourceTimeMs)?.let { return cues[it].endMs }
        var low = 0
        var high = cues.size
        while (low < high) {
            val middle = low + (high - low) / 2
            if (cues[middle].startMs <= sourceTimeMs) low = middle + 1 else high = middle
        }
        return cues.getOrNull(low)?.startMs
    }

    fun cuesInWindow(
        cues: List<PGSOverlayCue>,
        window: PGSOverlayTimeWindow,
    ): List<PGSOverlayCue> = cues.asSequence()
        .dropWhile { it.endMs <= window.lowerMs }
        .takeWhile { it.startMs < window.upperExclusiveMs }
        // One extra lets the caller reject an over-dense window instead of
        // silently omitting the active cue after a truncated prefix.
        .take(maximumWindowCues + 1)
        .toList()

    fun decodedWindowBytes(cues: List<PGSOverlayCue>): Long? {
        val paths = mutableSetOf<String>()
        var total = 0L
        cues.asSequence().flatMap { it.objects.asSequence() }.forEach { object_ ->
            if (!paths.add(object_.image)) return@forEach
            val bytes = runCatching {
                Math.multiplyExact(Math.multiplyExact(object_.width.toLong(), object_.height.toLong()), 4L)
            }.getOrNull() ?: return null
            if (bytes <= 0 || bytes > maximumObjectBytes || bytes > decodedImageBudgetBytes - total) {
                return null
            }
            total += bytes
        }
        return total
    }

    fun videoRect(containerWidth: Float, containerHeight: Float, videoAspect: Float): PGSOverlayRect {
        if (containerWidth <= 0 || containerHeight <= 0 || videoAspect <= 0) {
            return PGSOverlayRect(0f, 0f, 0f, 0f)
        }
        val containerAspect = containerWidth / containerHeight
        return if (containerAspect > videoAspect) {
            val width = containerHeight * videoAspect
            PGSOverlayRect((containerWidth - width) / 2f, 0f, width, containerHeight)
        } else {
            val height = containerWidth / videoAspect
            PGSOverlayRect(0f, (containerHeight - height) / 2f, containerWidth, height)
        }
    }

    fun objectRect(
        object_: PGSOverlayObject,
        canvasWidth: Int,
        canvasHeight: Int,
        destination: PGSOverlayRect,
    ): PGSOverlayRect {
        if (canvasWidth <= 0 || canvasHeight <= 0) return PGSOverlayRect(0f, 0f, 0f, 0f)
        val scale = minOf(destination.width / canvasWidth, destination.height / canvasHeight)
        val originX = destination.x + (destination.width - canvasWidth * scale) / 2f
        val originY = destination.y + (destination.height - canvasHeight * scale) / 2f
        return PGSOverlayRect(
            x = originX + object_.x * scale,
            y = originY + object_.y * scale,
            width = object_.width * scale,
            height = object_.height * scale,
        )
    }
}

private fun String.isLowercaseSha256(): Boolean =
    length == 64 && all { it in '0'..'9' || it in 'a'..'f' }
