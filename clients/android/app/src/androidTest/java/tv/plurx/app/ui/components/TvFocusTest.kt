package tv.plurx.app.ui.components

import androidx.compose.foundation.layout.Column
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Text
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.dp
import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.test.assertHeightIsEqualTo
import androidx.compose.ui.test.assertIsFocused
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.performSemanticsAction
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import tv.plurx.app.ui.FormFactor
import tv.plurx.app.ui.currentFormFactor
import tv.plurx.app.ui.theme.PlurxTheme

class TvFocusTest {
    @get:Rule
    val compose = createComposeRule()

    @Test
    fun focusedButtonPublishesItsVisibleFocusTreatment() {
        compose.setContent {
            PlurxTheme {
                TvOutlinedButton(onClick = {}, modifier = Modifier.testTag("tv-action")) {
                    Text("TV action")
                }
            }
        }

        val action = compose.onNodeWithTag("tv-action")
        action.performSemanticsAction(SemanticsActions.RequestFocus)
        action.assertIsFocused()
        assertTrue(
            "D-pad focus must activate the high-contrast focus treatment",
            action.fetchSemanticsNode().config[TvFocusVisibleKey],
        )
    }

    /**
     * The tight focus bounds are a ten-foot fix and are scoped to television
     * devices: on a phone the 48dp minimum interactive size is an accessibility
     * guarantee for fingers, and surrendering it there bought nothing, because
     * a phone has no focus ring to tighten.
     */
    @Test
    fun buttonHeightIsTheFocusRingOnTvAndTheTouchTargetEverywhereElse() {
        var television = false
        compose.setContent {
            PlurxTheme {
                television = currentFormFactor() == FormFactor.Television
                Column {
                    TvButton(onClick = {}, modifier = Modifier.testTag("filled")) { Text("Play") }
                    TvOutlinedButton(onClick = {}, modifier = Modifier.testTag("outlined")) { Text("Start over") }
                    TvTextButton(onClick = {}, modifier = Modifier.testTag("text")) { Text("Mark watched") }
                }
            }
        }

        val expected = if (television) ButtonDefaults.MinHeight else MINIMUM_TOUCH_TARGET
        listOf("filled", "outlined", "text").forEach { tag ->
            compose.onNodeWithTag(tag).assertHeightIsEqualTo(expected)
        }
    }

    private companion object {
        /** Material's `LocalMinimumInteractiveComponentSize` default. */
        val MINIMUM_TOUCH_TARGET = 48.dp
    }
}
