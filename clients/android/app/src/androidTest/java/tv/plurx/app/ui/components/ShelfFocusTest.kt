package tv.plurx.app.ui.components

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusProperties
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.input.key.Key
import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.test.SemanticsNodeInteraction
import androidx.compose.ui.test.assertIsFocused
import androidx.compose.ui.test.assertIsNotFocused
import androidx.compose.ui.test.hasAnyAncestor
import androidx.compose.ui.test.hasTestTag
import androidx.compose.ui.test.isFocused
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.test.performKeyInput
import androidx.compose.ui.test.performScrollToIndex
import androidx.compose.ui.test.performSemanticsAction
import androidx.compose.ui.test.pressKey
import androidx.compose.ui.unit.dp
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import tv.plurx.app.data.Item
import tv.plurx.app.ui.theme.PlurxTheme

/**
 * The Android TV focus graph, walked with a D-pad.
 *
 * CLIENTS-REMEDIATION-PLAN.md §7.1 asks for a reproduction that "passes
 * against the fix and fails against the old graph". Every movement below is
 * therefore a real directional key event — `pressKey(Key.DirectionUp/Down)`
 * dispatched at the root, which is what a remote produces — and never a bare
 * `FocusRequester.requestFocus()`. A direct `requestFocus()` skips both halves
 * of what is under test: it never consults `focusProperties`, and it never
 * asks whether the destination those properties name is still attached.
 * Seeding the *starting* focus with `SemanticsActions.RequestFocus` is fine —
 * it stands in for the viewer having walked there already — but every move
 * after it is a key press.
 *
 * The two failures these pin, and where each one bites:
 *
 * 1. **A requester on a disposed card.** A shelf's `FocusRequester` used to
 *    ride on the card at index 0, which a `LazyRow` disposes as soon as the
 *    viewer scrolls past it. Its neighbours went on aiming `up`/`down` there,
 *    so one horizontal scroll turned every vertical press into
 *    `requestFocus()` on a requester attached to nothing —
 *    `IllegalStateException: FocusRequester is not initialized`, or a dead
 *    press. `dpadUp…ScrolledAway` and `dpadDown…ScrolledAway` scroll exactly
 *    that card out and then press towards the shelf.
 *
 * 2. **Focus properties declared where no card can read them.** A card's
 *    `FocusTargetNode` collects properties with
 *    `visitSelfAndAncestors(FocusProperties, untilType = FocusTarget)`, so the
 *    walk stops at the first ancestor focus target — and a `LazyRow` has one
 *    inside it, because `ScrollableNode` delegates a
 *    `FocusTargetModifierNode(Focusability.Never)`. A `focusProperties` block
 *    on the `LazyRow` modifier is therefore never seen by the cards it
 *    contains. `dpadUpFromACardReachesItsOwnShelfsViewAll` and
 *    `dpadWalksDownThroughTheGroupByPickerAndBackUp` each name a destination
 *    that spatial search would *not* choose, so they fail the moment the
 *    declared order stops being consulted.
 *
 * None of this can run in CI: there is no device here. It is written to be run
 * against a TV profile, and `centrePressOpensTheFocusedCard` is labelled below
 * as a regression guard rather than a reproduction.
 */
class ShelfFocusTest {
    @get:Rule
    val compose = createComposeRule()

    private fun cards(prefix: String, count: Int): List<Item> =
        (1..count).map { Item(id = "$prefix-$it".hashCode().toLong(), kind = "movie", title = "$prefix $it") }

    /** A press on the remote: dispatched at the root, routed to whatever holds focus. */
    private fun press(key: Key) {
        compose.onRoot().performKeyInput { pressKey(key) }
        compose.waitForIdle()
    }

    /** Stand-in for "the viewer already walked here"; every *move* after it is directional. */
    private fun SemanticsNodeInteraction.seedFocus(): SemanticsNodeInteraction {
        performSemanticsAction(SemanticsActions.RequestFocus)
        compose.waitForIdle()
        assertIsFocused()
        return this
    }

    private fun assertFocusIsInsideShelf(title: String) {
        assertEquals(
            "Expected exactly one focused node, inside the \"$title\" shelf",
            1,
            compose.onAllNodes(isFocused() and hasAnyAncestor(hasTestTag(shelfTestTag(title))))
                .fetchSemanticsNodes().size,
        )
    }

