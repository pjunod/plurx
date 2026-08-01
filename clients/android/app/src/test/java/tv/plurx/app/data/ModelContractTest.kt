package tv.plurx.app.data

import kotlinx.serialization.json.Json
import kotlinx.serialization.encodeToString
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ModelContractTest {
    private val json = Json { ignoreUnknownKeys = true }

    @Test
    fun itemAcceptsTheFullWebViewerShape() {
        val item = json.decodeFromString<Item>(
            """{
              "id": 42, "library_id": 7, "kind": "episode", "parent_id": 11,
              "title": "The Example", "year": 2026, "air_date": "2026-07-31",
              "recorded_at": null, "tags": ["demo"], "resolution": 2160,
              "child_count": null, "watch": {"position_ms": 12000, "watched": false},
              "future_server_field": true
            }""".trimIndent(),
        )
        assertEquals(42L, item.id)
        assertEquals(2160L, item.resolution)
        assertEquals(listOf("demo"), item.tags)
        assertFalse(item.watch!!.watched)
    }

    @Test
    fun decisionCarriesTheExecutableDeliveryAndPlaybackControls() {
        val decision = json.decodeFromString<Decision>(
            """{
              "file_id": 9, "method": "transcode", "play_url": "/stream.mp4",
              "delivery": {"mode": "transcode", "sessions_url": "/hls/sessions"},
              "reasons": ["HDR tone map"],
              "markers": [{"kind":"intro","label":"Skip intro","start_ms":1000,"end_ms":3000}],
              "audio_offset_ms": 50, "declared_offset_ms": -21
            }""".trimIndent(),
        )
        assertEquals("transcode", decision.delivery!!.mode)
        assertEquals(50L, decision.audio_offset_ms)
        assertEquals("Skip intro", decision.markers.single().label)
    }

    @Test
    fun audioSyncRidesOnlyOnThePlaybackSessionRequest() {
        val wire = Json { explicitNulls = false }
        val adjusted = wire.encodeToString(
            CreateSessionReq(playback_id = "player", audio_offset_ms = 250),
        )
        val freshPlay = wire.encodeToString(CreateSessionReq(playback_id = "next-player"))

        assertTrue(adjusted.contains("\"audio_offset_ms\":250"))
        assertFalse(freshPlay.contains("audio_offset_ms"))
    }
}
