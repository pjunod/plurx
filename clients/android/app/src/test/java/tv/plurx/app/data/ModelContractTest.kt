package tv.plurx.app.data

import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import kotlinx.serialization.encodeToString
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import retrofit2.http.GET
import retrofit2.http.POST
import retrofit2.http.PUT
import retrofit2.http.Query
import retrofit2.http.DELETE

@Serializable
private data class NativeApiContractFixture(
    val server: Server,
    val item_detail: ItemDetail,
    val audiobook_detail: ItemDetail,
    val page: Page,
    val decision: Decision,
)

class ModelContractTest {
    // The wire Json, restated: `Net` builds it with exactly these two.
    private val json = Json {
        ignoreUnknownKeys = true
        explicitNulls = false
    }

    @Test
    fun readingStateRoutesMatchTheServerContract() {
        val get = PlurxApi::class.java.declaredMethods.single { it.name == "readingState" }
        assertEquals(
            "items/{id}/reading-state",
            requireNotNull(get.getAnnotation(GET::class.java)).value,
        )
        assertEquals(
            listOf("file_id"),
            get.parameterAnnotations
                .flatMap { annotations -> annotations.filterIsInstance<Query>() }
                .map(Query::value),
        )
        val put = PlurxApi::class.java.declaredMethods.single { it.name == "putReadingState" }
        assertEquals(
            "items/{id}/reading-state",
            requireNotNull(put.getAnnotation(PUT::class.java)).value,
        )
        val delete = PlurxApi::class.java.declaredMethods.single { it.name == "deleteReadingState" }
        assertEquals(
            "items/{id}/reading-state",
            requireNotNull(delete.getAnnotation(DELETE::class.java)).value,
        )
    }

    @Test
    fun offlineRoutesMatchTheServerContract() {
        val options = PlurxApi::class.java.declaredMethods.single { it.name == "offlineOptions" }
        assertEquals(
            "files/{id}/offline-options",
            requireNotNull(options.getAnnotation(GET::class.java)).value,
        )
        assertEquals(
            setOf("audio_lang", "subtitle_lang"),
            options.parameterAnnotations
                .flatMap { annotations -> annotations.filterIsInstance<Query>() }
                .map(Query::value)
                .toSet(),
        )
        val create = PlurxApi::class.java.declaredMethods.single {
            it.name == "createOfflinePackage"
        }
        assertEquals(
            "files/{id}/offline-packages",
            requireNotNull(create.getAnnotation(POST::class.java)).value,
        )
    }

    @Test
    fun sharedNativeApiFixtureDecodesWithoutConsumerDrift() {
        val wire = checkNotNull(javaClass.classLoader?.getResource("native-api.json")) {
            "tests/contracts/native-api.json is not on the JVM test classpath"
        }.readText()
        val fixture = json.decodeFromString<NativeApiContractFixture>(wire)

        assertEquals("Contract server", fixture.server.name)
        assertEquals("The Contract", fixture.item_detail.item.title)
        assertTrue(fixture.audiobook_detail.item.isAudiobook)
        assertEquals("A. Contract", fixture.audiobook_detail.item.author)
        assertEquals("curator:work:contract", fixture.audiobook_detail.item.book_work_id)
        assertEquals("curator:edition:ebook", fixture.audiobook_detail.editions.first().book_edition_id)
        assertEquals(
            listOf(0L, 60_000L, 180_000L),
            fixture.audiobook_detail.files.map(MediaFileDto::part_offset_ms),
        )
        assertEquals("Opening", fixture.audiobook_detail.files.first().chapters.first().title)
        assertNull(fixture.item_detail.reading)
        assertEquals(20L, fixture.page.items.single().rollup!!.leaves)
        assertEquals("remux", fixture.decision.delivery!!.mode)
        assertEquals("dolby_vision", fixture.decision.delivered_dynamic_range)

        // The shared track facts every first-party detail screen renders.
        val defaults = checkNotNull(fixture.item_detail.files.single().playback_defaults)
        assertEquals(1L, defaults.audio.selected_index)
        assertEquals("selected", defaults.audio.preferred_language_status)
        assertEquals(2L, defaults.subtitle.selected_index)
        assertEquals("eng", defaults.subtitle.preferred_language)
    }

