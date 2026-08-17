package tv.plurx.app.ui.components

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import tv.plurx.app.data.AudioStream
import tv.plurx.app.data.PlaybackTrackDefault
import tv.plurx.app.data.SubtitleStream

/**
 * The detail screen's half of docs/CLIENTS.md §"Shared track facts". Every
 * assertion here is about rendering the server's answer — none of it re-derives
 * which track plays.
 */
class TrackFactsTest {
    private fun audio(
        index: Long,
        language: String? = "eng",
        title: String? = null,
        codec: String = "eac3",
        channels: Int? = 6,
    ) = AudioStream(
        index = index,
        codec = codec,
        channels = channels,
        language = language,
        title = title,
        default = false,
    )

    private fun subtitle(
        index: Long,
        language: String? = "eng",
        codec: String = "subrip",
        title: String? = null,
        forced: Boolean = false,
        sdh: Boolean = false,
    ) = SubtitleStream(
        index = index,
        codec = codec,
        language = language,
        title = title,
        forced = forced,
        hearing_impaired = sdh,
    )

    private fun default(status: String, language: String = "eng", selected: Long? = null) =
        PlaybackTrackDefault(
            selected_index = selected,
            preferred_language = language,
            preferred_language_status = status,
        )

    @Test
    fun everyContractStatusMapsToItsOwnWireSpelling() {
        assertEquals(
            listOf(
                PreferredLanguageStatus.Selected,
                PreferredLanguageStatus.Available,
                PreferredLanguageStatus.Missing,
                PreferredLanguageStatus.Unknown,
                PreferredLanguageStatus.NoTracks,
            ),
            listOf("selected", "available", "missing", "unknown", "no_tracks")
                .map(PreferredLanguageStatus::fromWire),
        )
    }

    @Test
    fun anUnrecognizedOrAbsentStatusClaimsNothing() {
        // A server older than the contract, or a sixth state a future one adds.
        // Inventing one of the five here would put a claim on screen that
        // nobody made.
        assertNull(PreferredLanguageStatus.fromWire(null))
        assertNull(PreferredLanguageStatus.fromWire(""))
        assertNull(PreferredLanguageStatus.fromWire("partially_available"))
        assertNull(
            preferredLanguageLine(TrackKind.Subtitle, null, "eng"),
        )
        assertNull(audioPreferredLanguageLine(null, listOf(audio(0))))
        assertNull(subtitlePreferredLanguageLine(null, listOf(subtitle(0))))
    }

    /** Criterion 2's own example, in one file's two lines. */
    @Test
    fun theViewerLearnsEnglishAudioAndNoEnglishSubtitlesWithoutPlaying() {
        val audioLine = audioPreferredLanguageLine(
            default("selected", selected = 0),
            listOf(audio(0)),
        )
        val subtitleLine = subtitlePreferredLanguageLine(
            default("missing"),
            listOf(subtitle(0, language = "jpn")),
        )

        assertEquals("English audio.", audioLine)
        assertEquals("No English subtitles.", subtitleLine)
    }

    /**
     * The anime case the contract is written around: the server selects
     * Japanese original audio and still reports the English dub as available,
     * so the line has to name what actually plays instead.
     */
    @Test
    fun availableNamesTheTrackThatPlaysInstead() {
        val line = audioPreferredLanguageLine(
            default("available", selected = 1),
            listOf(audio(0, language = "eng"), audio(1, language = "jpn")),
        )
        assertEquals("English audio available — Japanese plays by default.", line)
    }

    @Test
    fun availableSubtitlesSayWhenTheServerChoseOff() {
        // `selected_index` is absent: policy picked no subtitle at all, which
        // is a different sentence from "another track is on".
        val line = subtitlePreferredLanguageLine(
            default("available", selected = null),
            listOf(subtitle(0)),
        )
        assertEquals("English subtitles available — subtitles are off by default.", line)
    }

