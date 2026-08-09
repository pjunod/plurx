package tv.plurx.app.player

import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import okio.Buffer
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Test
import tv.plurx.app.data.Net

class PlaybackTelemetryTest {

    @Test
    fun ttffIsTheFirstFrameAfterPrepareAndOnlyThatFrame() {
        val tracker = PlaybackTTFFTracker()
        val attempt = tracker.begin("cold-start", observedAtMs = 1_000)

        // A departing item's last frame cannot satisfy the new attempt.
        assertNull(tracker.firstFrame(observedAtMs = 1_100))
        tracker.prepared(attempt)

        val measurement = tracker.firstFrame(observedAtMs = 1_725)
        assertEquals("a1", measurement?.attempt?.id)
        assertEquals("cold-start", measurement?.attempt?.reason)
        assertEquals(725L, measurement?.elapsedMs)
        assertNull(tracker.firstFrame(observedAtMs = 1_900))

        val retry = tracker.begin("fallback", observedAtMs = 2_000)
        tracker.prepared(retry)
        assertEquals("a2", tracker.firstFrame(observedAtMs = 2_300)?.attempt?.id)
    }

    @Test
    fun aSupersededAttemptCannotCancelTheCurrentOne() {
        val tracker = PlaybackTTFFTracker()
        val old = tracker.begin("seek", observedAtMs = 10)
        val current = tracker.begin("audio", observedAtMs = 20)

        tracker.cancel(old)
        tracker.prepared(current)

        assertEquals("audio", tracker.firstFrame(observedAtMs = 45)?.attempt?.reason)
    }

    @Test
    fun stallRequiresSixSecondsOfStagnantBufferingAndProgressRearmsIt() {
        val tracker = BufferingStallTracker()

        assertNull(tracker.sample(buffering = true, positionMs = 5_000, observedAtMs = 0))
        assertNull(tracker.sample(buffering = true, positionMs = 5_000, observedAtMs = 5_999))
        assertEquals(
            StallMeasurement(durationMs = 6_000, positionMs = 5_000),
            tracker.sample(buffering = true, positionMs = 5_000, observedAtMs = 6_000),
        )
        // One event per stagnant interval.
        assertNull(tracker.sample(buffering = true, positionMs = 5_000, observedAtMs = 7_000))

        // 250 ms of playhead progress begins a fresh interval.
        assertNull(tracker.sample(buffering = true, positionMs = 5_250, observedAtMs = 8_000))
        assertEquals(
            StallMeasurement(durationMs = 6_000, positionMs = 5_250),
            tracker.sample(buffering = true, positionMs = 5_250, observedAtMs = 14_000),
        )

        assertNull(tracker.sample(buffering = false, positionMs = 5_250, observedAtMs = 15_000))
        assertNull(tracker.sample(buffering = true, positionMs = 5_250, observedAtMs = 16_000))
    }

    @Test
    fun clientLogRequestMatchesTheTypedServerContractWithoutSecretsOrMediaUrls() {
        val request = clientLogRequest(
            origin = "http://plurx.test:32400/",
            event = PlaybackClientLog(
                level = "info",
                event = "ttff",
                message = "first frame after 900 ms",
                method = "transcode",
                title = "Example",
                fileId = 42,
                vcodec = "hevc",
                attempt = "a1",
                reason = "cold-start",
                runway = 3.2,
                ms = 900,
                height = 1080,
                encoder = "qsv",
                sessionId = "session-7",
            ),
        )
        val buffer = Buffer()
        request.body!!.writeTo(buffer)
        val body = buffer.readUtf8()
        val json = Net.json.parseToJsonElement(body).jsonObject

        assertEquals("POST", request.method)
        assertEquals("http://plurx.test:32400/api/v1/client-log", request.url.toString())
        assertEquals("application", request.body!!.contentType()?.type)
        assertEquals("json", request.body!!.contentType()?.subtype)
        assertEquals("ttff", json.getValue("event").jsonPrimitive.content)
        assertEquals("transcode", json.getValue("method").jsonPrimitive.content)
        assertEquals("42", json.getValue("file_id").jsonPrimitive.content)
        assertEquals("session-7", json.getValue("session_id").jsonPrimitive.content)
        assertEquals("900", json.getValue("ms").jsonPrimitive.content)
        assertFalse(json.containsKey("src"))
        assertFalse(json.containsKey("session"))
        assertFalse(body.contains("Bearer "))
        assertNull(request.header("Authorization"))
    }
}
