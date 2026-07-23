package tv.plurx.app.ui.theme

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.sp

// noirr palette — the same ink-dark base and signal-red accent as the web UI.
val Bg = Color(0xFF0A0A0C)
val Surface = Color(0xFF141418)
val SurfaceHi = Color(0xFF1C1C22)
val Accent = Color(0xFFE5484D)
val OnBg = Color(0xFFECECEF)
val Muted = Color(0xFF8A8A94)
val Outline = Color(0xFF2A2A31)

private val PlurxColors = darkColorScheme(
    primary = Accent,
    onPrimary = Color.White,
    secondary = Accent,
    onSecondary = Color.White,
    background = Bg,
    onBackground = OnBg,
    surface = Surface,
    onSurface = OnBg,
    surfaceVariant = SurfaceHi,
    onSurfaceVariant = Muted,
    outline = Outline,
    error = Accent,
)

// Monospace throughout keeps the "terminal/CLI" character of the brand without
// bundling a custom font file (the device monospace face renders everywhere).
private val Mono = FontFamily.Monospace

val PlurxTypography = Typography(
    headlineMedium = TextStyle(fontFamily = Mono, fontWeight = FontWeight.Bold, fontSize = 24.sp),
    titleLarge = TextStyle(fontFamily = Mono, fontWeight = FontWeight.Bold, fontSize = 20.sp),
    titleMedium = TextStyle(fontFamily = Mono, fontWeight = FontWeight.SemiBold, fontSize = 16.sp),
    bodyLarge = TextStyle(fontFamily = Mono, fontSize = 15.sp),
    bodyMedium = TextStyle(fontFamily = Mono, fontSize = 14.sp),
    labelLarge = TextStyle(fontFamily = Mono, fontWeight = FontWeight.SemiBold, fontSize = 14.sp),
    labelMedium = TextStyle(fontFamily = Mono, fontSize = 12.sp),
)

@Composable
fun PlurxTheme(content: @Composable () -> Unit) {
    MaterialTheme(
        colorScheme = PlurxColors,
        typography = PlurxTypography,
        content = content,
    )
}