    /**
     * The one folding the contract forbids: an untagged track means absence
     * *cannot be claimed*, so this must read "can't tell" and must not be the
     * `missing` sentence.
     */
    @Test
    fun unknownSaysCantTellAndIsNeverTheMissingWording() {
        val unknown = requireNotNull(
            subtitlePreferredLanguageLine(default("unknown"), listOf(subtitle(0, language = null))),
        )
        val missing = subtitlePreferredLanguageLine(default("missing"), listOf(subtitle(0)))

        assertTrue(unknown, unknown.startsWith("Can't tell"))
        assertTrue(unknown, unknown.contains("no language tag"))
        assertNotEquals(missing, unknown)

        val unknownAudio = requireNotNull(
            audioPreferredLanguageLine(default("unknown"), listOf(audio(0, language = null))),
        )
        assertTrue(unknownAudio, unknownAudio.startsWith("Can't tell"))
    }

    @Test
    fun noTracksSaysSoPlainlyForEachKind() {
        assertEquals(
            "No subtitles in this file.",
            subtitlePreferredLanguageLine(default("no_tracks"), emptyList()),
        )
        assertEquals(
            "No audio tracks in this file.",
            audioPreferredLanguageLine(default("no_tracks"), emptyList()),
        )
    }

    @Test
    fun everyStatusRendersADistinctSentence() {
        val lines = PreferredLanguageStatus.values().map {
            preferredLanguageLine(TrackKind.Subtitle, it, "eng", defaultTrackLabel = "Japanese")
        }
        assertEquals(5, lines.size)
        assertEquals(5, lines.toSet().size)
    }

    @Test
    fun aSubtitleRowCarriesLanguageFormatAndTheForcedAndSdhMarkers() {
        assertEquals(
            "English · SRT",
            subtitleStreamLabel(subtitle(0)),
        )
        assertEquals(
            "English · Signs · SRT · Forced",
            subtitleStreamLabel(subtitle(1, title = "Signs", forced = true)),
        )
        assertEquals(
            "English · PGS · SDH",
            subtitleStreamLabel(subtitle(2, codec = "hdmv_pgs_subtitle", sdh = true)),
        )
        // The server's own rule: a title says "Forced" where the disposition
        // does not, and both count.
        assertEquals("English · Forced · SRT", subtitleStreamLabel(subtitle(3, title = "Forced")))
        // ...but the negated titles that exist in real libraries do not earn
        // the marker, exactly as `titleMarksForced` decides for the player.
        assertEquals(
            "English · Non-Forced · SRT",
            subtitleStreamLabel(subtitle(4, title = "Non-Forced")),
        )
    }

    @Test
    fun anUntaggedTrackIsNamedRatherThanLeftBlank() {
        // This is the fact that produces `unknown`; hiding it would make that
        // status unreadable.
        assertEquals("Unknown language · SRT", subtitleStreamLabel(subtitle(0, language = null)))
        assertTrue(audioStreamLabel(audio(0, language = null)).startsWith("Unknown language"))
    }

    @Test
    fun anAudioRowCarriesLanguageTitleChannelsAndCodec() {
        assertEquals(
            "English · Atmos · 7.1 · TRUEHD",
            audioStreamLabel(audio(1, title = "Atmos", codec = "truehd", channels = 8)),
        )
        assertEquals("Japanese · Stereo · AAC", audioStreamLabel(audio(2, "jpn", codec = "aac", channels = 2)))
    }

    @Test
    fun subtitleFormatsUseTheNamesAViewerReads() {
        assertEquals("SRT", subtitleFormatLabel("subrip"))
        assertEquals("PGS", subtitleFormatLabel("hdmv_pgs_subtitle"))
        assertEquals("VobSub", subtitleFormatLabel("dvd_subtitle"))
        assertEquals("ASS", subtitleFormatLabel("ass"))
        assertEquals("MOV Text", subtitleFormatLabel("mov_text"))
        // An unknown codec still names itself rather than disappearing.
        assertEquals("SOMETHINGNEW", subtitleFormatLabel("somethingnew"))
    }
}
