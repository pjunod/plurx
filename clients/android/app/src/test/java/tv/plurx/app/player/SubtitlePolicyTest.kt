package tv.plurx.app.player

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import tv.plurx.app.data.PlaybackQuality
import tv.plurx.app.data.ReopenReason
import tv.plurx.app.data.SubTrack

/**
 * The subtitle contract, exercised where it is decidable: which arm a
 * selection lands in, the body that arm posts, the rendition ordinal, and the
 * one veto over the server's automatic pick.
 */
class SubtitlePolicyTest {
    private fun srt(index: Long, language: String? = "eng", default: Boolean = false, forced: Boolean = false, title: String? = null) =
        SubTrack(index = index, codec = "subrip", language = language, title = title, default = default, forced = forced, text = true, native = true)

    private fun pgs(index: Long, language: String? = "eng", default: Boolean = false, forced: Boolean = false, title: String? = null) =
        SubTrack(index = index, codec = "hdmv_pgs_subtitle", language = language, title = title, default = default, forced = forced, text = false, native = false)

    private fun pgsOverlay(index: Long, default: Boolean = false) =
        SubTrack(
            index = index,
            codec = "hdmv_pgs_subtitle",
            default = default,
            text = false,
            native = false,
            overlay = "pgs-v1",
        )

    private fun ass(index: Long, language: String? = "jpn", default: Boolean = false) =
        SubTrack(index = index, codec = "ass", language = language, default = default, text = true, native = false)

    // ---- routing -----------------------------------------------------------

    @Test
    fun offReturnsToThePlanAndOnlyLeavingABurnReopens() {
        assertEquals(
            SubtitleRoute(SubtitleDelivery.Plan, reopen = false),
            subtitleRoute(null, "direct", SubtitleDelivery.Plan),
        )
        assertEquals(
            SubtitleRoute(SubtitleDelivery.Plan, reopen = false),
            subtitleRoute(null, "remux", SubtitleDelivery.Plan),
        )
        assertEquals(
            SubtitleRoute(SubtitleDelivery.Plan, reopen = true),
            subtitleRoute(null, "remux", SubtitleDelivery.NativeSession),
        )
        assertEquals(
            SubtitleRoute(SubtitleDelivery.Plan, reopen = true),
            subtitleRoute(null, "transcode", SubtitleDelivery.Burn),
        )
    }

    @Test
    fun nativeTextOnDirectPlayStaysOnThePlansOwnStream() {
        assertEquals(
            SubtitleRoute(SubtitleDelivery.Plan, reopen = false),
            subtitleRoute(srt(1), "direct", SubtitleDelivery.Plan),
        )
    }

    @Test
    fun nativeTextOnRemuxAndTranscodeOpensANativeSession() {
        assertEquals(
            SubtitleRoute(SubtitleDelivery.NativeSession, reopen = true),
            subtitleRoute(srt(1), "remux", SubtitleDelivery.Plan),
        )
        assertEquals(
            SubtitleRoute(SubtitleDelivery.NativeSession, reopen = true),
            subtitleRoute(srt(1), "transcode", SubtitleDelivery.Plan),
        )
    }

    @Test
    fun switchingBetweenTwoTextTracksDoesNotCreateANewSession() {
        // The acceptance criterion of §5.4, in both places it has to hold:
        // two renditions of one live session, and two embedded tracks of one
        // directly-played file.
        assertFalse(subtitleRoute(srt(3), "remux", SubtitleDelivery.NativeSession).reopen)
        assertFalse(subtitleRoute(srt(3), "transcode", SubtitleDelivery.NativeSession).reopen)
        assertFalse(subtitleRoute(srt(3), "direct", SubtitleDelivery.Plan).reopen)
    }

    @Test
    fun bitmapAndStyledTracksBurnInEveryMode() {
        for (mode in listOf("direct", "remux", "transcode")) {
            assertEquals(
                "burn expected for a bitmap track on $mode",
                SubtitleDelivery.Burn,
                subtitleRoute(pgs(2), mode, SubtitleDelivery.Plan).delivery,
            )
            // ASS carries text and still cannot be a rendition: routing on
            // `text` here is what earns a 400 and then a fallback burn.
            assertEquals(
                "burn expected for a styled track on $mode",
                SubtitleDelivery.Burn,
                subtitleRoute(ass(4), mode, SubtitleDelivery.Plan).delivery,
            )
        }
    }

