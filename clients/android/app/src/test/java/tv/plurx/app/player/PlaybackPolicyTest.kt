package tv.plurx.app.player

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import tv.plurx.app.data.HdrType
import tv.plurx.app.data.PlaybackQuality
import tv.plurx.app.data.Rung
import tv.plurx.app.data.SubTrack

private fun sub(
    index: Long,
    codec: String = "subrip",
    language: String? = "eng",
    title: String? = null,
    default: Boolean = false,
    forced: Boolean = false,
    text: Boolean = true,
    native: Boolean? = null,
) = SubTrack(index, codec, language, title, default, forced, text, native)

class PlaybackPolicyTest {

    // ---- §3.1 the four rows of the subtitle table --------------------------

    @Test
    fun forcedTracksAutoApplyWhateverTheirCodec() {
        val flagged = listOf(sub(0), sub(1, codec = "hdmv_pgs_subtitle", forced = true, text = false))
        assertEquals(1L, automaticSubtitleIndex(flagged, "eng"))

        // A title sniff is the same signal: plenty of muxes never set the flag.
        val titled = listOf(sub(0), sub(2, title = "English (Forced)"))
        assertEquals(2L, automaticSubtitleIndex(titled, "eng"))
    }

    @Test
    fun defaultFlaggedNativeTextAutoApplies() {
        val tracks = listOf(sub(0, language = "ita"), sub(1, default = true))
        assertEquals(1L, automaticSubtitleIndex(tracks, "eng"))
    }

    @Test
    fun defaultFlaggedBitmapNeverAutoBurns() {
        // The standard 4K disc remux: cold start must stay a copy.
        val tracks = listOf(sub(0, codec = "hdmv_pgs_subtitle", default = true, text = false))
        assertNull(automaticSubtitleIndex(tracks, "eng"))
    }

    @Test
    fun merelyMatchingLanguageIsNeverAutoApplied() {
        val tracks = listOf(sub(0), sub(1, title = "SDH"))
        assertNull(automaticSubtitleIndex(tracks, "eng"))
    }

    @Test
    fun aFlaggedDefaultInAnotherLanguageIsNotAFallback() {
        val tracks = listOf(sub(0, language = "ita", default = true), sub(1, language = "ita", forced = true))
        assertNull(automaticSubtitleIndex(tracks, "eng"))
    }

    @Test
    fun subtitlesOffMeansNoAutomaticSelection() {
        assertNull(automaticSubtitleIndex(listOf(sub(0, forced = true)), "off"))
        assertNull(automaticSubtitleIndex(listOf(sub(0, forced = true)), ""))
    }

    @Test
    fun twoAndThreeLetterLanguageCodesAreTheSameLanguage() {
        assertEquals(0L, automaticSubtitleIndex(listOf(sub(0, language = "en", forced = true)), "eng"))
        assertEquals(0L, automaticSubtitleIndex(listOf(sub(0, language = "eng", forced = true)), "en"))
    }

    // ---- §5.4 where a selection goes ---------------------------------------

    @Test
    fun textGoesNativeAndOnlyBitmapBurns() {
        val text = sub(3)
        val bitmap = sub(4, codec = "hdmv_pgs_subtitle", text = false)

        assertEquals(SubtitleRoute.Off, subtitleRoute(null, "remux"))
        assertEquals(SubtitleRoute.EmbeddedText, subtitleRoute(text, "direct"))
        assertEquals(SubtitleRoute.NativeRendition, subtitleRoute(text, "remux"))
        assertEquals(SubtitleRoute.NativeRendition, subtitleRoute(text, "transcode"))
        // A burn is a burn in every mode, including direct play.
        assertEquals(SubtitleRoute.Burn, subtitleRoute(bitmap, "direct"))
        assertEquals(SubtitleRoute.Burn, subtitleRoute(bitmap, "remux"))
    }

