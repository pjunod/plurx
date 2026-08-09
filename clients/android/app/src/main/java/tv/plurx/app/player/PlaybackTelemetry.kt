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
    val ua: String,
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

/** Player values the telemetry path reads, isolated so its wiring is JVM-testable. */
internal interface PlaybackTelemetryPlayer {
    val buffering: Boolean
    val playbackRequested: Boolean
    val playerPositionMs: Long
    val mediaPositionMs: Long
    val bufferedPositionMs: Long
    val videoHeight: Int
}

internal data class PlaybackTelemetryContext(
    val method: String,
    val encoder: String?,
    val sessionId: String?,
)

/**
 * The production bridge from Controller player callbacks to typed wire events.
 * Keeping the bridge free of Android runtime objects lets a fake PlanLike and
 * stub player prove the complete start -> prepared -> first-frame path.
 */
internal class ControllerPlaybackTelemetry(
    private val plan: PlanLike,
    private val player: PlaybackTelemetryPlayer,
    private val context: () -> PlaybackTelemetryContext,
    private val emit: (PlaybackClientLog) -> Unit,
) {
    private val ttffTracker = PlaybackTTFFTracker()
    private val stallTracker = BufferingStallTracker()

    fun begin(reason: String, observedAtMs: Long): PlaybackAttempt {
        stallTracker.reset()
        return ttffTracker.begin(reason, observedAtMs)
    }

    fun prepared(attempt: PlaybackAttempt) = ttffTracker.prepared(attempt)

    fun cancel(attempt: PlaybackAttempt) = ttffTracker.cancel(attempt)

    fun firstFrame(observedAtMs: Long): TtffMeasurement? =
        ttffTracker.firstFrame(observedAtMs)?.also { measurement ->
            report(
                event = "ttff",
                level = "info",
                message = "first frame after ${measurement.elapsedMs} ms",
                ms = measurement.elapsedMs,
                attempt = measurement.attempt,
            )
        }

    fun sampleStall(establishedPlayback: Boolean, observedAtMs: Long): StallMeasurement? =
        stallTracker.sample(
            buffering = player.buffering,
            playbackRequested = player.playbackRequested,
            establishedPlayback = establishedPlayback,
            positionMs = player.mediaPositionMs,
            observedAtMs = observedAtMs,
        )?.also { measurement ->
            report(
                event = "stall",
                level = "warn",
                message = "buffering playhead stagnant for ${measurement.durationMs} ms",
                ms = measurement.durationMs,
                detail = "state=recovered position_ms=${measurement.positionMs}",
            )
        }

    fun report(
        event: String,
        level: String,
        message: String,
        code: Int? = null,
        detail: String? = null,
        ms: Long? = null,
        attempt: PlaybackAttempt? = ttffTracker.currentAttempt(),
    ) {
        val currentPosition = player.playerPositionMs
        val bufferedPosition = player.bufferedPositionMs
        val runway = if (currentPosition >= 0 && bufferedPosition >= currentPosition) {
            (bufferedPosition - currentPosition) / 1_000.0
        } else {
            null
        }
        val current = context()
        emit(
            PlaybackClientLog(
                level = level,
                event = event,
                message = message,
                method = current.method,
                code = code,
                title = plan.title,
                fileId = plan.fileId,
                vcodec = plan.videoCodec,
                detail = detail,
                ua = "Android Media3",
                attempt = attempt?.id,
                reason = attempt?.reason,
                runway = runway,
                ms = ms,
                height = player.videoHeight.takeIf { it > 0 } ?: plan.sourceHeight,
                encoder = current.encoder,
                sessionId = current.sessionId,
            ),
        )
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
 * Reports one final-duration event when an established, requested playback
 * recovers after the playhead was stagnant for the threshold while Media3 was
 * buffering. Startup and paused buffering are excluded; policy belongs to N4.
 */
internal class BufferingStallTracker(
    private val thresholdMs: Long = 6_000,
    private val progressThresholdMs: Long = 250,
) {
    private var baselinePositionMs: Long? = null
    private var stagnantSinceMs: Long? = null

    fun sample(
        buffering: Boolean,
        playbackRequested: Boolean,
        establishedPlayback: Boolean,
        positionMs: Long,
        observedAtMs: Long,
    ): StallMeasurement? {
        // Startup buffering and a paused player's buffer work are not viewer
        // stalls. Drop any partial interval instead of reporting it later.
        if (!playbackRequested || !establishedPlayback) {
            reset()
            return null
        }

        if (!buffering) {
            val measurement = finish(positionMs, observedAtMs)
            reset()
            return measurement
        }

        val baseline = baselinePositionMs
        if (baseline == null) {
            baselinePositionMs = positionMs
            stagnantSinceMs = observedAtMs
            return null
        }

        if (abs(positionMs - baseline) < progressThresholdMs) return null

        // A moving playhead ends the stagnant interval even if Media3 has not
        // changed state yet. Report its final duration, then arm a fresh
        // baseline in case buffering continues.
        val measurement = finish(positionMs, observedAtMs)
        baselinePositionMs = positionMs
        stagnantSinceMs = observedAtMs
        return measurement
    }

    private fun finish(positionMs: Long, observedAtMs: Long): StallMeasurement? {
        val since = stagnantSinceMs ?: return null
        val durationMs = (observedAtMs - since).coerceAtLeast(0)
        return if (durationMs >= thresholdMs) {
            StallMeasurement(durationMs = durationMs, positionMs = positionMs)
        } else {
            null
        }
    }

    fun reset() {
        baselinePositionMs = null
        stagnantSinceMs = null
    }
}

internal fun monotonicNowMs(): Long = System.nanoTime() / 1_000_000