    @Test
    fun recognizedPgsOverlayNeverBurnsOrReopensPlanVideo() {
        for (mode in listOf("direct", "remux", "transcode")) {
            assertEquals(
                SubtitleRoute(SubtitleDelivery.BitmapOverlay, reopen = false),
                subtitleRoute(pgsOverlay(2), mode, SubtitleDelivery.Plan),
            )
            assertFalse(subtitleBurnWouldDiscardHdr(pgsOverlay(2), "dolby_vision"))
        }
        assertEquals(
            SubtitleRoute(SubtitleDelivery.Plan, reopen = false),
            subtitleRoute(null, "direct", SubtitleDelivery.BitmapOverlay),
        )
        assertEquals(
            SubtitleRoute(SubtitleDelivery.BitmapOverlay, reopen = false),
            subtitleRoute(pgsOverlay(3), "direct", SubtitleDelivery.BitmapOverlay),
        )
        assertEquals(
            SubtitleRoute(SubtitleDelivery.BitmapOverlay, reopen = true),
            subtitleRoute(pgsOverlay(3), "remux", SubtitleDelivery.NativeSession),
        )
        assertEquals(
            SubtitleRoute(SubtitleDelivery.NativeSession, reopen = true),
            subtitleRoute(srt(4), "remux", SubtitleDelivery.BitmapOverlay),
        )
        assertEquals(
            SubtitleRoute(SubtitleDelivery.Burn, reopen = true),
            subtitleRoute(pgs(5), "direct", SubtitleDelivery.BitmapOverlay),
        )
    }

    @Test
    fun unknownOverlayVersionRetainsLegacyBurnAndHdrGuard() {
        val future = pgs(2).copy(overlay = "pgs-v2")
        assertEquals(
            SubtitleDelivery.Burn,
            subtitleRoute(future, "direct", SubtitleDelivery.Plan).delivery,
        )
        assertTrue(subtitleBurnWouldDiscardHdr(future, "hdr10"))
    }

    @Test
    fun changingTheBurntTrackIsAlwaysANewSession() {
        assertEquals(
            SubtitleRoute(SubtitleDelivery.Burn, reopen = true),
            subtitleRoute(pgs(5), "remux", SubtitleDelivery.Burn),
        )
    }

    // ---- ordinal mapping ---------------------------------------------------

    @Test
    fun renditionOrdinalCountsNativeTracksOnlyWithBitmapAndStyledInterleaved() {
        val tracks = listOf(
            pgs(0, language = "eng"),   // not advertised
            srt(1, language = "eng"),   // rendition 0
            ass(2, language = "jpn"),   // not advertised
            srt(3, language = "ita"),   // rendition 1
            pgs(4, language = "fra"),   // not advertised
            srt(5, language = "spa"),   // rendition 2
        )
        assertNull(nativeSubtitleOrdinal(0, tracks))
        assertEquals(0, nativeSubtitleOrdinal(1, tracks))
        assertNull(nativeSubtitleOrdinal(2, tracks))
        assertEquals(1, nativeSubtitleOrdinal(3, tracks))
        assertNull(nativeSubtitleOrdinal(4, tracks))
        assertEquals(2, nativeSubtitleOrdinal(5, tracks))
        assertNull(nativeSubtitleOrdinal(9, tracks))
    }

    @Test
    fun anOlderServerWithoutTheNativeFieldStillOrdersRenditionsTheSameWay() {
        // `native` absent: bitmap is excluded by `text`, ASS by the codec list.
        val tracks = listOf(
            SubTrack(index = 0, codec = "hdmv_pgs_subtitle", text = false, native = null),
            SubTrack(index = 1, codec = "subrip", text = true, native = null),
            SubTrack(index = 2, codec = "ass", text = true, native = null),
            SubTrack(index = 3, codec = "webvtt", text = true, native = null),
        )
        assertEquals(0, nativeSubtitleOrdinal(1, tracks))
        assertEquals(1, nativeSubtitleOrdinal(3, tracks))
        assertNull(nativeSubtitleOrdinal(2, tracks))
        assertEquals(SubtitleDelivery.Burn, subtitleRoute(tracks[2], "remux", SubtitleDelivery.Plan).delivery)
    }

