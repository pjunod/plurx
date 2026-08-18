package tv.plurx.app.player

import kotlinx.serialization.json.Json
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import tv.plurx.app.data.AudioTrack
import tv.plurx.app.data.Decision
import tv.plurx.app.data.PlaybackQuality

/**
 * The pre-play half of the track contract: what travels on the route, what the
 * first request carries, and what the viewer is told a choice costs — all
 * before a single frame is decoded.
 */
class TrackSelectionTest {
    private val json = Json { ignoreUnknownKeys = true; explicitNulls = false }

    @Test
    fun anUnchosenPlaybackNavigatesToExactlyTheRouteItAlwaysDid() {
        assertEquals("", preplayRouteQuery(PreplayTracks.NONE))
        assertTrue(PreplayTracks.NONE.isEmpty)
        assertNull(PreplayTracks.NONE.subtitleQuery)
    }

    @Test
    fun offIsAChoiceAndAbsenceIsNot() {
        // The distinction the whole value class exists for: `-1` asks the
        // server for no subtitles, while omitting the parameter keeps the
        // shared playback-default policy.
        val off = PreplayTracks(subtitle = SubtitleChoice(null))
        assertEquals(-1L, off.subtitleQuery)
        assertEquals("?subtitle=-1", preplayRouteQuery(off))

        assertNull(PreplayTracks(audio = 3).subtitleQuery)
        assertEquals("?audio=3", preplayRouteQuery(PreplayTracks(audio = 3)))
    }

    @Test
    fun theRouteRoundTripsBothHalvesOfAChoice() {
        val chosen = PreplayTracks(audio = 2, subtitle = SubtitleChoice(5))
        assertEquals("?audio=2&subtitle=5", preplayRouteQuery(chosen))
        assertEquals(chosen, preplayTracksFromRoute(audio = "2", subtitle = "5"))

        assertEquals(
            PreplayTracks(audio = 2, subtitle = SubtitleChoice(null)),
            preplayTracksFromRoute(audio = "2", subtitle = "-1"),
        )
        assertEquals(PreplayTracks.NONE, preplayTracksFromRoute(audio = null, subtitle = null))
    }

    @Test
    fun aBrokenRouteFallsBackToPolicyRatherThanToOff() {
        // Treating garbage as `-1` would silently switch a viewer's subtitles
        // off; treating it as absent hands the choice back to the server.
        assertEquals(
            PreplayTracks.NONE,
            preplayTracksFromRoute(audio = "not-a-number", subtitle = "también no"),
        )
    }

    @Test
    fun anUnchosenDecisionRequestSendsNeitherParameter() {
        // Criterion 7's other half, and the compatibility rule: an unselected
        // request keeps the response byte-for-byte what it was.
        assertEquals(emptyMap<String, String>(), preplayQueryParams(PreplayTracks.NONE))
        assertEquals(mapOf("audio" to "3"), preplayQueryParams(PreplayTracks(audio = 3)))
        assertEquals(
            mapOf("audio" to "3", "subtitle" to "-1"),
            preplayQueryParams(PreplayTracks(audio = 3, subtitle = SubtitleChoice(null))),
        )
        assertEquals(
            mapOf("subtitle" to "5"),
            preplayQueryParams(PreplayTracks(subtitle = SubtitleChoice(5))),
        )
    }

    /**
     * Criterion 5: the plan is executed as given. The server already applied
     * the selection to the remux URL, so the transport must not send `audio`
     * twice — and a later in-player switch must not leave the superseded track
     * in front of the new one.
     */
    @Test
    fun theProgressiveRemuxUrlCarriesTheSelectionExactlyOnce() {
        val planned = "http://tv/api/v1/files/9/stream.mp4?audio=3"

        // The pre-play choice, replayed at a seek: the server's own parameter
        // is normalized, never doubled.
        assertEquals(
            "http://tv/api/v1/files/9/stream.mp4?start=12.0&audio=3&vcodec=h264",
            progressiveRemuxUri(
                plannedUrl = planned,
                startSeconds = 12.0,
                audioIndex = 3,
                audioOffsetMs = 0,
                caps = mapOf("vcodec" to "h264"),
                encode = { it },
            ),
        )

        // And an in-player switch away from it leaves no trace of the old one.
        assertEquals(
            "http://tv/api/v1/files/9/stream.mp4?start=0.0&audio=1&audio_offset_ms=250",
            progressiveRemuxUri(
                plannedUrl = planned,
                startSeconds = 0.0,
                audioIndex = 1,
                audioOffsetMs = 250,
                caps = emptyMap(),
                encode = { it },
            ),
        )

        // With no selection at all the plan URL is untouched, exactly as before
        // this feature existed.
        assertEquals(
            "http://tv/api/v1/files/9/stream.mp4?start=4.5",
            progressiveRemuxUri(
                plannedUrl = "http://tv/api/v1/files/9/stream.mp4",
                startSeconds = 4.5,
                audioIndex = null,
                audioOffsetMs = 0,
                caps = emptyMap(),
                encode = { it },
            ),
        )
    }

