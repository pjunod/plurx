package tv.plurx.app.player

import androidx.compose.ui.test.assertHasClickAction
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.assertIsFocused
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
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
                    title = "Example feature film",
                    fileId = 42,
                    deliveryMode = "direct_play",
                    bufferedPercent = 73,
                    durationMs = 7_384_000,
                    reasons = listOf("Device supports the source video and audio codecs"),
                    onDismiss = {},
                )
            }
        }

        compose.onNodeWithText("Playback info").assertIsDisplayed()
        compose.onNodeWithText("Example feature film").assertIsDisplayed()
        compose.onNodeWithText("Delivery").assertIsDisplayed()
        compose.onNodeWithText("Direct play").assertIsDisplayed()
        compose.onNodeWithText("Buffered").assertIsDisplayed()
        compose.onNodeWithText("73%").assertIsDisplayed()
        compose.onNodeWithText("2:03:04").assertIsDisplayed()
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
