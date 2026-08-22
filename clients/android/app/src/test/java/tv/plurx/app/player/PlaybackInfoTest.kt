package tv.plurx.app.player

import org.junit.Assert.assertEquals
import org.junit.Test
import tv.plurx.app.data.Item

class PlaybackInfoTest {
    @Test
    fun playbackInfoUsesTheSharedThreeModeContract() {
        assertEquals(
            listOf("Mini", "Standard", "Debug"),
            PlaybackStatsMode.entries.map { it.label },
        )
    }

    @Test
    fun playMenuCarriesTheWebMetadataHierarchy() {
        val episode = Item(
            id = 7,
            kind = "episode",
            title = "The Return",
            show_title = "Example Show",
            season_number = 2,
            episode_number = 5,
        )

        assertEquals("Example Show   ·   S2E5", playerSubtitle(episode))
        assertEquals(
            "Example Show   ·   S2E5   ·   The Return",
            playerHeading(episode.title, playerSubtitle(episode)),
        )
        assertEquals("Jun 10, 2026", playerDateLabel("2026-06-10", 2025))
        assertEquals("2025", playerDateLabel(null, 2025))
        assertEquals("1h 53m", playerRuntimeLabel(6_780_000))
        assertEquals("Jun 10, 2026   ·   1h 53m", playerContextLine("Jun 10, 2026", "1h 53m"))
    }

    @Test
    fun videoHealthSummarizesRenderedAndDroppedFrames() {
        assertEquals(
            "12,340 rendered · 25 dropped (0.2%) · max streak 3",
            videoHealthSummary(
                renderedFrames = 12_340,
                droppedFrames = 25,
                maxConsecutiveDroppedFrames = 3,
            ),
        )
    }

    @Test
    fun videoHealthHandlesAWaitingDecoder() {
        assertEquals(
            "0 rendered · 0 dropped (0.0%) · max streak 0",
            videoHealthSummary(
                renderedFrames = 0,
                droppedFrames = 0,
                maxConsecutiveDroppedFrames = 0,
            ),
        )
    }
}
