package tv.plurx.app.ui.components

import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.test.SemanticsMatcher
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.assertIsFocused
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import tv.plurx.app.data.ConnectionAction
import tv.plurx.app.data.ConnectionFailure
import tv.plurx.app.data.copyFor
import tv.plurx.app.ui.theme.PlurxTheme

/**
 * The contract's `every_error_offers_retry` says every surface that renders a
 * class must *expose* its actions — which is a claim about pixels, not about
 * the copy table, so no JVM test can make it. These are the tests that fail
 * when the action row is deleted, when Retry stops taking initial D-pad focus,
 * or when the rendered strings stop being the contract's.
 */
class ConnectionErrorStateTest {
    @get:Rule
    val compose = createComposeRule()

    @Test
    fun theFullSurfaceRendersTheContractsTitleDetailAndEveryAction() {
        val copy = copyFor(ConnectionFailure.UNKNOWN_HOST, "Living Room")
        compose.setContent {
            PlurxTheme {
                ConnectionErrorState(
                    failure = ConnectionFailure.UNKNOWN_HOST,
                    server = "Living Room",
                    onRetry = {},
                    onChangeServer = {},
                )
            }
        }

        compose.onNodeWithText(copy.title).assertIsDisplayed()
        compose.onNodeWithText(copy.detail).assertIsDisplayed()
        // `unknown_host` earns change_server as well as retry, and both must
        // be on screen — not merely in the copy object.
        assertEquals(
            listOf(ConnectionAction.Retry, ConnectionAction.ChangeServer),
            copy.actions,
        )
        copy.actions.forEach { action ->
            compose.onNodeWithText(action.label).assertIsDisplayed()
        }
    }

    /**
     * A ten-foot error state whose button the D-pad cannot reach is the same
     * as an error state with no button. Same guarantee `DetailInitialFocusTest`
     * makes for the play button.
     */
    @Test
    fun retryTakesInitialDpadFocus() {
        compose.setContent {
            PlurxTheme {
                ConnectionErrorState(
                    failure = ConnectionFailure.UNREACHABLE,
                    server = "Living Room",
                    onRetry = {},
                    onChangeServer = {},
                )
            }
        }

        val retry = compose.onNodeWithText(ConnectionAction.Retry.label)
        compose.waitUntil(timeoutMillis = 2_000) {
            retry.fetchSemanticsNode().config.getOrElse(SemanticsProperties.Focused) { false }
        }
        retry.assertIsFocused()
    }

    @Test
    fun retryAndChangeServerInvokeTheirHandlers() {
        var retried = 0
        var changed = 0
        compose.setContent {
            PlurxTheme {
                ConnectionErrorState(
                    failure = ConnectionFailure.INSECURE,
                    server = "Living Room",
                    onRetry = { retried++ },
                    onChangeServer = { changed++ },
                )
            }
        }

        compose.onNodeWithText(ConnectionAction.Retry.label).performClick()
        compose.onNodeWithText(ConnectionAction.ChangeServer.label).performClick()
        assertEquals(1, retried)
        assertEquals(1, changed)
    }

    /**
     * A surface with nowhere to send the viewer draws only the actions it can
     * honour — a dead "Change server" is worse than one action.
     */
    @Test
    fun changeServerIsOmittedWhenTheSurfaceCannotHonourIt() {
        compose.setContent {
            PlurxTheme {
                ConnectionErrorState(
                    failure = ConnectionFailure.UNREACHABLE,
                    server = "Living Room",
                    onRetry = {},
                )
            }
        }

        compose.onNodeWithText(ConnectionAction.Retry.label).assertIsDisplayed()
        compose.onAllNodesWithText(ConnectionAction.ChangeServer.label).assertCountEquals(0)
    }

    /**
     * `cached_content_wins`: the banner is a surface too, so it carries Retry.
     * It renders `short`, never `title`/`detail`.
     */
    @Test
    fun theBannerRendersShortCopyAndStillOffersRetry() {
        val copy = copyFor(ConnectionFailure.TIMEOUT, "Living Room")
        var retried = 0
        compose.setContent {
            PlurxTheme {
                ConnectionErrorBanner(
                    failure = ConnectionFailure.TIMEOUT,
                    server = "Living Room",
                    onRetry = { retried++ },
                )
            }
        }

        compose.onNodeWithText(copy.short).assertIsDisplayed()
        compose.onNodeWithText(ConnectionAction.Retry.label).assertIsDisplayed().performClick()
        assertEquals(1, retried)
    }

    /** No surface may leak the native text the classifier was handed. */
    @Test
    fun noRenderedNodeContainsNativeTransportText() {
        compose.setContent {
            PlurxTheme {
                ConnectionErrorState(
                    failure = ConnectionFailure.UNKNOWN_HOST,
                    server = "Living Room",
                    onRetry = {},
                    onChangeServer = {},
                )
            }
        }

        val texts = compose.onAllNodes(SemanticsMatcher.keyIsDefined(SemanticsProperties.Text))
            .fetchSemanticsNodes()
            .flatMap { node ->
                node.config.getOrElse(SemanticsProperties.Text) { emptyList() }.map { it.text }
            }
        assertTrue("the error state rendered no text at all", texts.isNotEmpty())
        assertTrue(
            "a rendered node leaked native transport text: $texts",
            texts.none { it.contains("Unable to resolve host") },
        )
    }
}