    /** Two shelves wired the way HomeScreen wires consecutive rows. */
    private fun twoShelves(topLength: Int, bottomLength: Int) {
        val top = FocusRequester()
        val bottom = FocusRequester()
        compose.setContent {
            PlurxTheme {
                Column(Modifier.fillMaxSize()) {
                    MediaRow(
                        title = TOP_SHELF,
                        items = cards("Top", topLength),
                        posterWidth = 96.dp,
                        rowFocusRequester = top,
                        nextRowFocusRequester = bottom,
                        onOpen = {},
                    )
                    MediaRow(
                        title = BOTTOM_SHELF,
                        items = cards("Bottom", bottomLength),
                        posterWidth = 96.dp,
                        rowFocusRequester = bottom,
                        previousRowFocusRequester = top,
                        onOpen = {},
                    )
                }
            }
        }
    }

    /**
     * Press UP into a shelf whose first card no longer exists.
     *
     * Against the old graph this is the crash: `up` resolved to the top shelf's
     * requester, that requester was attached to "Top 1", and "Top 1" was
     * disposed by the scroll — `FocusRequester` has no node left to hand focus
     * to. Against the fix the requester is on the row container, which a
     * horizontal scroll cannot dispose, and the row passes focus to a card that
     * is on screen.
     */
    @Test
    fun dpadUpEntersAShelfWhoseFirstCardHasScrolledAway() {
        twoShelves(topLength = SHELF_LENGTH, bottomLength = 8)

        compose.onNodeWithTag(shelfTestTag(TOP_SHELF)).performScrollToIndex(SHELF_LENGTH - 1)
        compose.waitForIdle()
        compose.onNodeWithText("Top 1").assertDoesNotExist()

        compose.onNodeWithText("Bottom 1").seedFocus()
        press(Key.DirectionUp)

        compose.onNodeWithText("Bottom 1").assertIsNotFocused()
        assertFocusIsInsideShelf(TOP_SHELF)
    }

    /** The same defect on the other direction of travel: `down` into a scrolled shelf. */
    @Test
    fun dpadDownEntersAShelfWhoseFirstCardHasScrolledAway() {
        twoShelves(topLength = 8, bottomLength = SHELF_LENGTH)

        compose.onNodeWithTag(shelfTestTag(BOTTOM_SHELF)).performScrollToIndex(SHELF_LENGTH - 1)
        compose.waitForIdle()
        compose.onNodeWithText("Bottom 1").assertDoesNotExist()

        compose.onNodeWithText("Top 1").seedFocus()
        press(Key.DirectionDown)

        compose.onNodeWithText("Top 1").assertIsNotFocused()
        assertFocusIsInsideShelf(BOTTOM_SHELF)
    }

    /**
     * The declared order has to beat the geometry, or it is not being read.
     *
     * "View all" sits in the shelf's header, hard right. This starts from the
     * leftmost card of the shelf, and what lies directly above that card is a
     * card in the shelf above — which is what Compose's spatial search picks,
     * because that card overlaps the source's beam and "View all" does not. So
     * the assertion only holds while the shelf's own `up = viewAllFocusRequester`
     * is reaching the card. It does not hold when that block is declared on the
     * `LazyRow` modifier, where `ScrollableNode`'s focus target hides it from
     * every card in the row.
     *
     * The last phase repeats the press from a card that did not exist when the
     * shelf was first laid out — the case the "one declaration governs every
     * card" claim was really about.
     */
    @Test
    fun dpadUpFromACardReachesItsOwnShelfsViewAll() {
        val above = FocusRequester()
        val shelf = FocusRequester()
        compose.setContent {
            PlurxTheme {
                Column(Modifier.fillMaxSize()) {
                    MediaRow(
                        title = TOP_SHELF,
                        items = cards("Above", 8),
                        posterWidth = 96.dp,
                        rowFocusRequester = above,
                        nextRowFocusRequester = shelf,
                        onOpen = {},
                    )
                    MediaRow(
                        title = BOTTOM_SHELF,
                        items = cards("Card", SHELF_LENGTH),
                        posterWidth = 96.dp,
                        onViewAll = {},
                        rowFocusRequester = shelf,
                        previousRowFocusRequester = above,
                        onOpen = {},
                    )
                }
            }
        }

        compose.onNodeWithText("Card 1").seedFocus()
        press(Key.DirectionUp)
        compose.onNodeWithText("View all").assertIsFocused()

        // …and from "View all" the walk carries on into the shelf above.
        press(Key.DirectionUp)
        compose.onNodeWithText("View all").assertIsNotFocused()
        assertFocusIsInsideShelf(TOP_SHELF)

        // A card composed only after a scroll carries the same order.
        compose.onNodeWithTag(shelfTestTag(BOTTOM_SHELF)).performScrollToIndex(SHELF_LENGTH - 1)
        compose.waitForIdle()
        compose.onNodeWithText("Card $SHELF_LENGTH").seedFocus()
        press(Key.DirectionUp)
        compose.onNodeWithText("View all").assertIsFocused()
    }

