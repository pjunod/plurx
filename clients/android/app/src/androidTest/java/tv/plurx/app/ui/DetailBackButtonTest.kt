package tv.plurx.app.ui

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.ui.Modifier
import androidx.compose.ui.test.assertHasClickAction
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.getUnclippedBoundsInRoot
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.unit.dp
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test

class DetailBackButtonTest {
    @get:Rule
    val compose = createComposeRule()

    @Test
    fun backButtonClearsTopAndSideSystemInsets() {
        val topInset = 32.dp
        val sideInset = 18.dp

        compose.setContent {
            Box(Modifier.fillMaxSize()) {
                DetailBackButton(
                    onBack = {},
                    safeInsets = WindowInsets(left = sideInset, top = topInset),
                )
            }
        }

        val back = compose.onNodeWithContentDescription("Back")
            .assertIsDisplayed()
            .assertHasClickAction()
        val bounds = back.getUnclippedBoundsInRoot()

        assertTrue("Back button must clear the status bar", bounds.top >= topInset)
        assertTrue("Back button must clear a side display cutout", bounds.left >= sideInset)
    }
}