    // ---- automatic selection and its one veto ------------------------------

    @Test
    fun aServerDefaultedNonForcedBitmapTrackDoesNotAutoBurn() {
        assertNull(autoSubtitleSelection(listOf(srt(0), pgs(1, default = true))))
        assertNull(autoSubtitleSelection(listOf(ass(2, default = true))))
        assertEquals(3L, autoSubtitleSelection(listOf(pgsOverlay(3, default = true))))
    }

    @Test
    fun aServerDefaultedForcedBitmapTrackDoesAutoBurn() {
        assertEquals(1L, autoSubtitleSelection(listOf(srt(0), pgs(1, default = true, forced = true))))
        // Disposition says nothing; the title does. The server reads both, so
        // this client has to as well or the two disagree about the carve-out.
        assertEquals(
            1L,
            autoSubtitleSelection(listOf(srt(0), pgs(1, default = true, title = "Italian Forced"))),
        )
    }

    @Test
    fun aForcedAutoBurnRunsAtSourceHeight() {
        val body = subtitleSessionBody(
            playbackId = "p", requestId = "r", startSeconds = 0.0,
            delivery = SubtitleDelivery.Burn, subtitleIndex = 1,
            copyableVideo = true, aac = false, preserveDolbyVision = true,
            audioIndex = 0, audioOffsetMs = 0,
            quality = PlaybackQuality.Auto, sourceHeight = 2160,
        )
        assertEquals(2160, body.height)
        assertEquals(1, body.subtitle_burn)
        assertNull("a burn is never a copy", body.copy)
        assertNull(body.native_subtitles)
    }

    @Test
    fun hdrSubtitleGuardKeepsTheCurrentPictureForBurnOnlyTracks() {
        for (range in listOf("dolby_vision", "hdr10", "hlg")) {
            assertTrue(subtitleBurnWouldDiscardHdr(pgs(0), range))
            assertTrue(subtitleBurnWouldDiscardHdr(ass(1), range))
            assertFalse(subtitleBurnWouldDiscardHdr(srt(2), range))
        }
        assertFalse(subtitleBurnWouldDiscardHdr(pgs(0), "sdr"))
        assertFalse(subtitleBurnWouldDiscardHdr(pgs(0), null))
        assertFalse(subtitleBurnWouldDiscardHdr(null, "hdr10"))
    }

    @Test
    fun theServersOwnPickIsAppliedWithoutRederivingPolicy() {
        // Two English tracks, the second flagged: policy is the server's, so
        // "first English" must not win over "the one it stamped".
        val tracks = listOf(srt(0), srt(1, default = true))
        assertEquals(1L, autoSubtitleSelection(tracks))
        // Nothing stamped means nothing shown, even with a perfectly eligible
        // track sitting there — that is the server saying no.
        assertNull(autoSubtitleSelection(listOf(srt(0), srt(1))))
        assertNull(autoSubtitleSelection(emptyList()))
    }

    @Test
    fun forcedDetectionMatchesTheServersWordBoundaryRule() {
        assertTrue(titleMarksForced("Forced"))
        assertTrue(titleMarksForced("English (Forced)"))
        assertTrue(titleMarksForced("SDH · forced"))
        assertFalse(titleMarksForced("Non-Forced"))
        assertFalse(titleMarksForced("not forced"))
        assertFalse(titleMarksForced("Unforced"))
        assertFalse(titleMarksForced("Reinforced"))
        assertFalse(titleMarksForced("English"))
        assertTrue(titleMarksForced("unforced but also forced"))
    }

    // ---- request bodies ----------------------------------------------------

