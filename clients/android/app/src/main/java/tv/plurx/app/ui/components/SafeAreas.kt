package tv.plurx.app.ui.components

import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.displayCutout
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.statusBars
import androidx.compose.foundation.layout.union
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material3.Icon
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp

/**
 * Status bars *and* the display cutout.
 *
 * The doc comments here have always claimed cutout safety; only the status bar
 * was ever in the number. On a landscape foldable or a hole-punch phone held
 * sideways the cutout is on a *side*, where the status-bar inset is zero — the
 * back button then sits under the camera. The union is the honest inset, and
 * it costs nothing on the devices (and every TV) where the cutout is empty.
 */
@Composable
internal fun safeDisplayInsets(): WindowInsets =
    WindowInsets.statusBars.union(WindowInsets.displayCutout)

/** A top action row whose content always clears status bars and display cutouts. */
@Composable
internal fun SafeTopRow(
    modifier: Modifier = Modifier,
    safeInsets: WindowInsets = safeDisplayInsets(),
    content: @Composable RowScope.() -> Unit,
) {
    Row(
        modifier = modifier.windowInsetsPadding(safeInsets),
        verticalAlignment = Alignment.CenterVertically,
        content = content,
    )
}

/** A full-screen overlay back button that remains visible and tappable around cutouts. */
@Composable
internal fun SafeBackButton(
    onBack: () -> Unit,
    modifier: Modifier = Modifier,
    safeInsets: WindowInsets = safeDisplayInsets(),
    tint: Color = Color.White,
) {
    TvIconButton(
        onClick = onBack,
        modifier = modifier.windowInsetsPadding(safeInsets).padding(8.dp),
        showFocusRing = false,
    ) {
        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back", tint = tint)
    }
}
