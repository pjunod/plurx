package tv.plurx.app.ui.components

import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.test.assertIsFocused
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performSemanticsAction
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import tv.plurx.app.data.Appearance
import tv.plurx.app.ui.theme.PlurxTheme

class ChoicePickerFocusTest {
    @get:Rule
    val compose = createComposeRule()

    @Test
    fun paintedSelectorCarriesTheVisibleFocusTreatment() {
        compose.setContent {
            PlurxTheme(appearance = Appearance.Dark) {
                ChoicePicker(
                    label = "Theme",
                    value = "Classic",
                    options = listOf("Classic", "Terminal"),
                    optionLabel = { it },
                    onSelect = {},
                )
            }
        }

        val selector = compose.onNodeWithText("Classic")
        selector.performSemanticsAction(SemanticsActions.RequestFocus)
        selector.assertIsFocused()
        assertTrue(
            "The painted selector and its focus treatment must share one focus node",
            selector.fetchSemanticsNode().config[TvFocusVisibleKey],
        )
    }
}
