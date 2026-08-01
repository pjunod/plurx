package tv.plurx.app.ui

import androidx.compose.ui.test.assertHasClickAction
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import tv.plurx.app.data.Item
import tv.plurx.app.ui.theme.PlurxTheme

class AncestorBreadcrumbTest {
    @get:Rule
    val compose = createComposeRule()

    @Test
    fun episodeAncestorsOpenTheShowAndSeasonScreens() {
        val opened = mutableListOf<Long>()
        compose.setContent {
            PlurxTheme {
                AncestorBreadcrumb(
                    ancestors = listOf(
                        Item(id = 10, kind = "show", title = "Shameless"),
                        Item(id = 20, kind = "season", title = "Season 1"),
                    ),
                    onOpenItem = opened::add,
                )
            }
        }

        compose.onNodeWithText("Shameless").assertHasClickAction().performClick()
        compose.onNodeWithText("Season 1").assertHasClickAction().performClick()

        assertEquals(listOf(10L, 20L), opened)
    }
}
