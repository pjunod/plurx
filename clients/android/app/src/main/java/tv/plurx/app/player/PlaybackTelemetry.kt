package tv.plurx.app.player

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.encodeToString
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import tv.plurx.app.data.Net
import tv.plurx.app.data.Session
import kotlin.math.abs

/** The typed subset of `POST /api/v1/client-log` Android can measure. */
@Serializable
internal data class PlaybackClientLog(
    val level: String,
    val event: String,
    val message: String,
    val method: String? = null,
    val code: Int? = null,
    val title: String? = null,
    @SerialName("file_id") val fileId: Long? = null,
    val vcodec: String? = null,
    val detail: String? = null,
    val ua: String = "Android Media3",
    val attempt: String? = null,
    val reason: String? = null,
    val runway: Double? = null,
    val ms: Long? = null,
    val height: Int? = null,
    val encoder: String? = null,
    @SerialName("session_id") val sessionId: String? = null,
)

/** Build the request separately so the wire contract stays unit-testable. */
internal fun clientLogRequest(origin: String, event: PlaybackClientLog): Request {
    val body = Net.json.encodeToString(event)
        .toRequestBody("application/json".toMediaType())
    return Request.Builder()
        .url("${origin.trimEnd('/')}/api/v1/client-log")
        .post(body)
        .build()
}

/**
 * Fire-and-forget diagnostics through the app's shared authenticated client.
 * Missing connection state means offline playback, where no beacon is sent.
 */
internal fun postPlaybackClientLog(scope: CoroutineScope, event: PlaybackClientLog) {
    val origin = Session.origin
    if (origin.isBlank() || Session.token.isNullOrBlank()) return
    val request = try {
        clientLogRequest(origin, event)
    } catch (_: Exception) {
        return
    }
    scope.launch(Dispatchers.IO) {
        try {
            Net.client.newCall(request).execute().use { }
        } catch (_: Exception) {
            // Telemetry is best effort and must never become a playback error.
        }
    }
}

internal data class PlaybackAttempt(
    val id: String,
    val reason: String,
    val startedAtMs: Long,
)

internal data class TtffMeasurement(
    val attempt: PlaybackAttempt,
    val elapsedMs: Long,
)

/** One first-frame measurement for each prepare-backed playback attempt. */
internal class PlaybackTTFFTracker {
    private var sequence = 0L
    private var current: PlaybackAttempt? = null
    private var preparedAttemptId: String? = null

    fun begin(reason: String, observedAtMs: Long): PlaybackAttempt {
        val attempt = PlaybackAttempt(
            id = "a${++sequence}",
            reason = reason,
            startedAtMs = observedAtMs,
        )
        current = attempt
        preparedAttemptId = null
        return attempt
    }

    /** Ignore any last frame from the departing item until prepare has run. */
    fun prepared(attempt: PlaybackAttempt) {
        if (current?.id == attempt.id) preparedAttemptId = attempt.id
    }

    fun firstFrame(observedAtMs: Long): TtffMeasurement? {
        val attempt = current ?: return null
        if (preparedAttemptId != attempt.id) return null
        preparedAttemptId = null
        return TtffMeasurement(
            attempt = attempt,
            elapsedMs = (observedAtMs - attempt.startedAtMs).coerceAtLeast(0),
        )
    }

    fun cancel(attempt: PlaybackAttempt) {
        if (current?.id != attempt.id) return
        current = null
        preparedAttemptId = null
    }

    fun currentAttempt(): PlaybackAttempt? = current
}

internal data class StallMeasurement(
    val durationMs: Long,
    val positionMs: Long,
)

/**
 * Reports one event after the playhead is stagnant for the threshold while
 * Media3 is buffering. Progress re-arms it; policy belongs to Performance N4.
 */
internal class BufferingStallTracker(
    private val thresholdMs: Long = 6_000,
    private val progressThresholdMs: Long = 250,
) {
    private var baselinePositionMs: Long? = null
    private var stagnantSinceMs: Long? = null
    private var reported = false

    fun sample(
        buffering: Boolean,
        positionMs: Long,
        observedAtMs: Long,
    ): StallMeasurement? {
        if (!buffering) {
            reset()
            return null
        }

        val baseline = baselinePositionMs
        if (
            baseline == null ||
            abs(positionMs - baseline) >= progressThresholdMs
        ) {
            baselinePositionMs = positionMs
            stagnantSinceMs = observedAtMs
            reported = false
            return null
        }

        val since = stagnantSinceMs ?: observedAtMs.also { stagnantSinceMs = it }
        val durationMs = (observedAtMs - since).coerceAtLeast(0)
        if (reported || durationMs < thresholdMs) return null
        reported = true
        return StallMeasurement(durationMs = durationMs, positionMs = positionMs)
    }

    fun reset() {
        baselinePositionMs = null
        stagnantSinceMs = null
        reported = false
    }
}

internal fun monotonicNowMs(): Long = System.nanoTime() / 1_000_000
