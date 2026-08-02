package tv.plurx.app.ui.components

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.test.assertIsFocused
import androidx.compose.ui.test.assertIsNotFocused
import androidx.compose.ui.test.isFocused
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performScrollToIndex
import androidx.compose.ui.test.performSemanticsAction
import androidx.compose.ui.unit.dp
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import tv.plurx.app.data.Item
import tv.plurx.app.ui.theme.PlurxTheme

/**
 * The Android TV focus graph, as a reproduction.
 *
 * CLIENTS-REMEDIATION-PLAN.md §7.1 asked for this test *before* the fix, so
 * "TV focus breaks on a scrolled shelf" would be evidence rather than a code
 * reading. The failure it reproduces: a shelf's `FocusRequester` used to be
 * attached to the card at index 0, which a `LazyRow` disposes the moment the
 * viewer scrolls past it. Every neighbouring shelf aims its `up`/`down` at that
 * requester, so after one scroll the vertical walk called `requestFocus()` on a
 * requester attached to nothing — `IllegalStateException: FocusRequester is not
 * initialized` on older Compose, a silently dead D-pad press on newer.
 *
 * Against the fixed graph — requester and `focusGroup` on the row container,
 * one focus target per card — these pass. Against the pre-fix graph the first
 * throws at `requestFocus`, and the third finds a focused node with no click
 * action on it.
 */
class ShelfFocusTest {
    @get:Rule
    val compose = createComposeRule()

    private fun cards(prefix: String, count: Int): List<Item> =
        (1..count).map { Item(id = "$prefix-$it".hashCode().toLong(), kind = "movie", title = "$prefix $it") }

    /** Two shelves wired exactly as HomeScreen wires consecutive rows. */
    private fun twoShelves(top: FocusRequester, bottom: FocusRequester) {
        compose.setContent {
            PlurxTheme {
                Column(Modifier.fillMaxSize()) {
                    MediaRow(
                        title = TOP_SHELF,
                        items = cards("Top", SHELF_LENGTH),
                        posterWidth = 96.dp,
                        rowFocusRequester = top,
                        nextRowFocusRequester = bottom,
                        onOpen = {},
                    )
                    MediaRow(
                        title = BOTTOM_SHELF,
                        items = cards("Bottom", 8),
                        posterWidth = 96.dp,
                        rowFocusRequester = bottom,
                        previousRowFocusRequester = top,
                        onOpen = {},
                    )
                }
            }
        }
    }

    private fun focusedNodeCount(): Int = compose.onAllNodes(isFocused()).fetchSemanticsNodes().size

    @Test
    fun aShelfStaysReachableAfterItsFirstCardScrollsAway() {
        val top = FocusRequester()
        val bottom = FocusRequester()
        twoShelves(top, bottom)

        // Scroll until the card the requester used to live on is disposed.
        compose.onNodeWithTag(shelfTestTag(TOP_SHELF)).performScrollToIndex(SHELF_LENGTH - 1)
        compose.waitForIdle()
        compose.onNodeWithText("Top 1").assertDoesNotExist()

        // The call every neighbouring shelf makes when the D-pad walks up.
        compose.runOnUiThread { top.requestFocus() }
        compose.waitForIdle()

        assertTrue(
            "A scrolled shelf must still accept focus from its neighbours",
            focusedNodeCount() > 0,
        )
    }

    @Test
    fun focusWalksDownToTheNextShelfAndBackUp() {
        val top = FocusRequester()
        val bottom = FocusRequester()
        twoShelves(top, bottom)

        compose.onNodeWithTag(shelfTestTag(TOP_SHELF)).performScrollToIndex(SHELF_LENGTH - 1)
        compose.waitForIdle()

        // Down into the shelf below…
        compose.runOnUiThread { bottom.requestFocus() }
        compose.waitForIdle()
        compose.onNodeWithText("Bottom 1").assertIsFocused()

        // …and back up into the one that scrolled. Which card takes it is the
        // lazy row's business; that *some* card does is the point.
        compose.runOnUiThread { top.requestFocus() }
        compose.waitForIdle()
        assertTrue("Walking back up must land on a card", focusedNodeCount() > 0)
        compose.onNodeWithText("Bottom 1").assertIsNotFocused()
    }

    @Test
    fun aPosterCardHasOneFocusTargetAndItIsTheClickableOne() {
        compose.setContent {
            PlurxTheme {
                MediaRow(
                    title = TOP_SHELF,
                    items = cards("Top", 3),
                    posterWidth = 96.dp,
                    onOpen = {},
                )
            }
        }

        // The doubled `.clickable(...).focusable()` put an unclickable focus
        // target in front of the clickable one: focus landed, centre press did
        // nothing.
        val card = compose.onNodeWithText("Top 1")
        card.performSemanticsAction(SemanticsActions.RequestFocus)
        card.assertIsFocused()
        assertTrue(
            "The focused poster must be the node that also handles the press",
            card.fetchSemanticsNode().config.contains(SemanticsActions.OnClick),
        )
    }

    private companion object {
        const val TOP_SHELF = "Continue watching"
        const val BOTTOM_SHELF = "Recently added"
        const val SHELF_LENGTH = 30
    }
}