    @Test
    fun styledTextHasNoRenditionAndBurnsWithTheBitmaps() {
        // The trap: `/decision` sets text=true for ASS/SSA (it is not a
        // bitmap), but the session endpoint rejects it as a native rendition
        // and the master never advertises it. Offering it would be a 400.
        val ass = sub(1, codec = "ass")
        val ssa = sub(2, codec = "ssa")

        assertEquals(SubtitleRoute.Burn, subtitleRoute(ass, "remux"))
        assertEquals(SubtitleRoute.Burn, subtitleRoute(ssa, "transcode"))
        // Direct play renders the container's own styled text for free.
        assertEquals(SubtitleRoute.EmbeddedText, subtitleRoute(ass, "direct"))
        // And it is never the automatic pick on a default flag.
        assertNull(automaticSubtitleIndex(listOf(sub(0, codec = "ass", default = true)), "eng"))
    }

    @Test
    fun movTextHasNoRenditionEither() {
        // The MP4 trap, and the one that actually bit: a 2160p WEB-DL with 23
        // `mov_text` tracks. `/decision` calls every one of them text=true (it
        // is not a bitmap), the master advertises none of them, and an
        // explicit rendition pick is a 400 "requires burn-in".
        val movText = sub(1, codec = "mov_text")

        assertEquals(SubtitleRoute.Burn, subtitleRoute(movText, "remux"))
        assertEquals(SubtitleRoute.Burn, subtitleRoute(movText, "transcode"))
        // Direct play reads the container's own track — free, and the sidecar
        // endpoint would extract it too, since it turns away only bitmaps.
        assertEquals(SubtitleRoute.EmbeddedText, subtitleRoute(movText, "direct"))
        // And a default flag on one is not an invitation to burn.
        assertNull(automaticSubtitleIndex(listOf(sub(0, codec = "mov_text", default = true)), "eng"))
    }

    @Test
    fun theThreeSubtitleClassesRouteToThreeDifferentPlaces() {
        // One table, one assertion per class, on a session plan.
        assertEquals(SubtitleRoute.NativeRendition, subtitleRoute(sub(0, codec = "subrip"), "transcode"))
        assertEquals(SubtitleRoute.Burn, subtitleRoute(sub(1, codec = "mov_text"), "transcode"))
        assertEquals(SubtitleRoute.Burn, subtitleRoute(sub(2, codec = "ass"), "transcode"))
        assertEquals(
            SubtitleRoute.Burn,
            subtitleRoute(sub(3, codec = "hdmv_pgs_subtitle", text = false), "transcode"),
        )
    }

    @Test
    fun renditionOrdinalCountsOnlyTheNativeTextTracks() {
        // Absolute stream indexes with bitmaps and non-native text
        // interleaved: the HLS master carries only the native ones, in source
        // order. Anything else counted here shifts every rendition after it,
        // and the viewer silently gets the neighbouring language.
        val tracks = listOf(
            sub(0, codec = "hdmv_pgs_subtitle", text = false),
            sub(1),
            sub(2, codec = "dvd_subtitle", text = false),
            // Styled text is not in the master either, so it must not shift
            // the ordinals of the tracks that are.
            sub(3, codec = "ass"),
            // Neither is mov_text, for all that it is text.
            sub(4, codec = "mov_text"),
            sub(5),
        )
        assertEquals(0, nativeSubtitleOrdinal(1, tracks))
        assertEquals(1, nativeSubtitleOrdinal(5, tracks))
        assertNull(nativeSubtitleOrdinal(0, tracks))
        assertNull(nativeSubtitleOrdinal(3, tracks))
        assertNull(nativeSubtitleOrdinal(4, tracks))
        assertNull(nativeSubtitleOrdinal(9, tracks))
    }

    // ---- §2.3 the server owns the notion; the codec table is the fallback ---

    @Test
    fun theServersNativeFlagOutranksTheClientsCodecTable() {
        // The point of decoding `native`: the server can change its mind about
        // a codec without a client release. A codec this client would call
        // native, that the server says is not — and the reverse.
        val serverSaysNo = sub(1, codec = "subrip", native = false)
        assertEquals(SubtitleRoute.Burn, subtitleRoute(serverSaysNo, "transcode"))
        assertNull(automaticSubtitleIndex(listOf(sub(0, native = false, default = true)), "eng"))

        val serverSaysYes = sub(2, codec = "some_future_text_codec", native = true)
        assertEquals(SubtitleRoute.NativeRendition, subtitleRoute(serverSaysYes, "transcode"))
        assertEquals(
            0L,
            automaticSubtitleIndex(
                listOf(sub(0, codec = "some_future_text_codec", native = true, default = true)),
                "eng",
            ),
        )

        // And the ordinals follow the same answer, not the codec.
        val tracks = listOf(serverSaysNo, serverSaysYes, sub(3))
        assertEquals(0, nativeSubtitleOrdinal(2, tracks))
        assertEquals(1, nativeSubtitleOrdinal(3, tracks))
        assertNull(nativeSubtitleOrdinal(1, tracks))
    }