    @Test
    fun aSelectionAwarePlanUrlNeverCarriesTwoAudioParameters() {
        val planned = "http://tv/api/v1/files/9/stream.mp4?audio=3"
        assertEquals(
            "http://tv/api/v1/files/9/stream.mp4",
            withoutAudioQueryParam(planned),
        )

        // `audio_offset_ms` is a different parameter with the same prefix and
        // survives untouched, as do the caps.
        assertEquals(
            "http://tv/api/v1/files/9/stream.mp4?audio_offset_ms=250&vcodec=h264",
            withoutAudioQueryParam(
                "http://tv/api/v1/files/9/stream.mp4?audio=3&audio_offset_ms=250&vcodec=h264",
            ),
        )

        // A plan with no selection is left exactly as the server sent it.
        val bare = "http://tv/api/v1/files/9/stream.mp4"
        assertEquals(bare, withoutAudioQueryParam(bare))
        assertEquals("$bare?start=12.0", withoutAudioQueryParam("$bare?start=12.0"))
    }

    /**
     * Criterion 4: the choice is on the *first* session body. Starting on the
     * policy default and switching afterwards is the visible re-buffer this
     * forbids.
     */
    @Test
    fun theFirstSessionCarriesThePreplayAudioAndSubtitle() {
        val body = subtitleSessionBody(
            playbackId = "player",
            requestId = "first",
            startSeconds = 0.0,
            delivery = SubtitleDelivery.NativeSession,
            subtitleIndex = 5,
            copyableVideo = true,
            aac = false,
            preserveDolbyVision = false,
            // `delivery.audio`, repeated into the body exactly as the contract
            // says the HLS transport takes it.
            audioIndex = 2,
            audioOffsetMs = 0,
            quality = PlaybackQuality.Auto,
            sourceHeight = 2160,
        )

        assertEquals(2, body.audio)
        assertEquals(5, body.subtitle)
        assertEquals(true, body.native_subtitles)
        // A text rendition on a copyable plan costs no encoder.
        assertEquals(true, body.copy)
        assertNull(body.subtitle_burn)
    }

    @Test
    fun aBitmapPreplayChoiceBurnsAtSourceHeightOnTheFirstSessionToo() {
        val body = subtitleSessionBody(
            playbackId = "player",
            requestId = "first",
            startSeconds = 0.0,
            delivery = SubtitleDelivery.Burn,
            subtitleIndex = 4,
            copyableVideo = true,
            aac = false,
            preserveDolbyVision = false,
            audioIndex = 1,
            audioOffsetMs = 0,
            quality = PlaybackQuality.Auto,
            sourceHeight = 2160,
        )

        assertEquals(4, body.subtitle_burn)
        assertEquals(1, body.audio)
        assertEquals(2160, body.height)
        assertNull(body.copy)
        assertNull(body.native_subtitles)
    }

    /** Criterion 6, the bitmap half: the server prices it, not a codec table. */
    @Test
    fun aBitmapChoiceIsPricedByTheServersOwnSelectionAnswer() {
        val decision = json.decodeFromString<Decision>(
            """{
              "file_id": 9, "method": "transcode", "play_url": "/s.mp4",
              "subtitles": [{"index": 2, "codec": "hdmv_pgs_subtitle", "text": false, "native": false}],
              "selection": {
                "audio_index": 1, "subtitle_index": 2,
                "subtitle_requires_burn_in": true,
                "subtitle_burn_in_blocked_by_hdr": false
              }
            }""".trimIndent(),
        )

        val cost = preplaySubtitleCost(decision, chosenIndex = 2)
        assertTrue(cost.requiresBurnIn)
        assertFalse(cost.blockedByHdr)
        assertEquals(
            "These subtitles are burned into the picture, which re-encodes the video.",
            preplaySubtitleNotice(cost),
        )
    }

