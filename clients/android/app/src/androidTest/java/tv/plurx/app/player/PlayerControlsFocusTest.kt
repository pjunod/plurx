package tv.plurx.app.player

import androidx.compose.ui.input.key.Key
import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.test.assertIsFocused
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.performKeyInput
import androidx.compose.ui.test.performSemanticsAction
import androidx.compose.ui.test.pressKey
import androidx.compose.ui.semantics.SemanticsProperties
import org.junit.Rule
import org.junit.Test
import tv.plurx.app.ui.theme.PlurxTheme

class PlayerControlsFocusTest {
    @get:Rule
    val compose = createComposeRule()

    @Test
    fun playPauseReceivesFocusWhenControlsOpen() {
        compose.setContent {
            PlurxTheme {
                Controls(
                    title = "Example episode",
                    positionMs = 30_000,
                    durationMs = 60_000,
                    isPlaying = true,
                    requestInitialFocus = true,
                    onBack = {},
                    onPlayPause = {},
                    onSeekBack = {},
                    onSeekForward = {},
                    onScrubStart = {},
                    onScrub = {},
                    onScrubEnd = {},
                    onTracks = {},
                    onSettings = {},
                    onInfo = {},
                    onPip = null,
                )
            }
        }

        val pause = compose.onNodeWithContentDescription("Pause")
        compose.waitUntil(timeoutMillis = 2_000) {
            pause.fetchSemanticsNode().config.getOrElse(SemanticsProperties.Focused) { false }
        }
        pause.assertIsFocused()
    }

    @Test
    fun verticalDpadLeavesTheProgressSliderWithoutSeeking() {
        compose.setContent {
            PlurxTheme {
                Controls(
                    title = "Example episode",
                    positionMs = 30_000,
                    durationMs = 60_000,
                    isPlaying = true,
                    requestInitialFocus = false,
                    onBack = {},
                    onPlayPause = {},
                    onSeekBack = {},
                    onSeekForward = {},
                    onScrubStart = {},
                    onScrub = {},
                    onScrubEnd = {},
                    onTracks = {},
                    onSettings = {},
                    onInfo = {},
                    onPip = null,
                )
            }
        }

        val slider = compose.onNodeWithContentDescription("Playback position")
        val pause = compose.onNodeWithContentDescription("Pause")

        slider.performSemanticsAction(SemanticsActions.RequestFocus)
        slider.assertIsFocused()
        slider.performKeyInput { pressKey(Key.DirectionUp) }
        pause.assertIsFocused()

        slider.performSemanticsAction(SemanticsActions.RequestFocus)
        slider.assertIsFocused()
        slider.performKeyInput { pressKey(Key.DirectionDown) }
        pause.assertIsFocused()
    }
}
