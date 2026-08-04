package tv.plurx.app.player

import android.view.KeyEvent
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import tv.plurx.app.data.AudioStream
import tv.plurx.app.data.MediaFileDto

class PlayerPolicyTest {
    @Test
    fun backClosesPlayerUiBeforeLeavingPlayback() {
        assertEquals(PlayerBackAction.ClosePanel, playerBackAction(panelOpen = true, controlsVisible = false))
        assertEquals(PlayerBackAction.HideControls, playerBackAction(panelOpen = false, controlsVisible = true))
        assertEquals(PlayerBackAction.ExitPlayback, playerBackAction(panelOpen = false, controlsVisible = false))
    }

    @Test
    fun directionalSeeksUseShortHorizontalAndLongVerticalSteps() {
        assertEquals(-10_000L, playerSeekDeltaMs(KeyEvent.KEYCODE_DPAD_LEFT, controlsVisible = false))
        assertEquals(10_000L, playerSeekDeltaMs(KeyEvent.KEYCODE_DPAD_RIGHT, controlsVisible = false))
        assertEquals(-30_000L, playerSeekDeltaMs(KeyEvent.KEYCODE_DPAD_DOWN, controlsVisible = false))
        assertEquals(30_000L, playerSeekDeltaMs(KeyEvent.KEYCODE_DPAD_UP, controlsVisible = false))
        assertNull(playerSeekDeltaMs(KeyEvent.KEYCODE_DPAD_CENTER, controlsVisible = false))
        assertNull(
            "visible controls retain D-pad focus navigation",
            playerSeekDeltaMs(KeyEvent.KEYCODE_DPAD_RIGHT, controlsVisible = true),
        )
    }

    @Test
    fun playbackInfoSummarizesUsefulSourceDetails() {
        val file = MediaFileDto(
            id = 42,
            filename = "Example.2160p.mkv",
            container = "mkv",
            video_codec = "hevc",
            video_profile = "Main 10",
            width = 3840,
            height = 2160,
            bit_depth = 10,
            hdr_format = "HDR10",
            bitrate = 48_200_000,
            audio_streams = listOf(
                AudioStream(codec = "truehd", channels = 8, language = "en", default = true),
                AudioStream(codec = "aac", channels = 2, language = "en"),
            ),
        )

        assertEquals("HEVC · Main 10 · 3840×2160 · HDR10 · 10-bit · 48.2 Mbps", sourceVideoSummary(file))
        assertEquals("TRUEHD · 7.1 · English · +1 track", sourceAudioSummary(file))
        assertEquals("Direct play", deliveryLabel("direct"))
        assertEquals("Transcode · HLS", deliveryLabel("transcode"))
    }
}