    /**
     * Criterion 6, the honest half: a true `subtitle_burn_in_blocked_by_hdr`
     * means the HDR guard kept the current delivery, so the subtitle will not
     * appear. Reporting that as subtitles-on is the lie this test exists for.
     */
    @Test
    fun anHdrBlockedBurnIsReportedAsNotShownRatherThanAsSubtitlesOn() {
        val decision = json.decodeFromString<Decision>(
            """{
              "file_id": 9, "method": "direct_play", "play_url": "/direct",
              "subtitles": [{"index": 2, "codec": "hdmv_pgs_subtitle", "text": false, "native": false}],
              "selection": {
                "subtitle_index": 2,
                "subtitle_requires_burn_in": true,
                "subtitle_burn_in_blocked_by_hdr": true
              }
            }""".trimIndent(),
        )

        val notice = requireNotNull(preplaySubtitleNotice(preplaySubtitleCost(decision, 2)))
        assertTrue(notice, notice.contains("will not be shown"))
        assertTrue(notice, notice.contains("HDR playback is kept unchanged"))
    }

    /**
     * Text tracks keep the existing `native` flag. `selection` reports only the
     * *bitmap* burn, so ASS/SSA and `mov_text` — which carry text and still
     * burn — would be under-priced by asking the server's bitmap answer alone.
     */
    @Test
    fun aStyledTextChoiceStillDisclosesItsBurnAndAnSrtDisclosesNothing() {
        val decision = json.decodeFromString<Decision>(
            """{
              "file_id": 9, "method": "remux", "play_url": "/s.mp4",
              "subtitles": [
                {"index": 0, "codec": "subrip", "text": true, "native": true},
                {"index": 1, "codec": "ass", "text": true, "native": false},
                {"index": 2, "codec": "hdmv_pgs_subtitle", "text": false, "native": false,
                 "overlay": "pgs-v1"}
              ],
              "selection": {
                "subtitle_index": 1,
                "subtitle_requires_burn_in": false,
                "subtitle_burn_in_blocked_by_hdr": false
              }
            }""".trimIndent(),
        )

        assertTrue(preplaySubtitleCost(decision, chosenIndex = 1).requiresBurnIn)
        // A servable rendition costs nothing and says nothing.
        assertFalse(preplaySubtitleCost(decision, chosenIndex = 0).requiresBurnIn)
        assertNull(preplaySubtitleNotice(preplaySubtitleCost(decision, 0)))
        // Nor does a PGS track the server offers as an application overlay.
        assertFalse(preplaySubtitleCost(decision, chosenIndex = 2).requiresBurnIn)
    }

    @Test
    fun aServerThatSendsNoSelectionPricesNothingRatherThanGuessing() {
        val decision = json.decodeFromString<Decision>(
            """{"file_id": 1, "method": "direct_play", "play_url": "/direct"}""",
        )
        assertNull(decision.selection)
        assertFalse(preplaySubtitleCost(decision, chosenIndex = 0).requiresBurnIn)
        assertNull(preplaySubtitleNotice(preplaySubtitleCost(decision, 0)))
    }

    /**
     * Direct play is the one transport that hands the player every source
     * track, so the choice is applied by ExoPlayer there. Matching is by
     * language and then order within it — raw position fails silently when the
     * player drops a track its renderers refused.
     */
    @Test
    fun aDirectPlayedAudioChoiceMapsOntoThePlayersOwnTrackOrder() {
        val tracks = listOf(
            AudioTrack(index = 0, codec = "eac3", language = "eng"),
            AudioTrack(index = 1, codec = "truehd", language = "eng"),
            AudioTrack(index = 2, codec = "aac", language = "jpn"),
        )
        // Media3 normalizes "eng" to "en", which is why the match is on the
        // ISO 639-2 form rather than on the string.
        val published = listOf("en", "en", "ja")

        assertEquals(0, embeddedAudioTrackIndex(0, tracks, published))
        assertEquals(1, embeddedAudioTrackIndex(1, tracks, published))
        assertEquals(2, embeddedAudioTrackIndex(2, tracks, published))

        // The player refused the TrueHD track: the Japanese one still resolves
        // to what it actually is, rather than sliding onto its neighbour.
        assertEquals(1, embeddedAudioTrackIndex(2, tracks, listOf("en", "ja")))
        // And a track the player never published resolves to nothing, which is
        // the honest answer — ExoPlayer's own pick then stands.
        assertNull(embeddedAudioTrackIndex(2, tracks, listOf("en", "en")))
        assertNull(embeddedAudioTrackIndex(9, tracks, published))
    }
}
