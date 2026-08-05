package tv.plurx.app.player

import android.graphics.Bitmap
import androidx.compose.runtime.mutableStateOf
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.test.SemanticsMatcher
import androidx.compose.ui.test.assert
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onAllNodesWithContentDescription
import androidx.compose.ui.test.onNodeWithContentDescription
import org.junit.Rule
import org.junit.Test

class PGSOverlayComposeTest {
    @get:Rule
    val compose = createComposeRule()

    @Test
    fun overlaySwitchAndOffReplaceOnlyTheActiveComposition() {
        val generation = "a".repeat(64)
        val object_ = PGSOverlayObject(
            image = "overlay/$generation/objects/${"b".repeat(64)}.png",
            x = 10,
            y = 10,
            width = 2,
            height = 2,
        )
        val bitmap = Bitmap.createBitmap(2, 2, Bitmap.Config.ARGB_8888)
        fun frame(id: String, revision: Long) = PGSOverlayFrame(
            revision = revision,
            cue = PGSOverlayCue(id, 0, 1_000, 1920, 1080, listOf(object_)),
            objects = listOf(PGSOverlayRenderedObject(object_, bitmap)),
        )
        val selected = mutableStateOf<PGSOverlayFrame?>(frame("first", 1))

        compose.setContent {
            PGSBitmapOverlay(
                frame = selected.value,
                videoAspectRatio = 16f / 9f,
                modifier = Modifier,
            )
        }

        val overlay = compose.onNodeWithContentDescription("PGS subtitle overlay")
        compose.onAllNodesWithContentDescription("PGS subtitle overlay").assertCountEquals(1)
        overlay.assert(
            SemanticsMatcher.expectValue(SemanticsProperties.StateDescription, "first"),
        )

        compose.runOnIdle { selected.value = frame("second", 2) }
        overlay.assert(
            SemanticsMatcher.expectValue(SemanticsProperties.StateDescription, "second"),
        )

        compose.runOnIdle { selected.value = null }
        compose.onAllNodesWithContentDescription("PGS subtitle overlay").assertCountEquals(0)
    }
}
