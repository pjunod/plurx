package tv.plurx.app.ui

import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.Text
import androidx.compose.ui.Modifier
import androidx.compose.ui.test.assertHasClickAction
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.getUnclippedBoundsInRoot
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.unit.dp
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import tv.plurx.app.ui.components.SafeBackButton
import tv.plurx.app.ui.components.SafeTopRow

class SystemSafeAreaTest {
    @get:Rule
    val compose = createComposeRule()

    @Test
    fun sharedTopRowClearsStatusBarAndSideCutout() {
        val topInset = 32.dp
        val sideInset = 18.dp

        compose.setContent {
            SafeTopRow(
                modifier = Modifier.fillMaxWidth(),
                safeInsets = WindowInsets(left = sideInset, top = topInset),
            ) {
                Text("Safe header")
            }
        }

        val header = compose.onNodeWithText("Safe header").assertIsDisplayed()
        val bounds = header.getUnclippedBoundsInRoot()
        assertTrue("Header must clear the status bar", bounds.top >= topInset)
        assertTrue("Header must clear a side display cutout", bounds.left >= sideInset)
    }

    @Test
    fun sharedBackButtonClearsStatusBarAndSideCutout() {
        val topInset = 32.dp
        val sideInset = 18.dp

        compose.setContent {
            SafeBackButton(
                onBack = {},
                safeInsets = WindowInsets(left = sideInset, top = topInset),
            )
        }

        val back = compose.onNodeWithContentDescription("Back")
            .assertIsDisplayed()
            .assertHasClickAction()
        val bounds = back.getUnclippedBoundsInRoot()
        assertTrue("Back button must clear the status bar", bounds.top >= topInset)
        assertTrue("Back button must clear a side display cutout", bounds.left >= sideInset)
    }
}
