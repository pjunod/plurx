package tv.plurx.app.player

import org.junit.Assert.assertEquals
import org.junit.Test

class PlaybackInfoTest {
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