    @Test
    fun anOlderServerWithoutTheNativeFlagFallsBackToTheCodecTable() {
        // Every `sub()` above omits `native`; this pins that the omission is
        // the codec table's job and not a crash or a blanket "no".
        assertEquals(SubtitleRoute.NativeRendition, subtitleRoute(sub(0, native = null), "remux"))
        assertEquals(SubtitleRoute.Burn, subtitleRoute(sub(1, codec = "mov_text", native = null), "remux"))
    }

    // ---- §5.3 / §3.2 force and heights -------------------------------------

    @Test
    fun forceSaysWhatTheMenuMeans() {
        assertEquals("auto", decisionForce(PlaybackQuality.Auto))
        assertEquals("original", decisionForce(PlaybackQuality.Original))
        // Every rung is a request to transcode; how tall is the create's job.
        assertEquals("transcode", decisionForce(PlaybackQuality.Q2160))
        assertEquals("transcode", decisionForce(PlaybackQuality.Q1080))
        assertEquals("transcode", decisionForce(PlaybackQuality.Q720))
        assertEquals("transcode", decisionForce(PlaybackQuality.Q480))
        assertEquals("transcode", decisionForce(PlaybackQuality.Q360))
    }

    @Test
    fun heightsKeepTheirPromises() {
        val source = 2160

        // True Auto: the server's rung, not the client's guess.
        assertNull(sessionHeight(PlaybackQuality.Auto, burningSubtitle = false, sourceHeight = source))
        // Original is a promise, and so is a burn — both carry source height.
        assertEquals(source, sessionHeight(PlaybackQuality.Original, false, source))
        assertEquals(source, sessionHeight(PlaybackQuality.Auto, true, source))
        assertEquals(source, sessionHeight(PlaybackQuality.Original, true, source))
        // An explicit rung is a rung, burn or not.
        assertEquals(720, sessionHeight(PlaybackQuality.Q720, false, source))
        assertEquals(720, sessionHeight(PlaybackQuality.Q720, true, source))
        // An unprobed source has no promise to make: Auto.
        assertNull(sessionHeight(PlaybackQuality.Original, true, null))
    }

    // ---- §4.2 the one-shot rescue ------------------------------------------

    @Test
    fun oneCompatibilityRescuePerItemAndOnlyFromACheaperMode() {
        assertEquals(
            PlaybackErrorAction.RetryAsCompatibilityTranscode,
            playbackErrorAction("direct", rescueAlreadyUsed = false),
        )
        assertEquals(
            PlaybackErrorAction.RetryAsCompatibilityTranscode,
            playbackErrorAction("remux", rescueAlreadyUsed = false),
        )
        // Second failure: no rescue left.
        assertEquals(PlaybackErrorAction.Fail, playbackErrorAction("direct", true))
        assertEquals(PlaybackErrorAction.Fail, playbackErrorAction("remux", true))
        // A transcode that fails has nothing cheaper to fall back to; retrying
        // it would loop.
        assertEquals(PlaybackErrorAction.Fail, playbackErrorAction("transcode", false))
        assertEquals(PlaybackErrorAction.Fail, playbackErrorAction("transcode", true))
    }

    // ---- §5.6 the menu is the server's ladder ------------------------------

