package tv.plurx.app.player

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import tv.plurx.app.data.HdrType
import tv.plurx.app.data.PlaybackQuality
import tv.plurx.app.data.Rung

/**
 * The decisions this client still makes on its own. The subtitle contract —
 * routing, rendition ordinals, the server's automatic pick, and the session
 * body each arm posts — is `SubtitlePolicyTest`'s.
 */
class PlaybackPolicyTest {

    // ---- §5.3 force says what the menu means --------------------------------

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

    // ---- §4.2 the fidelity-preserving rescue ladder ------------------------

    @Test
    fun dolbyVisionDirectPlayGetsAContainerRescueBeforeTheSdrLastResort() {
        assertEquals(
            PlaybackErrorAction.RetryAsDolbyVisionRemux,
            playbackErrorAction("direct", true, false, false),
        )
        assertEquals(
            PlaybackErrorAction.RetryAsCompatibilityTranscode,
            playbackErrorAction("remux", true, true, false),
        )
        assertEquals(
            PlaybackErrorAction.RetryAsCompatibilityTranscode,
            playbackErrorAction("direct", false, false, false),
        )
        // Once the compatibility transcode has been tried there is no lower
        // fidelity mode left, and retrying it would loop.
        assertEquals(PlaybackErrorAction.Fail, playbackErrorAction("remux", true, true, true))
        // A transcode that fails has nothing cheaper to fall back to; retrying
        // it would loop.
        assertEquals(PlaybackErrorAction.Fail, playbackErrorAction("transcode", true, false, false))
        assertEquals(PlaybackErrorAction.Fail, playbackErrorAction("transcode", false, true, true))
    }

    @Test
    fun establishedHdrRetriesTheSameDeliveryAndNeverFallsThroughToSdr() {
        assertEquals(
            PlaybackErrorAction.RetrySameHDRDelivery,
            playbackErrorAction(
                deliveryMode = "remux",
                preservesDolbyVision = false,
                remuxRescueAlreadyUsed = false,
                transcodeRescueAlreadyUsed = false,
                deliveredRange = "hdr10",
                establishedPlayback = true,
                sameHdrRetryAlreadyUsed = false,
            ),
        )
        assertEquals(
            PlaybackErrorAction.Fail,
            playbackErrorAction(
                deliveryMode = "remux",
                preservesDolbyVision = false,
                remuxRescueAlreadyUsed = false,
                transcodeRescueAlreadyUsed = false,
                deliveredRange = "hdr10",
                establishedPlayback = true,
                sameHdrRetryAlreadyUsed = true,
            ),
        )
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