    @Test
    fun aNativeSessionOnARemuxVerdictCopiesTheVideo() {
        val body = subtitleSessionBody(
            playbackId = "pb", requestId = "rq", startSeconds = 12.5,
            delivery = SubtitleDelivery.NativeSession, subtitleIndex = 3,
            copyableVideo = true, aac = true, preserveDolbyVision = true,
            audioIndex = 1, audioOffsetMs = -250,
            quality = PlaybackQuality.Q720, sourceHeight = 2160,
        )
        assertEquals(true, body.native_subtitles)
        assertEquals(3, body.subtitle)
        assertNull("subtitle_burn must stay absent on the free path", body.subtitle_burn)
        assertEquals(true, body.copy)
        assertEquals(true, body.aac)
        assertEquals(true, body.preserve_dolby_vision)
        // A copy has no rung to honour; naming one would be a promise the
        // session is not going to keep.
        assertNull(body.height)
        assertEquals(12.5, body.start!!, 0.0)
        assertEquals(1, body.audio)
        assertEquals(-250L, body.audio_offset_ms)
    }

    @Test
    fun aNativeSessionOnATranscodeVerdictKeepsTodaysVideoRecipe() {
        val body = subtitleSessionBody(
            playbackId = "pb", requestId = "rq", startSeconds = 0.0,
            delivery = SubtitleDelivery.NativeSession, subtitleIndex = 2,
            copyableVideo = false, aac = true, preserveDolbyVision = true,
            audioIndex = null, audioOffsetMs = 0,
            quality = PlaybackQuality.Q1080, sourceHeight = 2160,
        )
        assertEquals(true, body.native_subtitles)
        assertEquals(2, body.subtitle)
        assertNull(body.copy)
        assertNull(body.aac)
        assertNull(body.preserve_dolby_vision)
        assertEquals(1080, body.height)
        assertNull(body.audio)
        assertNull(body.audio_offset_ms)
    }

    @Test
    fun aPlanSessionCarriesNoSubtitleFieldsAtAll() {
        val body = subtitleSessionBody(
            playbackId = "pb", requestId = "rq", startSeconds = 4.0,
            delivery = SubtitleDelivery.Plan, subtitleIndex = null,
            copyableVideo = false, aac = false, preserveDolbyVision = false,
            audioIndex = 0, audioOffsetMs = 0,
            quality = PlaybackQuality.Auto, sourceHeight = 1080,
        )
        assertNull(body.native_subtitles)
        assertNull(body.subtitle)
        assertNull(body.subtitle_burn)
        assertNull("Auto is the server's rung to choose", body.height)
    }

    @Test
    fun overlayCarriesNoSubtitleSessionFields() {
        val body = subtitleSessionBody(
            playbackId = "pb", requestId = "rq", startSeconds = 4.0,
            delivery = SubtitleDelivery.BitmapOverlay, subtitleIndex = 3,
            copyableVideo = true, aac = false, preserveDolbyVision = true,
            audioIndex = 0, audioOffsetMs = 0,
            quality = PlaybackQuality.Auto, sourceHeight = 2160,
        )
        assertNull(body.native_subtitles)
        assertNull(body.subtitle)
        assertNull(body.subtitle_burn)
    }

    // ---- unifying the server row and the embedded row ----------------------

    @Test
    fun embeddedTracksMapOneForOneWhenTheDemuxerSurfacedThemAll() {
        val tracks = listOf(pgs(0, "eng"), srt(1, "eng"), srt(2, "ita"))
        val embedded = listOf("en", "en", "it")
        assertEquals(0, embeddedTextTrackIndex(0, tracks, embedded))
        assertEquals(1, embeddedTextTrackIndex(1, tracks, embedded))
        assertEquals(2, embeddedTextTrackIndex(2, tracks, embedded))
    }

    @Test
    fun embeddedTracksSurviveARefusedTrackShiftingEveryPosition() {
        // The renderer refused the French PGS track, so raw position is off by
        // one from there on — and off-by-one here means the Italian row plays
        // the first Italian track's neighbour.
        val tracks = listOf(pgs(0, "fra"), srt(1, "eng"), srt(2, "ita"), srt(3, "ita"))
        val embedded = listOf("en", "it", "it")
        assertEquals(0, embeddedTextTrackIndex(1, tracks, embedded))
        assertEquals(1, embeddedTextTrackIndex(2, tracks, embedded))
        assertEquals(2, embeddedTextTrackIndex(3, tracks, embedded))
        assertNull("the refused track resolves to nothing", embeddedTextTrackIndex(0, tracks, embedded))
    }