    /**
     * The "Group by" picker is a stop on the vertical chain, not a touch-only
     * control.
     *
     * Wired the way HomeScreen wires it: right-aligned, a third of the width,
     * with the shelf above and the shelf below naming it as their `down`/`up`.
     * From the leftmost card of the shelf above, spatial search would step
     * straight past it into the shelf below — the picker is outside that card's
     * beam and the full-width shelf is inside it. The press has to land on the
     * picker, carry on down, and come all the way back.
     */
    @Test
    fun dpadWalksDownThroughTheGroupByPickerAndBackUp() {
        val top = FocusRequester()
        val picker = FocusRequester()
        val bottom = FocusRequester()
        compose.setContent {
            PlurxTheme {
                Column(Modifier.fillMaxSize()) {
                    MediaRow(
                        title = TOP_SHELF,
                        items = cards("Top", 8),
                        posterWidth = 96.dp,
                        rowFocusRequester = top,
                        nextRowFocusRequester = picker,
                        onOpen = {},
                    )
                    Row(
                        Modifier.fillMaxWidth().padding(horizontal = 20.dp, vertical = 12.dp),
                        horizontalArrangement = Arrangement.End,
                    ) {
                        ChoicePicker(
                            label = "Group by",
                            value = GROUPING_LIBRARY,
                            options = listOf(GROUPING_LIBRARY, "Category"),
                            optionLabel = { it },
                            onSelect = {},
                            modifier = Modifier
                                .fillMaxWidth(.32f)
                                .focusRequester(picker)
                                .focusProperties {
                                    up = top
                                    down = bottom
                                },
                        )
                    }
                    MediaRow(
                        title = BOTTOM_SHELF,
                        items = cards("Bottom", 8),
                        posterWidth = 96.dp,
                        rowFocusRequester = bottom,
                        previousRowFocusRequester = picker,
                        onOpen = {},
                    )
                }
            }
        }

        compose.onNodeWithText("Top 1").seedFocus()

        press(Key.DirectionDown)
        compose.onNodeWithText(GROUPING_LIBRARY).assertIsFocused()

        press(Key.DirectionDown)
        compose.onNodeWithText("Bottom 1").assertIsFocused()

        press(Key.DirectionUp)
        compose.onNodeWithText(GROUPING_LIBRARY).assertIsFocused()

        press(Key.DirectionUp)
        compose.onNodeWithText("Top 1").assertIsFocused()
    }

    /**
     * A regression guard, not a reproduction — and it says so.
     *
     * The doubled `.clickable(...).focusable()` on a poster was removed as
     * hygiene, and this test would pass against that older card too: focus
     * search collects the *outermost* focus target on a layout node, which was
     * `clickable`'s, and even if the inner `focusable()` had won, `clickable`'s
     * key handler sits above it in the same chain and would still see the
     * bubbled press. There is no observable difference left to assert, so this
     * pins the behaviour the collapse was meant to protect instead: the node
     * the D-pad focuses is the node that handles the centre press.
     */
    @Test
    fun centrePressOpensTheFocusedCard() {
        var opened: String? = null
        compose.setContent {
            PlurxTheme {
                MediaRow(
                    title = TOP_SHELF,
                    items = cards("Top", 3),
                    posterWidth = 96.dp,
                    onOpen = { opened = it.title },
                )
            }
        }

        compose.onNodeWithText("Top 2").seedFocus()
        press(Key.DirectionCenter)

        assertEquals("The focused poster must be the node that handles the press", "Top 2", opened)
    }

    private companion object {
        const val TOP_SHELF = "Continue watching"
        const val BOTTOM_SHELF = "Recently added"
        const val GROUPING_LIBRARY = "Library"
        const val SHELF_LENGTH = 30
    }
}
