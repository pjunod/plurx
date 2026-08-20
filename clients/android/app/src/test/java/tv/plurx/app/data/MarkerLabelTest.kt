package tv.plurx.app.data

import org.junit.Assert.assertEquals
import org.junit.Test

class MarkerLabelTest {
    @Test
    fun chapterDerivedMarkerShowsExactLabel() {
        val marker = Marker(
            kind = "credits",
            label = "Skip Credits",
            start_ms = 600_000,
            end_ms = 660_000,
            chapter = true,
        )

        assertEquals("Skip Credits", marker.displayLabel)
    }

    @Test
    fun estimatedMarkerShowsHedgedLabel() {
        val marker = Marker(
            kind = "credits",
            label = "Skip Credits",
            start_ms = 600_000,
            end_ms = 660_000,
            chapter = false,
        )

        assertEquals("Skip Credits (estimated)", marker.displayLabel)
    }

    @Test
    fun markerDefaultsToChapterTrue() {
        val marker = Marker(
            kind = "credits",
            label = "Skip Credits",
            start_ms = 600_000,
            end_ms = 660_000,
        )

        assertEquals(true, marker.chapter)
        assertEquals("Skip Credits", marker.displayLabel)
    }
}