    @Test
    fun embeddedLookupMatchesUntaggedTracksAmongUntaggedTracks() {
        val tracks = listOf(srt(0, language = null), srt(1, "eng"), srt(2, language = null))
        val embedded = listOf(null, "en", null)
        assertEquals(0, embeddedTextTrackIndex(0, tracks, embedded))
        assertEquals(1, embeddedTextTrackIndex(1, tracks, embedded))
        assertEquals(2, embeddedTextTrackIndex(2, tracks, embedded))
    }

    @Test
    fun embeddedLookupRefusesTracksThatArentThere() {
        // An index the decision never listed.
        assertNull(embeddedTextTrackIndex(7, listOf(srt(1)), listOf("en")))
        // Two English tracks in the decision, one in the player: the second
        // has nowhere to land, and landing it on the first would show the
        // wrong cues under a row that claims to be the other track.
        assertNull(embeddedTextTrackIndex(1, listOf(srt(0), srt(1)), listOf("en")))
    }

    // ---- stall reopen fields and quality_auto -----------------------------------

    @Test
    fun stallReopenCarriesPreviousSessionIdAndReopenReason() {
        val body = subtitleSessionBody(
            playbackId = "pb", requestId = "rq", startSeconds = 4.0,
            delivery = SubtitleDelivery.Plan, subtitleIndex = null,
            copyableVideo = false, aac = false, preserveDolbyVision = false,
            audioIndex = 0, audioOffsetMs = 0,
            quality = PlaybackQuality.Auto, sourceHeight = 2160,
            previousSessionId = "prev-session-42",
            reopenReason = ReopenReason.Stall,
        )
        assertEquals("prev-session-42", body.previous_session_id)
        assertEquals(ReopenReason.Stall, body.reopen_reason)
    }

    @Test
    fun ordinarySeekOmitsStallFields() {
        val body = subtitleSessionBody(
            playbackId = "pb", requestId = "rq", startSeconds = 4.0,
            delivery = SubtitleDelivery.Plan, subtitleIndex = null,
            copyableVideo = false, aac = false, preserveDolbyVision = false,
            audioIndex = 0, audioOffsetMs = 0,
            quality = PlaybackQuality.Auto, sourceHeight = 2160,
        )
        assertNull("no previous_session_id on a seek", body.previous_session_id)
        assertNull("no reopen_reason on a seek", body.reopen_reason)
    }

    @Test
    fun autoBurnSendsQualityAutoTrue() {
        val body = subtitleSessionBody(
            playbackId = "pb", requestId = "rq", startSeconds = 4.0,
            delivery = SubtitleDelivery.Burn, subtitleIndex = 1,
            copyableVideo = false, aac = false, preserveDolbyVision = false,
            audioIndex = 0, audioOffsetMs = 0,
            quality = PlaybackQuality.Auto, sourceHeight = 2160,
        )
        assertEquals(true, body.quality_auto)
    }

    @Test
    fun explicitQualityBurnOmitQualityAuto() {
        val body = subtitleSessionBody(
            playbackId = "pb", requestId = "rq", startSeconds = 4.0,
            delivery = SubtitleDelivery.Burn, subtitleIndex = 1,
            copyableVideo = false, aac = false, preserveDolbyVision = false,
            audioIndex = 0, audioOffsetMs = 0,
            quality = PlaybackQuality.Q720, sourceHeight = 2160,
        )
        assertNull("manual quality burn does not need quality_auto", body.quality_auto)
    }

    @Test
    fun autoPlanSessionWithoutBurnOmitQualityAuto() {
        val body = subtitleSessionBody(
            playbackId = "pb", requestId = "rq", startSeconds = 4.0,
            delivery = SubtitleDelivery.Plan, subtitleIndex = null,
            copyableVideo = false, aac = false, preserveDolbyVision = false,
            audioIndex = 0, audioOffsetMs = 0,
            quality = PlaybackQuality.Auto, sourceHeight = 2160,
        )
        assertNull("no height sent means no quality_auto needed", body.quality_auto)
    }

