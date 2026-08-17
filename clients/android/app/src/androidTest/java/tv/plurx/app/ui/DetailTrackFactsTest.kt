package tv.plurx.app.ui

import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import tv.plurx.app.data.AudioStream
import tv.plurx.app.data.MediaFileDto
import tv.plurx.app.data.PlaybackDefaults
import tv.plurx.app.data.PlaybackTrackDefault
import tv.plurx.app.data.SubtitleStream
import tv.plurx.app.player.PreplayTracks
import tv.plurx.app.player.SubtitleChoice
import tv.plurx.app.ui.theme.PlurxTheme

/**
 * The detail screen's answer to "does this have my audio and subtitles, and can
 * I choose before pressing play?" (issue #289 criteria 1–3 and 6).
 *
 * Instrumented because it is Compose rendering and click behavior; the wording
 * and the five-state vocabulary underneath are covered by the JVM
 * `TrackFactsTest`, which is what the CI gate runs.
 */
class DetailTrackFactsTest {
    @get:Rule
    val compose = createComposeRule()

    private val dualAudio = MediaFileDto(
        id = 70,
        filename = "anime.mkv",
        audio_streams = listOf(
            AudioStream(index = 0, codec = "aac", channels = 2, language = "eng", default = true),
            AudioStream(index = 1, codec = "flac", channels = 2, language = "jpn"),
        ),
        subtitle_streams = listOf(
            SubtitleStream(index = 0, codec = "subrip", language = "eng"),
            SubtitleStream(
                index = 1,
                codec = "hdmv_pgs_subtitle",
                language = "jpn",
                forced = true,
                hearing_impaired = true,
            ),
        ),
        // Japanese original audio selected against an English preference — the
        // anime case docs/CLIENTS.md is written around.
        playback_defaults = PlaybackDefaults(
            audio = PlaybackTrackDefault(1, "eng", "available"),
            subtitle = PlaybackTrackDefault(0, "eng", "selected"),
        ),
    )

    private fun show(
        file: MediaFileDto = dualAudio,
        tracks: PreplayTracks = PreplayTracks.NONE,
        notice: String? = null,
        onTracks: (PreplayTracks) -> Unit = {},
    ) {
        compose.setContent {
            PlurxTheme {
                TrackFactsSection(
                    file = file,
                    enabled = true,
                    tracks = tracks,
                    subtitleNotice = notice,
                    onTracks = onTracks,
                )
            }
        }
    }

    @Test
    fun everySubtitleTrackIsListedWithLanguageFormatAndItsMarkers() {
        show()

        compose.onNodeWithText("Subtitles").assertIsDisplayed()
        compose.onNodeWithText("English · SRT").assertIsDisplayed()
        compose.onNodeWithText("Japanese · PGS · Forced · SDH").assertIsDisplayed()
        // Audio keeps its existing facts beside them.
        compose.onNodeWithText("English · Stereo · AAC").assertIsDisplayed()
        compose.onNodeWithText("Japanese · Stereo · FLAC").assertIsDisplayed()
    }

    @Test
    fun bothListsReportTheServersVerdictWithoutStartingPlayback() {
        show()

        // The server picked Japanese audio and still says English exists.
        compose.onNodeWithText("English audio available — Japanese plays by default.")
            .assertIsDisplayed()
        compose.onNodeWithText("English subtitles.").assertIsDisplayed()
        // And the server's own picks are marked as the defaults — one per list.
        compose.onAllNodesWithText("Default").assertCountEquals(2)
    }

    @Test
    fun aFileWithNoSubtitlesSaysSoRatherThanRenderingAnEmptyRow() {
        show(
            dualAudio.copy(
                subtitle_streams = emptyList(),
                playback_defaults = PlaybackDefaults(
                    audio = PlaybackTrackDefault(0, "eng", "selected"),
                    subtitle = PlaybackTrackDefault(null, "eng", "no_tracks"),
                ),
            ),
        )

        compose.onNodeWithText("Subtitles").assertIsDisplayed()
        compose.onNodeWithText("No subtitles in this file.").assertIsDisplayed()
    }

    @Test
    fun aViewerCanPickAnAudioTrackAndASubtitleTrackBeforePressingPlay() {
        var chosen = PreplayTracks.NONE
        show(tracks = chosen, onTracks = { chosen = it })

        compose.onNodeWithText("English · Stereo · AAC").performClick()
        assertEquals(PreplayTracks(audio = 0), chosen)

        chosen = PreplayTracks.NONE
        compose.onNodeWithText("Japanese · PGS · Forced · SDH").performClick()
        assertEquals(PreplayTracks(subtitle = SubtitleChoice(1)), chosen)
    }

    @Test
    fun offIsAChoiceOfItsOwnAndNotMerelyTheAbsenceOfOne() {
        var chosen = PreplayTracks.NONE
        show(onTracks = { chosen = it })

        compose.onNodeWithText("Off").performClick()
        assertEquals(PreplayTracks(subtitle = SubtitleChoice(null)), chosen)
        // Which is distinct from having chosen nothing at all.
        assertEquals(-1L, chosen.subtitleQuery)
    }

    @Test
    fun aBurnInCostIsDisclosedBeforePlaybackStarts() {
        show(
            tracks = PreplayTracks(subtitle = SubtitleChoice(1)),
            notice = "These subtitles are burned into the picture, which re-encodes the video.",
        )

        compose.onNodeWithText(
            "These subtitles are burned into the picture, which re-encodes the video.",
        ).assertIsDisplayed()
    }

    @Test
    fun anHdrBlockedBurnIsReportedHonestlyRatherThanAsSubtitlesOn() {
        show(
            tracks = PreplayTracks(subtitle = SubtitleChoice(1)),
            notice = "These subtitles need an SDR burn-in. HDR playback is kept unchanged, " +
                "so they will not be shown.",
        )

        compose.onNodeWithText(
            "These subtitles need an SDR burn-in. HDR playback is kept unchanged, " +
                "so they will not be shown.",
        ).assertIsDisplayed()
    }
}