    @Test
    fun readingStateDecodesItsRevisionAndRendererNeutralLocator() {
        val detail = json.decodeFromString<ItemDetail>(
            """{
              "item": {"id": 9, "kind": "book", "title": "Contract Book"},
              "reading": {
                "file_id": 90,
                "revision": {"size": 4096, "mtime": 100},
                "locator": {
                  "version": 1,
                  "href": "Text/chapter-3.xhtml",
                  "locations": {"progression": 0.6, "totalProgression": 0.55}
                },
                "progression": 0.55,
                "completed": false,
                "updated_at": 200
              }
            }""".trimIndent(),
        )

        assertEquals(90L, detail.reading?.file_id)
        assertEquals(4096L, detail.reading?.revision?.size)
        assertEquals("Text/chapter-3.xhtml", detail.reading?.locator?.href)
        assertEquals(0.55, detail.reading?.locator?.locations?.totalProgression)
    }

    /**
     * docs/CLIENTS.md §"Shared track facts": the detail screen marks
     * [PlaybackDefaults] and renders the status; it does not fetch admin
     * settings or re-run `select_tracks`. All five states have to survive the
     * wire, and `unknown` must arrive as itself rather than as `missing`.
     */
    @Test
    fun itemDetailCarriesTheServersOwnTrackDefaultsAndSubtitleFacts() {
        val detail = json.decodeFromString<ItemDetail>(
            """{
              "item": {"id": 7, "kind": "movie", "title": "Dual audio"},
              "files": [{
                "id": 70, "filename": "anime.mkv",
                "audio_streams": [
                  {"index": 0, "codec": "aac", "language": "eng", "default": true},
                  {"index": 1, "codec": "flac", "language": "jpn"}
                ],
                "subtitle_streams": [
                  {"index": 0, "codec": "subrip", "language": "eng", "hearing_impaired": true},
                  {"index": 1, "codec": "ass", "forced": true}
                ],
                "playback_defaults": {
                  "audio": {
                    "selected_index": 1, "preferred_language": "eng",
                    "preferred_language_status": "available"
                  },
                  "subtitle": {
                    "selected_index": 0, "preferred_language": "eng",
                    "preferred_language_status": "selected"
                  }
                }
              }]
            }""".trimIndent(),
        )

        val file = detail.files.single()
        // The anime case the contract is written around: Japanese original
        // audio selected, the English dub still reported as available.
        assertEquals(1L, file.playback_defaults!!.audio.selected_index)
        assertEquals("available", file.playback_defaults.audio.preferred_language_status)
        // SDH and forced are per-stream facts the detail screen marks.
        assertTrue(file.subtitle_streams.first().hearing_impaired)
        assertTrue(file.subtitle_streams.last().forced)
        assertFalse(file.subtitle_streams.last().hearing_impaired)

        // An older server omits the whole block; the file still decodes and
        // the screen lists tracks without claiming which one plays.
        val old = json.decodeFromString<ItemDetail>(
            """{
              "item": {"id": 8, "kind": "movie", "title": "Old server"},
              "files": [{"id": 80, "filename": "a.mkv"}]
            }""".trimIndent(),
        )
        assertNull(old.files.single().playback_defaults)
        assertEquals(emptyList<SubtitleStream>(), old.files.single().subtitle_streams)
    }

