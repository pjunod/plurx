package tv.plurx.app.ui

import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onAllNodesWithTag
import androidx.compose.ui.test.onNodeWithTag
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import tv.plurx.app.data.Item
import tv.plurx.app.ui.components.PosterCard
import tv.plurx.app.ui.components.PosterResolutionPlacement
import tv.plurx.app.ui.theme.PlurxTheme

class PosterCardResolutionTest {
    @get:Rule
    val compose = createComposeRule()

    @Test
    fun belowArtworkResolutionIsNotDrawnOverTheThumbnail() {
        compose.setContent {
            PlurxTheme {
                PosterCard(
                    item = Item(id = 1, kind = "movie", title = "Resolution check", resolution = 2160),
                    resolutionPlacement = PosterResolutionPlacement.BelowArtwork,
                    onClick = {},
                )
            }
        }

        assertTrue(
            compose.onAllNodesWithTag("poster-resolution-overlay", useUnmergedTree = true)
                .fetchSemanticsNodes().isEmpty()
        )
        val artwork = compose.onNodeWithTag("poster-artwork", useUnmergedTree = true)
            .fetchSemanticsNode().boundsInRoot
        val metadata = compose.onNodeWithTag("poster-resolution-metadata", useUnmergedTree = true)
            .fetchSemanticsNode().boundsInRoot
        assertTrue(metadata.top >= artwork.bottom)
    }
}
