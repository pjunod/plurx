package tv.plurx.app.ui

import android.content.res.Configuration
import androidx.compose.runtime.Composable
import androidx.compose.runtime.ReadOnlyComposable
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp

enum class FormFactor { Compact, Expanded, Television }

@Composable
@ReadOnlyComposable
fun currentFormFactor(): FormFactor {
    val configuration = LocalConfiguration.current
    val television = configuration.uiMode and Configuration.UI_MODE_TYPE_MASK ==
        Configuration.UI_MODE_TYPE_TELEVISION
    return when {
        television -> FormFactor.Television
        configuration.screenWidthDp >= 600 -> FormFactor.Expanded
        else -> FormFactor.Compact
    }
}

fun FormFactor.horizontalPadding(): Dp = when (this) {
    FormFactor.Compact -> 20.dp
    FormFactor.Expanded -> 32.dp
    FormFactor.Television -> 56.dp
}

fun FormFactor.posterScale(): Float = when (this) {
    FormFactor.Compact -> 1f
    FormFactor.Expanded -> 1.08f
    FormFactor.Television -> 1.25f
}
