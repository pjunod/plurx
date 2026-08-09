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

    private class StubTelemetryPlayer : PlaybackTelemetryPlayer {
        override var buffering = false
        override var playbackRequested = true
        override var playerPositionMs = 2_000L
        override var mediaPositionMs = 122_000L
        override var bufferedPositionMs = 5_000L
        override var videoHeight = 1080
    }

    private val fakePlan = object : PlanLike {
        override val title = "Wiring Fixture"
        override val videoCodec = "hevc"
        override val fileId = 42L
        override val playUrl = "http://plurx.test/video"
        override val mode = "transcode"
        override val durationMs = 300_000L
        override val audio = emptyList<tv.plurx.app.data.AudioTrack>()
        override val subtitles = emptyList<tv.plurx.app.data.SubTrack>()
        override val sourceHeight = 2160
        override val aac = true
        override val preserveDolbyVision = false
        override val deliveredDynamicRange = "sdr"
    }

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
    fun controllerWiringPostsResumeTtffFromStartThroughFirstFrame() {
        val player = StubTelemetryPlayer()
        val events = mutableListOf<PlaybackClientLog>()
        val telemetry = ControllerPlaybackTelemetry(
            plan = fakePlan,
            player = player,
            context = {
                PlaybackTelemetryContext(
                    method = "transcode",
                    encoder = "qsv",
                    sessionId = "session-7",
                )
            },
            emit = events::add,
        )

        val attempt = telemetry.begin("resume", observedAtMs = 100)
        telemetry.prepared(attempt)
        telemetry.firstFrame(observedAtMs = 850)

        val event = events.single()
        assertEquals("ttff", event.event)
        assertEquals("resume", event.reason)
        assertEquals("a1", event.attempt)
        assertEquals(750L, event.ms)
        assertEquals("Android Media3", event.ua)
        assertEquals("transcode", event.method)
        assertEquals("session-7", event.sessionId)
        assertEquals(3.0, event.runway)
        assertEquals(1080, event.height)
    }

    @Test
    fun stallReportsFinalDurationAtRecoveryAndProgressRearmsIt() {
        val tracker = BufferingStallTracker()

        assertNull(tracker.sample(true, true, true, positionMs = 5_000, observedAtMs = 0))
        assertNull(tracker.sample(true, true, true, positionMs = 5_000, observedAtMs = 5_999))
        // Crossing the threshold does not freeze the reported value at 6 s.
        assertNull(tracker.sample(true, true, true, positionMs = 5_000, observedAtMs = 6_000))
        assertEquals(
            StallMeasurement(durationMs = 8_000, positionMs = 5_000),
            tracker.sample(false, true, true, positionMs = 5_000, observedAtMs = 8_000),
        )

        // Progress ends one stagnant interval and begins a fresh one even if
        // Media3 still says buffering.
        assertNull(tracker.sample(true, true, true, positionMs = 5_000, observedAtMs = 10_000))
        assertEquals(
            StallMeasurement(durationMs = 7_000, positionMs = 5_250),
            tracker.sample(true, true, true, positionMs = 5_250, observedAtMs = 17_000),
        )
        assertNull(tracker.sample(true, true, true, positionMs = 5_250, observedAtMs = 22_999))
        assertEquals(
            StallMeasurement(durationMs = 6_000, positionMs = 5_250),
            tracker.sample(false, true, true, positionMs = 5_250, observedAtMs = 23_000),
        )
    }

    @Test
    fun startupAndPausedBufferingNeverBecomeStalls() {
        val tracker = BufferingStallTracker()

        assertNull(tracker.sample(true, true, false, positionMs = 0, observedAtMs = 0))
        assertNull(tracker.sample(true, true, false, positionMs = 0, observedAtMs = 20_000))

        assertNull(tracker.sample(true, true, true, positionMs = 5_000, observedAtMs = 21_000))
        assertNull(tracker.sample(true, true, true, positionMs = 5_000, observedAtMs = 27_000))
        // Pausing cancels the in-flight interval instead of emitting it later.
        assertNull(tracker.sample(true, false, true, positionMs = 5_000, observedAtMs = 28_000))
        assertNull(tracker.sample(false, false, true, positionMs = 5_000, observedAtMs = 35_000))

        assertNull(tracker.sample(true, true, true, positionMs = 5_000, observedAtMs = 36_000))
        assertNull(tracker.sample(false, true, true, positionMs = 5_000, observedAtMs = 41_999))
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
                ua = "Android Media3",
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
        assertEquals("Android Media3", json.getValue("ua").jsonPrimitive.content)
        assertFalse(json.containsKey("src"))
        assertFalse(json.containsKey("session"))
        assertFalse(body.contains("Bearer "))
        assertNull(request.header("Authorization"))
    }
}
