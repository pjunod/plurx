package tv.plurx.app.ui.components

import androidx.compose.foundation.layout.Column
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Text
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.test.assertHeightIsEqualTo
import androidx.compose.ui.test.assertIsFocused
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.performSemanticsAction
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
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

    @Test
    fun tvButtonFocusBoundsMatchThePaintedButtonHeight() {
        compose.setContent {
            PlurxTheme {
                Column {
                    TvButton(onClick = {}, modifier = Modifier.testTag("filled")) { Text("Play") }
                    TvOutlinedButton(onClick = {}, modifier = Modifier.testTag("outlined")) { Text("Start over") }
                    TvTextButton(onClick = {}, modifier = Modifier.testTag("text")) { Text("Mark watched") }
                }
            }
        }

        listOf("filled", "outlined", "text").forEach { tag ->
            compose.onNodeWithTag(tag).assertHeightIsEqualTo(ButtonDefaults.MinHeight)
        }
    }
}
