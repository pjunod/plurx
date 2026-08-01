package tv.plurx.app.ui.components

import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.statusBars
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp

/** A top action row whose content always clears status bars and display cutouts. */
@Composable
internal fun SafeTopRow(
    modifier: Modifier = Modifier,
    safeInsets: WindowInsets = WindowInsets.statusBars,
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
    safeInsets: WindowInsets = WindowInsets.statusBars,
    tint: Color = Color.White,
) {
    IconButton(
        onClick = onBack,
        modifier = modifier.windowInsetsPadding(safeInsets).padding(8.dp),
    ) {
        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back", tint = tint)
    }
}
