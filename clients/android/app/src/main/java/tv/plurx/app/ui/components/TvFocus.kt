package tv.plurx.app.ui.components

import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.Button
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.composed
import androidx.compose.ui.draw.scale
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.semantics.SemanticsPropertyKey
import androidx.compose.ui.semantics.SemanticsPropertyReceiver
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.zIndex

internal val TvFocusVisibleKey = SemanticsPropertyKey<Boolean>("TvFocusVisible")
internal var SemanticsPropertyReceiver.tvFocusVisible by TvFocusVisibleKey

/** Requests focus after the target has been attached and laid out for a frame. */
@Composable
fun RequestInitialFocus(focusRequester: FocusRequester, enabled: Boolean = true) {
    val view = LocalView.current
    DisposableEffect(view, focusRequester, enabled) {
        val request = Runnable { focusRequester.requestFocus() }
        if (enabled) view.postOnAnimation(request)
        onDispose { view.removeCallbacks(request) }
    }
}

@Composable
internal fun tvFocusGradient(): Brush = Brush.linearGradient(
    listOf(
        MaterialTheme.colorScheme.primary.copy(alpha = 0.88f),
        MaterialTheme.colorScheme.onBackground.copy(alpha = 0.72f),
        MaterialTheme.colorScheme.secondary.copy(alpha = 0.88f),
    ),
)

/**
 * A clearly visible focus treatment for ten-foot/D-pad navigation.
 *
 * Material 3's phone components accept focus but do not draw a focused state,
 * which made Android TV navigation move invisibly. This modifier observes the
 * component's focus target and adds a theme-aware gradient ring, subtle fill, scale,
 * and z-order lift. It is harmless on touch devices because it only appears
 * while keyboard or D-pad focus is present.
 */
fun Modifier.tvFocusRing(
    shape: Shape? = null,
    focusedScale: Float = 1.06f,
    ringWidth: Dp = 2.dp,
    showRing: Boolean = true,
): Modifier = composed {
    var focused by remember { mutableStateOf(false) }
    val scale by animateFloatAsState(if (focused) focusedScale else 1f, label = "tv-focus-scale")
    val resolvedShape = shape ?: MaterialTheme.shapes.medium
    val ring = tvFocusGradient()
    val fill = MaterialTheme.colorScheme.primary.copy(alpha = 0.10f)

    this
        .onFocusChanged { focused = it.hasFocus }
        .zIndex(if (focused) 1f else 0f)
        .scale(scale)
        .background(if (focused) fill else Color.Transparent, resolvedShape)
        .then(if (focused && showRing) Modifier.border(ringWidth, ring, resolvedShape) else Modifier)
        .semantics { tvFocusVisible = focused }
}

@Composable
fun TvButton(
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    content: @Composable RowScope.() -> Unit,
) {
    val shape = MaterialTheme.shapes.extraLarge
    Button(
        onClick = onClick,
        modifier = modifier.tvFocusRing(shape),
        enabled = enabled,
        shape = shape,
        content = content,
    )
}

@Composable
fun TvOutlinedButton(
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    content: @Composable RowScope.() -> Unit,
) {
    val shape = MaterialTheme.shapes.extraLarge
    OutlinedButton(
        onClick = onClick,
        modifier = modifier.tvFocusRing(shape),
        enabled = enabled,
        shape = shape,
        content = content,
    )
}

@Composable
fun TvTextButton(
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    content: @Composable RowScope.() -> Unit,
) {
    val shape = MaterialTheme.shapes.extraLarge
    TextButton(
        onClick = onClick,
        modifier = modifier.tvFocusRing(shape),
        enabled = enabled,
        shape = shape,
        content = content,
    )
}

@Composable
fun TvIconButton(
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    showFocusRing: Boolean = true,
    content: @Composable () -> Unit,
) {
    IconButton(
        onClick = onClick,
        modifier = modifier.tvFocusRing(CircleShape, focusedScale = 1.12f, showRing = showFocusRing),
        enabled = enabled,
        content = content,
    )
}
