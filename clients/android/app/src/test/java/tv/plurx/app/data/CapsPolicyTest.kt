package tv.plurx.app.data

import org.junit.Assert.assertEquals
import org.junit.Test

class CapsPolicyTest {

    @Test
    fun directPlayContainersIncludeSupportedAudiobookSources() {
        val containers = DIRECT_PLAY_CONTAINERS.split(',').toSet()
        assertEquals(
            setOf("m4a", "m4b", "mp3", "aac", "flac", "ogg", "opus", "wav"),
            containers.intersect(setOf("m4a", "m4b", "mp3", "aac", "flac", "ogg", "opus", "wav")),
        )
    }

    @Test
    fun dolbyVisionClaimsOnlyTheSingleLayerDeliveryProfiles() {
        // A Shield-class decoder lists dual-layer P7 alongside 4/5/8.
        val decoder = listOf(
            DolbyVisionCodecProfile.DVHE_DTR,
            DolbyVisionCodecProfile.DVHE_STN,
            DolbyVisionCodecProfile.DVHE_DTB,
            DolbyVisionCodecProfile.DVHE_ST,
        )
        assertEquals(listOf(4, 5, 8), dolbyVisionProfiles(decoder))
    }

    @Test
    fun dolbyVisionNeverClaimsProfileSevenAlone() {
        // P7 is not a delivery profile: a decoder that lists only DvheDtb
        // must keep taking the server's strip path.
        assertEquals(emptyList<Int>(), dolbyVisionProfiles(listOf(DolbyVisionCodecProfile.DVHE_DTB)))
    }

    @Test
    fun dolbyVisionIgnoresTheAvcAndAv1BasedProfiles() {
        assertEquals(
            emptyList<Int>(),
            dolbyVisionProfiles(
                listOf(
                    DolbyVisionCodecProfile.DVAV_PER,
                    DolbyVisionCodecProfile.DVAV_PEN,
                    DolbyVisionCodecProfile.DVHE_DER,
                    DolbyVisionCodecProfile.DVHE_DEN,
                    DolbyVisionCodecProfile.DVHE_DTH,
                    DolbyVisionCodecProfile.DVAV_SE,
                    DolbyVisionCodecProfile.DVAV_110,
                ),
            ),
        )
    }

    @Test
    fun dolbyVisionIsClaimedOnlyWhenDecoderAndDisplayBothAgree() {
        assertEquals(
            mapOf("dv" to "1", "dvprofile" to "5,8"),
            dolbyVisionCaps(listOf(5, 8), displaySupportsDolbyVision = true),
        )
        // An HDR10-only panel: claiming DV buys a stream the TV cannot show.
        assertEquals(emptyMap<String, String>(), dolbyVisionCaps(listOf(5, 8), false))
        // No DV decoder: the display alone proves nothing.
        assertEquals(emptyMap<String, String>(), dolbyVisionCaps(emptyList(), true))
    }

    @Test
    fun audioIsClaimedFromADecoderOrFromTheSink() {
        val base = listOf("aac", "mp3", "opus", "flac")

        // Neither: bare stereo output.
        assertEquals(base, audioCodecClaims(emptySet(), emptySet()))

        // Decoder only — a phone that decodes E-AC3 in software.
        assertEquals(
            base + listOf("eac3"),
            audioCodecClaims(setOf("audio/eac3"), emptySet()),
        )

        // Sink only — a Shield with no TrueHD decoder feeding an AVR that
        // decodes it. This is the case that used to lose lossless Atmos.
        assertEquals(
            base + listOf("truehd"),
            audioCodecClaims(emptySet(), setOf(AudioSinkEncoding.DOLBY_TRUEHD)),
        )

        // Both, from either side, and never duplicated.
        assertEquals(
            base + listOf("ac3", "eac3", "dts", "truehd"),
            audioCodecClaims(
                setOf("audio/ac3", "audio/vnd.dts"),
                setOf(
                    AudioSinkEncoding.AC3,
                    AudioSinkEncoding.E_AC3_JOC,
                    AudioSinkEncoding.DTS_HD,
                    AudioSinkEncoding.DOLBY_TRUEHD,
                ),
            ),
        )
    }
}