    @Test
    fun qualityAutoIsFalseForAutoPlanWithExplicitRung() {
        val body = subtitleSessionBody(
            playbackId = "pb", requestId = "rq", startSeconds = 4.0,
            delivery = SubtitleDelivery.NativeSession, subtitleIndex = 2,
            copyableVideo = false, aac = false, preserveDolbyVision = false,
            audioIndex = 0, audioOffsetMs = 0,
            quality = PlaybackQuality.Q1080, sourceHeight = 2160,
        )
        assertNull("explicit 1080p rung is not a promise-height", body.quality_auto)
    }

    @Test
    fun stallReopenWithBurnSendsQualityAutoAndStallFields() {
        val body = subtitleSessionBody(
            playbackId = "pb", requestId = "rq-stall", startSeconds = 4.0,
            delivery = SubtitleDelivery.Burn, subtitleIndex = 1,
            copyableVideo = false, aac = false, preserveDolbyVision = false,
            audioIndex = 0, audioOffsetMs = 0,
            quality = PlaybackQuality.Auto, sourceHeight = 2160,
            previousSessionId = "prev-session-99",
            reopenReason = ReopenReason.Stall,
        )
        assertEquals("prev-session-99", body.previous_session_id)
        assertEquals(ReopenReason.Stall, body.reopen_reason)
        assertEquals(true, body.quality_auto)
    }

    // ---- qualityAuto helper tests -------------------------------------------

    @Test
    fun qualityAutoIsTrueForAutoBurn() {
        assertEquals(true, qualityAuto(PlaybackQuality.Auto, SubtitleDelivery.Burn))
    }

    @Test
    fun qualityAutoIsFalseForManualBurn() {
        assertEquals(false, qualityAuto(PlaybackQuality.Q720, SubtitleDelivery.Burn))
    }

    @Test
    fun qualityAutoIsFalseForAutoPlan() {
        assertEquals(false, qualityAuto(PlaybackQuality.Auto, SubtitleDelivery.Plan))
    }

    @Test
    fun qualityAutoIsFalseForAutoNativeSession() {
        assertEquals(false, qualityAuto(PlaybackQuality.Auto, SubtitleDelivery.NativeSession))
    }

    @Test
    fun qualityAutoIsFalseForOriginalPlan() {
        assertEquals(false, qualityAuto(PlaybackQuality.Original, SubtitleDelivery.Plan))
    }

    @Test
    fun qualityAutoIsFalseForOriginalBurn() {
        // Original is not Auto; it is its own special mode that does not need
        // the flag because the server already treats Original specially.
        assertEquals(false, qualityAuto(PlaybackQuality.Original, SubtitleDelivery.Burn))
    }

    @Test
    fun heightIsAPromiseForBurnsAndOriginalAndARungOtherwise() {
        assertEquals(2160, sessionHeight(PlaybackQuality.Auto, SubtitleDelivery.Burn, 2160))
        assertEquals(2160, sessionHeight(PlaybackQuality.Q720, SubtitleDelivery.Burn, 2160))
        assertEquals(2160, sessionHeight(PlaybackQuality.Original, SubtitleDelivery.Plan, 2160))
        assertNull(sessionHeight(PlaybackQuality.Auto, SubtitleDelivery.Plan, 2160))
        assertEquals(720, sessionHeight(PlaybackQuality.Q720, SubtitleDelivery.NativeSession, 2160))
        // An unknown source height leaves the promise unmade rather than
        // inventing a rung the server would then snap.
        assertNull(sessionHeight(PlaybackQuality.Original, SubtitleDelivery.Burn, null))
    }

    @Test
    fun languageNormalizationBridgesTheTwoSpellings() {
        assertEquals(normalizedLanguage("eng"), normalizedLanguage("en"))
        assertEquals(normalizedLanguage("jpn"), normalizedLanguage("ja"))
        assertEquals(normalizedLanguage("pt-BR"), normalizedLanguage("por"))
        assertNull(normalizedLanguage("und"))
        assertNull(normalizedLanguage(null))
        assertNull(normalizedLanguage("  "))
    }
}
