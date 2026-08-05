package tv.plurx.app.player

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import tv.plurx.app.data.HlsStart

class MediaOriginTest {
    @Test
    fun hlsPrefersTheTrueOriginAndRetainsOldServerFallbacks() {
        val modern = HlsStart(
            session_id = "modern",
            playlist_url = "/hls/modern/index.m3u8",
            start_seconds = 10.5,
            media_origin_ms = 10_000,
        )
        assertEquals(10_000L, sessionMediaOriginMs(modern))

        val oldServer = HlsStart(
            session_id = "old",
            playlist_url = "/hls/old/index.m3u8",
            start_seconds = 10.5,
        )
        assertEquals(10_500L, sessionMediaOriginMs(oldServer))

        val cached = modern.copy(vod = true)
        assertEquals(0L, sessionMediaOriginMs(cached))
    }

    @Test
    fun progressiveHeaderParsingIsCaseInsensitiveAndRejectsBadValues() {
        assertEquals(
            10_000L,
            mediaOriginMsFromHeaders(mapOf("x-plurx-media-origin-ms" to listOf("10000"))),
        )
        assertNull(mediaOriginMsFromHeaders(mapOf(MEDIA_ORIGIN_HEADER to listOf("-1"))))
        assertNull(mediaOriginMsFromHeaders(mapOf(MEDIA_ORIGIN_HEADER to listOf("not-a-time"))))
    }

    @Test
    fun aLateProgressiveResponseCannotReplaceTheNewSeekEpoch() {
        val origin = ProgressiveMediaOrigin()
        val first = "http://server/stream.mp4?start=10.5"
        val second = "http://server/stream.mp4?start=20.5"
        val headers = mapOf(MEDIA_ORIGIN_HEADER to listOf("10000"))

        origin.begin(first, 10_500)
        assertTrue(origin.acceptResponse(first, headers))
        assertEquals(10_000L, origin.currentOriginMs())

        origin.begin(second, 20_500)
        assertFalse(origin.acceptResponse(first, headers))
        assertEquals(20_500L, origin.currentOriginMs())

        assertTrue(
            origin.acceptResponse(
                second,
                mapOf(MEDIA_ORIGIN_HEADER to listOf("20000")),
            ),
        )
        assertEquals(20_000L, origin.currentOriginMs())
    }
}
