package tv.plurx.app.ui

import androidx.compose.foundation.layout.Column
import androidx.compose.material3.Text
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.test.assertIsFocused
import androidx.compose.ui.test.assertIsNotFocused
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import org.junit.Rule
import org.junit.Test
import tv.plurx.app.ui.theme.PlurxTheme

class DetailInitialFocusTest {
    @get:Rule
    val compose = createComposeRule()

    @Test
    fun primaryActionReceivesInitialTvFocusInsteadOfBackButton() {
        compose.setContent {
            PlurxTheme {
                Column {
                    DetailBackButton(onBack = {})
                    DetailPrimaryActionButton(
                        onClick = {},
                        requestInitialFocus = true,
                        modifier = Modifier.testTag("primary-action"),
                    ) {
                        Text("Resume")
                    }
                }
            }
        }

        val primary = compose.onNodeWithTag("primary-action")
        compose.waitUntil(timeoutMillis = 2_000) {
            primary.fetchSemanticsNode().config.getOrElse(SemanticsProperties.Focused) { false }
        }
        primary.assertIsFocused()
        compose.onNodeWithContentDescription("Back").assertIsNotFocused()
    }
}
