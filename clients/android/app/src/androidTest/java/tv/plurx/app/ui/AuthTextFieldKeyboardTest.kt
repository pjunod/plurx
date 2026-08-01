package tv.plurx.app.ui

import androidx.compose.ui.Modifier
import androidx.compose.ui.input.key.Key
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.test.assertIsFocused
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.performKeyInput
import androidx.compose.ui.test.performSemanticsAction
import androidx.compose.ui.test.pressKey
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import tv.plurx.app.ui.theme.PlurxTheme

class AuthTextFieldKeyboardTest {
    @get:Rule
    val compose = createComposeRule()

    @Test
    fun tvTraversalFocusWaitsForSelectBeforeEnteringEditMode() {
        compose.setContent {
            PlurxTheme {
                AuthTextField(
                    value = "",
                    onValueChange = {},
                    label = "Server address",
                    modifier = Modifier.testTag("server-address"),
                )
            }
        }

        val field = compose.onNodeWithTag("server-address")
        field.performSemanticsAction(SemanticsActions.RequestFocus)
        field.assertIsFocused()
        assertFalse(field.fetchSemanticsNode().config[AuthTextEditingKey])

        field.performKeyInput { pressKey(Key.DirectionCenter) }

        assertTrue(field.fetchSemanticsNode().config[AuthTextEditingKey])
    }
}