    @Test
    fun theQualityMenuIsTheServersLadder() {
        // A 1080p source: the server drops every rung above it.
        val ladder = listOf(
            Rung(1080, total_kbps = 8_192),
            Rung(720, total_kbps = 4_096),
            Rung(480, total_kbps = 2_048),
            Rung(360, total_kbps = 896),
        )
        val options = qualityOptions(ladder)

        assertEquals(
            listOf(
                PlaybackQuality.Auto,
                PlaybackQuality.Original,
                PlaybackQuality.Q1080,
                PlaybackQuality.Q720,
                PlaybackQuality.Q480,
                PlaybackQuality.Q360,
            ),
            options.map { it.quality },
        )
        assertEquals("1080p · 8.2 Mbps", options[2].label)
        assertEquals("360p · 896 kbps", options[5].label)
    }

    @Test
    fun anOldServerWithoutALadderKeepsTheStoredEnumAsItsMenu() {
        assertEquals(PlaybackQuality.entries.map { it }, qualityOptions(emptyList()).map { it.quality })
    }

    // ---- Badges M3: what is actually on screen ------------------------------

    @Test
    fun aServerThatSaysNothingLeavesTheBadgeSourceOnly() {
        assertNull(renderedRange(null, null, null, setOf(HdrType.DOLBY_VISION)))
    }

    @Test
    fun anSdrPanelRendersSdrWhateverTheServerDelivers() {
        // The one case that needs no decoder at all: there is nowhere for the
        // extra range to go.
        assertEquals(SDR, renderedRange(DOLBY_VISION, null, null, emptySet()))
        assertEquals(SDR, renderedRange(HDR10, null, null, emptySet()))
        assertEquals(SDR, renderedRange(HLG, null, null, emptySet()))
    }

    @Test
    fun theDisplayGatesEachGradeSeparately() {
        // An HDR10-only panel is not a Dolby Vision panel; a DV panel is a PQ
        // panel and an HLG panel both.
        assertEquals(SDR, renderedRange(DOLBY_VISION, null, null, setOf(HdrType.HDR10)))
        assertEquals(DOLBY_VISION, renderedRange(DOLBY_VISION, null, null, setOf(HdrType.DOLBY_VISION)))
        assertEquals(HDR10, renderedRange(HDR10, null, null, setOf(HdrType.HDR10_PLUS)))
        assertEquals(SDR, renderedRange(HLG, null, null, setOf(HdrType.HDR10)))
        assertEquals(HLG, renderedRange(HLG, null, null, setOf(HdrType.HLG)))
    }

    @Test
    fun theDecoderWinsWhenItContradictsThePlan() {
        val dvPanel = setOf(HdrType.DOLBY_VISION, HdrType.HDR10)
        // Preserved DV that the device is in fact playing as its PQ base.
        assertEquals(HDR10, renderedRange(DOLBY_VISION, "video/hevc", COLOR_TRANSFER_ST2084, dvPanel))
        // The DV decoder engaged: a PQ transfer on a DV sample MIME is still DV.
        assertEquals(
            DOLBY_VISION,
            renderedRange(DOLBY_VISION, "video/dolby-vision", COLOR_TRANSFER_ST2084, dvPanel),
        )
        // An explicit SDR transfer contradicts an HDR plan outright.
        assertEquals(SDR, renderedRange(HDR10, "video/avc", COLOR_TRANSFER_SDR, dvPanel))
    }

    @Test
    fun silenceFromTheDecoderIsNotAContradiction() {
        val panel = setOf(HdrType.HDR10)
        // An HLS variant can publish no ColorInfo at all. Absence of evidence
        // must not become evidence of SDR — the server's answer stands.
        assertEquals(HDR10, renderedRange(HDR10, "video/hevc", null, panel))
        assertEquals(HDR10, renderedRange(HDR10, "video/hevc", COLOR_TRANSFER_UNSET, panel))
        assertEquals(HDR10, renderedRange(HDR10, null, null, panel))
    }

    private companion object {
        const val DOLBY_VISION = "dolby_vision"
        const val HDR10 = "hdr10"
        const val HLG = "hlg"
        const val SDR = "sdr"

        // Media3 `C.COLOR_TRANSFER_*` / `Format.NO_VALUE`, restated so the
        // table above reads as a table.
        const val COLOR_TRANSFER_SDR = 3
        const val COLOR_TRANSFER_ST2084 = 6
        const val COLOR_TRANSFER_UNSET = -1
    }
}
