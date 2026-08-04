package tv.plurx.app.ui

import org.junit.Assert.assertEquals
import org.junit.Test
import tv.plurx.app.data.Item

class CompactMediaLayoutTest {
    @Test
    fun phoneHeroOwnsTheFirstContinueItemWithoutDuplicatingItInTheShelf() {
        val items = listOf(
            Item(id = 1, kind = "movie", title = "First"),
            Item(id = 2, kind = "movie", title = "Second"),
        )

        assertEquals(listOf(2L), continueWatchingShelfItems(items, FormFactor.Compact).map { it.id })
        assertEquals(listOf(1L, 2L), continueWatchingShelfItems(items, FormFactor.Television).map { it.id })
    }

    @Test
    fun phoneDetailUsesSmallPlainFactsWithoutSeparatorGlyphs() {
        val item = Item(
            id = 3,
            kind = "movie",
            title = "Example",
            year = 2026,
            tags = listOf("Comedy", "Horror"),
        )

        assertEquals(
            listOf("2026", "1h 39m", "Comedy", "Horror"),
            compactDetailFacts(item, 5_940_000),
        )
    }
}
