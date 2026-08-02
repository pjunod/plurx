package tv.plurx.app.data

import kotlinx.serialization.json.Json
import kotlinx.serialization.encodeToString
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
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
    fun itemDecodesTheServersAddedAtForMergedCollectionSorting() {
        val item = json.decodeFromString<Item>(
            """{"id": 1, "kind": "movie", "title": "A", "added_at": 1754092800}""",
        )
        assertEquals(1_754_092_800L, item.added_at)
    }

    @Test
    fun decisionCarriesTheSourceHeightAndTheServersLadder() {
        val decision = json.decodeFromString<Decision>(
            """{
              "file_id": 9, "method": "remux", "play_url": "/stream.mp4",
              "delivery": {
                "mode": "remux", "url": "/stream.mp4",
                "sessions_url": "/hls/sessions", "aac": true,
                "preserve_dolby_vision": true
              },
              "source": {"width": 3840, "height": 2160, "hdr": "dolby_vision", "bit_depth": 10},
              "ladder": [
                {"height": 1080, "total_kbps": 8192, "peak_kbps": 12416},
                {"height": 720, "total_kbps": 4096, "peak_kbps": 6208}
              ],
              "subtitles": [{"index": 2, "codec": "subrip", "language": "eng", "text": true}]
            }""".trimIndent(),
        )
        // The number every height promise is made of.
        assertEquals(2160L, decision.source!!.height)
        assertTrue(decision.delivery!!.preserve_dolby_vision)
        assertTrue(decision.delivery.aac)
        assertEquals(1080, decision.ladder.first().height)
        assertEquals(2L, decision.subtitles.single().index)
    }

    @Test
    fun bothResponsesCarryTheDeliveredDynamicRange() {
        // The badge's whole contract: the decision says what the plan would
        // deliver, the session says what it actually is, same vocabulary.
        val decision = json.decodeFromString<Decision>(
            """{
              "file_id": 6041, "method": "remux", "play_url": "/stream.mp4",
              "delivery": {"mode": "remux", "preserve_dolby_vision": false},
              "source": {"hdr": "dolby_vision", "hdr_format": "Dolby Vision · Profile 7 (HDR10-compatible)"},
              "delivered_dynamic_range": "hdr10"
            }""".trimIndent(),
        )
        assertEquals("hdr10", decision.delivered_dynamic_range)

        val start = json.decodeFromString<HlsStart>(
            """{
              "session_id": "s2", "playlist_url": "/hls/s2/index.m3u8",
              "delivered_dynamic_range": "sdr"
            }""".trimIndent(),
        )
        assertEquals("sdr", start.delivered_dynamic_range)

        // An older server omits it entirely, and both stay decodable — the
        // client then falls back to describing the source alone.
        val old = json.decodeFromString<Decision>(
            """{"file_id": 1, "method": "direct_play", "play_url": "/direct"}""",
        )
        assertNull(old.delivered_dynamic_range)
        assertNull(
            json.decodeFromString<HlsStart>(
                """{"session_id": "s3", "playlist_url": "/hls/s3/index.m3u8"}""",
            ).delivered_dynamic_range,
        )
    }

    @Test
    fun aContainerCarriesTheWatchRollupTheGridFiltersOn() {
        // `list_items` attaches this for shows, seasons, and folders; a leaf
        // still carries only `watch`.
        val show = json.decodeFromString<Item>(
            """{
              "id": 90, "kind": "show", "title": "Half watched",
              "rollup": {"leaves": 20, "watched": 9}
            }""".trimIndent(),
        )
        assertEquals(20L, show.rollup!!.leaves)
        assertEquals(9L, show.rollup.watched)

        val episode = json.decodeFromString<Item>(
            """{"id": 91, "kind": "episode", "title": "Leaf", "watch": {"watched": true}}""",
        )
        assertNull(episode.rollup)
    }

    @Test
    fun sessionStartCarriesVodAndTheLadder() {
        val start = json.decodeFromString<HlsStart>(
            """{
              "session_id": "s1", "playlist_url": "/hls/s1/index.m3u8?native=1&subtitle=2",
              "duration_ms": 7200000, "start_seconds": 0.0, "encoder": "cached",
              "vod": true, "ladder": [{"height": 1080, "total_kbps": 8192, "peak_kbps": 12416}]
            }""".trimIndent(),
        )
        assertTrue(start.vod)
        assertEquals(1080, start.ladder.single().height)
    }

    @Test
    fun aTextSubtitleAsksForARenditionAndNeverAnEncoder() {
        val wire = Json { explicitNulls = false }
        // Absolute subtitle-stream index, not an ordinal among the text ones.
        val native = wire.encodeToString(
            CreateSessionReq(
                playback_id = "player",
                native_subtitles = true,
                subtitle = 3,
                copy = true,
                aac = true,
                preserve_dolby_vision = true,
                height = 2160,
            ),
        )
        assertTrue(native.contains("\"native_subtitles\":true"))
        assertTrue(native.contains("\"subtitle\":3"))
        assertTrue(native.contains("\"copy\":true"))
        assertTrue(native.contains("\"preserve_dolby_vision\":true"))
        // The one field that costs the stream its resolution and its HDR.
        assertFalse(native.contains("subtitle_burn"))

        // A bitmap track has no text to send: burn, at source height.
        val burn = wire.encodeToString(
            CreateSessionReq(playback_id = "player", subtitle_burn = 4, height = 2160),
        )
        assertTrue(burn.contains("\"subtitle_burn\":4"))
        assertTrue(burn.contains("\"height\":2160"))
        assertFalse(burn.contains("native_subtitles"))
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
