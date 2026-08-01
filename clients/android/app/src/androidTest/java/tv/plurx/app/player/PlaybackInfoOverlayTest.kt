package tv.plurx.app.player

import androidx.compose.ui.test.assertHasClickAction
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.assertIsFocused
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.semantics.SemanticsProperties
import org.junit.Rule
import org.junit.Test
import tv.plurx.app.data.Appearance
import tv.plurx.app.data.ThemeId
import tv.plurx.app.ui.theme.PlurxTheme

class PlaybackInfoOverlayTest {
    @get:Rule
    val compose = createComposeRule()

    @Test
    fun lightThemeStillShowsStructuredPlaybackDetails() {
        compose.setContent {
            PlurxTheme(ThemeId.Classic, Appearance.Light) {
                PlaybackInfoOverlay(
                    details = PlaybackInfoDetails(
                        title = "Example feature film",
                        fileId = 42,
                        delivery = "Direct play",
                        position = "0:31:04 / 2:03:04",
                        buffer = "0:00 ahead · 73%",
                        videoHealth = "12,340 rendered · 2 dropped (0.0%) · max streak 1",
                        sourceFile = "Example.2160p.mkv",
                        sourceVideo = "HEVC · Main 10 · 3840×2160 · HDR10 · 10-bit · 48.2 Mbps",
                        sourceAudio = "TRUEHD · 7.1 · English",
                        playingVideo = "HEVC · 3840×2160 · HDR10 / PQ",
                        playingAudio = "English · 7.1 · TrueHD",
                        subtitles = "Off",
                    ),
                    reasons = listOf("Device supports the source video and audio codecs"),
                    onDismiss = {},
                )
            }
        }

        compose.onNodeWithText("Playback info").assertIsDisplayed()
        compose.onNodeWithText("Example feature film").assertIsDisplayed()
        compose.onNodeWithText("Delivery").assertIsDisplayed()
        compose.onNodeWithText("Direct play").assertIsDisplayed()
        compose.onAllNodesWithText("Position").assertCountEquals(0)
        compose.onNodeWithText("0:31:04 / 2:03:04").assertIsDisplayed()
        compose.onNodeWithText("Frames").assertIsDisplayed()
        compose.onNodeWithText("12,340 rendered · 2 dropped (0.0%) · max streak 1").assertIsDisplayed()
        compose.onNodeWithText("SOURCE MEDIA").assertIsDisplayed()
        compose.onNodeWithText("Example.2160p.mkv").assertIsDisplayed()
        compose.onNodeWithText("HEVC · Main 10 · 3840×2160 · HDR10 · 10-bit · 48.2 Mbps").assertIsDisplayed()
        compose.onNodeWithText("NOW PLAYING").assertIsDisplayed()
        compose.onNodeWithText("HEVC · 3840×2160 · HDR10 / PQ").assertIsDisplayed()
        compose.onNodeWithText("English · 7.1 · TrueHD").assertIsDisplayed()
        val close = compose.onNodeWithContentDescription("Close playback info")
        compose.waitUntil(timeoutMillis = 2_000) {
            close.fetchSemanticsNode().config.getOrElse(SemanticsProperties.Focused) { false }
        }
        close
            .assertIsDisplayed()
            .assertHasClickAction()
            .assertIsFocused()
    }
}
