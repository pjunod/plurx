package tv.plurx.app.ui.theme

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Shapes
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.ReadOnlyComposable
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import tv.plurx.app.data.Appearance
import tv.plurx.app.data.ThemeId

data class ViewerColors(
    val background: Color,
    val surface: Color,
    val surfaceHigh: Color,
    val outline: Color,
    val text: Color,
    val muted: Color,
    val accent: Color,
    val accentSecondary: Color,
    val good: Color,
    val warning: Color,
    val bad: Color,
)

private val ClassicDark = ViewerColors(
    background = Color(0xFF0E0F13), surface = Color(0xFF171922),
    surfaceHigh = Color(0xFF1E2230), outline = Color(0xFF2A2F3E),
    text = Color(0xFFE8EAF0), muted = Color(0xFF9AA2B4),
    accent = Color(0xFF6EA8FE), accentSecondary = Color(0xFF8B7CF6),
    good = Color(0xFF4ADE80), warning = Color(0xFFFBBF24), bad = Color(0xFFF87171),
)

private val ClassicLight = ViewerColors(
    background = Color(0xFFF6F7F9), surface = Color.White,
    surfaceHigh = Color(0xFFEEF1F6), outline = Color(0xFFDDE2EA),
    text = Color(0xFF1A1D24), muted = Color(0xFF5B6472),
    accent = Color(0xFF2F6FE0), accentSecondary = Color(0xFF6D5BD0),
    good = Color(0xFF1A9D5A), warning = Color(0xFFB45309), bad = Color(0xFFD64550),
)

private val TerminalDark = ViewerColors(
    background = Color(0xFF050705), surface = Color(0xFF0B100B),
    surfaceHigh = Color(0xFF111A12), outline = Color(0xFF1E3020),
    text = Color(0xFFC9E8C0), muted = Color(0xFF7A9670),
    accent = Color(0xFF3FE170), accentSecondary = Color(0xFF2AA858),
    good = Color(0xFF3FE170), warning = Color(0xFFD9C25A), bad = Color(0xFFFF7066),
)

private val TerminalLight = ViewerColors(
    background = Color(0xFFEEE8D5), surface = Color(0xFFFDF6E3),
    surfaceHigh = Color(0xFFE6DFC8), outline = Color(0xFFD5CDB4),
    text = Color(0xFF073642), muted = Color(0xFF657B83),
    accent = Color(0xFF6E7F00), accentSecondary = Color(0xFF268BD2),
    good = Color(0xFF859900), warning = Color(0xFFB58900), bad = Color(0xFFDC322F),
)

private val NoirrDark = ViewerColors(
    background = Color(0xFF0A0A0C), surface = Color(0xFF101014),
    surfaceHigh = Color(0xFF16161B), outline = Color(0xFF29292E),
    text = Color(0xFFEDEDEF), muted = Color(0xFF9A9AA3),
    accent = Color(0xFFE5484D), accentSecondary = Color(0xFF8E1C22),
    good = Color(0xFF5FB582), warning = Color(0xFFD9A05B), bad = Color(0xFFFF7A66),
)

private val NoirrLight = ViewerColors(
    background = Color(0xFFF2EFE8), surface = Color(0xFFFAF8F2),
    surfaceHigh = Color.White, outline = Color(0xFFDEDAD2),
    text = Color(0xFF1A1A1E), muted = Color(0xFF5D5C63),
    accent = Color(0xFFC2343A), accentSecondary = Color(0xFF8E1C22),
    good = Color(0xFF35855C), warning = Color(0xFFA3742B), bad = Color(0xFFC96442),
)

private val LocalViewerColors = staticCompositionLocalOf { NoirrDark }

val Bg: Color
    @Composable @ReadOnlyComposable get() = LocalViewerColors.current.background
val Surface: Color
    @Composable @ReadOnlyComposable get() = LocalViewerColors.current.surface
val SurfaceHi: Color
    @Composable @ReadOnlyComposable get() = LocalViewerColors.current.surfaceHigh
val Accent: Color
    @Composable @ReadOnlyComposable get() = LocalViewerColors.current.accent
val OnBg: Color
    @Composable @ReadOnlyComposable get() = LocalViewerColors.current.text
val Muted: Color
    @Composable @ReadOnlyComposable get() = LocalViewerColors.current.muted
val Outline: Color
    @Composable @ReadOnlyComposable get() = LocalViewerColors.current.outline

private fun typography(theme: ThemeId): Typography {
    val family = if (theme == ThemeId.Terminal) FontFamily.Monospace else FontFamily.SansSerif
    return Typography(
        headlineMedium = TextStyle(fontFamily = family, fontWeight = FontWeight.Bold, fontSize = 26.sp),
        titleLarge = TextStyle(fontFamily = family, fontWeight = FontWeight.Bold, fontSize = 22.sp),
        titleMedium = TextStyle(fontFamily = family, fontWeight = FontWeight.SemiBold, fontSize = 17.sp),
        bodyLarge = TextStyle(fontFamily = family, fontSize = 16.sp),
        bodyMedium = TextStyle(fontFamily = family, fontSize = 14.sp, lineHeight = 21.sp),
        labelLarge = TextStyle(fontFamily = family, fontWeight = FontWeight.SemiBold, fontSize = 14.sp),
        labelMedium = TextStyle(fontFamily = family, fontSize = 12.sp),
    )
}

@Composable
fun PlurxTheme(
    theme: ThemeId = ThemeId.Classic,
    appearance: Appearance = Appearance.System,
    content: @Composable () -> Unit,
) {
    val dark = when (appearance) {
        Appearance.System -> isSystemInDarkTheme()
        Appearance.Light -> false
        Appearance.Dark -> true
    }
    val colors = when (theme) {
        ThemeId.Classic -> if (dark) ClassicDark else ClassicLight
        ThemeId.Terminal -> if (dark) TerminalDark else TerminalLight
        ThemeId.Noirr -> if (dark) NoirrDark else NoirrLight
    }
    val scheme = if (dark) {
        darkColorScheme(
            primary = colors.accent, onPrimary = Color.White,
            secondary = colors.accentSecondary, background = colors.background,
            onBackground = colors.text, surface = colors.surface, onSurface = colors.text,
            surfaceVariant = colors.surfaceHigh, onSurfaceVariant = colors.muted,
            outline = colors.outline, error = colors.bad,
        )
    } else {
        lightColorScheme(
            primary = colors.accent, onPrimary = Color.White,
            secondary = colors.accentSecondary, background = colors.background,
            onBackground = colors.text, surface = colors.surface, onSurface = colors.text,
            surfaceVariant = colors.surfaceHigh, onSurfaceVariant = colors.muted,
            outline = colors.outline, error = colors.bad,
        )
    }
    val radius = if (theme == ThemeId.Terminal) 0.dp else if (theme == ThemeId.Noirr) 10.dp else 12.dp
    androidx.compose.runtime.CompositionLocalProvider(LocalViewerColors provides colors) {
        MaterialTheme(
            colorScheme = scheme,
            typography = typography(theme),
            shapes = Shapes(
                extraSmall = RoundedCornerShape(radius), small = RoundedCornerShape(radius),
                medium = RoundedCornerShape(radius), large = RoundedCornerShape(radius),
                extraLarge = RoundedCornerShape(radius),
            ),
            content = content,
        )
    }
}