    /**
     * The selection-aware `/decision`: the plan carries the audio index the
     * server actually applied, and `selection` prices the subtitle before
     * playback starts. Both are absent for an unselected request, which keeps
     * the response older clients get byte-for-byte.
     */
    @Test
    fun decisionCarriesThePlansAudioAndTheSelectionPreflight() {
        val selected = json.decodeFromString<Decision>(
            """{
              "file_id": 9, "method": "remux", "play_url": "/stream.mp4?audio=3",
              "delivery": {
                "mode": "remux", "url": "/stream.mp4?audio=3",
                "sessions_url": "/hls/sessions", "aac": false,
                "preserve_dolby_vision": false, "audio": 3
              },
              "selection": {
                "audio_index": 3, "subtitle_index": 2,
                "subtitle_requires_burn_in": true,
                "subtitle_burn_in_blocked_by_hdr": true
              }
            }""".trimIndent(),
        )
        assertEquals(3L, selected.delivery!!.audio)
        assertEquals(3L, selected.selection!!.audio_index)
        assertEquals(2L, selected.selection.subtitle_index)
        assertTrue(selected.selection.subtitle_requires_burn_in)
        assertTrue(selected.selection.subtitle_burn_in_blocked_by_hdr)

        val unselected = json.decodeFromString<Decision>(
            """{
              "file_id": 9, "method": "remux", "play_url": "/stream.mp4",
              "delivery": {"mode": "remux", "url": "/stream.mp4", "sessions_url": "/hls/sessions"}
            }""".trimIndent(),
        )
        assertNull(unselected.selection)
        assertNull(unselected.delivery!!.audio)
    }

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
    fun audiobookDetailCarriesPlaybackAndChapterMetadata() {
        val detail = json.decodeFromString<ItemDetail>(
            """{
              "item": {"id": 44, "kind": "audiobook", "title": "The Long Book", "runtime_ms": 300000},
              "files": [{
                "id": 440, "filename": "01.m4b", "duration_ms": 120000,
                "container": "mov,mp4,m4a,3gp,3g2,mj2", "part_offset_ms": 120000,
                "audio_streams": [{"index": 0, "codec": "aac", "channels": 2}],
                "chapters": [{"index": 0, "title": "Opening", "start_ms": 0, "end_ms": 60000}]
              }]
            }""".trimIndent(),
        )

        assertTrue(detail.item.isAudiobook)
        assertTrue(detail.item.isPlayable)
        assertEquals(120_000L, detail.files.single().part_offset_ms)
        assertEquals("aac", detail.files.single().audio_streams.single().codec)
        assertEquals("Opening", detail.files.single().chapters.single().title)
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
        assertTrue(decision.markers.single().chapter)
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
              "media_origin_ms": 0, "vod": true,
              "ladder": [{"height": 1080, "total_kbps": 8192, "peak_kbps": 12416}]
            }""".trimIndent(),
        )
        assertTrue(start.vod)
        assertEquals(0L, start.media_origin_ms)
        assertEquals(1080, start.ladder.single().height)

        val oldServer = json.decodeFromString<HlsStart>(
            """{"session_id":"old","playlist_url":"/hls/old/index.m3u8"}""",
        )
        assertNull(oldServer.media_origin_ms)
    }

    @Test
    fun playbackSessionStatusDecodesTheSharedDiagnosticsContract() {
        val status = json.decodeFromString<PlaybackSessionStatus>(
            """{
              "id": "session-7", "target_height": 1080, "encoder": "videotoolbox",
              "recent_speed": 1.18, "ahead_seconds": 14, "ahead_bytes": 7340032,
              "delivered_bps": 12400000, "suspended": false, "suspend_count": 2,
              "playlist_shape": "sliding", "last_request": "segment-41.ts"
            }""".trimIndent(),
        )

        assertEquals("session-7", status.id)
        assertEquals(1080L, status.target_height)
        assertEquals("videotoolbox", status.encoder)
        assertEquals(1.18, status.recent_speed!!, 0.001)
        assertEquals(12_400_000L, status.delivered_bps)
        assertEquals("sliding", status.playlist_shape)
        assertFalse(status.suspended!!)
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
    fun subtitleTracksCarryBothTextAndTheServersNativeVerdict() {
        // The two flags are different questions and the server sends both:
        // `text` is "has extractable text" (!is_bitmap_subtitle), `native` is
        // "can be an HLS rendition" (is_native_text_subtitle). They disagree
        // on mov_text and on styled ASS/SSA.
        val decision = json.decodeFromString<Decision>(
            """{
              "file_id": 5615, "method": "transcode", "play_url": "/stream.mp4",
              "subtitles": [
                {"index": 0, "codec": "subrip", "text": true, "native": true},
                {"index": 1, "codec": "mov_text", "text": true, "native": false},
                {"index": 2, "codec": "ass", "text": true, "native": false},
                {"index": 3, "codec": "hdmv_pgs_subtitle", "text": false, "native": false}
              ]
            }""".trimIndent(),
        )
        val (srt, movText, ass, pgs) = decision.subtitles

        assertEquals(true, srt.native)
        assertTrue(srt.text)
        // The whole reason the field exists: text and native disagree here.
        assertTrue(movText.text)
        assertEquals(false, movText.native)
        assertTrue(ass.text)
        assertEquals(false, ass.native)
        assertFalse(pgs.text)
        assertEquals(false, pgs.native)

        // And the routing predicate takes the server at its word.
        assertTrue(srt.isNativeHls)
        assertFalse(movText.isNativeHls)
        assertFalse(ass.isNativeHls)
        assertFalse(pgs.isNativeHls)
    }

    @Test
    fun pgsTrackCarriesOnlyTheRecognizedOverlayCapability() {
        val decision = json.decodeFromString<Decision>(
            """{
              "file_id": 47, "method": "direct_play", "play_url": "/direct",
              "subtitles": [
                {"index": 3, "codec": "hdmv_pgs_subtitle", "text": false,
                 "native": false, "overlay": "pgs-v1"},
                {"index": 4, "codec": "hdmv_pgs_subtitle", "text": false,
                 "native": false, "overlay": "pgs-v2"}
              ]
            }""".trimIndent(),
        )
        assertTrue(decision.subtitles[0].isPgsOverlay)
        assertFalse(decision.subtitles[1].isPgsOverlay)
        assertEquals("pgs-v2", decision.subtitles[1].overlay)
    }

    @Test
    fun aServerWithoutNativeStillDecodesAndFallsBackToTheCodec() {
        // The field is new. An older server omits it, the track still decodes,
        // and `native` stays null so the client's codec table decides.
        val decision = json.decodeFromString<Decision>(
            """{
              "file_id": 1, "method": "remux", "play_url": "/stream.mp4",
              "subtitles": [
                {"index": 0, "codec": "subrip", "text": true},
                {"index": 1, "codec": "mov_text", "text": true},
                {"index": 2, "codec": "ass", "text": true},
                {"index": 3, "codec": "hdmv_pgs_subtitle", "text": false}
              ]
            }""".trimIndent(),
        )
        val (srt, movText, ass, pgs) = decision.subtitles

        assertNull(srt.native)
        assertNull(movText.native)
        // With no verdict from the server, `isNativeHls` falls back to "has
        // text, and is not one of the styled formats". A server that predates
        // the field is one that serves an SRT rendition perfectly well, so
        // guessing "no" here would burn every text track on every remux — the
        // bug this milestone exists to remove. `mov_text` is the cost of that
        // choice: an old server may answer the rendition request with a 400,
        // which is a recoverable request, where an unnecessary burn is a
        // 4K re-encode nobody asked for.
        assertTrue(srt.isNativeHls)
        assertTrue(movText.isNativeHls)
        // Styled text and bitmaps still fall out: ASS by the codec list, PGS
        // because it carries no text at all.
        assertFalse(ass.isNativeHls)
        assertFalse(pgs.isNativeHls)

        // An explicit JSON null is the same absence (`explicitNulls = false`
        // on the wire Json, and a nullable field with a null default here).
        assertNull(
            json.decodeFromString<Decision>(
                """{
                  "file_id": 1, "method": "remux", "play_url": "/s.mp4",
                  "subtitles": [{"index": 0, "codec": "subrip", "native": null}]
                }""".trimIndent(),
            ).subtitles.single().native,
        )
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

    @Test
    fun offlineContractCarriesOneExplicitPresentationAndStableLease() {
        val options = json.decodeFromString<OfflineOptions>(
            """{
              "file_id": 91,
              "qualities": [{
                "height": 720, "label": "Standard", "estimated_bytes": 1200000,
                "reserved_bytes": 1500000
              }],
              "audio": [{"index": 2, "codec": "aac", "language": "eng"}],
              "subtitles": [{
                "index": 5, "codec": "webvtt", "language": "eng",
                "offline_mode": "native"
              }],
              "recommended_audio_index": 2, "recommended_subtitle_index": 5
            }""".trimIndent(),
        )
        assertEquals(720, options.qualities.single().height)
        assertEquals(2L, options.recommended_audio_index)
        assertEquals("native", options.subtitles.single().offline_mode)

        val ready = json.decodeFromString<OfflinePackageStatus>(
            """{
              "id":"pkg", "state":"ready", "phase":"complete",
              "status_url":"/api/v1/offline/packages/pkg", "bytes_ready":1200000,
              "estimated_bytes":1200000, "actual_bytes":1190000, "duration_ms":60000,
              "output":{
                "height":720, "video_codec":"h264", "audio_codec":"aac",
                "dynamic_range":"sdr", "subtitle_mode":"native"
              }
            }""".trimIndent(),
        )
        assertEquals("h264", ready.output.video_codec)
        assertEquals(1_190_000L, ready.actual_bytes)

        val progress = json.encodeToString(ProgressReq(12_000, 60_000, 1_785_967_200))
        assertTrue(progress.contains("\"recorded_at\":1785967200"))
    }
}
