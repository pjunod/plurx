package tv.plurx.app.ui.components

import androidx.compose.material3.Text
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.SemanticsActions
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
}
